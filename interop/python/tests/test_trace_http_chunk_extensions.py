"""Only complete chunk-extension grammar can confirm a trace export."""
import json
import unittest

from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder
from test_trace_http_framing import raw_collector

HEAD = (b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n'
        b'Connection: close\r\nTransfer-Encoding: chunked\r\n\r\n')
INVALID = [
    b';', b';=value', b';bad name=value', b';name=', b';name=bad value',
    b';name="unfinished', b';name="closed"junk', b';name="trailing\\',
    b';name=\x00', b';name=\x01', b';name=\x7f', b';name=\xff',
    b';name="nul\x00"', b';name="del\x7f"', b';name="escaped\\\x00"',
    b';name="escaped\\\x7f"', b';na\x80me=value', b';name=value;',
]
VALID = [
    b'', b';name', b';name=value', b';name=""', b';name="two words"',
    b';name="a;b=c"', b';name="escaped\\\"quote\\\\slash"',
    b';name="tab\tand\xff"', b';name="escaped\\\xff"',
    b';one=first;two="second";flag', b' \t; \tname \t= \tvalue \t; next',
    b";!#$%&'*+-.^_`|~012AZaz=!#$%&'*+-.^_`|~012AZaz",
]


def response(extension, terminal=False):
    return HEAD + (b'2\r\n{}\r\n0' + extension + b'\r\n\r\n' if terminal else
                   b'2' + extension + b'\r\n{}\r\n0\r\n\r\n')


class TraceHttpChunkExtensionTests(unittest.TestCase):
    def exchange(self, raw):
        with raw_collector(raw) as (endpoint, received):
            recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=0.5, max_attempts=1, interval=0.01)))
            with recorder.handler('/user/orders', 'confirmed', 'ask'):
                pass
            result = recorder.finish_export(3)
            self.assertEqual(len(received), 1)
            self.assertFalse(result['running'])
            return result, recorder.snapshot()

    def test_malformed_data_and_terminal_extensions_cannot_release_records(self):
        for terminal in [False, True]:
            for extension in INVALID:
                with self.subTest(terminal=terminal, extension=extension):
                    result, snapshot = self.exchange(response(extension, terminal))
                    self.assertFalse(result['ok'], 'malformed extension acknowledged an unconfirmed batch')
                    self.assertEqual(result['error'], 'TraceExportRejected')
                    self.assertEqual((result['pending'], result['exported'], snapshot['acknowledged']), (1, 0, 0))

    def test_tokens_quotes_escaping_optional_whitespace_and_obs_text_are_supported(self):
        for terminal in [False, True]:
            for extension in VALID:
                with self.subTest(terminal=terminal, extension=extension):
                    result, snapshot = self.exchange(response(extension, terminal))
                    self.assertTrue(result['ok'], result)
                    self.assertEqual((result['pending'], result['exported'], snapshot['acknowledged']), (0, 1, 1))

    def test_extension_line_accepts_exact_bound_and_rejects_one_byte_over(self):
        for terminal in [False, True]:
            for size in [8192, 8193]:
                with self.subTest(terminal=terminal, size=size):
                    extension = b';name=' + b'x' * (size - 9)
                    self.assertEqual(len(b'2' + extension + b'\r\n'), size)
                    result, snapshot = self.exchange(response(extension, terminal))
                    self.assertEqual(result['ok'], size == 8192)
                    self.assertEqual(snapshot['acknowledged'], int(size == 8192))

    def test_broken_extension_after_valid_extension_still_rejects_the_receipt(self):
        result, snapshot = self.exchange(response(b';valid=yes;broken="unterminated'))
        self.assertFalse(result['ok'])
        self.assertEqual((result['pending'], result['exported'], snapshot['acknowledged']), (1, 0, 0))

    def test_retry_after_invalid_terminal_extension_preserves_exact_span_identity(self):
        with raw_collector([response(b';name="unterminated', True), response(b';accepted=yes', True)]) as (endpoint, received):
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
