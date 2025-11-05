"""Actual TCP receipts must have a complete, valid, bounded HTTP response head."""
import io
import json
import unittest

from parrot_protocol.trace_export import _ReceiptResponse, _read_receipt, _accepted
from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder
from test_trace_http_framing import raw_collector

PREFIX = b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n'


def bounded_head(size):
    head = PREFIX + b'Content-Length: 2\r\n'
    remaining = size - len(head) - 2
    while remaining > 8192:
        head += b'X-Padding: ' + b'x' * (8192 - 13) + b'\r\n'
        remaining -= 8192
    if remaining < 13:
        raise ValueError('invalid fixture head size')
    head += b'X-Padding: ' + b'x' * (remaining - 13) + b'\r\n\r\n'
    if len(head) != size:
        raise ValueError('fixture did not produce the requested head size')
    return head


class TraceHttpHeadTests(unittest.TestCase):
    def exchange(self, response):
        with raw_collector(response) as (endpoint, received):
            recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=0.5, max_attempts=1, interval=0.01)))
            with recorder.handler('/user/orders', 'confirmed', 'ask'):
                pass
            result = recorder.finish_export(3)
            self.assertGreaterEqual(len(received), 1, 'collector did not receive a real export')
            self.assertFalse(result['running'], 'export owner did not retire')
            return result, recorder.snapshot()

    def rejected(self, response, reason='invalid HTTP head must retain its records'):
        result, snapshot = self.exchange(response)
        self.assertFalse(result['ok'], reason)
        self.assertIsNotNone(result['error'], 'head rejection needs an explicit error receipt')
        self.assertEqual(result['exported'], 0)
        self.assertEqual(result['pending'], 1)
        self.assertEqual(snapshot['acknowledged'], 0)
        self.assertEqual(len(snapshot['records']), 1)

    def accepted(self, response):
        result, snapshot = self.exchange(response)
        self.assertTrue(result['ok'], result)
        self.assertIsNone(result['error'])
        self.assertEqual((result['exported'], result['pending'], snapshot['acknowledged']), (1, 0, 1))

    def test_malformed_header_cannot_hide_truncated_content_length(self):
        self.rejected(PREFIX + b'Invalid-Header-Line\r\nContent-Length: 100\r\n\r\n{}',
            'malformed header hid Content-Length and released an unconfirmed record')

    def test_malformed_header_cannot_hide_conflicting_transfer_framing(self):
        self.rejected(PREFIX + b'Content-Length: 2\r\nInvalid-Header-Line\r\n'
            b'Transfer-Encoding: chunked\r\n\r\n{}')

    def test_invalid_field_names_and_whitespace_before_colon_are_rejected(self):
        for name in [b'', b'Bad(Name)', b'Bad Name', b'Bad\tName', b'Bad\x80Name', b'Bad\x00Name', b'Content-Length ']:
            with self.subTest(name=name):
                self.rejected(PREFIX + name + b': ignored\r\nContent-Length: 2\r\n\r\n{}')

    def test_folded_headers_are_not_interpreted_as_successful_receipts(self):
        for first in [b'X-Info: ignored\r\n', b'']:
            with self.subTest(first=first):
                self.rejected(PREFIX + first + b' Content-Length: 100\r\n\r\n{}')

    def test_control_bytes_in_header_values_are_rejected(self):
        for value in [b'a\x00b', b'a\x01b', b'a\x7fb', b'a\rb']:
            with self.subTest(value=value):
                self.rejected(PREFIX + b'X-Info: ' + value + b'\r\nContent-Length: 2\r\n\r\n{}')

    def test_bare_lf_in_status_field_and_separator_is_rejected(self):
        response = PREFIX + b'Content-Length: 2\r\n\r\n{}'
        for before, after in [(b'200 OK\r\n', b'200 OK\n'),
                              (b'Content-Length: 2\r\n', b'Content-Length: 2\n'),
                              (b'\r\n\r\n{}', b'\r\n\n{}')]:
            with self.subTest(before=before):
                self.rejected(response.replace(before, after))

    def test_unterminated_head_is_not_a_receipt(self):
        self.rejected(PREFIX + b'Content-Length: 2\r\n')

    def test_header_line_exact_bound_and_one_byte_over(self):
        for length in [8192, 8193]:
            response = PREFIX + b'X-Info: ' + b'x' * (length - 10) + b'\r\nContent-Length: 2\r\n\r\n{}'
            with self.subTest(line_bytes=length):
                (self.accepted if length == 8192 else self.rejected)(response)

    def test_total_head_exact_bound_and_one_byte_over(self):
        for size in [65536, 65537]:
            with self.subTest(head_bytes=size):
                (self.accepted if size == 65536 else self.rejected)(bounded_head(size) + b'{}')

    def test_informational_heads_share_the_final_head_byte_budget(self):
        interim = b'HTTP/1.1 100 Continue\r\nX-Info: preparing\r\n\r\n'
        self.accepted(interim + bounded_head(65536 - len(interim)) + b'{}')
        self.rejected(interim + bounded_head(65537 - len(interim)) + b'{}')

    def test_malformed_informational_headers_are_rejected(self):
        self.rejected(b'HTTP/1.1 100 Continue\r\nInvalid-Header-Line\r\n\r\n' +
            PREFIX + b'Content-Length: 2\r\n\r\n{}')

    def test_valid_field_tokens_ows_obs_text_and_body_framing_remain_supported(self):
        fields = b"!#$%&'*+-.^_`|~012AZaz: value\r\nX-Info:\t latin-1-\xff\t\r\n"
        for version in [b'HTTP/1.0', b'HTTP/1.1']:
            for framing, body in [(b'Content-Length: 2\r\n', b'{}'),
                                  (b'Transfer-Encoding: chunked\r\n', b'2\r\n{}\r\n0\r\n\r\n'),
                                  (b'', b'{}')]:
                with self.subTest(version=version, framing=framing):
                    self.accepted(PREFIX.replace(b'HTTP/1.1', version) + fields + framing + b'\r\n' + body)

    def test_repeated_begin_keeps_the_original_parsed_response(self):
        class Socket:
            def makefile(self, *args):
                return io.BytesIO(PREFIX + b'Content-Length: 2\r\n\r\n{}')
        with _ReceiptResponse(Socket()) as response:
            response.begin()
            headers = response.headers
            response.begin()
            self.assertIs(response.headers, headers)
            body = _read_receipt(response)
            self.assertEqual(body, b'{}')
            _accepted(body, 1)

    def test_explicit_retry_after_head_rejection_keeps_the_exact_span_identity(self):
        bad = PREFIX + b'Invalid-Header-Line\r\nContent-Length: 100\r\n\r\n{}'
        good = PREFIX + b'Content-Length: 2\r\n\r\n{}'
        with raw_collector([bad, good]) as (endpoint, received):
            recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=0.5, max_attempts=1, interval=0.01)))
            with recorder.handler('/user/orders', 'confirmed', 'ask'):
                pass
            first = recorder.finish_export(3)
            try:
                self.assertFalse(first['ok'], 'malformed head must not acknowledge or discard the batch')
                self.assertEqual((first['pending'], first['exported']), (1, 0))
            finally:
                second = recorder.finish_export(3)
            self.assertTrue(second['ok'], second)
            self.assertEqual((second['pending'], second['exported']), (0, 1))
            self.assertEqual(len(received), 2)
            self.assertEqual(json.loads(received[0][1]), json.loads(received[1][1]))


if __name__ == '__main__':
    unittest.main(verbosity=2)
