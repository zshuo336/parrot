"""Only a syntactically valid HTTP status line can acknowledge an OTLP batch."""
import json
import unittest

from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder
from test_trace_http_framing import raw_collector


FIELDS = b'Content-Type: application/json\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}'
INVALID_FINAL = [
    b'HTTP/1.1 +200 OK\r\n',
    b'HTTP/1.1 0200 OK\r\n',
    b'HTTP/1.1  200 OK\r\n',
    b'HTTP/1.1\t200 OK\r\n',
    b' HTTP/1.1 200 OK\r\n',
    b'HTTP/1.1 200\tOK\r\n',
    b'HTTP/1.1 200\r\n',
    b'HTTP/1.1 200 OK\x00\r\n',
    b'HTTP/1.1 200 OK\x01\r\n',
    b'HTTP/1.1 200 OK\x7f\r\n',
    b'HTTP/1.1 200 OK\rreason\r\n',
    b'HTTP/1.10 200 OK\r\n',
    b'HTTP/1.x 200 OK\r\n',
]


class TraceHttpStatusTests(unittest.TestCase):
    def exchange(self, response):
        with raw_collector(response) as (endpoint, received):
            recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=0.5, max_attempts=1, interval=0.01)))
            with recorder.handler('/user/orders', 'confirmed', 'ask'):
                pass
            result = recorder.finish_export(3)
            self.assertEqual(len(received), 1, 'collector must receive the actual batch')
            self.assertFalse(result['running'], 'export owner must retire')
            return result, recorder.snapshot()

    def rejected(self, response):
        result, snapshot = self.exchange(response)
        self.assertFalse(result['ok'], 'malformed status released an unconfirmed batch')
        self.assertEqual(result['error'], 'TraceExportRejected')
        self.assertEqual((result['attempts'], result['pending'], result['exported']), (1, 1, 0))
        self.assertEqual(snapshot['acknowledged'], 0)
        self.assertEqual(len(snapshot['records']), 1)

    def test_invalid_final_status_cannot_acknowledge_records(self):
        for line in INVALID_FINAL:
            with self.subTest(line=line):
                self.rejected(line + FIELDS)

    def test_invalid_interim_status_cannot_be_hidden_by_a_valid_final_response(self):
        for line in [b'HTTP/1.1 +100 Continue\r\n', b'HTTP/1.1 0100 Continue\r\n',
                     b'HTTP/1.1\t100 Continue\r\n', b'HTTP/1.1 100 Continue\x00\r\n',
                     b'HTTP/1.x 100 Continue\r\n']:
            with self.subTest(line=line):
                self.rejected(line + b'\r\nHTTP/1.1 200 OK\r\n' + FIELDS)

    def test_valid_versions_empty_reason_whitespace_and_obs_text_are_supported(self):
        for version in [b'HTTP/1.0', b'HTTP/1.1']:
            for reason in [b'', b'OK', b'accepted\twith details', b'latin-1-\xff', b'  accepted  ']:
                with self.subTest(version=version, reason=reason):
                    result, snapshot = self.exchange(version + b' 200 ' + reason + b'\r\n' + FIELDS)
                    self.assertTrue(result['ok'], result)
                    self.assertIsNone(result['error'])
                    self.assertEqual((result['pending'], result['exported'], snapshot['acknowledged']), (0, 1, 1))

    def test_status_line_at_exact_byte_limit_is_accepted_and_one_over_is_rejected(self):
        for size in [8192, 8193]:
            line = b'HTTP/1.1 200 ' + b'x' * (size - 15) + b'\r\n'
            self.assertEqual(len(line), size)
            result, snapshot = self.exchange(line + FIELDS)
            if size == 8192:
                self.assertTrue(result['ok'], result)
                self.assertEqual(snapshot['acknowledged'], 1)
            else:
                self.assertFalse(result['ok'])
                self.assertEqual((result['pending'], result['exported'], snapshot['acknowledged']), (1, 0, 0))

    def test_retry_after_invalid_status_preserves_the_exact_span_identity(self):
        with raw_collector([b'HTTP/1.1 +200 OK\r\n' + FIELDS,
                            b'HTTP/1.1 200 OK\r\n' + FIELDS]) as (endpoint, received):
            recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=0.5, max_attempts=1, interval=0.01)))
            with recorder.handler('/user/orders', 'confirmed', 'ask'):
                pass
            first = recorder.finish_export(3)
            snapshot = recorder.snapshot()
            second = recorder.finish_export(3)
            self.assertFalse(first['running'])
            self.assertFalse(first['ok'])
            self.assertEqual((first['pending'], first['exported'], snapshot['acknowledged']), (1, 0, 0))
            self.assertTrue(second['ok'], second)
            self.assertEqual((second['attempts'], second['pending'], second['exported']), (2, 0, 1))
            self.assertEqual(len(received), 2)
            self.assertEqual(received[0][1], received[1][1])
            span = json.loads(received[1][1])['resourceSpans'][0]['scopeSpans'][0]['spans'][0]
            self.assertEqual(span['spanId'], snapshot['records'][0]['span_id'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
