"""Real Erlang registration sockets; no mocked codec or connection driver."""
from contextlib import contextmanager
from pathlib import Path
import os
import selectors
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'interop/python'))
from parrot_protocol import wire, admin_v2
GATEWAY = Path(os.environ.get('PARROT_TEST_ERLANG_GATEWAY', Path(__file__).with_name('parrot_gw.es')))


@contextmanager
def gateway_process(args):
    with tempfile.TemporaryFile() as errors:
        proc = subprocess.Popen(['escript', str(GATEWAY), *args],
            stdout=subprocess.PIPE, stderr=errors)
        reader = None
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(proc.stdout, selectors.EVENT_READ)
                if not ready.select(6):
                    raise RuntimeError('gateway startup deadline')
                line = proc.stdout.readline()
                if not line.startswith(b'PARROT_ERL_PORT='):
                    errors.seek(0)
                    raise RuntimeError(f'missing startup marker: {line!r}; {errors.read(4096)!r}')
                port = int(line.split(b'=')[1])
            # Drain incrementally so gateway logs cannot fill the stdout pipe.
            def drain_stdout():
                while proc.stdout.read(65536):
                    pass
            reader = threading.Thread(target=drain_stdout, daemon=True)
            reader.start()
            yield proc, port
        finally:
            if proc.poll() is None:
                proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
            if reader is not None:
                reader.join(timeout=5)
                if reader.is_alive():
                    raise RuntimeError('gateway stdout reader did not retire')
            proc.stdout.close()


@contextmanager
def gateway_peer(family=socket.AF_INET, host=None):
    host = host or ('::1' if family == socket.AF_INET6 else '127.0.0.1')
    with socket.socket(family) as listener:
        listener.bind((host, 0))
        listener.listen()
        listener.settimeout(6)
        endpoint = f'[{host}]' if family == socket.AF_INET6 else host
        with gateway_process(['0', f'parrot={endpoint}:{listener.getsockname()[1]}']) as (proc, _):
            yield listener, proc


class Peer:
    def __init__(self, listener):
        self.sock, _ = listener.accept()
        self.sock.settimeout(5)
        self.decoder = wire.FrameDecoder()
        self.sent = 0
        self.received = 0
        hello = self.read()
        if hello.ft != wire.FT_HANDSHAKE:
            raise AssertionError('gateway did not send HELLO')
        self.capabilities = wire.decode_handshake(hello.payload)['capabilities']

    def read(self):
        while True:
            frame = self.decoder.next_frame()
            if frame is not None:
                self.received += 32 + len(frame.path.encode()) + len(frame.type_key.encode()) + len(frame.payload)
                return frame
            chunk = self.sock.recv(65536)
            if not chunk:
                raise AssertionError('gateway closed before reply')
            self.decoder.feed(chunk)

    def ack(self, *tail, node='hub'):
        data = wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', wire.handshake_ack_body(node))
        self.send(data + b''.join(tail))

    def send(self, data):
        self.sock.sendall(data)
        self.sent += len(data)

    def sample(self, request, measured=False):
        kind = 'measured_metrics' if measured else 'metrics'
        body = admin_v2.encode_admin_cmd_v2({'kind': kind, 'req_id': request})
        self.send(wire.build_frame(wire.FT_SYSTEM_EVENT, request, 'admin', '', body))
        self.tx_before_sample = self.received
        frame = self.read()
        if frame.cid != request or frame.ft != wire.FT_SYSTEM_EVENT:
            raise AssertionError('unexpected metrics reply envelope')
        reply = admin_v2.decode_admin_reply_v2(frame.payload)
        if reply['kind'] != kind:
            raise AssertionError('unexpected metrics reply kind')
        return reply['metrics'] if measured else reply['snapshot']


class SupportContracts(unittest.TestCase):
    def test_first_frame_must_be_hello(self):
        listener = mock.Mock()
        listener.accept.return_value = (mock.Mock(), ('127.0.0.1', 1))
        wrong = wire.Frame(wire.FT_HEARTBEAT, 1, '', '', wire.handshake_body('support-peer'))
        with mock.patch.object(Peer, 'read', return_value=wrong):
            with self.assertRaises(AssertionError):
                Peer(listener)

    def test_eof_cannot_be_ignored_before_a_later_frame(self):
        peer = Peer.__new__(Peer)
        peer.received = 0
        peer.decoder = wire.FrameDecoder()
        peer.sock = mock.Mock()
        # The second chunk makes an omitted EOF guard fail finitely, without a watchdog.
        peer.sock.recv.side_effect = [b'', wire.build_frame(wire.FT_HEARTBEAT, 1, '', '', b'')]
        with self.assertRaises(AssertionError):
            peer.read()
        self.assertEqual(peer.sock.recv.call_count, 1)

    def sample_peer(self, frame):
        peer = Peer.__new__(Peer)
        peer.received = 0
        peer.send = mock.Mock()
        peer.read = mock.Mock(return_value=frame)
        return peer

    def test_sample_validates_type_and_correlation(self):
        for kind, cid in [(wire.FT_HEARTBEAT, 17), (wire.FT_SYSTEM_EVENT, 18)]:
            with self.subTest(kind=kind, cid=cid):
                peer = self.sample_peer(wire.Frame(kind, cid, '', '', b''))
                with mock.patch.object(admin_v2, 'decode_admin_reply_v2',
                                       return_value={'kind': 'metrics', 'snapshot': {}}):
                    with self.assertRaises(AssertionError):
                        peer.sample(17)

    def test_sample_validates_decoded_reply_kind(self):
        peer = self.sample_peer(wire.Frame(wire.FT_SYSTEM_EVENT, 17, '', '', b''))
        with mock.patch.object(admin_v2, 'decode_admin_reply_v2',
                               return_value={'kind': 'measured_metrics', 'snapshot': {}}):
            with self.assertRaises(AssertionError):
                peer.sample(17)


class Registration(unittest.TestCase):
    def test_nul_ack_is_rejected_before_pipelined_heartbeat(self):
        with gateway_peer() as (listener, proc):
            peer = Peer(listener)
            try:
                # Independent TLV bytes keep exercising the decoder after encoder hardening.
                body = (b'\x01\x0a\0left\0right' + b'\x04\x04\0\x03\0\0\0' +
                        b'\x05\x04\0\0\0\x10\0' + b'\x06\x01\0\0')
                peer.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', body) +
                          wire.build_frame(wire.FT_HEARTBEAT, 7, '', '', b''))
                self.assertEqual(peer.sock.recv(1), b'')
            finally:
                peer.sock.close()
            peer = Peer(listener)
            try:
                peer.ack(wire.build_frame(wire.FT_HEARTBEAT, 8, '', '', b''))
                response = peer.read()
                self.assertEqual((response.ft, response.cid), (wire.FT_HEARTBEAT_ACK, 8))
                self.assertIsNone(proc.poll())
            finally:
                peer.sock.close()

    def test_registration_handler_completion_receipts(self):
        with gateway_peer() as (listener, _):
            peer = Peer(listener)
            try:
                self.assertTrue(peer.capabilities & (1 << 8))
                self.assertFalse(peer.capabilities & (1 << 9))
                peer.ack()
                peer.send(wire.build_frame(wire.FT_TELL, 100, '/u', 'bin:u:Ping', struct.pack('<Q', 7), flags=16))
                reply = peer.read()
                self.assertEqual((reply.ft, reply.cid, reply.type_key, reply.payload),
                    (wire.FT_REPLY, 100, 'parrot:completion#v1', b''))
                peer.send(wire.build_frame(wire.FT_TELL, 101, '/u', 'no-such-handler', b'', flags=16))
                reply = peer.read()
                self.assertEqual((reply.ft, reply.cid), (wire.FT_REPLY_ERR, 101))
                # /u has no deployed actor; probe Ping is the explicit exception.
                self.assertEqual(reply.payload[:4], struct.pack('<HH', 1, 0))
            finally:
                peer.sock.close()

    def test_rejects_unsupported_ack_flags(self):
        with gateway_peer() as (listener, _):
            peer = Peer(listener)
            try:
                peer.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', wire.handshake_ack_body('hub'), flags=1))
                self.assertEqual(peer.sock.recv(1), b'')
            finally:
                peer.sock.close()

    def test_measured_epoch_and_units_across_reconnect_and_restart(self):
        epochs = []
        for _ in range(2):
            with gateway_peer() as (listener, _):
                samples = []
                for n in (1, 2):
                    peer = Peer(listener)
                    try:
                        self.assertTrue(peer.capabilities & (1 << 10))
                        peer.ack()
                        report = peer.sample(n + 10, measured=True)
                        self.assertEqual(report['process_kind'], 'erlang_processes')
                        snapshot = report['snapshot']
                        self.assertGreater(snapshot['processes'], 0)
                        self.assertEqual(snapshot['connections'], 1)
                        self.assertEqual(snapshot['handshakes_ok'], n)
                        available = (1 << 17) - 1
                        if sys.platform != 'linux':
                            available &= ~(1 << 15)
                            self.assertEqual(snapshot['memory_rss'], 0)
                        else:
                            self.assertGreater(snapshot['memory_rss'], 0)
                        self.assertEqual(report['available'], available)
                        samples.append(report)
                    finally:
                        peer.sock.close()
                self.assertTrue(samples[0]['epoch'])
                self.assertEqual(samples[0]['epoch'], samples[1]['epoch'])
                self.assertEqual(samples[0]['snapshot']['uptime_start_ms'], samples[1]['snapshot']['uptime_start_ms'])
                epochs.append(samples[0]['epoch'])
        self.assertNotEqual(epochs[0], epochs[1])

    def test_complete_wire_accounting_includes_utf8_and_error_replies(self):
        with gateway_peer() as (listener, _):
            peer = Peer(listener)
            try:
                peer.ack()
                first = peer.sample(2)
                self.assertEqual(first['bytes_rx'], peer.sent)
                self.assertEqual(first['bytes_tx'], peer.tx_before_sample)
                peer.send(wire.build_frame(wire.FT_ASK, 3, '/路径🙂', 'bin:未知', b'\0' * 4))
                reply = peer.read()
                self.assertEqual((reply.ft, reply.cid), (wire.FT_REPLY_ERR, 3))
                # The worker may be preempted between send success and accounting.
                # Bound that scheduling window, then check exact whole-frame totals.
                until = time.monotonic() + 2
                request = 4
                while True:
                    second = peer.sample(request)
                    if (second['replies_tx'] == first['replies_tx'] + 1
                            and second['reply_errs'] == first['reply_errs'] + 1):
                        break
                    self.assertLess(time.monotonic(), until, 'reply accounting did not complete')
                    request += 1
                self.assertEqual(second['bytes_rx'], peer.sent)
                self.assertEqual(second['bytes_tx'], peer.tx_before_sample)
                self.assertEqual(second['asks_rx'] - first['asks_rx'], 1)
                self.assertEqual(second['replies_tx'] - first['replies_tx'], 1)
                self.assertEqual(second['reply_errs'] - first['reply_errs'], 1)
            finally:
                peer.sock.close()

    def test_direct_peer_identity_uses_utf8_and_rejects_wrong_node(self):
        with gateway_peer() as (listener, _), socket.socket() as direct:
            direct.bind(('127.0.0.1', 0))
            direct.listen()
            direct.settimeout(5)
            hub = Peer(listener)
            try:
                hub.ack()
                for expected, actual in [('目标🙂', '目标🙂'), ('another', 'wrong')]:
                    node = expected.encode()
                    addr = f'127.0.0.1:{direct.getsockname()[1]}'.encode()
                    hint = struct.pack('<H', len(node)) + node + struct.pack('<H', len(addr)) + addr
                    hub.send(wire.build_frame(wire.FT_ROUTE_HINT, 10, '', '', hint))
                    peer = Peer(direct)
                    try:
                        peer.ack(wire.build_frame(wire.FT_HEARTBEAT, 11, '', '', b''), node=actual)
                        if expected == actual:
                            reply = peer.read()
                            self.assertEqual((reply.ft, reply.cid), (wire.FT_HEARTBEAT_ACK, 11))
                            self.assertTrue(peer.capabilities & (1 << 8))
                            peer.send(wire.build_frame(wire.FT_TELL, 12, '/u', 'bin:u:Ping', struct.pack('<Q', 8), flags=16))
                            reply = peer.read()
                            self.assertEqual((reply.ft, reply.cid, reply.type_key, reply.payload),
                                (wire.FT_REPLY, 12, 'parrot:completion#v1', b''))
                        else:
                            self.assertEqual(peer.sock.recv(1), b'')
                    finally:
                        peer.sock.close()
            finally:
                hub.sock.close()

    def test_rejects_invalid_ack_then_retries(self):
        with gateway_peer() as (listener, proc):
            peer = Peer(listener)
            try:
                peer.sock.sendall(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', b''))
                self.assertEqual(peer.sock.recv(1), b'')
            finally:
                peer.sock.close()
            peer = Peer(listener)
            try:
                peer.ack(wire.build_frame(wire.FT_HEARTBEAT, 7, '', '', b''))
                response = peer.read()
                self.assertEqual((response.ft, response.cid), (wire.FT_HEARTBEAT_ACK, 7))
                self.assertIsNone(proc.poll())
            finally:
                peer.sock.close()

    def test_ack_does_not_discard_pipelined_frames(self):
        with gateway_peer() as (listener, _):
            peer = Peer(listener)
            try:
                peer.ack(*(wire.build_frame(wire.FT_HEARTBEAT, n, '', '', b'') for n in (7, 8)))
                for n in (7, 8):
                    reply = peer.read()
                    self.assertEqual((reply.ft, reply.cid), (wire.FT_HEARTBEAT_ACK, n))
            finally:
                peer.sock.close()

    def test_reconnect_retires_gauge_and_preserves_counters(self):
        with gateway_peer() as (listener, _):
            peer = Peer(listener)
            try:
                peer.ack()
                first = peer.sample(2)
                self.assertEqual(first['connections'], 1)
                self.assertEqual(first['handshakes_ok'], 1)
            finally:
                peer.sock.close()
            peer = Peer(listener)
            try:
                peer.ack()
                second = peer.sample(3)
                self.assertEqual(second['connections'], 1)
                self.assertEqual(second['handshakes_ok'], 2)
                self.assertGreater(second['bytes_rx'], first['bytes_rx'])
                self.assertGreater(second['bytes_tx'], first['bytes_tx'])
            finally:
                peer.sock.close()


if __name__ == '__main__':
    unittest.main(verbosity=2)
