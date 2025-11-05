"""Hostile TCP peers exercise the production C ABI, optionally under ASan/UBSan."""
import argparse
import json
import os
import pathlib
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import unittest

HERE = pathlib.Path(__file__).resolve().parent
BINARY = None
RESOLVER_BINARY = None
COMPLETION_BINARY = None
POLL_BINARY = None
FORCED_POLL_BINARY = None
ADDRESS_BINARY = None
DECODER_LIMITS_BINARY = None
HEARTBEAT_BINARY = None
TERMINAL_ERROR_BINARY = None


class ReceiptRunner(unittest.TextTestRunner):
    """Report discovered and executed cases independently of human log wording."""

    def run(self, test):
        # TestSuite clears its children during execution, so count before run().
        selected = test.countTestCases()
        result = super().run(test)
        print('PARROT_CPP_TEST_RECEIPT ' + json.dumps({
            'schema': 1, 'selected': selected, 'executed': result.testsRun,
            'failures': len(result.failures), 'errors': len(result.errors),
            'skipped': len(result.skipped),
            'expected_failures': len(result.expectedFailures),
            'unexpected_successes': len(result.unexpectedSuccesses),
            'successful': result.wasSuccessful(),
        }), flush=True)
        return result

def tlv(tag, value):
    return bytes([tag]) + struct.pack('<H', len(value)) + value

def ack_payload():
    return (tlv(1, b'peer') + tlv(4, struct.pack('<I', 2)) +
            tlv(5, struct.pack('<I', 1 << 20)) + tlv(6, b'\0') +
            tlv(7, b'\x08') + tlv(9, b'pb'))

def frame(ft, payload=b'', path=b'', key=b'', flags=0, cid=42):
    body = (struct.pack('<BBHQBB', 1, ft, flags, cid, 0, 8) + bytes(6) +
            struct.pack('<I', len(path)) + path + struct.pack('<I', len(key)) + key + payload)
    return struct.pack('<I', len(body)) + body

def receive(sock):
    def exact(n):
        out = bytearray()
        while len(out) < n:
            data = sock.recv(n-len(out))
            if not data:
                raise EOFError('peer closed before full frame')
            out.extend(data)
        return bytes(out)
    size, = struct.unpack('<I', exact(4))
    if not 28 <= size <= 16 * 1024 * 1024:
        raise AssertionError(f'bad client frame size {size}')
    return exact(size)

class TransportContracts(unittest.TestCase):
    def terminal_error_peer(self, mode='terminal-first-success', payload=b'\x0c\0\0\0bad', flags=0):
        errors = []
        observations = []
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0))
        listener.listen(1)
        listener.settimeout(3)
        reply_payload = b'\x03\0\0\0' if mode.endswith('-remote-error') else (b'' if mode.endswith('-empty') else b'\x2a')
        split = '-split-' in mode
        reply_size = 32 + len(reply_payload)
        terminal_size = 32 + len(payload)
        boundary = reply_size if split else reply_size + terminal_size

        def serve():
            try:
                with listener.accept()[0] as sock:
                    sock.settimeout(3)
                    self.assertEqual(receive(sock)[1], 1)
                    sock.sendall(frame(2, ack_payload()))
                    ask = receive(sock)
                    self.assertEqual(ask[1], 0x10)
                    self.assertEqual(ask[24:33], b'/terminal')
                    cid = struct.unpack_from('<Q', ask, 4)[0]
                    reply = frame(0x12 if mode.endswith('-remote-error') else 0x11, reply_payload, cid=cid)
                    terminal = frame(0x7f, payload, flags=flags, cid=cid)
                    # Both chunks originate from real socket bytes; the C++ seam
                    # only controls the first read's exact framing boundary.
                    sock.sendall(terminal + reply if '-first-' in mode else reply + terminal)
                    try:
                        while sock.recv(4096):
                            pass
                    except ConnectionResetError:
                        pass
                    observations.append('retired')
            except BaseException as error:
                errors.append(error)
            finally:
                listener.close()

        worker = threading.Thread(target=serve, name='owned-terminal-error-peer')
        worker.start()
        try:
            result = subprocess.run([str(TERMINAL_ERROR_BINARY), mode,
                                     f'parrot://127.0.0.1:{listener.getsockname()[1]}', str(boundary)],
                                    capture_output=True, timeout=7,
                                    env={**os.environ, 'ASAN_OPTIONS': 'detect_stack_use_after_return=1',
                                         'UBSAN_OPTIONS': 'halt_on_error=1'})
        finally:
            worker.join(4)
            listener.close()
        self.assertFalse(worker.is_alive(), 'terminal peer must retire')
        self.assertFalse(errors, repr(errors))
        self.assertEqual(observations, ['retired'])
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors='replace'))
        self.assertEqual(result.stdout, b'TERMINAL ERROR PASS\n')

    def test_terminal_error_codes_close_pending_connection(self):
        for code in range(1, 14):
            with self.subTest(code=code):
                self.terminal_error_peer(payload=struct.pack('<HH', code, 0) + b'remote error')

    def test_terminal_error_payload_vectors_still_close(self):
        rows = [line.split('\t') for line in
                (HERE.parent.parent/'docs/vectors/error-payloads.tsv').read_text().splitlines()
                if not line.startswith('#')]
        self.assertEqual(len(rows), 30)
        for name, _, payload, _, _ in rows:
            with self.subTest(name=name):
                self.terminal_error_peer(payload=bytes.fromhex(payload))

    def test_terminal_error_empty_payload_still_closes(self):
        self.terminal_error_peer(payload=b'')

    def test_terminal_error_flags_still_close(self):
        for flags in [1 << bit for bit in range(16)] + [0xffff]:
            with self.subTest(flags=flags):
                self.terminal_error_peer(flags=flags)

    def test_terminal_error_before_success_cannot_be_overridden(self):
        self.terminal_error_peer()

    def test_terminal_error_before_remote_error_cannot_be_overridden(self):
        self.terminal_error_peer(mode='terminal-first-remote-error')

    def test_terminal_error_after_success_preserves_completion(self):
        self.terminal_error_peer(mode='terminal-after-success')

    def test_terminal_error_after_empty_success_preserves_completion(self):
        self.terminal_error_peer(mode='terminal-after-empty')

    def test_terminal_error_after_remote_error_preserves_completion(self):
        self.terminal_error_peer(mode='terminal-after-remote-error')

    def test_terminal_error_split_after_success_preserves_completion(self):
        self.terminal_error_peer(mode='terminal-split-success')

    def test_terminal_error_split_after_empty_success_preserves_completion(self):
        self.terminal_error_peer(mode='terminal-split-empty')

    def test_terminal_error_split_after_remote_error_preserves_completion(self):
        self.terminal_error_peer(mode='terminal-split-remote-error')

    def heartbeat_peer(self, ft, payload=b'', flags=0, valid=False):
        errors = []
        observations = []
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0))
        listener.listen(1)
        listener.settimeout(3)
        port = listener.getsockname()[1]

        def serve():
            try:
                with listener.accept()[0] as sock:
                    sock.settimeout(3)
                    self.assertEqual(receive(sock)[1], 1)
                    sock.sendall(frame(2, ack_payload()))
                    first = receive(sock)
                    self.assertEqual(first[1], 0x10)
                    self.assertEqual(first[24:40], b'/heartbeat-first')
                    cid = struct.unpack_from('<Q', first, 4)[0]
                    # The first ACK deliberately shares the pending ASK cid.
                    cids = [cid, 0, (1 << 64) - 1] if valid else [cid]
                    controls = b''.join(frame(ft, payload, flags=flags, cid=value) for value in cids)
                    sock.sendall(controls + frame(0x11, b'\x11\x22\x33', cid=cid))
                    if valid:
                        if ft == 3:
                            for value in cids:
                                ack = receive(sock)
                                self.assertEqual(len(ack), 28)
                                self.assertEqual(ack[1], 4)
                                self.assertEqual(struct.unpack_from('<H', ack, 2)[0], 0)
                                self.assertEqual(struct.unpack_from('<Q', ack, 4)[0], value)
                                self.assertEqual(ack[20:], bytes(8))
                        second = receive(sock)
                        self.assertEqual(second[1], 0x10)
                        self.assertEqual(second[24:39], b'/heartbeat-next')
                        self.assertNotEqual(struct.unpack_from('<Q', second, 4)[0], cid)
                        sock.sendall(frame(0x11, b'\x44\x55', cid=struct.unpack_from('<Q', second, 4)[0]))
                        final = receive(sock)
                        self.assertEqual(final[1], 0x13)
                        self.assertEqual(final[24:43], b'/heartbeat-finished')
                    while sock.recv(4096):
                        pass
                    observations.append('valid-business-complete' if valid else 'peer-eof')
            except BaseException as error:
                errors.append(error)
            finally:
                listener.close()

        worker = threading.Thread(target=serve, name='owned-heartbeat-peer')
        worker.start()
        try:
            result = subprocess.run([str(HEARTBEAT_BINARY), 'valid' if valid else 'invalid',
                                     f'parrot://127.0.0.1:{port}'],
                                    capture_output=True, timeout=7,
                                    env={**os.environ, 'ASAN_OPTIONS': 'detect_stack_use_after_return=1',
                                         'UBSAN_OPTIONS': 'halt_on_error=1'})
        finally:
            worker.join(4)
            listener.close()
        self.assertFalse(worker.is_alive(), 'heartbeat peer must retire')
        self.assertFalse(errors, repr(errors))
        self.assertEqual(observations, ['valid-business-complete' if valid else 'peer-eof'])
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors='replace'))
        self.assertEqual(result.stdout, b'HEARTBEAT ENVELOPE PASS ' + (b'valid\n' if valid else b'invalid\n'))

    def test_heartbeat_request_flags_rejected(self):
        for flags in [1 << bit for bit in range(16)] + [0xffff]:
            with self.subTest(flags=flags):
                self.heartbeat_peer(3, flags=flags)

    def test_heartbeat_ack_flags_rejected(self):
        for flags in [1 << bit for bit in range(16)] + [0xffff]:
            with self.subTest(flags=flags):
                self.heartbeat_peer(4, flags=flags)

    def test_heartbeat_request_payload_rejected(self):
        for payload in [b'\0', b'\xff' * 64, b'looks-like-a-business-reply']:
            with self.subTest(payload=payload.hex()):
                self.heartbeat_peer(3, payload=payload)

    def test_heartbeat_ack_payload_rejected(self):
        for payload in [b'\0', b'\xff' * 64, b'looks-like-a-business-reply']:
            with self.subTest(payload=payload.hex()):
                self.heartbeat_peer(4, payload=payload)

    def test_heartbeat_request_preserves_pending_business(self):
        self.heartbeat_peer(3, valid=True)

    def test_heartbeat_ack_preserves_pending_business(self):
        self.heartbeat_peer(4, valid=True)

    def decoder_limits(self, mode):
        result = subprocess.run([str(DECODER_LIMITS_BINARY), mode], capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors='replace'))
        self.assertEqual(result.stdout, ('DECODER LIMIT PASS ' + mode + '\n').encode())

    def test_decoder_limits_invalid_constructor(self):
        self.decoder_limits('invalid')

    def test_decoder_limits_exact_bounds_and_partial_frames(self):
        self.decoder_limits('bounds')

    def test_decoder_limits_rejection_and_recovery(self):
        self.decoder_limits('recovery')

    def test_decoder_limits_overflow_prefix_isolated(self):
        self.decoder_limits('overflow')

    def address_peer(self, scheme='parrot', family=socket.AF_INET, host=None, invalid=None):
        listener = socket.socket(family, socket.SOCK_STREAM)
        if family == socket.AF_INET6:
            listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        listener.bind(('::1' if family == socket.AF_INET6 else '127.0.0.1', 0))
        listener.listen(4); listener.settimeout(.1)
        port = listener.getsockname()[1]
        authority_host = host or ('[::1]' if family == socket.AF_INET6 else '127.0.0.1')
        url = f'{scheme}://{authority_host}:{port}'
        done = threading.Event()
        errors, accepted, business = [], [], []
        def envelope(body):
            path_length = struct.unpack_from('<I', body, 20)[0]
            key_at = 24 + path_length
            key_length = struct.unpack_from('<I', body, key_at)[0]
            return body[24:key_at], body[key_at+4:key_at+4+key_length], body[key_at+4+key_length:]
        def serve():
            try:
                while not done.is_set():
                    try:
                        sock, _ = listener.accept()
                    except socket.timeout:
                        continue
                    accepted.append(sock.family)
                    with sock:
                        sock.settimeout(3)
                        try:
                            self.assertEqual(receive(sock)[1], 1)
                            sock.sendall(frame(2, ack_payload()))
                            if invalid is not None:
                                ask = receive(sock)
                                self.assertEqual(ask[1], 0x10)
                                path, key, payload = envelope(ask)
                                self.assertEqual((path, key), (b'/still-connected', b'pb:k'))
                                sock.sendall(frame(0x11, b'\x63', cid=struct.unpack_from('<Q', ask, 4)[0]))
                                business.append('preserved')
                            else:
                                registration = receive(sock)
                                self.assertEqual(registration[1], 0x13)
                                self.assertEqual(envelope(registration), (b'/user/address', b'$receptionist.register:address-service', b''))
                                ask = receive(sock)
                                self.assertEqual(ask[1], 0x10)
                                path, key, payload = envelope(ask)
                                self.assertEqual((path, key), (b'/address', b'pb:k'))
                                reply_length = struct.unpack_from('<I', payload)[0]
                                round_payload = payload[4+reply_length:]
                                self.assertEqual(round_payload, bytes([len(business)]))
                                sock.sendall(frame(0x11, bytes([42+len(business)]), cid=struct.unpack_from('<Q', ask, 4)[0]))
                                business.append('reconnected' if business else 'connected')
                            while sock.recv(4096): pass
                        except (EOFError, ConnectionResetError, BrokenPipeError):
                            # A failed control may close before sending its next ASK.
                            pass
            except BaseException as error:
                errors.append(error)
            finally:
                listener.close()
        worker = threading.Thread(target=serve, name='owned-address-peer'); worker.start()
        command = [str(ADDRESS_BINARY), 'preserve' if invalid is not None else 'business', url]
        if invalid is not None:
            command.append(invalid.format(port=port))
        try:
            result = subprocess.run(command, capture_output=True, timeout=12,
                env={**os.environ, 'ASAN_OPTIONS': 'detect_stack_use_after_return=1', 'UBSAN_OPTIONS': 'halt_on_error=1'})
        finally:
            done.set(); worker.join(4); listener.close()
        self.assertFalse(worker.is_alive(), 'address peer must retire')
        self.assertFalse(errors, repr(errors))
        self.assertEqual(result.returncode, 0, result.stdout.decode(errors='replace') + result.stderr.decode(errors='replace'))
        self.assertEqual(len(accepted), 1 if invalid is not None else 2)
        self.assertEqual(business, ['preserved'] if invalid is not None else ['connected', 'reconnected'])

    def invalid_addresses(self, authorities):
        for scheme in ['parrot', 'tcp']:
            for authority in authorities:
                with self.subTest(scheme=scheme, authority=authority):
                    self.address_peer(scheme=scheme, invalid=scheme + '://' + authority)

    def test_address_ipv4_parrot_business(self):
        self.address_peer()

    def test_address_ipv4_tcp_business(self):
        self.address_peer(scheme='tcp')

    def test_address_ipv6_parrot_business(self):
        self.address_peer(family=socket.AF_INET6)

    def test_address_ipv6_tcp_business(self):
        self.address_peer(scheme='tcp', family=socket.AF_INET6)

    def test_address_hostname_business(self):
        for scheme in ['parrot', 'tcp']:
            with self.subTest(scheme=scheme):
                self.address_peer(scheme=scheme, host='localhost')

    def test_address_ipv4_mapped_ipv6_business(self):
        for scheme in ['parrot', 'tcp']:
            with self.subTest(scheme=scheme):
                self.address_peer(scheme=scheme, host='[::ffff:127.0.0.1]')

    def test_address_bracket_grammar_preserves_connection(self):
        self.invalid_addresses(['[127.0.0.1]:{port}', '[localhost]:{port}', '[::1:{port}',
            '::1]:{port}', '[::1]]:{port}', '[[::1]]:{port}', '[::1]extra:{port}', '[::1]{port}'])

    def test_address_unbracketed_ipv6_preserves_connection(self):
        self.invalid_addresses(['::1:{port}', '::ffff:127.0.0.1:{port}'])

    def test_address_authority_components_preserve_connection(self):
        self.invalid_addresses(['127.0.0.1/path:{port}', '127.0.0.1?query:{port}',
            '127.0.0.1#fragment:{port}', 'user@127.0.0.1:{port}', '127.0.0.1\\path:{port}', '127.0.0.1%2f:{port}'])

    def test_address_whitespace_preserves_connection(self):
        self.invalid_addresses([' 127.0.0.1:{port}', '127.0.0.1 :{port}', '127.0.0.1\t:{port}',
            '127.0.0.1\n:{port}', '127.0.0.1: {port}', '127.0.0.1:{port} '])

    def test_address_port_bounds_preserve_connection(self):
        self.invalid_addresses(['127.0.0.1', '127.0.0.1:', '127.0.0.1:0', '127.0.0.1:65536',
            '127.0.0.1:4294967295', '127.0.0.1:-1', '127.0.0.1:+1', '127.0.0.1:abc', '127.0.0.1:{port}/path'])

    def test_address_empty_authority_preserves_connection(self):
        self.invalid_addresses(['', ':{port}', '[]:{port}', '[::1]:', '[::1]:0', '[::1]', '127.0.0.1::{port}'])

    def test_address_unsupported_scheme_preserves_connection(self):
        for prefix in ['udp://', 'PARROT://', 'http://', '']:
            with self.subTest(prefix=prefix):
                self.address_peer(invalid=prefix + '127.0.0.1:{port}')

    def test_address_scoped_ipv6_rejected_before_resolution(self):
        self.invalid_addresses(['[fe80::1%lo0]:{port}', '[fe80::1%25lo0]:{port}'])

    def pollsets(self, mode, forced_only=False):
        native = 'epoll' if sys.platform.startswith('linux') else (
            'kqueue' if sys.platform == 'darwin' or sys.platform.startswith(('freebsd', 'openbsd', 'netbsd', 'dragonfly')) else 'poll')
        programs = [(FORCED_POLL_BINARY, 'poll')] if forced_only else [(POLL_BINARY, native), (FORCED_POLL_BINARY, 'poll')]
        for program, backend in programs:
            with self.subTest(backend=backend):
                result = subprocess.run([str(program), mode, backend], capture_output=True, timeout=5,
                    env={**os.environ, 'ASAN_OPTIONS': 'detect_stack_use_after_return=1', 'UBSAN_OPTIONS': 'halt_on_error=1'})
                self.assertEqual(result.returncode, 0, result.stdout.decode(errors='replace') + result.stderr.decode(errors='replace'))
                prefix = 'PARROT_POLLSET_CASE '
                receipts = [json.loads(line[len(prefix):]) for line in result.stdout.decode().splitlines() if line.startswith(prefix)]
                self.assertEqual(receipts, [dict(mode=mode, backend=backend, passed=True)])

    def test_pollset_idempotent_registration_and_readiness(self):
        self.pollsets('lifecycle')

    def test_pollset_timeout_clears_prior_readiness(self):
        self.pollsets('timeout')

    def test_pollset_signal_interruption_clears_prior_readiness(self):
        self.pollsets('interrupt')

    def test_pollset_system_errors_are_reported(self):
        self.pollsets('hard-error')

    def test_pollset_invalid_handles_and_indices(self):
        self.pollsets('invalid')

    def test_pollset_closed_descriptor_is_reported(self):
        self.pollsets('closed', forced_only=True)

    def test_pollset_closed_and_readable_descriptors_are_reported(self):
        self.pollsets('mixed', forced_only=True)

    def test_coalesced_success_survives_later_protocol_violation(self):
        self.peer('completion-coalesced-reply', program=COMPLETION_BINARY)

    def test_coalesced_empty_success_survives_later_protocol_violation(self):
        self.peer('completion-coalesced-empty', program=COMPLETION_BINARY)

    def test_coalesced_error_survives_later_protocol_violation(self):
        self.peer('completion-coalesced-error', program=COMPLETION_BINARY)

    def test_split_success_survives_later_protocol_violation(self):
        self.peer('completion-split-reply', program=COMPLETION_BINARY)

    def test_split_empty_success_survives_later_protocol_violation(self):
        self.peer('completion-split-empty', program=COMPLETION_BINARY)

    def test_split_error_survives_later_protocol_violation(self):
        self.peer('completion-split-error', program=COMPLETION_BINARY)

    def test_violation_before_success_fails_pending_request(self):
        self.peer('completion-violation-first-reply', program=COMPLETION_BINARY)

    def test_violation_before_empty_success_fails_pending_request(self):
        self.peer('completion-violation-first-empty', program=COMPLETION_BINARY)

    def test_violation_before_error_fails_pending_request(self):
        self.peer('completion-violation-first-error', program=COMPLETION_BINARY)

    def reply_deadline(self, mode):
        errors = []
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0)); listener.listen(1); listener.settimeout(3)
        proc = subprocess.Popen([str(BINARY), mode, f'parrot://127.0.0.1:{listener.getsockname()[1]}'],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                env={**os.environ, 'ASAN_OPTIONS': 'detect_stack_use_after_return=1',
                                     'UBSAN_OPTIONS': 'halt_on_error=1'})
        def serve():
            try:
                with listener.accept()[0] as sock:
                    sock.settimeout(4)
                    self.assertEqual(receive(sock)[1], 1)
                    sock.sendall(frame(2, ack_payload()))
                    first = receive(sock)
                    self.assertEqual(first[1], 0x10)
                    self.assertEqual(first[24:33], b'/deadline')
                    sock.sendall(frame(0x13, path=b'/hold'))
                    self.assertEqual(proc.stdout.read(1), b'r')
                    error = mode.endswith('-error')
                    reply = frame(0x12 if error else 0x11,
                                  b'\x03\0\0\0stopped' if error else b'\x2a',
                                  cid=struct.unpack_from('<Q', first, 4)[0])
                    sock.sendall(reply + frame(0x13, path=b'/paired'))
                    second = receive(sock)
                    self.assertEqual(second[1], 0x10)
                    self.assertEqual(second[24:39], b'/after-deadline')
                    sock.sendall(frame(0x11, b'\x63', cid=struct.unpack_from('<Q', second, 4)[0]))
                    while sock.recv(4096): pass
            except BaseException as error:
                errors.append(error)
        worker = threading.Thread(target=serve, name='owned-reply-deadline-peer')
        worker.start()
        timed_out = False
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            timed_out = True
            proc.kill(); proc.wait()
        finally:
            worker.join(5); listener.close()
            output = proc.stdout.read() + proc.stderr.read()
            proc.stdout.close(); proc.stderr.close()
        self.assertFalse(worker.is_alive(), 'deadline peer must retire')
        self.assertFalse(timed_out, 'deadline child exceeded fixture bound')
        self.assertFalse(errors, repr(errors) + output.decode(errors='replace'))
        self.assertEqual(proc.returncode, 0, output.decode(errors='replace'))

    def test_reply_paired_after_deadline_is_timeout(self):
        self.reply_deadline('reply-deadline-late-reply')

    def test_error_paired_after_deadline_is_timeout(self):
        self.reply_deadline('reply-deadline-late-error')

    def test_timely_reply_survives_delayed_callback_return(self):
        self.reply_deadline('reply-deadline-timely-reply')

    def test_timely_error_survives_delayed_callback_return(self):
        self.reply_deadline('reply-deadline-timely-error')

    def peer(self, mode, payload=None, handshake=None, ft=2, program=None):
        errors = []
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0)); listener.listen(1); listener.settimeout(3)
        port = listener.getsockname()[1]
        def serve():
            try:
                try:
                    sock, _ = listener.accept()
                except socket.timeout:
                    if mode == 'tls': return
                    raise
                with sock:
                    sock.settimeout(4)
                    hello = receive(sock)
                    if mode == 'tls':
                        raise AssertionError('TLS request sent a plaintext HELLO')
                    self.assertEqual(hello[1], 1)
                    sock.sendall(frame(ft, ack_payload() if handshake is None else handshake) + (payload if mode == 'coalesced' or mode.startswith('negotiated-coalesced-') else b''))
                    if mode.startswith('completion-'):
                        ask = receive(sock)
                        self.assertEqual(ask[1], 0x10)
                        self.assertEqual(ask[24:35], b'/completion')
                        error = mode.endswith('-error')
                        body = b'\x03\0\0\0' if error else (b'' if mode.endswith('-empty') else b'\x2a')
                        reply = frame(0x12 if error else 0x11, body, cid=struct.unpack_from('<Q', ask, 4)[0])
                        invalid = frame(0x13, flags=1)
                        sock.sendall(invalid + reply if '-violation-first-' in mode else reply + invalid)
                        while sock.recv(4096): pass
                        return
                    if mode.startswith('negotiated-'):
                        limit = int(mode.rsplit('-', 1)[1])
                        if 'coalesced' not in mode:
                            exact = receive(sock)
                            self.assertEqual(len(exact), limit)
                            self.assertEqual(exact[1], 0x13)
                            sock.sendall(frame(0x13, b'\x00' * (limit - 28)) + frame(3, cid=77))
                            while True:
                                reply = receive(sock)
                                if reply[1] == 4:
                                    self.assertEqual(struct.unpack_from('<Q', reply, 4)[0], 77)
                                    break
                            sock.sendall(struct.pack('<I', limit + 1))
                        while sock.recv(4096): pass
                        return
                    if mode.startswith('error-code-'):
                        first = receive(sock)
                        self.assertEqual(first[1], 0x10)
                        sock.sendall(frame(0x12, payload, cid=struct.unpack_from('<Q', first, 4)[0]))
                        second = receive(sock)
                        self.assertEqual(second[1], 0x10)
                        sock.sendall(frame(0x11, b'\x63', cid=struct.unpack_from('<Q', second, 4)[0]))
                        while sock.recv(4096): pass
                        return
                    if mode == 'reject-handshake':
                        while sock.recv(4096): pass
                        return
                    if mode in ('disconnect', 'background-disconnect'):
                        self.assertEqual(receive(sock)[1], 0x10)
                        return
                    if mode == 'inflight-barrier':
                        requests = [receive(sock) for _ in range(8)]
                        self.assertEqual({body[-1] for body in requests}, set(range(8)))
                        self.assertEqual(len({struct.unpack_from('<Q',body,4)[0] for body in requests}), 8)
                        for body in reversed(requests):
                            self.assertEqual(body[1], 0x10)
                            sock.sendall(frame(0x11,body[-1:],cid=struct.unpack_from('<Q',body,4)[0]))
                        while sock.recv(4096): pass
                        return
                    if mode == 'concurrent-frames':
                        for _ in range(24):
                            body = receive(sock)
                            self.assertEqual(body[1], 0x10)
                            path_len = struct.unpack_from('<I', body,20)[0]
                            key_offset = 24 + path_len
                            key_len = struct.unpack_from('<I', body,key_offset)[0]
                            payload_offset = key_offset + 4 + key_len
                            reply_len = struct.unpack_from('<I', body,payload_offset)[0]
                            data = body[payload_offset+4+reply_len:]
                            self.assertEqual(len(data), 128*1024)
                            self.assertLess(data[0], 8)
                            self.assertEqual(data, bytes([data[0]])*len(data))
                            sock.sendall(frame(0x11,data[:1],cid=struct.unpack_from('<Q',body,4)[0]))
                        while sock.recv(4096): pass
                        return
                    if mode in ('exit-pump', 'dns-numeric'):
                        while sock.recv(4096): pass
                        return
                    if mode in ('outbound-bounds', 'limit'):
                        self.assertEqual(receive(sock)[1], 0x13)
                        while sock.recv(4096): pass
                        return
                    if mode == 'timeout-recovery':
                        first, second = receive(sock), receive(sock)
                        sock.sendall(frame(0x11, b'late', cid=struct.unpack_from('<Q',first,4)[0]) +
                                     frame(0x11, b'\x63', cid=struct.unpack_from('<Q',second,4)[0]))
                        while sock.recv(4096): pass
                        return
                    self.assertEqual(receive(sock)[1], 0x13)  # callback-ready barrier
                    if mode != 'coalesced': sock.sendall(payload)
                    if mode == 'callback-ask':
                        nested = receive(sock)
                        self.assertEqual(nested[1], 0x10)
                        sock.sendall(frame(0x11,b'\x63',cid=struct.unpack_from('<Q',nested,4)[0]))
                    while sock.recv(4096): pass
            except (ConnectionResetError, BrokenPipeError):
                pass  # protocol rejection closes the offending connection
            except BaseException as error:
                errors.append(error)
            finally:
                listener.close()
        worker = threading.Thread(target=serve, name='owned-cpp-test-peer')
        worker.start()
        try:
            result = subprocess.run([str(program or BINARY), mode, f'parrot://127.0.0.1:{port}'],
                                    capture_output=True, timeout=12,
                                    env={**os.environ, 'ASAN_OPTIONS': 'detect_stack_use_after_return=1', 'UBSAN_OPTIONS': 'halt_on_error=1'})
        finally:
            worker.join(5)
            listener.close()
        self.assertFalse(worker.is_alive(), 'test peer must retire')
        self.assertFalse(errors, repr(errors) + result.stderr.decode(errors='replace'))
        self.assertEqual(result.returncode, 0,
                         result.stdout.decode(errors='replace') + result.stderr.decode(errors='replace'))

    def backpressure(self, mode):
        errors = []
        done = threading.Event()
        listener = socket.socket()
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 65536)
        listener.bind(('127.0.0.1', 0)); listener.listen(1); listener.settimeout(3)
        proc = subprocess.Popen([str(BINARY), mode, f'parrot://127.0.0.1:{listener.getsockname()[1]}'],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        def serve():
            try:
                with listener.accept()[0] as sock:
                    sock.settimeout(3)
                    self.assertEqual(receive(sock)[1], 1)
                    if mode != 'close-handshake':
                        ack = ack_payload().replace(tlv(5, struct.pack('<I',1<<20)), tlv(5, struct.pack('<I',16<<20)))
                        sock.sendall(frame(2, ack))
                        prefix = sock.recv(4, socket.MSG_WAITALL)
                        self.assertEqual(len(prefix), 4)
                        self.assertGreater(struct.unpack('<I', prefix)[0], 512<<10)
                        self.assertLessEqual(struct.unpack('<I', prefix)[0], 1<<20)
                    if mode == 'heartbeat-backpressure': sock.sendall(frame(3,cid=77))
                    proc.stdin.write(b'g'); proc.stdin.flush()
                    if mode in ('queued-recovery', 'heartbeat-backpressure'):
                        self.assertEqual(proc.stdout.read(1), b'r')
                        size = struct.unpack('<I', prefix)[0]
                        data = bytearray()
                        while len(data) < size:
                            chunk = sock.recv(min(65536, size-len(data)))
                            if not chunk: raise EOFError('writer failed before complete frame')
                            data.extend(chunk)
                        self.assertEqual(data[-(512<<10):], b'\x5a'*(512<<10))
                        if mode == 'heartbeat-backpressure':
                            heartbeat = receive(sock)
                            self.assertEqual(heartbeat[1], 4)
                            self.assertEqual(struct.unpack_from('<Q',heartbeat,4)[0], 77)
                        following = receive(sock)
                        self.assertEqual(following[1], 0x13) # cancelled queued ASK was never sent
                        self.assertEqual(following[24:30], b'/after')
                    if mode == 'reconnect-write':
                        with listener.accept()[0] as replacement:
                            replacement.settimeout(3)
                            self.assertEqual(receive(replacement)[1], 1)
                            replacement.sendall(frame(2, ack_payload()))
                            ask = receive(replacement)
                            self.assertEqual(ask[1], 0x10)
                            replacement.sendall(frame(0x11,b'\x63',cid=struct.unpack_from('<Q',ask,4)[0]))
                            while replacement.recv(4096): pass
                    if not done.wait(8): raise AssertionError('child must finish without peer draining socket')
            except BaseException as error:
                errors.append(error)
        worker = threading.Thread(target=serve, name='owned-backpressure-peer'); worker.start()
        try:
            proc.wait(timeout=4)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait()
            self.fail(f'{mode}: child blocked beyond total deadline')
        finally:
            done.set(); worker.join(4); listener.close()
            proc.stdin.close()
            output = proc.stdout.read() + proc.stderr.read()
            proc.stdout.close(); proc.stderr.close()
        self.assertFalse(worker.is_alive())
        self.assertFalse(errors, repr(errors))
        self.assertEqual(proc.returncode, 0, output.decode(errors='replace'))

    def test_negotiated_prefix_and_outbound_limits(self):
        for limit in [28, 256, 1 << 20]:
            with self.subTest(limit=limit):
                declared = limit if limit < (1 << 20) else 2 << 20
                ack = ack_payload().replace(tlv(5, struct.pack('<I', 1 << 20)), tlv(5, struct.pack('<I', declared)))
                self.peer(f'negotiated-limit-{limit}', handshake=ack)

    def test_negotiated_prefix_buffered_behind_ack(self):
        for limit in [28, 256]:
            with self.subTest(limit=limit):
                ack = ack_payload().replace(tlv(5, struct.pack('<I', 1 << 20)), tlv(5, struct.pack('<I', limit)))
                self.peer(f'negotiated-coalesced-{limit}', handshake=ack, payload=struct.pack('<I', limit + 1))

    def test_global_handshake_limit_precedes_local_established_limit(self):
        ack = ack_payload() + tlv(255, b'x' * 40) * 32768
        self.peer('negotiated-limit-1048576', handshake=ack)

    def test_reconnect_cancels_only_previous_writer(self):
        self.backpressure('reconnect-write')

    def test_heartbeat_does_not_block_pump_on_writer(self):
        self.backpressure('heartbeat-backpressure')

    def test_default_tell_and_register_write_deadlines(self):
        for mode in ['tell-write-deadline', 'register-write-deadline']:
            with self.subTest(mode=mode):
                self.backpressure(mode)

    def test_numeric_connect_recovers_during_abandoned_dns(self):
        self.peer('dns-numeric', program=RESOLVER_BINARY)

    def test_dns_cancellation_and_bounded_worker(self):
        for mode in ['cancel', 'timeout']:
            with self.subTest(mode=mode):
                self.resolver_timing(mode)

    def resolver_timing(self, mode):
        result = subprocess.run([str(RESOLVER_BINARY), mode], capture_output=True, timeout=13)
        print('PARROT_CPP_RESOLVER_OBSERVATION ' + result.stderr.decode(errors='replace').strip(), flush=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors='replace'))

    def test_dns_timeout_ignores_observer_delay(self):
        self.resolver_timing('timeout-observer-delay')

    def test_queued_timeout_preserves_unsent_connection(self):
        self.backpressure('queued-recovery')

    def test_concurrent_writes_and_immediate_replies(self):
        self.peer('concurrent-frames')

    def test_ask_deadline_includes_blocked_write(self):
        self.backpressure('ask-write-deadline')

    def test_close_interrupts_blocked_write(self):
        self.backpressure('close-write')

    def test_ask_deadline_includes_waiting_for_writer(self):
        self.backpressure('queued-deadline')

    def test_close_interrupts_handshake(self):
        self.backpressure('close-handshake')

    def test_eight_requests_in_flight_before_first_reply(self):
        self.peer('inflight-barrier')

    def test_process_exit_retires_background_pump(self):
        self.peer('exit-pump')

    def test_callback_reentry(self):
        for mode in ['callback-close','callback-ask','callback-stop-pump']:
            with self.subTest(mode=mode):
                self.peer(mode,payload=frame(0x13,b'\x07\x08\x09',b'/target',b'pb:k'))

    def test_negotiated_outbound_limit(self):
        self.peer('limit',handshake=ack_payload().replace(tlv(5,struct.pack('<I',1<<20)),tlv(5,struct.pack('<I',64))))

    def test_advertised_inbound_limit(self):
        self.peer('invalid-message',payload=struct.pack('<I',(1<<20)+1))

    def test_late_reply_after_timeout(self):
        self.peer('timeout-recovery')

    def test_ack_and_business_frame_in_one_read(self):
        self.peer('coalesced',payload=frame(0x13,b'\x07\x08\x09',b'/target',b'pb:k'))

    def test_peer_eof_retires_stack_pending(self):
        self.peer('disconnect')

    def test_background_eof_completes_waiter(self):
        self.peer('background-disconnect')

    def test_outbound_bounds_before_copy(self):
        self.peer('outbound-bounds')

    def test_tls_request_never_sends_plaintext(self):
        self.peer('tls')

    def test_ack_requires_complete_negotiation(self):
        cases = [b'', ack_payload()+b'\x01', ack_payload()+tlv(1,b'again'),
                 ack_payload().replace(tlv(9,b'pb'),tlv(9,b'bin')),
                 ack_payload().replace(tlv(4,struct.pack('<I',2)),tlv(4,struct.pack('<I',1))),
                 ack_payload().replace(tlv(1,b'peer'),tlv(1,b'\xff')),
                 ack_payload().replace(tlv(7,b'\x08'),tlv(7,b'\0'))]
        for payload in cases:
            with self.subTest(payload=payload.hex()):
                self.peer('reject-handshake',handshake=payload)

    def test_unexpected_pre_handshake_frame_is_rejected(self):
        self.peer('reject-handshake',handshake=b'',ft=0x13)

    def test_malformed_ask_never_dispatches(self):
        for payload in [b'', b'\x01', struct.pack('<I',1), struct.pack('<I',0xffffffff),
                        struct.pack('<I',0xfffffffd), struct.pack('<I',1)+b'\xff',
                        struct.pack('<I',3)+b'a\0b']:
            with self.subTest(payload=payload.hex()):
                self.peer('invalid-message',payload=frame(0x10,payload,b'/target',b'pb:k'))

    def test_shared_error_payload_rejections_close_real_connection(self):
        rows = [line.split('\t') for line in
                (HERE.parent.parent/'docs/vectors/error-payloads.tsv').read_text().splitlines()
                if not line.startswith('#')]
        self.assertEqual(len(rows), 30)
        for name, result, payload, _, _ in rows:
            if result == 'reject':
                with self.subTest(name=name):
                    self.peer('invalid-message', payload=frame(0x12, bytes.fromhex(payload)))

    def test_shared_valid_errors_preserve_code_and_connection(self):
        rows = [line.split('\t') for line in
                (HERE.parent.parent/'docs/vectors/error-payloads.tsv').read_text().splitlines()
                if not line.startswith('#')]
        self.assertEqual(sum(row[1] == 'accept' for row in rows), 16)
        for name, result, payload, code, _ in rows:
            if result == 'accept':
                with self.subTest(name=name):
                    self.peer('error-code-' + code, payload=bytes.fromhex(payload))

    def test_payload_flags_are_not_silently_ignored(self):
        for flag in [1,4,16,32,0x8000]:
            with self.subTest(flag=flag):
                self.peer('invalid-message',payload=frame(0x13,b'\x07\x08\x09',b'/target',b'pb:k',flag))

    def test_c_string_identities_cannot_be_truncated(self):
        for path,key in [(b'/target\0other',b'pb:k'),(b'/target',b'pb:k\0other')]:
            with self.subTest(path=path,key=key):
                self.peer('invalid-message',payload=frame(0x13,b'\x07\x08\x09',path,key))

    def test_valid_ask_and_tell_deliver_original_payload(self):
        for ft in [0x10,0x13]:
            body = b'\x07\x08\x09'
            if ft == 0x10: body = struct.pack('<I',6)+b'/reply'+body
            with self.subTest(ft=ft):
                self.peer('valid-message',payload=frame(ft,body,b'/target',b'pb:k',flags=10))

if __name__ == '__main__':
    parser = argparse.ArgumentParser(); parser.add_argument('--sanitize', action='store_true')
    args, remaining = parser.parse_known_args()
    with tempfile.TemporaryDirectory(prefix='parrot-cpp-contract-') as temp:
        BINARY = pathlib.Path(temp)/'transport'
        flags = ['-fsanitize=address,undefined','-fno-sanitize-recover=all','-fno-omit-frame-pointer','-g'] if args.sanitize else []
        subprocess.run([os.environ.get('CXX','clang++'),'-std=c++17','-O1','-Wall','-Wextra','-Werror','-pthread',
                        *flags,str(HERE/'test_transport.cpp'),str(HERE/'parrot_lite.cpp'),'-o',str(BINARY)],check=True)
        RESOLVER_BINARY = pathlib.Path(temp)/'resolver'
        subprocess.run([os.environ.get('CXX','clang++'),'-std=c++17','-O1','-Wall','-Wextra','-Werror','-pthread',
                        *flags,str(HERE/'test_resolver.cpp'),'-o',str(RESOLVER_BINARY)],check=True)
        COMPLETION_BINARY = pathlib.Path(temp)/'completion'
        subprocess.run([os.environ.get('CXX','clang++'),'-std=c++17','-O1','-Wall','-Wextra','-Werror','-pthread',
                        *flags,str(HERE/'test_completion.cpp'),'-o',str(COMPLETION_BINARY)],check=True)
        POLL_BINARY = pathlib.Path(temp)/'pollset'
        FORCED_POLL_BINARY = pathlib.Path(temp)/'pollset-forced-poll'
        for program, backend_flags in [(POLL_BINARY, []), (FORCED_POLL_BINARY, ['-DPL_FORCE_POLL'])]:
            subprocess.run([os.environ.get('CXX','clang++'),'-std=c++17','-O1','-Wall','-Wextra','-Werror','-pthread',
                            *flags,*backend_flags,str(HERE/'test_pollset.cpp'),'-o',str(program)],check=True)
        ADDRESS_BINARY = pathlib.Path(temp)/'address'
        subprocess.run([os.environ.get('CXX','clang++'),'-std=c++17','-O1','-Wall','-Wextra','-Werror','-pthread',
                        *flags,str(HERE/'test_address.cpp'),'-o',str(ADDRESS_BINARY)],check=True)
        DECODER_LIMITS_BINARY = pathlib.Path(temp)/'decoder-limits'
        subprocess.run([os.environ.get('CXX','clang++'),'-std=c++17','-O1','-Wall','-Wextra','-Werror','-pthread',
                        *flags,str(HERE/'test_decoder_limits.cpp'),str(HERE/'parrot_lite.cpp'),
                        '-o',str(DECODER_LIMITS_BINARY)],check=True)
        HEARTBEAT_BINARY = pathlib.Path(temp)/'heartbeat'
        subprocess.run([os.environ.get('CXX','clang++'),'-std=c++17','-O1','-Wall','-Wextra','-Werror','-pthread',
                        *flags,str(HERE/'test_heartbeat.cpp'),str(HERE/'parrot_lite.cpp'),
                        '-o',str(HEARTBEAT_BINARY)],check=True)
        TERMINAL_ERROR_BINARY = pathlib.Path(temp)/'terminal-error'
        subprocess.run([os.environ.get('CXX','clang++'),'-std=c++17','-O1','-Wall','-Wextra','-Werror','-pthread',
                        *flags,str(HERE/'test_terminal_error.cpp'),'-o',str(TERMINAL_ERROR_BINARY)],check=True)
        unittest.main(argv=[__file__,*remaining], testRunner=ReceiptRunner)
