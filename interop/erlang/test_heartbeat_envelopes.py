"""Real listener, registrar and route-hint TCP heartbeat execution contracts."""
from contextlib import contextmanager
import json
import socket
import struct
import unittest

import test_connection as support
import test_direct_terminal_error as direct
from test_terminal_error import Peer, retained_gateway_process

wire = support.wire


@contextmanager
def endpoint(test, role):
    if role == 'listener':
        with retained_gateway_process(['0']) as (process, port):
            with socket.create_connection(('127.0.0.1', port), timeout=3) as sock:
                peer = Peer(sock)
                peer.hello(test)
                yield peer
                test.assertIsNone(process.poll(), 'heartbeat fault killed gateway VM')
        return
    with support.gateway_peer() as (listener, process):
        sock, _ = listener.accept()
        with sock:
            hub = Peer(sock)
            test.assertEqual(hub.read().ft, wire.FT_HANDSHAKE)
            hub.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', wire.handshake_ack_body('hub')))
            hub.heartbeat(test)
            if role == 'registrar':
                yield hub
            else:
                helper = direct.DirectTerminalError('runTest')
                with socket.socket() as target:
                    target.bind(('127.0.0.1', 0))
                    target.listen()
                    target.settimeout(4)
                    node = b'heartbeat-target'
                    address = ('127.0.0.1:%d' % target.getsockname()[1]).encode()
                    hint = struct.pack('<H', len(node)) + node + struct.pack('<H', len(address)) + address
                    hub.send(wire.build_frame(wire.FT_ROUTE_HINT, 10, '', '', hint))
                    child, _ = target.accept()
                    with child:
                        peer = Peer(child)
                        test.assertEqual(peer.read().ft, wire.FT_HANDSHAKE)
                        peer.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '',
                                                  wire.handshake_ack_body(node.decode())))
                        peer.heartbeat(test)
                        helper.wait_connections(hub, 2)
                        try:
                            yield peer
                        finally:
                            peer.sock.close()
                            helper.wait_connections(hub, 1)
                            hub.heartbeat(test)
                            hub.ping(test)
            test.assertIsNone(process.poll(), 'heartbeat fault killed gateway VM')


def ping(cid):
    return wire.build_frame(wire.FT_ASK, cid, '/u', 'bin:u:Ping',
                            wire.with_reply_to_prefix('', struct.pack('<Q', 39)))


class HeartbeatEnvelopes(unittest.TestCase):
    def malformed(self, role, field):
        values = ([1 << bit for bit in range(16)] + [0xffff, 2 | 8] if field == 'flags'
                  else [b'\0', b'not-empty', b'\xff\0' * 128])
        for kind in [wire.FT_HEARTBEAT, wire.FT_HEARTBEAT_ACK]:
            for index, value in enumerate(values):
                with self.subTest(role=role, kind=kind, field=field, index=index):
                    with endpoint(self, role) as peer:
                        frame = wire.build_frame(kind, 77, '', '', b'' if field == 'flags' else value,
                                                 flags=value if field == 'flags' else 0)
                        peer.send(frame, ping(78))
                        self.assertIsNone(peer.read(), 'malformed heartbeat left connection active or produced a response')
                    print('HEARTBEAT_CASE_OK ' + json.dumps(dict(role=role, kind=kind, field=field, index=index)), flush=True)

    def legal(self, role):
        with endpoint(self, role) as peer:
            for cid in [0, 77, (1 << 64) - 1]:
                # The control CID can equal the pending ASK; ACK is never a
                # business reply and HEARTBEAT_ACK must not elicit another ACK.
                request = cid if cid else 77
                peer.send(wire.build_frame(wire.FT_HEARTBEAT, cid, '', '', b''),
                          wire.build_frame(wire.FT_HEARTBEAT_ACK, cid, '', '', b''), ping(request))
                reply = peer.read()
                self.assertIsNotNone(reply)
                self.assertEqual((reply.ft, reply.flags, reply.cid, reply.path, reply.type_key, reply.payload),
                                 (wire.FT_HEARTBEAT_ACK, 0, cid, '', '', b''))
                business = peer.read()
                self.assertIsNotNone(business)
                self.assertEqual((business.ft, business.cid, business.type_key, business.payload),
                                 (wire.FT_REPLY, request, 'bin:u:Pong', struct.pack('<Q', 42)))
            peer.ping(self, 79)
        print('HEARTBEAT_LEGAL_OK ' + role, flush=True)

    def test_listener_flags(self):
        self.malformed('listener', 'flags')

    def test_listener_payloads(self):
        self.malformed('listener', 'payload')

    def test_registrar_flags(self):
        self.malformed('registrar', 'flags')

    def test_registrar_payloads(self):
        self.malformed('registrar', 'payload')

    def test_direct_flags(self):
        self.malformed('direct', 'flags')

    def test_direct_payloads(self):
        self.malformed('direct', 'payload')

    def test_legal_listener(self):
        self.legal('listener')

    def test_legal_registrar(self):
        self.legal('registrar')

    def test_legal_direct(self):
        self.legal('direct')


if __name__ == '__main__':
    support.gateway_process = retained_gateway_process
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(HeartbeatEnvelopes)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    print('HEARTBEAT_TEST_RECEIPT ' + json.dumps(dict(tests=result.testsRun, failures=len(result.failures),
        errors=len(result.errors), skipped=len(result.skipped), successful=result.wasSuccessful())), flush=True)
    raise SystemExit(0 if result.wasSuccessful() else 1)
