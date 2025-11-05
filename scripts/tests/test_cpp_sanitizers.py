"""Sanitizer evidence must demonstrate real faults and a complete native suite."""
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('cpp_sanitizer_gate',
    Path(__file__).resolve().parents[1] / 'check_cpp_sanitizers.py')
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)


class SanitizerGateTests(unittest.TestCase):
    EXPECTED = dict(transport=90, interop_cli=4, vectors=33, boundaries=47)

    def validate(self, log, artifacts):
        return GATE.validate_suite(log, artifacts, self.EXPECTED)

    def test_designated_runtime_faults_are_required(self):
        GATE.validate_control('ERROR: AddressSanitizer: heap-use-after-free', 1, 'heap')
        GATE.validate_control('runtime error: signed integer overflow', 1, 'integer')
        GATE.validate_control('', 0, 'clean')
        cases = [('AddressSanitizer: heap-use-after-free', 0, 'heap'),
                 ('clang++: linker error', 1, 'heap'),
                 ('AddressSanitizer: heap-buffer-overflow', 1, 'heap'),
                 ('runtime error: null pointer access', 1, 'integer'),
                 ('runtime error: signed integer overflow', -9, 'integer'),
                 ('AddressSanitizer: heap-use-after-free', 0, 'clean')]
        for case in cases:
            with self.subTest(case=case), self.assertRaises(ValueError):
                GATE.validate_control(*case)

    def receipt(self):
        return dict(schema=1, selected=90, executed=90, failures=0, errors=0, skipped=0,
            expected_failures=0, unexpected_successes=0, successful=True)

    def log(self, receipt):
        return ('CPP_FIXTURE_RESULT tests=13 failures=0\n'
                'CPP shared wire boundaries: 47 cases\n'
                'CPP-LITE VECTORS PASS (33 vectors)\nCABI-SMOKE PASS\nRan 4 tests in 0.2s\n'
                'Ran 90 tests in 1.2s\n'
                'PARROT_CPP_TEST_RECEIPT ' + json.dumps(receipt) + '\n')

    def artifacts(self):
        return [dict(name=name) for name in GATE.BINARIES]

    def test_complete_suites_and_builds_are_accepted(self):
        receipt = self.receipt()
        self.assertEqual(self.validate(self.log(receipt), self.artifacts()), receipt)

    def test_skips_missing_executions_and_false_counters_are_rejected(self):
        for delta in [dict(selected=0, executed=0), dict(executed=89), dict(skipped=1),
                      dict(expected_failures=1), dict(unexpected_successes=1),
                      dict(successful=False), dict(failures=False), dict(schema=True),
                      dict(selected=1, executed=1)]:
            receipt = dict(self.receipt(), **delta)
            with self.subTest(delta=delta), self.assertRaises(ValueError):
                self.validate(self.log(receipt), self.artifacts())

    def test_missing_duplicate_and_unknown_builds_are_rejected(self):
        for artifacts in [self.artifacts()[:-1], self.artifacts() + self.artifacts()[:1],
                          [dict(name='invented')] + self.artifacts()[1:]]:
            with self.subTest(artifacts=artifacts), self.assertRaises(ValueError):
                self.validate(self.log(self.receipt()), artifacts)

    def test_missing_native_and_duplicate_transport_receipts_are_rejected(self):
        log = self.log(self.receipt())
        for altered in [log.replace('CPP_FIXTURE_RESULT tests=13 failures=0', ''),
                        log.replace('CPP-LITE VECTORS PASS (33 vectors)', ''),
                        log.replace('Ran 4 tests in 0.2s', ''), log + log]:
            with self.subTest(log=altered), self.assertRaises(ValueError):
                self.validate(altered, self.artifacts())

    def test_partial_native_vector_boundary_and_cli_runs_are_rejected(self):
        log = self.log(self.receipt())
        for before, after in [('tests=13', 'tests=1'), ('33 vectors', '1 vectors'),
                              ('47 cases', '1 cases'), ('Ran 4 tests', 'Ran 1 tests'),
                              ('Ran 90 tests', 'Ran 1 tests')]:
            with self.subTest(before=before), self.assertRaises(ValueError):
                self.validate(log.replace(before, after), self.artifacts())
        for line in log.splitlines()[:-1]:
            with self.subTest(duplicate=line), self.assertRaises(ValueError):
                self.validate(log + line + '\n', self.artifacts())

    def test_expectations_follow_actual_discovery_and_corpus_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            scripts = repo / 'interop/cpp-lite'; scripts.mkdir(parents=True)
            vectors = repo / 'docs/vectors'; vectors.mkdir(parents=True)
            source = ('import unittest\nclass Cases(unittest.TestCase):\n'
                      ' def test_one(self): raise AssertionError("must only discover")\n')
            for name in ['transport', 'interop_cli']:
                (scripts / ('test_' + name + '.py')).write_text(source)
            (vectors / 'wire1.json').write_text(json.dumps(dict(vectors=[{}, {}])))
            (vectors / 'wire1-boundaries.tsv').write_text('# comment\n\nfirst\nsecond\nthird\n')
            self.assertEqual(GATE.suite_expectations(repo),
                dict(transport=1, interop_cli=1, vectors=2, boundaries=3))
            (scripts / 'test_transport.py').write_text('import unittest\n')
            with self.assertRaisesRegex(ValueError, r'empty C\+\+ test discovery'):
                GATE.suite_expectations(repo)

    def test_compilation_failure_is_retained_without_claiming_fault_detection(self):
        compiler = shutil.which('false')
        self.assertIsNotNone(compiler)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / 'repo'; repo.mkdir()
            (repo / 'Makefile').write_text('')
            output = root / 'evidence'
            with self.assertRaisesRegex(ValueError, 'sanitizer control did not compile'):
                GATE.run_gate(repo, output, Path(compiler))
            result = json.loads((output / 'result.json').read_text())
            self.assertEqual(result['status'], 'failed')
            self.assertEqual(len(result['runs']), 1)
            self.assertEqual(result['runs'][0]['exit'], 1)
            self.assertNotIn('actual_runtime_faults_detected', result)
            self.assertTrue((output / 'compile-clean.log').is_file())
            self.assertTrue((output / 'cpp_sanitizer_heap.cpp').is_file())

    def test_existing_output_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            sentinel = path / 'sentinel'
            sentinel.write_text('existing evidence')
            with self.assertRaises(FileExistsError):
                GATE.run_gate(path, path, Path('/usr/bin/false'))
            self.assertEqual(sentinel.read_text(), 'existing evidence')


if __name__ == '__main__':
    unittest.main()
