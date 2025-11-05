import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'check_dev09_coverage.py'
spec = importlib.util.spec_from_file_location('dev09_coverage', SCRIPT)
coverage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(coverage)


class CoverageContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        files = []
        for crate in coverage.CRATES:
            source = self.root / 'crates' / crate / 'src/lib.rs'
            source.parent.mkdir(parents=True)
            source.write_text('pub fn example() {}\n')
            files.append({'filename': str(source), 'summary': {
                'lines': {'count': 100, 'covered': 100, 'percent': 100},
                'branches': {'count': 20, 'covered': 19, 'percent': 95},
            }})
        self.report = {'type': 'llvm.coverage.json.export', 'data': [{'files': files}]}

    def test_original_inclusive_branch_boundary_and_all_lines_pass(self):
        self.assertTrue(coverage.evaluate(self.report, self.root)['met'])

    def test_one_crate_cannot_hide_behind_aggregate_coverage(self):
        self.report['data'][0]['files'][0]['summary']['lines']['covered'] = 99
        result = coverage.evaluate(self.report, self.root)
        self.assertFalse(result['met'])
        self.assertFalse(result['crates']['parrot-app']['lines']['met'])

    def test_unmeasured_branches_are_not_a_pass(self):
        for file in self.report['data'][0]['files']:
            file['summary']['branches'] = {'count': 0, 'covered': 0, 'percent': 100}
        result = coverage.evaluate(self.report, self.root)
        self.assertFalse(result['met'])
        self.assertIsNone(result['crates']['parrot-abi']['branches']['percent'])

    def test_rounded_percent_does_not_raise_94999_to_95(self):
        self.report['data'][0]['files'][0]['summary']['branches'] = {
            'count': 100000, 'covered': 94999, 'percent': 95,
        }
        self.assertFalse(coverage.evaluate(self.report, self.root)['met'])

    def test_missing_source_file_and_missing_crate_fail(self):
        missing = self.root / 'crates/parrot-app/src/never_measured.rs'
        missing.write_text('pub fn omitted() {}\n')
        result = coverage.evaluate(self.report, self.root)
        self.assertEqual(result['crates']['parrot-app']['missing_files'],
                         ['crates/parrot-app/src/never_measured.rs'])
        self.assertFalse(result['met'])
        self.report['data'][0]['files'].pop()
        self.assertFalse(coverage.evaluate(self.report, self.root)['met'])

    def test_duplicate_file_cannot_inflate_counters(self):
        self.report['data'][0]['files'].append(copy.deepcopy(self.report['data'][0]['files'][0]))
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            coverage.evaluate(self.report, self.root)

    def test_bad_counter_types_and_impossible_counts_are_rejected(self):
        for count, covered in [(True, 1), (1, True), (1.0, 1), (2, 3), (-1, 0)]:
            with self.subTest(count=count, covered=covered):
                bad = copy.deepcopy(self.report)
                bad['data'][0]['files'][0]['summary']['lines'] = {'count': count, 'covered': covered}
                with self.assertRaises(ValueError):
                    coverage.evaluate(bad, self.root)

    def test_absent_branch_counters_and_empty_export_are_rejected(self):
        del self.report['data'][0]['files'][0]['summary']['branches']
        with self.assertRaisesRegex(ValueError, 'branches'):
            coverage.evaluate(self.report, self.root)
        with self.assertRaisesRegex(ValueError, 'empty'):
            coverage.evaluate({'type': 'llvm.coverage.json.export', 'data': []}, self.root)

    def test_source_classification_is_revoked_when_code_or_parent_changes(self):
        name = 'crates/parrot-app/src/decl.rs'
        source = self.root / name
        source.write_text('pub use super::example;\n')
        parent = self.root / 'crates/parrot-app/src/lib.rs'
        scope = {name: {'reason': 'declarations', 'inputs': {
            name: hashlib.sha256(source.read_bytes()).hexdigest(),
            'crates/parrot-app/src/lib.rs': hashlib.sha256(parent.read_bytes()).hexdigest(),
        }}}
        self.assertTrue(coverage.evaluate(self.report, self.root, scope)['met'])
        original = source.read_text()
        source.write_text('pub fn now_executable() {}\n')
        self.assertFalse(coverage.evaluate(self.report, self.root, scope)['met'])
        source.write_text(original)
        parent.write_text('pub fn changed_parent() {}\n')
        self.assertFalse(coverage.evaluate(self.report, self.root, scope)['met'])

    def test_classification_cannot_omit_its_source_binding(self):
        with self.assertRaisesRegex(ValueError, 'own file'):
            coverage.evaluate(self.report, self.root, {
                'crates/parrot-app/src/lib.rs': {'reason': 'invalid', 'inputs': {}},
            })

    def test_real_cli_distinguishes_met_unmet_and_invalid(self):
        report = self.root / 'report.json'
        command = [sys.executable, str(SCRIPT), '--root', str(self.root), '--report', str(report)]
        for expected in [0, 1, 2]:
            if expected == 1:
                self.report['data'][0]['files'][0]['summary']['branches']['covered'] = 18
            report.write_text(json.dumps(self.report) if expected != 2 else '{broken')
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, expected, result.stderr)
            if expected != 2:
                self.assertEqual(json.loads(result.stdout)['met'], expected == 0)


if __name__ == '__main__':
    unittest.main()
