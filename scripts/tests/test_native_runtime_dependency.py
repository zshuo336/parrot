"""Consumer generation and Cargo migration contracts without a Rust build."""
import importlib.util
import itertools
import json
import os
from pathlib import Path
import re
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(os.environ.get('PARROT_NATIVE_CONSUMER_ROOT', Path(__file__).resolve().parents[2]))
GATE = Path(os.environ.get('PARROT_NATIVE_CONSUMER_GATE', ROOT / 'scripts/check_native_runtime_dependency.py'))


def missing_feature(feature, package='tokio'):
    return f"package `consumer` depends on `{package}` with feature `{feature}` but `{package}` does not have that feature."


def gate_module():
    spec = importlib.util.spec_from_file_location('consumer_gate_contract', GATE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ConsumerGateContracts(unittest.TestCase):
    def test_native_consumers_can_import_the_trace_probe_without_changing_pure_abi(self):
        gate = gate_module()
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            checker = gate.Check(SimpleNamespace(root=ROOT, output_dir=directory / 'evidence',
                                                target_dir=directory / 'target', offline=True,
                                                resolution_only=True))
            for package, features, native in gate.CASES:
                with self.subTest(package=package, features=features):
                    consumer = checker.consumer(package + '-'.join(features), package, features, True, native)
                    manifest = (consumer / 'Cargo.toml').read_text()
                    dependency = re.search(r'^parrot-api=\{path=(.+)\}$', manifest, re.M)
                    if native:
                        self.assertIsNotNone(dependency, 'copied trace probes need an explicit dev dependency')
                        self.assertEqual(Path(json.loads(dependency.group(1))), ROOT / 'crates/parrot-api')
                    else:
                        self.assertIsNone(dependency, 'pure ABI must not acquire the trace dependency')

    def runtime_receipt(self, passed, failed=0, ignored=0):
        gate = gate_module()
        class Check(gate.Check):
            def metadata(self, name, consumer, native, patched, pure_abi=False, locked=False, remote=False,
                         blocking=False, startup=False):
                if patched and not pure_abi:
                    self.resolutions.append(dict(name=name, tokio=[dict(version='1.53.3', source=None)]))

            def run(self, name, consumer, arguments, expected=0):
                if name == 'newer-lock-seed':
                    return json.dumps(dict(packages=[dict(name='tokio', version='1.53.2', source='registry+fixture')])), ''
                if name == 'newer-lock-patched-not-selected':
                    return '', '--locked requires lock file update'
                if name.endswith('-progress'):
                    return f'test result: ok. {passed} passed; {failed} failed; {ignored} ignored; 0 measured;', ''
                return '', ''

        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            checker = Check(SimpleNamespace(root=ROOT, output_dir=directory / 'evidence',
                                            target_dir=directory / 'target', offline=True,
                                            resolution_only=False))
            with patch.object(gate.platform, 'system', return_value='Linux'), patch.object(gate.platform, 'libc_ver', return_value=('glibc', 'fixture')):
                checker.execute()

    def test_runtime_gate_accepts_the_complete_extended_suite(self):
        self.runtime_receipt(16)

    def test_runtime_gate_rejects_partial_failed_or_ignored_suites(self):
        for counts in [(8, 0, 0), (15, 0, 0), (16, 1, 0), (16, 0, 1)]:
            with self.subTest(counts=counts), self.assertRaisesRegex(RuntimeError, 'contracts must execute'):
                self.runtime_receipt(*counts)

    def check_metadata(self, stdout, stderr, *, patch, native=False, remote=True, blocking=False, startup=False):
        gate = gate_module()
        class Check(gate.Check):
            def run(self, name, consumer, arguments, expected=0):
                self.expected_exit = expected
                return stdout, stderr

        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            checker = Check(SimpleNamespace(root=ROOT, output_dir=directory / 'evidence',
                                            target_dir=directory / 'target', offline=True,
                                            resolution_only=True))
            checker.metadata('external', directory, native, patch, remote=remote, blocking=blocking, startup=startup)
            return checker.expected_exit, checker.resolutions

    def test_remote_only_missing_patch_must_identify_the_deferred_mpsc_capability(self):
        gate = gate_module()
        expected, resolutions = self.check_metadata('', missing_feature(gate.REMOTE_MARKER), patch=False)
        self.assertEqual(expected, 101)
        self.assertEqual(resolutions, [])
        for stderr in ['network unavailable', missing_feature(gate.MARKER),
                       gate.REMOTE_MARKER + ': unrelated compiler failure',
                       missing_feature('unrelated') + '\nhelp: available features: ' + gate.REMOTE_MARKER,
                       missing_feature(gate.REMOTE_MARKER, 'unrelated-package')]:
            with self.subTest(stderr=stderr), self.assertRaisesRegex(RuntimeError, 'missing capability'):
                self.check_metadata('', stderr, patch=False)

    def test_patched_native_remote_consumer_requires_both_runtime_capabilities(self):
        gate = gate_module()
        def metadata(features):
            return json.dumps(dict(packages=[dict(name='tokio', id='patched', version='1.53.3',
                source=None, manifest_path=str(ROOT / 'vendor/tokio/Cargo.toml'))],
                resolve=dict(nodes=[dict(id='patched', features=features)])))

        for features in [[gate.MARKER], [gate.REMOTE_MARKER], []]:
            with self.subTest(features=features), self.assertRaisesRegex(RuntimeError, 'capabilities missing'):
                self.check_metadata(metadata(features), '', patch=True, native=True)
        expected, resolutions = self.check_metadata(metadata([gate.MARKER, gate.REMOTE_MARKER]),
                                                    '', patch=True, native=True)
        self.assertEqual(expected, 0)
        self.assertEqual(len(resolutions), 1)

    def test_core_consumer_cannot_silently_use_an_unpatched_blocking_pool(self):
        gate = gate_module()
        required = [gate.BLOCKING_MARKER, gate.BLOCKING_CONTEXT_MARKER]
        for capability in required:
            with self.subTest(capability=capability):
                result = self.check_metadata('', missing_feature(capability), patch=False, remote=False, blocking=True)
                self.assertEqual(result, (101, []))
        for stderr in ['network unavailable', missing_feature(gate.MARKER)]:
            with self.subTest(stderr=stderr), self.assertRaisesRegex(RuntimeError, 'missing capability'):
                self.check_metadata('', stderr, patch=False, remote=False, blocking=True)
        def metadata(features):
            return json.dumps(dict(packages=[dict(name='tokio', id='patched', version='1.53.3',
                source=None, manifest_path=str(ROOT / 'vendor/tokio/Cargo.toml'))],
                resolve=dict(nodes=[dict(id='patched', features=features)])))
        for features in ([], [required[0]], [required[1]]):
            with self.subTest(features=features), self.assertRaisesRegex(RuntimeError, 'capabilities missing'):
                self.check_metadata(metadata(features), '', patch=True, remote=False, blocking=True)
        self.check_metadata(metadata(required), '', patch=True, remote=False, blocking=True)

    def migrate(self, versions):
        gate = gate_module()
        commands = []

        class Check(gate.Check):
            def metadata(self, name, consumer, native, patch, pure_abi=False, locked=False, remote=False,
                         blocking=False, startup=False):
                if patch and not pure_abi and versions is not None:
                    self.resolutions.append(dict(name=name, tokio=[dict(version=next(versions), source=None)]))

            def run(self, name, consumer, arguments, expected=0):
                commands.append((name, arguments))
                if name == 'newer-lock-seed':
                    return json.dumps(dict(packages=[dict(name='tokio', version='1.53.2', source='registry+fixture')])), ''
                if name == 'newer-lock-patched-not-selected':
                    return '', '--locked requires lock file update'
                if name == 'newer-lock-select-patch':
                    return '', ''
                raise AssertionError('unexpected Cargo boundary: ' + name)

        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            checker = Check(SimpleNamespace(root=ROOT, output_dir=directory / 'evidence',
                                            target_dir=directory / 'target', offline=True,
                                            resolution_only=True))
            checker.execute()
        return next(arguments for name, arguments in commands if name == 'newer-lock-select-patch')

    def test_migration_selects_the_runtime_resolved_by_consumers(self):
        for version in ['1.53.1', '1.53.3', '1.54.0']:
            with self.subTest(version=version):
                arguments = self.migrate(itertools.repeat(version))
                self.assertEqual(arguments, ['update', '-p', 'tokio', '--precise', version])

    def test_api_consumer_requires_worker_startup_capability(self):
        gate = gate_module()
        expected, resolutions = self.check_metadata(
            '', missing_feature(gate.STARTUP_MARKER),
            patch=False, remote=False, startup=True)
        self.assertEqual((expected, resolutions), (101, []))
        for stderr in ['network unavailable', missing_feature(gate.BLOCKING_MARKER)]:
            with self.subTest(stderr=stderr), self.assertRaisesRegex(RuntimeError, 'missing capability'):
                self.check_metadata('', stderr, patch=False, remote=False, startup=True)
        data = dict(packages=[dict(name='tokio', id='patched', version='1.53.3',
                    source=None, manifest_path=str(ROOT / 'vendor/tokio/Cargo.toml'))],
                    resolve=dict(nodes=[dict(id='patched', features=[])]))
        with self.assertRaisesRegex(RuntimeError, 'capabilities missing'):
            self.check_metadata(json.dumps(data), '', patch=True, remote=False, startup=True)
        data['resolve']['nodes'][0]['features'] = [gate.STARTUP_MARKER]
        self.check_metadata(json.dumps(data), '', patch=True, remote=False, startup=True)

    def test_missing_patched_identity_cannot_select_a_version(self):
        with self.assertRaisesRegex(RuntimeError, 'agree on one runtime version'):
            self.migrate(None)

    def test_conflicting_patched_versions_cannot_select_a_version(self):
        with self.assertRaisesRegex(RuntimeError, 'agree on one runtime version'):
            self.migrate(itertools.cycle(['1.53.3', '1.54.0']))

    def test_application_consumer_receives_the_exact_document(self):
        gate = gate_module()
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            checker = gate.Check(SimpleNamespace(root=ROOT, output_dir=directory / 'evidence',
                                                target_dir=directory / 'target', offline=True,
                                                resolution_only=True))
            consumer = checker.consumer('application', 'parrot-app', [], False, False)
            source = (consumer / 'src/main.rs').read_text()
            literal = re.search(r'AppManifest::from_toml\(([^\n]+)\)\.unwrap', source).group(1)
            self.assertEqual(json.loads(literal), 'name = "external"\nversion = "1.0.0"\ncomponents = []\n')


if __name__ == '__main__':
    unittest.main()
