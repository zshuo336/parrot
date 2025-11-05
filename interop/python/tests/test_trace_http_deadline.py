"""A collector that keeps sending bytes must not hold the export owner open."""
from contextlib import contextmanager
import http.client
import json
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
import threading
import unittest

from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder
from test_trace_http_framing import raw_collector

HEAD = b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n'


@contextmanager
def dripping_collector(responses, tls=None):
    listener = socket.socket()
    listener.bind(('127.0.0.1', 0))
    listener.listen(1)
    listener.settimeout(0.1)
    stopping, received = threading.Event(), threading.Event()
    requests, errors, sends = [], [], []

    def serve():
        try:
            for prefix, drip in responses:
                while not stopping.is_set():
                    try:
                        connection, _ = listener.accept()
                        break
                    except socket.timeout:
                        continue
                else:
                    return
                with connection:
                    connection.settimeout(5)
                    if tls is not None:
                        connection = tls.wrap_socket(connection, server_side=True)
                    with connection:
                        with connection.makefile('rb') as stream:
                            request = stream.readline()
                            headers = http.client.parse_headers(stream)
                            body = stream.read(int(headers['Content-Length']))
                            requests.append((request, body))
                        try:
                            connection.sendall(prefix)
                            received.set()
                            while drip is not None and not stopping.wait(0.02):
                                connection.sendall(drip)
                                sends.append(1)
                        except (BrokenPipeError, ConnectionResetError, ssl.SSLEOFError):
                            pass
        except BaseException as error:
            errors.append(error)

    thread = threading.Thread(target=serve, name='deadline-test-collector')
    thread.start()
    scheme = 'https' if tls is not None else 'http'
    endpoint = scheme + '://127.0.0.1:' + str(listener.getsockname()[1]) + '/v1/traces'
    try:
        yield endpoint, received, requests, sends, stopping
    finally:
        stopping.set()
        thread.join(6)
        listener.close()
        if thread.is_alive():
            raise RuntimeError('dripping collector did not retire')
        if errors:
            raise RuntimeError('dripping collector failed') from errors[0]


class TraceHttpDeadlineTests(unittest.TestCase):
    def bounded(self, prefix, drip, tls=None, ca_file=None):
        recorder = None
        try:
            with dripping_collector([(prefix, drip)], tls) as (endpoint, received, requests, sends, stopping):
                recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                    request_timeout=0.4, max_attempts=1, interval=0.01, ca_file=ca_file)))
                with recorder.handler('/user/orders', 'slow-receipt', 'ask'):
                    pass
                self.assertTrue(received.wait(5), 'collector never received the real request')
                result = recorder.finish_export(1.6)
                self.assertGreaterEqual(len(sends), 3, 'collector did not exercise continuous delivery')
                self.assertFalse(result['running'], 'continuous response bytes defeated the export timeout')
                self.assertFalse(result['ok'])
                self.assertEqual(result['error'], socket.timeout.__name__)
                self.assertEqual((result['attempts'], result['pending'], result['exported']), (1, 1, 0))
                self.assertEqual(recorder.snapshot()['acknowledged'], 0)
                self.assertEqual(len(requests), 1)
        finally:
            # Stopping the peer releases the original implementation too. Never
            # call finish_export here: it would restart a failed export owner.
            if recorder is not None:
                recorder._exporter._thread.join(6)
                self.assertFalse(recorder._exporter._thread.is_alive(), 'test left an export owner alive')

    def test_slow_status_line_has_a_total_response_budget(self):
        self.bounded(b'HTTP/1.1 200 ', b'x')

    def test_slow_header_line_has_a_total_response_budget(self):
        self.bounded(HEAD + b'X-Info: ', b'x')

    def test_continuous_informational_heads_share_one_response_budget(self):
        self.bounded(b'HTTP/1.1 100 Continue\r\n\r\n', b'HTTP/1.1 100 Continue\r\n\r\n')

    def test_slow_fixed_length_body_cannot_extend_the_response_budget(self):
        self.bounded(HEAD + b'Content-Length: 10000\r\n\r\n{}', b' ')

    def test_slow_close_delimited_body_cannot_extend_the_response_budget(self):
        self.bounded(HEAD + b'\r\n{}', b' ')

    def test_slow_chunk_metadata_cannot_extend_the_response_budget(self):
        self.bounded(HEAD + b'Transfer-Encoding: chunked\r\n\r\n2;', b'x')

    def test_slow_chunk_body_cannot_extend_the_response_budget(self):
        self.bounded(HEAD + b'Transfer-Encoding: chunked\r\n\r\n1000\r\n{', b' ')

    def test_slow_trailers_cannot_extend_the_response_budget(self):
        self.bounded(HEAD + b'Transfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\nX-Info: ', b'x')

    def test_tls_body_uses_the_same_total_response_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cert, key = root / 'cert.pem', root / 'key.pem'
            subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                '-keyout', str(key), '-out', str(cert), '-subj', '/CN=localhost',
                '-addext', 'subjectAltName=IP:127.0.0.1'], check=True, capture_output=True, timeout=10)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(cert, key)
            self.bounded(HEAD + b'Content-Length: 10000\r\n\r\n{}', b' ', context, str(cert))

    def test_retry_after_response_deadline_preserves_the_exact_span_identity(self):
        recorder = None
        bad = HEAD + b'Content-Length: 10000\r\n\r\n{}'
        good = HEAD + b'Content-Length: 2\r\n\r\n{}'
        try:
            with dripping_collector([(bad, b' '), (good, None)]) as (endpoint, received, requests, sends, stopping):
                recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                    request_timeout=0.4, max_attempts=1, interval=0.01)))
                with recorder.handler('/user/orders', 'slow-receipt', 'ask'):
                    pass
                self.assertTrue(received.wait(5))
                first = recorder.finish_export(1.6)
                self.assertFalse(first['running'], 'slow response kept the owner alive beyond its budget')
                self.assertEqual((first['pending'], first['exported']), (1, 0))
                identity = recorder.snapshot()['records'][0]['span_id']
                second = recorder.finish_export(3)
                self.assertTrue(second['ok'], second)
                self.assertEqual((second['attempts'], second['pending'], second['exported']), (2, 0, 1))
                self.assertEqual(len(requests), 2)
                self.assertEqual(json.loads(requests[0][1]), json.loads(requests[1][1]))
                documents = json.loads(requests[1][1])
                span = documents['resourceSpans'][0]['scopeSpans'][0]['spans'][0]
                self.assertEqual(span['spanId'], identity)
        finally:
            if recorder is not None:
                recorder._exporter._thread.join(6)
                self.assertFalse(recorder._exporter._thread.is_alive())

    def test_fast_fixed_chunked_and_close_delimited_receipts_remain_valid(self):
        for fields, body in [(b'Content-Length: 2\r\n', b'{}'),
                             (b'Transfer-Encoding: chunked\r\n', b'2\r\n{}\r\n0\r\n\r\n'),
                             (b'', b'{}')]:
            with self.subTest(fields=fields), raw_collector(HEAD + fields + b'\r\n' + body) as (endpoint, requests):
                recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                    request_timeout=0.4, max_attempts=1, interval=0.01)))
                with recorder.handler('/user/orders', 'fast-receipt', 'ask'):
                    pass
                result = recorder.finish_export(3)
                self.assertTrue(result['ok'], result)
                self.assertFalse(result['running'])
                self.assertEqual((result['pending'], result['exported']), (0, 1))
                self.assertEqual(len(requests), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
