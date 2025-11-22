"""Reject empty suites, registry fallbacks, feature leakage and incorrect build artifacts."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('tokio_upstream_gate', ROOT / 'scripts/check_tokio_upstream.py')
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)


class RuntimeGateTests(unittest.TestCase):
    def report(self):
        return ('running 2 tests\ntest first ... ok\ntest second ... ok\n'
                'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s\n')

    def resolution(self):
        return dict(packages=[dict(name='tokio', id='local-tokio', version='1.53.3', source=None,
                    manifest_path=str(ROOT / 'vendor/tokio/Cargo.toml'), features={'parrot-native-progress': []})],
                    resolve=dict(root='consumer', nodes=[dict(id='local-tokio', features=['sync', 'parking_lot'])]))

    def test_complete_report_and_minimal_resolution(self):
        GATE.verify_suite(self.report(), 0, 2)
        self.assertEqual(GATE.verify_resolution(self.resolution(), ROOT, True)['consumer'], 'consumer')

    def test_empty_filtered_ignored_or_duplicate_report_rejected(self):
        report = self.report()
        for broken in ['', report.replace('2 passed', '0 passed'), report.replace('0 ignored', '1 ignored'),
                       report.replace('0 filtered out', '1 filtered out'), report + report,
                       report.replace('test second ... ok\n', '')]:
            with self.subTest(report=broken), self.assertRaises(ValueError):
                GATE.verify_suite(broken, 0, 2)

    def test_failure_exit_cannot_be_hidden_by_a_success_report(self):
        with self.assertRaises(ValueError):
            GATE.verify_suite(self.report(), 101, 2)

    def test_registry_fallback_duplicate_identity_and_minimal_feature_leak_rejected(self):
        for fault in ['registry', 'duplicate', 'feature', 'missing-feature']:
            data = copy.deepcopy(self.resolution())
            if fault == 'registry':
                data['packages'][0]['source'] = 'registry+https://github.com/rust-lang/crates.io-index'
            elif fault == 'duplicate':
                data['packages'].append(copy.deepcopy(data['packages'][0]))
            elif fault == 'feature':
                data['resolve']['nodes'][0]['features'].append('rt')
            else:
                data['resolve']['nodes'][0]['features'].remove('parking_lot')
            with self.subTest(fault=fault), self.assertRaises(ValueError):
                GATE.verify_resolution(data, ROOT, True)

    def test_upstream_requires_full_and_test_util(self):
        data = self.resolution()
        with self.assertRaises(ValueError):
            GATE.verify_resolution(data, ROOT, False)
        data['resolve']['nodes'][0]['features'] += ['full', 'test-util']
        GATE.verify_resolution(data, ROOT, False)

    def test_artifact_source_profile_identity_and_complete_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory).resolve()
            binary = target / 'debug/deps/sync_mpsc'
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b'explicit synthetic artifact-selection fixture; never executed')
            artifact = dict(reason='compiler-artifact', package_id='consumer', executable=str(binary),
                            target=dict(name='sync_mpsc', src_path=str(ROOT / 'vendor/tokio/tests/sync_mpsc.rs'),
                                        kind=['test']), profile=dict(test=True))
            result = GATE.build_artifacts(json.dumps(artifact), 'consumer', ['sync_mpsc'], ROOT, target, 'debug')
            self.assertEqual(result, {'sync_mpsc': binary})
            for fault in ['source', 'profile', 'package', 'duplicate', 'missing']:
                changed = copy.deepcopy(artifact)
                if fault == 'source':
                    changed['target']['src_path'] = str(ROOT / 'vendor/tokio/tests/sync_oneshot.rs')
                elif fault == 'profile':
                    changed['profile']['test'] = False
                elif fault == 'package':
                    changed['package_id'] = 'unrelated-consumer'
                text = json.dumps(changed)
                if fault == 'duplicate':
                    text += '\n' + text
                if fault == 'missing':
                    text = ''
                with self.subTest(fault=fault), self.assertRaises(ValueError):
                    GATE.build_artifacts(text, 'consumer', ['sync_mpsc'], ROOT, target, 'debug')

    def test_minimal_notification_suite_requires_its_real_test_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory).resolve()
            name = 'parrot_notification_fanout'
            binary = target / 'debug/deps' / name
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b'explicit synthetic artifact-selection fixture; never executed')
            artifact = dict(reason='compiler-artifact', package_id='minimal-consumer', executable=str(binary),
                            target=dict(name=name, src_path=str(ROOT / 'vendor/tokio/tests' / (name + '.rs')),
                                        kind=['test']), profile=dict(test=True))
            result = GATE.build_artifacts(json.dumps(artifact), 'minimal-consumer', [name], ROOT,
                                          target, 'debug', minimal=True)
            self.assertEqual(result, {name: binary})
            for fault in ['source', 'kind', 'profile']:
                changed = copy.deepcopy(artifact)
                if fault == 'source':
                    changed['target']['src_path'] = str(ROOT / 'scripts/fixtures/tokio-minimal/src/main.rs')
                elif fault == 'kind':
                    changed['target']['kind'] = ['bin']
                else:
                    changed['profile']['test'] = False
                with self.subTest(fault=fault), self.assertRaises(ValueError):
                    GATE.build_artifacts(json.dumps(changed), 'minimal-consumer', [name], ROOT,
                                         target, 'debug', minimal=True)
            companion = target / 'debug' / GATE.MINIMAL
            companion.write_bytes(b'explicit synthetic companion binary; never executed')
            main = dict(reason='compiler-artifact', package_id='minimal-consumer', executable=str(companion),
                        target=dict(name=GATE.MINIMAL, src_path=str(ROOT / 'scripts/fixtures/tokio-minimal/src/main.rs'),
                                    kind=['bin']), profile=dict(test=False))
            text = json.dumps(artifact) + '\n' + json.dumps(main)
            result = GATE.build_artifacts(text, 'minimal-consumer', [name, GATE.MINIMAL], ROOT,
                                          target, 'debug', minimal=True)
            self.assertEqual(result, {name: binary, GATE.MINIMAL: companion})
            with self.assertRaises(ValueError):
                GATE.build_artifacts(json.dumps(artifact), 'minimal-consumer', [name, GATE.MINIMAL], ROOT,
                                     target, 'debug', minimal=True)


if __name__ == '__main__':
    unittest.main()
