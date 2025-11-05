"""Actual Erlang VM and TCP peers test terminal protocol errors, without mocks."""
from contextlib import contextmanager
from pathlib import Path
import os
import selectors
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest

import test_connection as support
from test_connection import wire, admin_v2

ARTIFACT_ROOT = Path(os.environ.get('PARROT_TERMINAL_ARTIFACTS') or
                     tempfile.mkdtemp(prefix='parrot-erlang-terminal-'))


@contextmanager
def retained_gateway_process(args):
    output = ARTIFACT_ROOT
    output.mkdir(parents=True, exist_ok=True)
    number = len(list(output.glob('vm-*.stderr.log')))
    error_path = output / ('vm-%03d.stderr.log' % number)
    stdout_path = output / ('vm-%03d.stdout.log' % number)
    reader = None
    with error_path.open('xb') as errors, stdout_path.open('xb') as stdout:
        proc = subprocess.Popen(['escript', str(support.GATEWAY), *args], stdout=subprocess.PIPE, stderr=errors)
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(proc.stdout, selectors.EVENT_READ)
                if not ready.select(10):
                    raise RuntimeError('gateway startup deadline')
                line = proc.stdout.readline()
                stdout.write(line)
                stdout.flush()
                if not line.startswith(b'PARROT_ERL_PORT='):
                    raise RuntimeError('missing gateway startup marker: %r' % line)
                port = int(line.split(b'=')[1])
            def drain():
                while True:
                    chunk = proc.stdout.read(65536)
                    if not chunk:
                        return
                    stdout.write(chunk)
                    stdout.flush()
            reader = threading.Thread(target=drain, name='erlang-test-stdout')
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
                reader.join(5)
            proc.stdout.close()
            if reader is not None and reader.is_alive():
                raise RuntimeError('gateway stdout reader escaped cleanup')


class Peer:
    def __init__(self, sock):
        self.sock = sock
        sock.settimeout(2)
        self.decoder = wire.FrameDecoder()

    def read(self):
        while True:
            frame = self.decoder.next_frame()
            if frame is not None:
                return frame
            try:
                chunk = self.sock.recv(65536)
            except ConnectionResetError:
                return None
            if not chunk:
                return None
            self.decoder.feed(chunk)

    def send(self, *frames):
        self.sock.sendall(b''.join(frames))

    def heartbeat(self, test, cid=91):
        self.send(wire.build_frame(wire.FT_HEARTBEAT, cid, '', '', b''))
        ack = self.read()
        test.assertIsNotNone(ack, 'ordinary frame incorrectly retired connection')
        test.assertEqual((ack.ft, ack.cid), (wire.FT_HEARTBEAT_ACK, cid))

    def hello(self, test, tail=b''):
        self.send(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '', wire.handshake_body('terminal-peer')), tail)
        ack = self.read()
        test.assertIsNotNone(ack, 'listener closed before handshake ACK')
        test.assertEqual(ack.ft, wire.FT_HANDSHAKE_ACK)

    def ping(self, test, cid=7):
        self.send(wire.build_frame(wire.FT_ASK, cid, '/u', 'bin:u:Ping', wire.with_reply_to_prefix('', struct.pack('<Q', 39))))
        reply = self.read()
        test.assertIsNotNone(reply, 'business probe never completed')
        test.assertEqual((reply.ft, reply.cid, reply.type_key, reply.payload),
                         (wire.FT_REPLY, cid, 'bin:u:Pong', struct.pack('<Q', 42)))


def terminal(payload=b'\x0c\0\0\0fatal', flags=0, cid=0):
    return wire.build_frame(0x7f, cid, '', '', payload, flags)


def later():
    return wire.build_frame(wire.FT_HEARTBEAT, 91, '', '', b'')


@contextmanager
def listener_peer(port):
    with socket.create_connection(('127.0.0.1', port), timeout=2) as sock:
        yield Peer(sock)


class TerminalError(unittest.TestCase):
    def test_payload_vectors_close_listener(self):
        corpus = Path(__file__).resolve().parents[2] / 'docs/vectors/error-payloads.tsv'
        rows = [line.split('\t') for line in corpus.read_text().splitlines() if line and not line.startswith('#')]
        self.assertEqual(len(rows), 30)
        with retained_gateway_process(['0']) as (process, port):
            for name, _, payload, _, _ in rows:
                with self.subTest(vector=name), listener_peer(port) as peer:
                    peer.hello(self)
                    peer.send(terminal(bytes.fromhex(payload)), later())
                    self.assertIsNone(peer.read(), 'terminal ERROR left Erlang connection active')
            self.assertIsNone(process.poll(), 'connection ERROR terminated gateway VM')

    def test_flags_and_arbitrary_cid_still_close(self):
        with retained_gateway_process(['0']) as (_, port):
            for flags in [1 << bit for bit in range(16)] + [0xffff]:
                with self.subTest(flags=flags), listener_peer(port) as peer:
                    peer.hello(self)
                    peer.send(terminal(flags=flags, cid=(1 << 64)-1), later())
                    self.assertIsNone(peer.read(), 'terminal ERROR left Erlang connection active')

    def test_split_and_pipelined_error_never_resume_reader(self):
        with retained_gateway_process(['0']) as (_, port):
            for cut in (1, 4, 9, 31):
                with self.subTest(cut=cut), listener_peer(port) as peer:
                    peer.hello(self)
                    encoded = terminal()
                    peer.send(encoded[:cut])
                    peer.send(encoded[cut:], later())
                    self.assertIsNone(peer.read(), 'terminal ERROR left Erlang connection active')
            with self.subTest(pipelined=True), listener_peer(port) as peer:
                peer.hello(self, terminal() + later())
                self.assertIsNone(peer.read(), 'terminal ERROR left Erlang connection active')

    def test_registration_error_closes_and_reconnects(self):
        # Exercise the real registrar and its production reconnect delay.
        with support.gateway_peer() as (listener, process):
            for cid in (0, (1 << 64)-1):
                with self.subTest(cid=cid):
                    sock, _ = listener.accept()
                    with sock:
                        peer = Peer(sock)
                        hello = peer.read()
                        self.assertIsNotNone(hello)
                        self.assertEqual(hello.ft, wire.FT_HANDSHAKE)
                        peer.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', wire.handshake_ack_body('hub')),
                                  terminal(cid=cid), later())
                        self.assertIsNone(peer.read(), 'terminal ERROR left Erlang connection active')
            sock, _ = listener.accept()
            with sock:
                peer = Peer(sock)
                self.assertEqual(peer.read().ft, wire.FT_HANDSHAKE)
                peer.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', wire.handshake_ack_body('hub')))
                peer.heartbeat(self)
                peer.ping(self)
            self.assertIsNone(process.poll())

    def test_business_error_preserves_connection_and_completed_reply(self):
        with retained_gateway_process(['0']) as (_, port), listener_peer(port) as peer:
            peer.hello(self)
            peer.send(wire.build_frame(wire.FT_ASK, 4, '/missing', 'bin:missing', wire.with_reply_to_prefix('', b'')))
            reply = peer.read()
            self.assertIsNotNone(reply)
            self.assertEqual((reply.ft, reply.cid), (wire.FT_REPLY_ERR, 4))
            peer.send(wire.build_frame(wire.FT_REPLY_ERR, 5, '', '', wire.encode_err_payload(3, 'business')))
            peer.heartbeat(self)
            peer.ping(self)
            peer.send(terminal(), later())
            self.assertIsNone(peer.read(), 'terminal ERROR left Erlang connection active')

    def test_sibling_and_connection_lease_survive(self):
        with retained_gateway_process(['0']) as (process, port), listener_peer(port) as sibling:
            sibling.hello(self)
            with listener_peer(port) as failed:
                failed.hello(self)
                failed.send(terminal(), later())
                self.assertIsNone(failed.read(), 'terminal ERROR left Erlang connection active')
            sibling.heartbeat(self)
            sibling.ping(self)
            deadline = time.monotonic() + 2
            while True:
                command = admin_v2.encode_admin_cmd_v2(dict(kind='measured_metrics', req_id=66))
                sibling.send(wire.build_frame(wire.FT_SYSTEM_EVENT, 66, '', '', command))
                reply = sibling.read()
                self.assertIsNotNone(reply)
                self.assertEqual((reply.ft, reply.cid), (wire.FT_SYSTEM_EVENT, 66))
                measured = admin_v2.decode_admin_reply_v2(reply.payload)
                if measured['metrics']['snapshot']['connections'] == 1:
                    break
                self.assertLess(time.monotonic(), deadline, 'terminal connection lease did not retire')
                time.sleep(.01)
            self.assertIsNone(process.poll())


if __name__ == '__main__':
    support.gateway_process = retained_gateway_process
    unittest.main(verbosity=2)
