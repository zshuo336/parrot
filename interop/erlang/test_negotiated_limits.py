"""Actual Erlang listener/registration limits, including buffered post-handshake prefixes."""
import socket
import struct
import unittest
from contextlib import contextmanager

from test_connection import gateway_process, gateway_peer, Peer, wire


def tlv(tag, value):
    return struct.pack('<BH', tag, len(value)) + value


def hello(limit, ack, extended):
    body = (tlv(1, b'peer') + tlv(4, b'\x03\0\0\0') + tlv(5, struct.pack('<I', limit)) +
            tlv(6, b'\0') + (tlv(9, b'bin') if ack else b''))
    if extended:
        body += tlv(255, b'x' * 40) * 32768
    return body


@contextmanager
def endpoint(client):
    if client:
        with gateway_peer() as (listener, _):
            peer = Peer(listener)
            try:
                yield peer.sock, peer.read
            finally:
                peer.sock.close()
    else:
        with gateway_process(['0']) as (_, port), socket.create_connection(('127.0.0.1', port), timeout=3) as sock:
            sock.settimeout(2)
            decoder = wire.FrameDecoder()
            def read():
                while True:
                    frame = decoder.next_frame()
                    if frame is not None:
                        return frame
                    chunk = sock.recv(65536)
                    if not chunk:
                        raise AssertionError('connection closed before response')
                    decoder.feed(chunk)
            yield sock, read


class NegotiatedLimits(unittest.TestCase):
    def test_limits_after_ack_in_both_roles(self):
        for client in (False, True):
            for limit, extended in ((28, False), (256, False), (2 << 20, True)):
                with self.subTest(client=client, limit=limit), endpoint(client) as (sock, read):
                    sock.settimeout(2)
                    kind = wire.FT_HANDSHAKE_ACK if client else wire.FT_HANDSHAKE
                    sock.sendall(wire.build_frame(kind, 1, '', '', hello(limit, client, extended)) +
                                 wire.build_frame(wire.FT_HEARTBEAT, 2, '', '', b''))
                    if not client:
                        self.assertEqual(read().ft, wire.FT_HANDSHAKE_ACK)
                    self.assertEqual(read().ft, wire.FT_HEARTBEAT_ACK)
                    sock.sendall(struct.pack('<I', min(limit, 1 << 20) + 1))
                    self.assertEqual(sock.recv(1), b'')

    def test_pipelined_prefix_uses_negotiated_limit(self):
        for client in (False, True):
            with self.subTest(client=client), endpoint(client) as (sock, read):
                sock.settimeout(2)
                kind = wire.FT_HANDSHAKE_ACK if client else wire.FT_HANDSHAKE
                sock.sendall(wire.build_frame(kind, 1, '', '', hello(256, client, False)) + struct.pack('<I', 257))
                if not client:
                    self.assertEqual(read().ft, wire.FT_HANDSHAKE_ACK)
                self.assertEqual(sock.recv(1), b'')


if __name__ == '__main__':
    unittest.main(verbosity=2)
