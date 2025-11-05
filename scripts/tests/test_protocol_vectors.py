"""Fixture validation and actual Python-to-Erlang data interoperability."""
import copy
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
import protocol_vectors as vectors


class ProtocolVectors(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='parrot-vector-json-')
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / 'fixture.json'
        self.wire = json.loads((ROOT / 'docs/vectors/wire1.json').read_text(encoding='utf-8'))

    def load(self, value, kind='wire'):
        self.path.write_text(json.dumps(value, ensure_ascii=True), encoding='utf-8')
        return vectors.load(kind, self.path)

    def test_shared_documents_load_all_rows(self):
        for kind, filename in [('wire', 'wire1.json'), ('admin', 'admin_v2.json')]:
            with self.subTest(kind=kind):
                path = ROOT / 'docs/vectors' / filename
                document = json.loads(path.read_text(encoding='utf-8'))
                expected = document['vectors']
                actual = vectors.load(kind, path)
                self.assertEqual([row['name'] for row in actual],
                                 [row['name'].encode('utf-8') for row in expected])
                for row, original in zip(actual, expected):
                    with self.subTest(vector=original['name']):
                        self.assertEqual(row['payload'], bytes.fromhex(original['payload_hex']))
                        if kind == 'wire':
                            self.assertEqual(row['bytes'], bytes.fromhex(original['bytes_hex']))

    def test_empty_or_nonlist_vectors_fail(self):
        for value in [[], None, {}, 'vectors']:
            with self.subTest(value=value):
                document = dict(self.wire, vectors=value)
                with self.assertRaises(ValueError):
                    self.load(document)

    def test_wrong_format_and_nonfrozen_documents_fail(self):
        for key, values in [('format', ['parrot-admin-v2', None]), ('frozen', [False, 1, 'true', None])]:
            for value in values:
                with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                    self.load(dict(self.wire, **{key: value}))

    def test_duplicate_keys_and_names_fail(self):
        self.path.write_text('{"format":"parrot-wire-1.0","format":"parrot-wire-1.0"}')
        with self.assertRaisesRegex(ValueError, 'duplicate JSON key'):
            vectors.load('wire', self.path)
        with self.assertRaisesRegex(ValueError, 'duplicate vector name'):
            self.load(dict(self.wire, vectors=[self.wire['vectors'][0]] * 2))

    def test_hex_is_complete_and_contains_no_ignored_whitespace(self):
        for field in ['bytes_hex', 'payload_hex']:
            for value in ['a', 'xz', '00 01', '00\n01', None, 12]:
                with self.subTest(field=field, value=value):
                    document = copy.deepcopy(self.wire)
                    document['vectors'][0][field] = value
                    with self.assertRaises(ValueError):
                        self.load(document)

    def test_ids_and_header_fields_preserve_unsigned_ranges(self):
        for field, bits in [('correlation_id', 64), ('flags', 16), ('hop_count', 8),
                            ('hop_limit', 8), ('sequence', 32)]:
            for value in [True, False, 1.5, -1, 1 << bits, '1']:
                with self.subTest(field=field, value=value):
                    document = copy.deepcopy(self.wire)
                    document['vectors'][0][field] = value
                    with self.assertRaises(ValueError):
                        self.load(document)

    def test_frame_type_requires_exactly_two_hex_digits(self):
        for value in ['0x+1', '0x 1', '0x-0', '0X10', '0x1', '0x100', '0xgg', None, 16]:
            with self.subTest(value=value):
                document = copy.deepcopy(self.wire)
                document['vectors'][0]['frame_type'] = value
                with self.assertRaises(ValueError):
                    self.load(document)
        document = copy.deepcopy(self.wire)
        document['vectors'][0]['frame_type'] = '0xAf'
        self.assertEqual(self.load(document)[0]['ft'], 175)

    def test_identities_reject_nul_and_unpaired_surrogates(self):
        for field in ['name', 'path', 'type_key']:
            for value in ['left\0right', '\ud800', '\udfff', 4, None]:
                with self.subTest(field=field, value=value):
                    document = copy.deepcopy(self.wire)
                    document['vectors'][0][field] = value
                    with self.assertRaises((ValueError, UnicodeError)):
                        self.load(document)

    def test_parser_handles_escaping_field_order_and_utf8(self):
        document = copy.deepcopy(self.wire)
        document['vectors'][0]['path'] = '\ufeff\u96ea"\\\n'
        self.path.write_text(json.dumps(document, ensure_ascii=True, sort_keys=True, indent=3))
        rows = vectors.load('wire', self.path)
        self.assertEqual(rows[0]['path'], b'\xef\xbb\xbf\xe9\x9b\xaa"\\\n')
        self.assertEqual(rows[0]['payload'], b'\0\0\0\0\xab')

    def test_malformed_json_nonobjects_and_nonstandard_numbers_fail(self):
        for value in ['{', '[]', 'null', '{"format":NaN}', '{"format":Infinity}']:
            with self.subTest(value=value):
                self.path.write_text(value)
                with self.assertRaises(ValueError):
                    vectors.load('wire', self.path)

    def test_each_vector_and_required_fields_are_checked(self):
        for value in [None, [], {}, dict(self.wire['vectors'][0], name='')]:
            with self.subTest(value=value), self.assertRaises((ValueError, KeyError)):
                self.load(dict(self.wire, vectors=[self.wire['vectors'][0], value]))

    def test_data_only_external_terms_reject_unsupported_types(self):
        for value in [True, False, -1, 1 << 64, 1.5, None, ()]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                vectors.external_term(value)

    def command(self, path):
        return [sys.executable, *(['-O'] if sys.flags.optimize else []),
                str(ROOT / 'scripts/protocol_vectors.py'), 'wire', str(path)]

    def test_cli_failures_have_no_success_payload(self):
        self.path.write_text('{')
        for path in [self.path, self.path.with_name('missing.json')]:
            with self.subTest(path=path):
                result = subprocess.run(self.command(path), capture_output=True, timeout=5)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(result.stdout, b'')
                self.assertIn(b'protocol vector fixture:', result.stderr)

    def test_cli_output_is_read_by_actual_erlang_without_atoms_or_precision_loss(self):
        document = copy.deepcopy(self.wire)
        row = document['vectors'][0]
        row.update(correlation_id=(1 << 64) - 1, flags=65535, sequence=(1 << 32) - 1,
                   path='\ufeff\u96ea"\\\n', payload_hex='00ff80')
        document['vectors'] = [row]
        self.load(document)
        result = subprocess.run(self.command(self.path), capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        terms = self.path.with_suffix('.etf')
        terms.write_bytes(result.stdout)
        erl = shutil.which('erl')
        self.assertIsNotNone(erl, 'actual Erlang is required; missing tools are not skips')
        expression = ('{ok,B}=file:read_file(os:getenv("PARROT_ETF_FIXTURE")),'
            '[#{<<"cid">>:=18446744073709551615,<<"flags">>:=65535,<<"sequence">>:=4294967295,'
            '<<"path">>:=<<239,187,191,233,155,170,34,92,10>>,<<"payload">>:=<<0,255,128>>}]'
            '=binary_to_term(B,[safe]),io:format("ETF_INTEROP_OK~n"),halt().')
        native = subprocess.run([erl, '+S', '2:2', '-noshell', '-eval', expression],
            env=dict(os.environ, PARROT_ETF_FIXTURE=str(terms)), capture_output=True, timeout=10)
        self.assertEqual(native.returncode, 0, native.stderr + native.stdout)
        self.assertEqual(native.stdout, b'ETF_INTEROP_OK\n')


if __name__ == '__main__':
    unittest.main(verbosity=2)
