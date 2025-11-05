"""Validate the native interoperability entrypoint through its public CLI/TCP."""
import argparse
from pathlib import Path
import socket
import struct
import subprocess
import sys
import threading
import unittest

BINARY = None


def frame(kind, payload=b'', cid=0):
    body = struct.pack('<BBHQBBIHII', 1, kind, 0, cid, 0, 8, 0, 0, 0, 0) + payload
    return struct.pack('<I', len(body)) + body


def receive(sock):
    def exact(size):
        data = bytearray()
        while len(data) < size:
            chunk = sock.recv(size - len(data))
            if not chunk:
                raise EOFError('client closed before completing its frame')
            data.extend(chunk)
        return bytes(data)
    size, = struct.unpack('<I', exact(4))
    if not 28 <= size <= 16 * 1024 * 1024:
        raise AssertionError('invalid client frame length')
    data = exact(size)
    path_size, = struct.unpack_from('<I', data, 20)
    key_start = 24 + path_size
    key_size, = struct.unpack_from('<I', data, key_start)
    payload_start = key_start + 4 + key_size
    if payload_start > len(data):
        raise AssertionError('invalid client identity lengths')
    return (data[1], struct.unpack_from('<Q', data, 4)[0], data[24:key_start],
            data[key_start + 4:payload_start], data[payload_start:])


class InteropCliContracts(unittest.TestCase):
    def run_client(self, *args):
        return subprocess.run([str(BINARY), *args], capture_output=True, timeout=10)

    def test_invalid_arguments_fail_before_smoke_or_connect(self):
        cases = [('',), ('abc',), ('80junk',), ('0junk',), ('-1',), ('+1',),
                 (' 0',), ('0 ',), ('0x0',), ('1.0',), ('65536',),
                 ('4294967296',), ('9' * 100,), ('0', 'extra'), ('0', '0')]
        for args in cases:
            with self.subTest(args=args):
                result = self.run_client(*args)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertEqual(result.stdout, b'')
                self.assertIn(b'[port: 0..65535]', result.stderr)

    def test_no_argument_and_zero_run_smoke(self):
        for args in [(), ('0',), ('00000',)]:
            with self.subTest(args=args):
                result = self.run_client(*args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b'CABI-SMOKE PASS\n')
                self.assertEqual(result.stderr, b'')

    def exchange(self, reply_payload=None):
        errors, observed = [], []
        listener = socket.socket()
        self.addCleanup(listener.close)
        listener.bind(('127.0.0.1', 0))
        listener.listen(1)
        listener.settimeout(3)
        port = listener.getsockname()[1]

        def serve():
            try:
                with listener.accept()[0] as sock:
                    sock.settimeout(3)
                    self.assertEqual(receive(sock)[0], 1)
                    def tlv(tag, value):
                        return bytes([tag]) + struct.pack('<H', len(value)) + value
                    ack = (tlv(1, b'cli-peer') + tlv(4, struct.pack('<I', 2)) +
                           tlv(5, struct.pack('<I', 1 << 20)) + tlv(6, b'\0') +
                           tlv(7, b'\x08') + tlv(9, b'pb'))
                    sock.sendall(frame(2, ack))
                    registration = receive(sock)
                    self.assertEqual((registration[0], *registration[2:]),
                        (0x13, b'/user/cpp1', b'$receptionist.register:edge/sensor', b''))
                    ask = receive(sock)
                    self.assertEqual((ask[0], ask[2], ask[3]), (0x10, b'/user/echo', b'bin:u:Echo'))
                    reply_size, = struct.unpack_from('<I', ask[4])
                    reply_to, payload = ask[4][4:4 + reply_size], ask[4][4 + reply_size:]
                    self.assertTrue(reply_to)
                    self.assertEqual(payload, bytes.fromhex('deadbeef'))
                    sock.sendall(frame(0x11, payload if reply_payload is None else reply_payload, ask[1]))
                    self.assertEqual(sock.recv(1), b'')
                    observed.append('closed-after-reply')
            except BaseException as error:
                errors.append(error)
            finally:
                listener.close()

        worker = threading.Thread(target=serve, name='interop-cli-peer')
        worker.start()
        try:
            result = self.run_client(str(port))
        finally:
            worker.join(4)
            listener.close()
        self.assertFalse(worker.is_alive(), 'TCP fixture must retire')
        self.assertEqual(errors, [])
        self.assertEqual(observed, ['closed-after-reply'])
        return result

    def test_real_tcp_registration_and_echo(self):
        result = self.exchange()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'CABI-INTEROP PASS\n')
        self.assertEqual(result.stderr, b'')

    def test_wrong_echo_cannot_report_success(self):
        for payload in [b'', b'wrong', bytes.fromhex('deadbe00')]:
            with self.subTest(payload=payload.hex()):
                result = self.exchange(reply_payload=payload)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertNotIn(b'PASS', result.stdout)
                self.assertIn(b'echo len' if len(payload) != 4 else b'echo bytes', result.stdout)
                self.assertNotIn(b'AddressSanitizer', result.stderr)
                self.assertNotIn(b'runtime error:', result.stderr)
                self.assertEqual(result.stderr, b'')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    args, remaining = parser.parse_known_args()
    BINARY = args.binary.resolve(strict=True)
    unittest.main(argv=[__file__, *remaining], verbosity=2)
