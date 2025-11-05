"""Application parser/state contracts against actual component dispatchers."""
import importlib.util
from pathlib import Path
import struct
import sys
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'interop/python'))


def payload(entries):
    return struct.pack('<I', len(entries)) + b''.join(struct.pack('<QI', doc, len(text)) + text for doc, text in entries)


class Contracts:
    def setUp(self):
        spec = importlib.util.spec_from_file_location('component_contract', ROOT / self.module)
        self.component = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.component)
        self.dispatcher = self.component.build_dispatcher()

    def stats(self, dispatcher=None):
        _, data = (dispatcher or self.dispatcher).dispatch(self.stats_key, b'')
        return struct.unpack('<IQ', data)

    def test_every_truncation_rejects_without_partial_index(self):
        valid = payload([(17, b'alpha'), (29, b'beta')])
        for length in range(len(valid)):
            with self.subTest(length=length):
                with self.assertRaises((ValueError, struct.error)):
                    self.dispatcher.dispatch(self.key, valid[:length])
                self.assertEqual(self.stats(), (0, 0))

    def test_counts_and_trailing_bytes_reject_without_mutation(self):
        valid = payload([(17, b'alpha')])
        for bad in [valid+b'\0', struct.pack('<I', 0)+valid[4:], struct.pack('<I', 2)+valid[4:]]:
            with self.subTest(payload=bad):
                with self.assertRaises((ValueError, struct.error)):
                    self.dispatcher.dispatch(self.key, bad)
                self.assertEqual(self.stats(), (0, 0))

    def test_stats_request_requires_empty_payload(self):
        with self.assertRaises(ValueError):
            self.dispatcher.dispatch(self.stats_key, b'\0')

    def test_late_tokenization_error_has_no_partial_business_state(self):
        def tokenize(text):
            if text in (b'fail', 'fail'):
                raise ValueError('injected second-document failure')
            return ['alpha']
        with patch.object(self.component, '_tokenize', tokenize):
            with self.assertRaisesRegex(ValueError, 'second-document'):
                self.dispatcher.dispatch(self.key, payload([(17, b'alpha'), (29, b'fail')]))
        self.assertEqual(self.stats(), (0, 0))

    def test_separate_dispatchers_have_separate_index_state(self):
        second = self.component.build_dispatcher()
        self.dispatcher.dispatch(self.key, payload([(17, b'alpha')]))
        self.assertEqual(self.stats(), (1, 1))
        self.assertEqual(self.stats(second), (0, 0))

    def test_independent_valid_unicode_and_empty_vectors(self):
        self.assertEqual(self.component._decode_pages(payload([])) if hasattr(self.component, '_decode_pages') else self.component._decode_texts(payload([])), [])
        self.dispatcher.dispatch(self.key, payload([(17, '中文 alphaword'.encode())]))
        terms, postings = self.stats()
        self.assertGreaterEqual(terms, 2)
        self.assertEqual(terms, postings)

    def test_document_budget_is_checked_before_processing(self):
        wire = struct.pack('<I', 65_537) + bytes(12 * 65_537)
        with patch.object(self.component, '_tokenize', side_effect=AssertionError('must not tokenize')):
            with self.assertRaises(ValueError):
                self.dispatcher.dispatch(self.key, wire)
        self.assertEqual(self.stats(), (0, 0))


class CrawlerContracts(Contracts, unittest.TestCase):
    module = 'apps/crawler-lab/python/index_builder.py'
    key = 'bin:crawl/IndexPage'
    stats_key = 'bin:crawl/IndexStats'


class WebsearchContracts(Contracts, unittest.TestCase):
    module = 'apps/websearch/python/tokenizer.py'
    key = 'bin:ws/Tokenize'
    stats_key = 'bin:ws/IndexStats'

    def test_retokenization_replaces_obsolete_and_empty_document_terms(self):
        self.dispatcher.dispatch(self.key, payload([(17, b'alpha beta'), (29, b'beta')]))
        self.assertEqual(self.stats(), (2, 3))
        replacement = payload([(17, b'gamma')])
        self.dispatcher.dispatch(self.key, replacement)
        self.assertEqual(self.stats(), (2, 2))
        self.dispatcher.dispatch(self.key, replacement)
        self.assertEqual(self.stats(), (2, 2))
        self.dispatcher.dispatch(self.key, payload([(17, b'')]))
        self.assertEqual(self.stats(), (1, 1))
        self.dispatcher.dispatch(self.key, payload([(29, b'')]))
        self.assertEqual(self.stats(), (0, 0))

    def test_duplicate_document_ids_reject_before_tokenization_or_replacement(self):
        self.dispatcher.dispatch(self.key, payload([(17, b'alpha')]))
        with patch.object(self.component, '_tokenize', side_effect=AssertionError('must validate first')):
            with self.assertRaisesRegex(ValueError, 'duplicate document'):
                self.dispatcher.dispatch(self.key, payload([(17, b'beta'), (17, b'gamma')]))
        self.assertEqual(self.stats(), (1, 1))

    def test_failed_retokenization_preserves_previously_committed_terms(self):
        self.dispatcher.dispatch(self.key, payload([(17, b'alpha'), (29, b'beta')]))
        for values in ([f'term{n}' for n in range(65_537)], ['a' * (8 * 1024 * 1024)]):
            with patch.object(self.component, '_tokenize', return_value=values):
                with self.assertRaises(ValueError):
                    self.dispatcher.dispatch(self.key, payload([(17, b'updated')]))
            self.assertEqual(self.stats(), (2, 2))

    def test_text_requires_valid_utf8(self):
        with self.assertRaises(ValueError):
            self.dispatcher.dispatch(self.key, payload([(17, b'alpha'), (29, b'\xff')]))
        self.assertEqual(self.stats(), (0, 0))

    def test_reply_entry_and_byte_budget_fail_before_index_mutation(self):
        for values in ([f'term{n}' for n in range(65_537)], ['a' * (8 * 1024 * 1024)]):
            with self.subTest(count=len(values)), patch.object(self.component, '_tokenize', return_value=values):
                with self.assertRaisesRegex(ValueError, 'reply exceeds'):
                    self.dispatcher.dispatch(self.key, payload([(17, b'alpha')]))
                self.assertEqual(self.stats(), (0, 0))

    def test_terms_reply_matches_an_independent_byte_vector(self):
        with patch.object(self.component, '_tokenize', return_value=['alpha', 'alpha', 'beta']):
            key, reply = self.dispatcher.dispatch(self.key, payload([(17, b'text')]))
        expected = struct.pack('<I', 2)
        for term, frequency in [(b'alpha', 2), (b'beta', 1)]:
            expected += struct.pack('<I', len(term)) + term + struct.pack('<QI', 17, frequency)
        self.assertEqual(key, 'bin:ws/Terms')
        self.assertEqual(reply, expected)
        self.assertEqual(self.stats(), (2, 2))


if __name__ == '__main__':
    unittest.main(verbosity=2)
