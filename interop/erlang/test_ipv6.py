"""Real IPv4/IPv6 gateway sockets, including recovery and business replies."""
import socket
from pathlib import Path
import tempfile
import struct
import subprocess
import time
import unittest

from test_connection import GATEWAY, Peer, admin_v2, gateway_peer, gateway_process, wire


def ask_ping(peer, cid, value):
    peer.send(wire.build_frame(wire.FT_ASK, cid, '/u', 'bin:u:Ping',
                              struct.pack('<I', 0) + struct.pack('<Q', value)))
    response = peer.read()
    if (response.ft, response.cid, response.type_key, response.payload) != (
            wire.FT_REPLY, cid, 'bin:u:Pong', struct.pack('<Q', value + 3)):
        raise AssertionError(f'wrong business response: {response!r}')


def hint(hub, node, host, port):
    node = node.encode()
    endpoint = (f'[{host}]:{port}' if ':' in host else f'{host}:{port}').encode()
    body = struct.pack('<H', len(node)) + node + struct.pack('<H', len(endpoint)) + endpoint
    hub.send(wire.build_frame(wire.FT_ROUTE_HINT, 90, '', '', body))


def admin(peer, command):
    cid = command['req_id']
    peer.send(wire.build_frame(wire.FT_SYSTEM_EVENT, cid, '/admin', '',
                              admin_v2.encode_admin_cmd_v2(command)))
    reply = peer.read()
    if (reply.ft, reply.cid) != (wire.FT_SYSTEM_EVENT, cid):
        raise AssertionError(f'wrong admin response: {reply!r}')
    return admin_v2.decode_admin_reply_v2(reply.payload)


class IPv6(unittest.TestCase):
    # Missing IPv6 is a failed capability gate, not an automatic skip.
    def test_registration_reconnect_and_business_on_ipv6(self):
        with gateway_peer(socket.AF_INET6) as (listener, proc):
            for generation in range(2):
                peer = Peer(listener)
                try:
                    peer.ack()
                    ask_ping(peer, 100 + generation, 41 + generation)
                    self.assertIsNone(proc.poll())
                finally:
                    peer.sock.close()

    def test_ipv6_direct_hint_checks_identity_and_relearns_after_disconnect(self):
        with gateway_peer() as (listener, proc), socket.socket(socket.AF_INET6) as direct:
            direct.bind(('::1', 0))
            direct.listen()
            direct.settimeout(5)
            hub = Peer(listener)
            try:
                hub.ack()
                hint(hub, 'ipv6-peer', '::1', direct.getsockname()[1])
                peer = Peer(direct)
                try:
                    peer.ack(wire.build_frame(wire.FT_HEARTBEAT, 200, '', '', b''), node='wrong-peer')
                    self.assertEqual(peer.sock.recv(1), b'')
                finally:
                    peer.sock.close()
                # The owned dialer retries after rejecting the wrong identity.
                peer = Peer(direct)
                try:
                    peer.ack(node='ipv6-peer')
                    ask_ping(peer, 201, 7)
                finally:
                    peer.sock.close()
                # Re-hint until the retired branch has released its identity.
                deadline = time.monotonic() + 5
                direct.settimeout(0.1)
                while True:
                    hint(hub, 'ipv6-peer', '::1', direct.getsockname()[1])
                    try:
                        peer = Peer(direct)
                        break
                    except socket.timeout:
                        self.assertLess(time.monotonic(), deadline, 'direct route did not retire/relearn')
                try:
                    peer.ack(node='ipv6-peer')
                    ask_ping(peer, 202, 8)
                finally:
                    peer.sock.close()
                self.assertIsNone(proc.poll())
            finally:
                hub.sock.close()

    def test_explicit_ipv6_listener_serves_real_business(self):
        with gateway_process(['0', 'listen=::1']) as (proc, port):
            with socket.socket(socket.AF_INET6) as sock:
                sock.settimeout(5)
                sock.connect(('::1', port))
                peer = object.__new__(Peer)
                peer.sock, peer.decoder = sock, wire.FrameDecoder()
                peer.sent, peer.received = 0, 0
                peer.send(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '', wire.handshake_body('client6')))
                self.assertEqual(peer.read().ft, wire.FT_HANDSHAKE_ACK)
                ask_ping(peer, 301, 99)
                self.assertIsNone(proc.poll())

    def test_dns_falls_back_to_ipv6_when_ipv4_port_is_closed(self):
        with socket.socket(socket.AF_INET) as refused, socket.socket(socket.AF_INET6) as listener:
            refused.bind(('127.0.0.1', 0))
            listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            listener.bind(('::1', refused.getsockname()[1]))
            listener.listen()
            listener.settimeout(5)
            with gateway_process(['0', f'parrot=localhost:{listener.getsockname()[1]}']) as (proc, _):
                peer = Peer(listener)
                try:
                    peer.ack()
                    ask_ping(peer, 401, 50)
                    self.assertIsNone(proc.poll())
                finally:
                    peer.sock.close()

    def test_ipv4_listener_remains_compatible(self):
        with gateway_process(['0']) as (proc, port):
            with socket.create_connection(('127.0.0.1', port), timeout=5) as sock:
                peer = object.__new__(Peer)
                peer.sock, peer.decoder = sock, wire.FrameDecoder()
                peer.sent, peer.received = 0, 0
                peer.send(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '', wire.handshake_body('client4')))
                self.assertEqual(peer.read().ft, wire.FT_HANDSHAKE_ACK)
                ask_ping(peer, 402, 100)
                self.assertIsNone(proc.poll())

    def test_stateful_component_survives_ipv6_registration_reconnect(self):
        with tempfile.TemporaryDirectory(prefix='parrot-ipv6-component-') as temporary:
            artifact = Path(temporary)
            (artifact / 'ipv6_fixture.erl').write_text(
                '-module(ipv6_fixture).\n-export([parrot_init/1,parrot_service/3]).\n'
                'parrot_init(_)->{ok,0}.\n'
                'parrot_service(<<"inc">>,_,N)->{reply,{<<"count">>,<<(N+1):64/little>>},N+1}.\n')
            subprocess.run(['erlc', 'ipv6_fixture.erl'], cwd=artifact, check=True, timeout=15)
            with gateway_peer(socket.AF_INET6) as (listener, proc):
                for generation in range(2):
                    peer = Peer(listener)
                    try:
                        peer.ack()
                        if generation == 0:
                            cmd = {'kind': 'deploy', 'req_id': 500, 'component': {
                                'name': 'ipv6', 'version': '1', 'artifact': {
                                    'kind': 'beam', 'app': 'ipv6_fixture', 'uri': artifact.as_uri()},
                                'instances': {'kind': 'singleton'}, 'config': None}}
                            self.assertEqual(admin(peer, cmd)['kind'], 'deployed')
                        for step in range(2):
                            cid = 501 + generation * 2 + step
                            peer.send(wire.build_frame(wire.FT_ASK, cid, '/user/ipv6', 'inc', b'\0' * 4))
                            reply = peer.read()
                            self.assertEqual((reply.ft, reply.cid, reply.type_key, reply.payload),
                                (wire.FT_REPLY, cid, 'count', struct.pack('<Q', generation * 2 + step + 1)))
                        if generation == 1:
                            self.assertEqual(admin(peer, {'kind': 'stop', 'req_id': 510,
                                'path_prefix': '/user/ipv6'})['kind'], 'stopped')
                        self.assertIsNone(proc.poll())
                    finally:
                        peer.sock.close()

    def test_invalid_listener_options_do_not_start_a_gateway(self):
        for args in [['-1'], ['65536'], ['0', 'listen=not-an-ip'],
                     ['0', 'unknown=1'], ['0', 'listen=::1', 'listen=127.0.0.1']]:
            with self.subTest(args=args):
                run = subprocess.run(['escript', str(GATEWAY), *args], capture_output=True, timeout=8)
                self.assertNotEqual(run.returncode, 0)
                self.assertNotIn(b'PARROT_ERL_PORT=', run.stdout)

    def test_invalid_endpoints_fail_before_publishing_listener(self):
        for target in ['::1:80', '[::1]:0', '[::1]:65536', '[::1]:-1',
                       '[::1]:+80', '[::1]junk:80', '[bad]:80', ':80',
                       'localhost:80:90', 'localhost: 80', 'localhost..:80',
                       '127.1:80', '0x7f000001:80']:
            with self.subTest(target=target):
                run = subprocess.run(['escript', str(GATEWAY), '0', 'parrot=' + target],
                                     capture_output=True, timeout=8)
                self.assertNotEqual(run.returncode, 0)
                self.assertNotIn(b'PARROT_ERL_PORT=', run.stdout)


if __name__ == '__main__':
    unittest.main(verbosity=2)
