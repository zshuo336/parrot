"""Only the final response can confirm a batch after bounded informational heads."""
import json
import unittest

from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder
from test_trace_http_framing import raw_collector
from test_trace_http_head import bounded_head


FINAL = (b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n'
         b'Connection: close\r\nContent-Length: 2\r\n\r\n{}')


def interim(code, fields=b''):
    return b'HTTP/1.1 ' + str(code).encode() + b' Interim\r\n' + fields + b'\r\n'


class TraceHttpInformationalTests(unittest.TestCase):
    def exchange(self, response):
        with raw_collector(response) as (endpoint, received):
            recorder = TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=0.5, max_attempts=1, interval=0.01)))
            with recorder.handler('/user/orders', 'confirmed', 'ask'):
                pass
            result = recorder.finish_export(3)
            self.assertEqual(len(received), 1)
            self.assertFalse(result['running'], 'export owner must retire')
            return result, recorder.snapshot()

    def check(self, response, accepted):
        result, snapshot = self.exchange(response)
        self.assertEqual(result['ok'], accepted, result)
        self.assertEqual((result['exported'], result['pending'], snapshot['acknowledged']),
                         (1, 0, 1) if accepted else (0, 1, 0))
        if not accepted:
            self.assertIsNotNone(result['error'])

    def test_valid_informational_responses_wait_for_the_final_receipt(self):
        for codes in [(100,), (102,), (103,), (199,), (100, 103, 102, 199, 103)]:
            with self.subTest(codes=codes):
                self.check(b''.join(interim(code, b'X-Info: preparing\r\n')
                                    for code in codes) + FINAL, True)

    def test_informational_framing_fields_are_rejected_even_before_a_valid_final(self):
        for code in [100, 102, 103, 199]:
            for field in [b'Content-Length: 0\r\n', b'Transfer-Encoding: chunked\r\n']:
                with self.subTest(code=code, field=field):
                    self.check(interim(code, field) + FINAL, False)

    def test_switching_protocols_nonfinal_and_failed_final_never_acknowledge(self):
        for response in [interim(101) + FINAL, interim(103),
                         interim(103) + FINAL.replace(b'200 OK', b'503 Unavailable'),
                         interim(103) + FINAL[:-1],
                         interim(103) + FINAL.replace(b'{}', b'{"partialSuccess":{"rejectedSpans":"1"}}')
                             .replace(b'Content-Length: 2\r\n', b'')]:
            with self.subTest(response=response):
                self.check(response, False)

    def test_interim_heads_share_the_exact_final_head_byte_limit(self):
        prefix = interim(100) + interim(103, b'Link: </v1/traces>\r\n') + interim(199)
        for size in [65536, 65537]:
            with self.subTest(size=size):
                self.check(prefix + bounded_head(size - len(prefix)) + b'{}', size == 65536)

    def test_invalid_interim_heads_cannot_be_hidden_by_a_valid_final(self):
        for fields in [b'Invalid-Header-Line\r\n', b' Folded: value\r\n',
                       b'X-Info: \x00\r\n', b'X-Info: value\n']:
            with self.subTest(fields=fields):
                self.check(interim(103, fields) + FINAL, False)

    def test_retry_after_invalid_interim_preserves_the_exact_span(self):
        with raw_collector([interim(100, b'Content-Length: 0\r\n') + FINAL,
                            interim(103) + FINAL]) as (endpoint, received):
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
