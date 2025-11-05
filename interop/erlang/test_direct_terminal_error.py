"""Real route hints and direct TCP links test terminal retirement and recovery."""
import socket
import struct
import time
import unittest

import test_connection as support
from test_connection import wire, admin_v2
from test_terminal_error import Peer, terminal, later, retained_gateway_process


class DirectTerminalError(unittest.TestCase):
    def sample(self, peer, cid):
        body = admin_v2.encode_admin_cmd_v2(dict(kind='measured_metrics', req_id=cid))
        peer.send(wire.build_frame(wire.FT_SYSTEM_EVENT, cid, '', '', body))
        reply = peer.read()
        self.assertIsNotNone(reply, 'parent connection incorrectly retired with direct child')
        self.assertEqual((reply.ft, reply.cid), (wire.FT_SYSTEM_EVENT, cid))
        return admin_v2.decode_admin_reply_v2(reply.payload)['metrics']['snapshot']['connections']

    def wait_connections(self, peer, expected):
        until = time.monotonic() + 3
        while True:
            if self.sample(peer, 55) == expected:
                return
            self.assertLess(time.monotonic(), until, 'direct connection lease did not retire')
            time.sleep(.005)

    def cycle(self, hub, listener, node, cid):
        address = ('127.0.0.1:%d' % listener.getsockname()[1]).encode()
        identity = node.encode()
        hint = struct.pack('<H', len(identity)) + identity + struct.pack('<H', len(address)) + address
        deadline = time.monotonic() + 4
        listener.settimeout(.05)
        while True:
            hub.send(wire.build_frame(wire.FT_ROUTE_HINT, 10, '', '', hint))
            try:
                sock, _ = listener.accept()
                break
            except socket.timeout:
                self.assertLess(time.monotonic(), deadline,
                                'retired direct node or branch still blocks route-hint admission')
        with sock:
            peer = Peer(sock)
            hello = peer.read()
            self.assertIsNotNone(hello)
            self.assertEqual(hello.ft, wire.FT_HANDSHAKE)
            peer.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', wire.handshake_ack_body(node)))
            peer.heartbeat(self)
            peer.ping(self)
            self.wait_connections(hub, 2)
            peer.send(terminal(cid=cid), later())
            self.assertIsNone(peer.read(), 'terminal ERROR left direct Erlang connection active')
        self.wait_connections(hub, 1)
        hub.heartbeat(self)
        hub.ping(self)

    def run_cycles(self, names):
        with support.gateway_peer() as (registrar, process), socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            listener.settimeout(4)
            sock, _ = registrar.accept()
            with sock:
                hub = Peer(sock)
                self.assertEqual(hub.read().ft, wire.FT_HANDSHAKE)
                hub.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', wire.handshake_ack_body('hub')))
                hub.heartbeat(self)
                self.wait_connections(hub, 1)
                for index, node in enumerate(names):
                    with self.subTest(index=index, node=node):
                        self.cycle(hub, listener, node, 0 if index % 2 == 0 else (1 << 64)-1)
                self.assertIsNone(process.poll(), 'direct ERROR terminated gateway VM')

    def test_same_node_rehint_reconnects_after_terminal_error(self):
        self.run_cycles(['same-target'] * 3)

    def test_retired_direct_branches_do_not_exhaust_eight_child_budget(self):
        self.run_cycles(['target-%d' % index for index in range(10)])


if __name__ == '__main__':
    support.gateway_process = retained_gateway_process
    unittest.main(verbosity=2)
