"""Malformed HTTP trailers cannot acknowledge an OTLP batch."""
import json
import unittest

from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder
from test_trace_http_framing import raw_collector


HEAD = (b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n'
        b'Connection: close\r\nTransfer-Encoding: chunked\r\n\r\n')
BODY = b'2\r\n{}\r\n0\r\n'
INVALID_TRAILERS = [
    b'Missing-Colon',
    b': empty-name',
    b'Bad Name: value',
    b'Bad\tName: value',
    b'Bad(Name): value',
    b'Bad\x80Name: value',
    b'Bad\x00Name: value',
    b'X-Info : value',
    b'X-Info\t: value',
    b' X-Info: folded',
    b'\tX-Info: folded',
    b'X-Info: a\x00b',
    b'X-Info: a\x01b',
    b'X-Info: a\x7fb',
    b'X-Info: a\rb',
    b'X-Info: a\nb',
]


class TraceHttpTrailerTests(unittest.TestCase):
    def exchange(self, trailers):
        with raw_collector(HEAD + BODY + trailers + b'\r\n') as (endpoint, received):
            recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=0.5, max_attempts=1, interval=0.01)))
            with recorder.handler('/user/orders', 'confirmed', 'ask'):
                pass
            result = recorder.finish_export(3)
            self.assertEqual(len(received), 1, 'collector must receive a real batch')
            self.assertFalse(result['running'], 'export owner must retire')
            return result, recorder.snapshot()

    def test_invalid_trailer_fields_cannot_acknowledge_records(self):
        for field in INVALID_TRAILERS:
            with self.subTest(field=field):
                result, snapshot = self.exchange(field + b'\r\n')
                self.assertFalse(result['ok'], 'invalid trailer released an unconfirmed batch')
                self.assertEqual(result['error'], 'TraceExportRejected')
                self.assertEqual((result['attempts'], result['pending'], result['exported']), (1, 1, 0))
                self.assertEqual(snapshot['acknowledged'], 0)
                self.assertEqual(len(snapshot['records']), 1)

    def test_valid_trailer_fields_and_optional_whitespace_are_supported(self):
        fields = [
            b'',
            b'X-Info: value\r\n',
            b'X-Empty:\r\n',
            b'X-Info:\t latin-1-\xff\t\r\n',
            b"!#$%&'*+-.^_`|~012AZaz: value\r\n",
            b'X-Info: one\r\nX-Info: two\r\n',
            b'X-Info: value\r\n' * 100,
        ]
        for trailers in fields:
            with self.subTest(trailers=trailers):
                result, snapshot = self.exchange(trailers)
                self.assertTrue(result['ok'], result)
                self.assertIsNone(result['error'])
                self.assertEqual((result['pending'], result['exported'], snapshot['acknowledged']), (0, 1, 1))

    def test_valid_trailer_line_at_exact_limit_is_accepted(self):
        trailers = b'X-Info: ' + b'x' * (8192 - 10) + b'\r\n'
        self.assertEqual(len(trailers), 8192)
        result, snapshot = self.exchange(trailers)
        self.assertTrue(result['ok'], result)
        self.assertEqual(snapshot['acknowledged'], 1)

    def test_folded_trailer_after_valid_field_is_rejected(self):
        result, snapshot = self.exchange(b'X-Info: valid\r\n continued\r\n')
        self.assertFalse(result['ok'])
        self.assertEqual((result['pending'], result['exported'], snapshot['acknowledged']), (1, 0, 0))

    def test_retry_after_invalid_trailer_preserves_exact_span_identity(self):
        with raw_collector([HEAD + BODY + b'Missing-Colon\r\n\r\n',
                            HEAD + BODY + b'X-Info: accepted\r\n\r\n']) as (endpoint, received):
            recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=0.5, max_attempts=1, interval=0.01)))
            with recorder.handler('/user/orders', 'confirmed', 'ask'):
                pass
            first = recorder.finish_export(3)
            snapshot = recorder.snapshot()
            # Always retire the second exchange, including with a broken exporter.
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
