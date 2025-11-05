"""Real TCP retirement; worker/Ray calls are explicit in-process spies."""
from contextlib import contextmanager
from pathlib import Path
import socket
import threading
import time
import types
import unittest

from parrot_protocol import ray_gw as gateway, wire


class FragmentedSocket:
    def __init__(self, sock, limit):
        self.sock, self.limit, self.chunks = sock, limit, []

    def recv(self, size):
        chunk = self.sock.recv(min(size, self.limit))
        self.chunks.append(len(chunk))
        return chunk

    def __getattr__(self, name):
        return getattr(self.sock, name)


@contextmanager
def link(test, *, initiator=False, state=None, fragment=None, blocked=None):
    state = gateway.RayAdminState() if state is None else state
    baseline = gateway.METRICS.snapshot()['connections']
    writers = {t for t in threading.enumerate() if t.name == 'parrot-gateway-writer'}
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        listener.listen(1)
        peer = socket.create_connection(listener.getsockname(), timeout=2)
        local, _ = listener.accept()
    local = local if fragment is None else FragmentedSocket(local, fragment)
    peer.settimeout(2)
    calls, hints, failures = [], [], []
    ready = threading.Event()

    def ask(path, key, payload):
        calls.append(('ask', path, key, payload))
        if blocked is not None:
            entered, release, finished = blocked
            entered.set()
            if not release.wait(5):
                raise TimeoutError('test did not release worker')
            finished.set()
        if key == 'bin:fail':
            raise ValueError('business failure')
        return dict(ok=True, key=key, payload=payload)

    worker = types.SimpleNamespace(
        ask=types.SimpleNamespace(remote=lambda *args: lambda: ask(*args)),
        deliver=types.SimpleNamespace(remote=lambda *args: calls.append(('tell', *args))))
    ray = types.SimpleNamespace(get=lambda ref: ref())

    def serve():
        try:
            gateway._serve_connection_guarded(local, gateway.ParrotDispatcher(), worker, ray,
                state, initiator=initiator, route_hint=hints.append, ready=ready.set)
        except BaseException as error:
            failures.append(error)

    driver = threading.Thread(target=serve, name='parrot-terminal-driver')
    driver.start()
    decoder = wire.FrameDecoder()

    def receive():
        while True:
            frame = decoder.next_frame()
            if frame is not None:
                return frame
            try:
                data = peer.recv(65536)
            except ConnectionResetError:
                return None
            if not data:
                return None
            decoder.feed(data)

    def handshake(extra=b''):
        if initiator:
            test.assertEqual(receive().ft, wire.FT_HANDSHAKE)
        ft = wire.FT_HANDSHAKE_ACK if initiator else wire.FT_HANDSHAKE
        body = (wire.handshake_ack_body if initiator else wire.handshake_body)('terminal-peer')
        peer.sendall(wire.build_frame(ft, 1, '', '', body) + extra)
        if not initiator:
            test.assertEqual(receive().ft, wire.FT_HANDSHAKE_ACK)
        test.assertTrue(ready.wait(2), 'driver did not complete the handshake')

    try:
        yield types.SimpleNamespace(peer=peer, calls=calls, hints=hints, driver=driver,
            receive=receive, handshake=handshake, local=local, state=state)
    finally:
        if blocked is not None:
            blocked[1].set()
        try:
            peer.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        peer.close()
        driver.join(3)
        if driver.is_alive():
            local.shutdown(socket.SHUT_RDWR)
            driver.join(3)
        test.assertFalse(driver.is_alive(), 'connection driver escaped cleanup')
        test.assertEqual(failures, [], 'unexpected driver exception')
        test.assertEqual(gateway.METRICS.snapshot()['connections'], baseline, 'connection lease leaked')
        test.assertEqual({t for t in threading.enumerate() if t.name == 'parrot-gateway-writer'}, writers,
                         'connection writer escaped cleanup')


def heartbeat(cid=73):
    return wire.build_frame(wire.FT_HEARTBEAT, cid, '', '', b'')


def tell(payload=b'after'):
    return wire.build_frame(wire.FT_TELL, 0, '/user/probe', 'bin:probe', payload)


def terminal(payload=b'\x0c\0\0\0fatal', flags=0, cid=0):
    # Literal code point also tests the original implementation lacking FT_ERROR.
    return wire.build_frame(0x7f, cid, '', '', payload, flags)


class GatewayTerminalErrorTests(unittest.TestCase):
    def expect_terminal(self, session, encoded):
        session.peer.sendall(encoded + tell() + heartbeat())
        self.assertIsNone(session.receive(), 'terminal ERROR did not retire connection')
        session.driver.join(2)
        self.assertFalse(session.driver.is_alive(), 'terminal ERROR left driver alive')

    def test_payload_vectors_always_retire(self):
        vectors = Path(__file__).resolve().parents[3] / 'docs/vectors/error-payloads.tsv'
        rows = [line.split('\t') for line in vectors.read_text().splitlines() if line and not line.startswith('#')]
        self.assertEqual(len(rows), 30)
        for initiator in (False, True):
            for name, _, payload, _, _ in rows:
                with self.subTest(initiator=initiator, vector=name), link(self, initiator=initiator) as session:
                    session.handshake()
                    self.expect_terminal(session, terminal(bytes.fromhex(payload)))
                    self.assertEqual(session.calls, [], 'post-ERROR business effect')

    def test_every_flag_bit_still_retires(self):
        for initiator in (False, True):
            for flags in [1 << bit for bit in range(16)] + [0xffff]:
                with self.subTest(initiator=initiator, flags=flags), link(self, initiator=initiator) as session:
                    session.handshake()
                    self.expect_terminal(session, terminal(flags=flags))
                    self.assertEqual(session.calls, [])

    def test_fragmented_error_retires_before_following_tell(self):
        for initiator in (False, True):
            for size in (1, 3, 4, 31):
                with self.subTest(initiator=initiator, fragment=size), link(self, initiator=initiator, fragment=size) as session:
                    session.handshake()
                    self.expect_terminal(session, terminal(cid=(1 << 64)-1))
                    self.assertTrue(session.local.chunks)
                    self.assertLessEqual(max(session.local.chunks), size)
                    self.assertEqual(session.calls, [])

    def test_error_pipelined_with_handshake_retires(self):
        for initiator in (False, True):
            with self.subTest(initiator=initiator), link(self, initiator=initiator) as session:
                session.handshake(terminal() + tell() + heartbeat())
                self.assertIsNone(session.receive(), 'terminal ERROR did not retire connection')
                self.assertEqual(session.calls, [])

    def test_error_before_handshake_never_admits_business(self):
        for initiator in (False, True):
            with self.subTest(initiator=initiator), link(self, initiator=initiator) as session:
                if initiator:
                    self.assertEqual(session.receive().ft, wire.FT_HANDSHAKE)
                self.expect_terminal(session, terminal())
                self.assertEqual(session.calls, [])

    def test_prior_tell_effect_is_preserved(self):
        for initiator in (False, True):
            with self.subTest(initiator=initiator), link(self, initiator=initiator) as session:
                session.handshake()
                self.expect_terminal(session, tell(b'before') + terminal())
                self.assertEqual(session.calls, [('tell', '/user/probe', 'bin:probe', b'before')])

    def test_error_cannot_deliver_later_route_hint(self):
        for initiator in (False, True):
            with self.subTest(initiator=initiator), link(self, initiator=initiator) as session:
                session.handshake()
                hint = wire.build_frame(wire.FT_ROUTE_HINT, 0, '', '', b'late-route')
                self.expect_terminal(session, terminal() + hint)
                self.assertEqual(session.hints, [])

    def test_sibling_connection_and_registry_survive(self):
        for initiator in (False, True):
            state = gateway.RayAdminState()
            registry = object()
            state.components['retained'] = registry
            with self.subTest(initiator=initiator), link(self, state=state) as sibling:
                sibling.handshake()
                with link(self, state=state, initiator=initiator) as failing:
                    failing.handshake()
                    self.expect_terminal(failing, terminal())
                self.assertIs(state.components['retained'], registry)
                self.assertFalse(state.closing)
                sibling.peer.sendall(tell(b'sibling') + heartbeat())
                self.assertEqual(sibling.receive().ft, wire.FT_HEARTBEAT_ACK)
                self.assertEqual(sibling.calls, [('tell', '/user/probe', 'bin:probe', b'sibling')])

    def test_running_ask_keeps_charge_until_actual_return(self):
        for initiator in (False, True):
            events = tuple(threading.Event() for _ in range(3))
            with self.subTest(initiator=initiator), link(self, initiator=initiator, blocked=events) as session:
                session.handshake()
                request = wire.with_reply_to_prefix('/reply', b'pending')
                session.peer.sendall(wire.build_frame(wire.FT_ASK, 55, '/user/probe', 'bin:probe', request))
                self.assertTrue(events[0].wait(2), 'ASK never entered owned worker')
                self.expect_terminal(session, terminal(cid=55))
                admission = session.state.ask_admission
                with admission.lock:
                    self.assertEqual(admission.pending, 1)
                    self.assertGreater(admission.bytes, 0)
                events[1].set()
                self.assertTrue(events[2].wait(2), 'owned worker never returned')
                deadline = time.monotonic() + 2
                with admission.changed:
                    while admission.pending and time.monotonic() < deadline:
                        admission.changed.wait(max(0, deadline - time.monotonic()))
                    self.assertEqual((admission.pending, admission.bytes), (0, 0), 'ASK charge leaked')
                self.assertEqual(session.calls, [('ask', '/user/probe', 'bin:probe', b'pending')])

    def test_business_reply_error_keeps_connection_usable(self):
        for initiator in (False, True):
            with self.subTest(initiator=initiator), link(self, initiator=initiator) as session:
                session.handshake()
                request = wire.with_reply_to_prefix('/reply', b'body')
                session.peer.sendall(wire.build_frame(wire.FT_ASK, 9, '/user/probe', 'bin:fail', request))
                reply = session.receive()
                self.assertIsNotNone(reply, 'business failure did not produce REPLY_ERR')
                self.assertEqual((reply.ft, reply.cid, reply.path), (wire.FT_REPLY_ERR, 9, '/reply'))
                self.assertEqual(wire.decode_err_payload(reply.payload)[0], wire.ERR_PROTOCOL_VIOLATION)
                session.peer.sendall(wire.build_frame(wire.FT_REPLY_ERR, 10, '', '', wire.encode_err_payload(3, 'business')) +
                                     tell(b'recovery') + heartbeat())
                ack = session.receive()
                self.assertIsNotNone(ack, 'business REPLY_ERR incorrectly retired connection')
                self.assertEqual(ack.ft, wire.FT_HEARTBEAT_ACK)
                self.assertEqual(session.calls, [('ask', '/user/probe', 'bin:fail', b'body'),
                                                 ('tell', '/user/probe', 'bin:probe', b'recovery')])

    def test_wire_decoder_keeps_error_diagnostics_lossless(self):
        encoded = terminal(b'\xff\x00\x80', flags=0xffff, cid=(1 << 64)-1)
        decoder = wire.FrameDecoder()
        for byte in encoded:
            decoder.feed(bytes([byte]))
        frame = decoder.next_frame()
        self.assertEqual((frame.ft, frame.payload, frame.flags, frame.cid), (0x7f, b'\xff\x00\x80', 0xffff, (1 << 64)-1))
        self.assertEqual(frame.encode(), encoded)
        self.assertIsNone(decoder.next_frame())


if __name__ == '__main__':
    unittest.main(verbosity=2)
