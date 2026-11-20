"""Mutation contracts for the Linux ledger; synthetic logs are not runtime evidence."""
import hashlib
import json
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'linux_core_results.py'
MODULE = runpy.run_path(str(SCRIPT))
BINARIES, CONFIGURATIONS, STAGES = (MODULE[name] for name in ('BINARIES', 'CONFIGURATIONS', 'STAGES'))
LIBRARY_COUNT = BINARIES['library']
LIBRARY_ANNOUNCEMENT = f'running {LIBRARY_COUNT} tests\n'
LIBRARY_RESULT = f'test result: ok. {LIBRARY_COUNT} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n'


def binary(name, count, target, profile='debug'):
    source = 'unittests src/lib.rs' if name == 'library' else 'tests/' + name + '.rs'
    return (f'     Running {source} ({target}/{profile}/deps/{name}-fixture)\n'
            + f'running {count} tests\n'
            + f'test result: ok. {count} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n')


class SummaryContracts(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory(prefix='parrot-linux-ledger-')
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name)
        self.manifest = self.root / 'manifest.json'
        self.manifest.write_text(json.dumps({'files': {'crates/parrot/src/thread/typed.rs': {'sha256': '0' * 64}}}))
        self.digest = hashlib.sha256(self.manifest.read_bytes()).hexdigest()
        self.target = '/workspace/build-targets/' + self.digest
        self.write('manifest.log', self.digest + '  /workspace/manifest.json\n')
        self.receipt = {'status': 'ok', 'files': 1, 'action': 'build-target',
                        'target_dir': self.target, 'manifest_sha256': self.digest, 'fresh': True}
        self.write('build-target.log', json.dumps(self.receipt))
        self.write('stages.tsv', ''.join(name + '\t0\n' for name in STAGES))
        self.write('container-state.json', json.dumps({'Status': 'exited', 'ExitCode': 0, 'OOMKilled': False}))
        for label in CONFIGURATIONS:
            self.write(label + '.log', ''.join(binary(name, count, self.target, label.split('-')[0])
                                               for name, count in BINARIES.items()))
        for label in ('input-before', 'execution-input-before', 'execution-input-after', 'input-after'):
            self.write(label + '.log', json.dumps({'status': 'ok', 'files': 1, 'action': 'verify'}))
        self.write('api-library.log', binary('library', 178, self.target))
        self.write('doctest.log', 'test result: ok. 24 passed; 0 failed; 0 ignored;\n')
        self.assertTrue(self.result()['passed'])

    def write(self, name, content):
        (self.root / name).write_text(content)

    def result(self):
        return MODULE['summarize'](self.root, self.manifest)

    def test_duplicate_binary_cannot_overwrite_original_result(self):
        for label in (*CONFIGURATIONS, 'api-library'):
            with self.subTest(label=label):
                path = self.root / (label + '.log')
                original = path.read_text()
                extra = binary('library', 178 if label == 'api-library' else LIBRARY_COUNT,
                               self.target, 'release' if label.startswith('release') else 'debug')
                self.write(path.name, original + extra)
                self.assertIn(label + ': duplicate test binary: library', self.result()['errors'])
                self.write(path.name, original)

    def test_unattributed_failed_or_missing_result_is_rejected(self):
        path = self.root / 'debug-default.log'
        original = path.read_text()
        mutations = (
            original + 'test result: ok. 1 passed; 0 failed; 0 ignored;\n',
            original + 'test result: FAILED. 0 passed; 1 failed; 0 ignored;\n',
            original + f'     Running tests/unfinished.rs ({self.target}/debug/deps/unfinished)\n',
            original.replace('test result: ok.', 'test result: FAILED.', 1),
            original.replace(LIBRARY_RESULT, ''),
        )
        for changed in mutations:
            with self.subTest(changed=changed[-100:]):
                self.assertNotEqual(changed, original)
                self.write(path.name, changed)
                self.assertFalse(self.result()['passed'])

    def test_doctest_failure_is_not_hidden_by_a_later_success(self):
        path = self.root / 'doctest.log'
        self.write(path.name, 'test result: FAILED. 0 passed; 1 failed; 0 ignored;\n' + path.read_text())
        self.assertIn('missing or unexpected doctest result', self.result()['errors'])

    def test_filtered_and_interleaved_children_do_not_replace_parent_result(self):
        path = self.root / 'debug-default.log'
        original = path.read_text()
        children = ('running 1 test\nrunning 1 test\n'
                    f'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; {LIBRARY_COUNT - 1} filtered out;\n'
                    'test result: \ninterleaved test name ... ok\n'
                    f'ok. 1 passed; 0 failed; 0 ignored; 0 measured; {LIBRARY_COUNT - 1} filtered out;\n')
        with_children = original.replace(LIBRARY_ANNOUNCEMENT, LIBRARY_ANNOUNCEMENT + children)
        self.assertNotEqual(with_children, original)
        self.write(path.name, with_children)
        result = self.result()
        self.assertTrue(result['passed'], result['errors'])
        self.assertEqual(result['configurations']['debug-default']['library']['passed'], LIBRARY_COUNT)
        for parent in ('', f'test result: FAILED. {LIBRARY_COUNT - 1} passed; 1 failed; 0 ignored; 0 measured; 0 filtered out;\n'):
            with self.subTest(parent=parent):
                changed = with_children.replace(LIBRARY_RESULT, parent)
                self.assertNotEqual(changed, with_children)
                self.write(path.name, changed)
                self.assertFalse(self.result()['passed'])

    def test_invalid_filtered_result_or_parent_announcement_is_rejected(self):
        path = self.root / 'debug-default.log'
        original = path.read_text()
        for changed in (
                original.replace(LIBRARY_ANNOUNCEMENT, LIBRARY_ANNOUNCEMENT +
                                 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 17 filtered out;\n'),
                original.replace(LIBRARY_ANNOUNCEMENT, f'running {LIBRARY_COUNT - 1} tests\n')):
            self.assertNotEqual(changed, original)
            self.write(path.name, changed)
            self.assertFalse(self.result()['passed'])

    def test_same_count_candidate_substitution_is_rejected(self):
        result = MODULE['check_identity'](self.root, self.manifest)
        self.assertTrue(result['passed'])
        self.assertNotEqual(result['positive_manifest_sha256'], result['substituted_manifest_sha256'])
        self.assertEqual(result['rejected_errors'], [
            'execution manifest identity does not match the candidate',
            'fresh build target receipt does not match the candidate'])

    def test_old_binary_is_rejected_even_when_all_counts_match(self):
        for label in (*CONFIGURATIONS, 'api-library'):
            path = self.root / (label + '.log')
            original = path.read_text()
            for old_target in ('/repo/target', '/workspace/build-targets/' + '1' * 64,
                               self.target + '/nested', 'relative-target'):
                with self.subTest(label=label, old_target=old_target):
                    changed = original.replace(self.target, old_target)
                    self.assertNotEqual(changed, original)
                    self.write(path.name, changed)
                    result = self.result()
                    self.assertFalse(result['passed'])
                    self.assertTrue(any('outside the fresh build target' in error for error in result['errors']))
            self.write(path.name, original)

    def test_missing_or_wrong_fresh_receipt_is_rejected(self):
        mutations = ('', '[]', '{}', json.dumps(dict(self.receipt, fresh=False)),
                     json.dumps(dict(self.receipt, fresh=1)),
                     json.dumps(dict(self.receipt, manifest_sha256='1' * 64)),
                     json.dumps(dict(self.receipt, target_dir='/repo/target')))
        for changed in mutations:
            with self.subTest(receipt=changed):
                self.write('build-target.log', changed)
                self.assertFalse(self.result()['passed'])
        (self.root / 'build-target.log').unlink()
        self.assertFalse(self.result()['passed'])

    def test_missing_duplicate_failed_and_malformed_stage_is_rejected(self):
        original = (self.root / 'stages.tsv').read_text()
        for changed in (original.replace('build-target\t0\n', ''), original + 'build-target\t0\n',
                        original.replace('build-target\t0', 'build-target\t1'),
                        original.replace('build-target\t0', 'build-target\tnot-an-exit-code')):
            self.assertNotEqual(changed, original)
            self.write('stages.tsv', changed)
            self.assertFalse(self.result()['passed'])

    def test_live_nonzero_or_oom_container_is_rejected(self):
        for state in ({'Status': 'running', 'ExitCode': 0, 'OOMKilled': False},
                      {'Status': 'exited', 'ExitCode': 1, 'OOMKilled': False},
                      {'Status': 'exited', 'ExitCode': 0, 'OOMKilled': True}, [], {}):
            self.write('container-state.json', json.dumps(state))
            self.assertIn('container did not terminate successfully', self.result()['errors'])

    def test_cli_retains_existing_report(self):
        output = self.root / 'report.json'
        command = [sys.executable, '-B', str(SCRIPT), str(self.root), str(output), str(self.manifest)]
        first = subprocess.run(command, capture_output=True, text=True, timeout=30)
        self.assertEqual(first.returncode, 0, first.stderr)
        expected = output.read_bytes()
        second = subprocess.run(command, capture_output=True, text=True, timeout=30)
        self.assertNotEqual(second.returncode, 0)
        self.assertEqual(output.read_bytes(), expected)


if __name__ == '__main__':
    unittest.main(verbosity=2)
