"""Build-state failures must never validate stale business components.

Compiler shims exercise receipt transactions and path selection deterministically;
real component builds and public host execution are separate required gates.
"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

MODULE = Path(__file__).resolve().parents[1] / 'crawler_business_artifacts.py'
spec = importlib.util.spec_from_file_location('business_artifacts', MODULE)
business = importlib.util.module_from_spec(spec)
spec.loader.exec_module(business)


class BuildReceipts(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='crawler-receipt-contract-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.environment = {'PATH': os.environ['PATH'], 'CARGO_HOME': str(self.root / 'cargo-home')}
        for relative in business.REQUIRED_ROOTS:
            directory = self.root / relative
            directory.mkdir(parents=True)
            (directory / 'input.rs').write_text('current business source\n')
        for relative in business.RECIPES:
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(relative + '\n')
        (self.root / 'apps/crawler-lab/wasm/Cargo.lock').write_text('locked wasm graph\n')
        self.target = self.root / 'target'
        self.commands = []
        self.fail = None
        self.change_during_build = False
        self.runner = patch.object(business, 'run', side_effect=self.run_tool)
        self.runner.start()
        self.addCleanup(self.runner.stop)

    def metadata(self, wasm):
        name = 'crawler-lab-wasm' if wasm else 'crawler-lab-dylib'
        directory = 'wasm' if wasm else 'dylib'
        packages = [dict(id=name, name=name, source=None,
                         manifest_path=str(self.root / 'apps/crawler-lab' / directory / 'Cargo.toml')),
                    dict(id='crawler-lab', name='crawler-lab', source=None,
                         manifest_path=str(self.root / 'apps/crawler-lab/Cargo.toml')),
                    dict(id='core', name='crawler-lab-core', source=None,
                         manifest_path=str(self.root / 'apps/crawler-lab/core/Cargo.toml'))]
        return dict(target_directory=str(self.target), packages=packages,
                    resolve=dict(nodes=[dict(id=name, deps=[dict(pkg='core', dep_kinds=[dict(kind=None)])]),
                                        dict(id='core', deps=[]),
                                        dict(id='crawler-lab', deps=[dict(pkg='core', dep_kinds=[dict(kind=None)])])]))

    def run_tool(self, command, root, environment, capture=False):
        self.commands.append(command)
        if self.fail and self.fail(command):
            raise subprocess.CalledProcessError(23, command)
        if '--version' in command:
            return subprocess.CompletedProcess(command, 0, stdout='fixture tool 1\n')
        if 'metadata' in command:
            self.target = Path(environment.get('CARGO_TARGET_DIR', str(self.root / 'target')))
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps(self.metadata('--manifest-path' in command)))
        if '--message-format=json' in command and '--bin' in command:
            path = self.target / 'debug/crawler-lab'
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'compiled-application'); path.chmod(0o755)
            event = dict(reason='compiler-artifact', package_id='crawler-lab', target=dict(name='crawler-lab', crate_types=['bin']), filenames=[str(path)], executable=str(path))
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps(event) + '\n')
        if '--message-format=json' in command:
            wasm = '--target' in command
            build_target = Path(command[command.index('--target-dir') + 1])
            path = (build_target / 'wasm32-unknown-unknown/release/crawler_lab_wasm.wasm'
                    if wasm else build_target / 'debug/libcrawler_lab_dylib.so')
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'compiled-wasm' if wasm else b'compiled-native')
            if self.change_during_build:
                (self.root / 'apps/crawler-lab/core/input.rs').write_text('source raced build\n')
            event = dict(reason='compiler-artifact', package_id='crawler-lab-wasm' if wasm else 'crawler-lab-dylib', target=dict(name='crawler_lab_wasm' if wasm else 'crawler_lab_dylib', crate_types=['cdylib']), filenames=[str(path)])
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps(event) + '\n')
        if command[:3] == ['wasm-tools', 'component', 'new']:
            Path(command[-1]).write_bytes(b'validated component ' + Path(command[3]).read_bytes())
        return subprocess.CompletedProcess(command, 0, stdout='')

    def build(self):
        return business.build(self.root, self.environment)

    def verify(self, **kwargs):
        return business.verify(self.root, self.environment, **kwargs)

    def test_fresh_receipt_verifies_without_invoking_any_tool(self):
        artifacts = self.build()
        self.commands.clear()
        with patch.object(business, 'run', side_effect=AssertionError('verification must not build')):
            self.assertEqual(self.verify(), artifacts)
        self.assertEqual(self.commands, [])

    def test_missing_receipt_rejects_even_when_both_artifacts_exist(self):
        artifacts = self.build()
        (self.root / business.RECEIPT).unlink()
        self.assertTrue(all(Path(a['path']).is_file() for a in artifacts.values()))
        with self.assertRaises(business.ArtifactError):
            self.verify()

    def test_source_modification_addition_and_deletion_invalidate_receipt(self):
        source = self.root / 'apps/crawler-lab/core/input.rs'
        for change in ['modify', 'add', 'delete']:
            with self.subTest(change=change):
                source.write_text('current business source\n')
                extra = source.parent / 'new_business.rs'
                extra.unlink(missing_ok=True)
                self.build()
                if change == 'modify': source.write_text('changed business\n')
                elif change == 'add': extra.write_text('new business\n')
                else: source.unlink()
                with self.assertRaisesRegex(business.ArtifactError, 'source inputs changed'):
                    self.verify()

    def test_wit_and_both_lockfiles_are_inputs(self):
        for relative in ['crates/parrot-wasm/wit/input.rs', 'Cargo.lock', 'apps/crawler-lab/wasm/Cargo.lock']:
            with self.subTest(path=relative):
                self.build()
                path = self.root / relative
                path.write_text(path.read_text() + 'changed\n')
                with self.assertRaises(business.ArtifactError): self.verify()

    def test_modified_or_missing_artifact_is_rejected_for_both_forms(self):
        for name in ['application', 'dylib', 'wasm']:
            for change in ['modified', 'same-size', 'missing']:
                with self.subTest(name=name, change=change):
                    artifact = Path(self.build()[name]['path'])
                    if change == 'modified': artifact.write_bytes(b'replacement')
                    elif change == 'same-size': artifact.write_bytes(bytes(b ^ 1 for b in artifact.read_bytes()))
                    else: artifact.unlink()
                    with self.assertRaises(business.ArtifactError): self.verify()

    def test_application_sources_and_executable_permission_are_verified(self):
        for relative in ['apps/crawler-lab/src/input.rs', 'apps/crawler-lab/Cargo.toml',
                         'apps/crawler-lab/crawler.app.toml', 'apps/crawler-lab/build.rs']:
            self.build()
            path = self.root / relative
            path.write_text('changed application source')
            with self.assertRaises(business.ArtifactError): self.verify()
        application = Path(self.build()['application']['path'])
        application.chmod(0o644)
        with self.assertRaises(business.ArtifactError): self.verify()

    def test_failed_native_build_invalidates_previous_success(self):
        artifacts = self.build()
        self.fail = lambda cmd: 'build' in cmd and 'crawler-lab-dylib' in cmd
        with self.assertRaises(subprocess.CalledProcessError): self.build()
        self.assertFalse((self.root / business.RECEIPT).exists())
        self.assertTrue(Path(artifacts['dylib']['path']).is_file())
        with self.assertRaises(business.ArtifactError): self.verify()

    def test_failed_metadata_and_component_validation_cannot_leave_a_receipt(self):
        for stage in ['metadata', 'validate']:
            with self.subTest(stage=stage):
                self.fail = None
                self.build()
                self.fail = lambda cmd: stage in cmd
                with self.assertRaises(subprocess.CalledProcessError): self.build()
                with self.assertRaises(business.ArtifactError): self.verify()

    def test_source_change_during_build_prevents_publication(self):
        self.change_during_build = True
        with self.assertRaisesRegex(business.ArtifactError, 'changed during the build'):
            self.build()
        self.assertFalse((self.root / business.RECEIPT).exists())

    def test_cargo_output_and_explicit_native_destination_are_used(self):
        self.environment['CARGO_TARGET_DIR'] = str(self.root / 'nondefault target')
        self.environment['CRAWLER_TEST_DYLIB'] = 'exports/custom.so'
        artifacts = self.build()
        self.assertEqual(artifacts['dylib']['path'], str(self.root / 'exports/custom.so'))
        self.assertEqual(self.verify(), artifacts)
        self.assertTrue((self.root / 'nondefault target/parrot-business-build/native/debug/libcrawler_lab_dylib.so').exists())
        self.environment['CRAWLER_TEST_DYLIB'] = 'exports/other.so'
        with self.assertRaisesRegex(business.ArtifactError, 'CRAWLER_TEST_DYLIB'): self.verify()

    def test_relative_target_directory_preserves_build_invocation_cwd(self):
        self.environment['CARGO_TARGET_DIR'] = os.path.relpath(self.root / 'relative target', Path.cwd())
        artifacts = self.build()
        self.assertTrue(Path(artifacts['dylib']['path']).is_relative_to(self.root / 'relative target'))
        self.assertEqual(self.verify(), artifacts)
        executable = self.root / 'relative target/release/deps/component-business'
        executable.parent.mkdir(parents=True)
        executable.touch()
        self.environment['CARGO_TARGET_DIR'] = 'a-different-runtime-working-directory'
        self.assertEqual(self.verify(test_executable=executable), artifacts)
        with self.assertRaises(business.ArtifactError): self.verify()
        with self.assertRaises(business.ArtifactError):
            self.verify(test_executable=self.root / 'other-target/debug/deps/test')

    def test_build_config_changes_and_new_config_files_invalidate(self):
        self.build()
        directory = self.root / '.cargo'
        directory.mkdir()
        (directory / 'config.toml').write_text('[build]\nrustflags=["--cfg", "changed"]\n')
        with self.assertRaisesRegex(business.ArtifactError, 'configuration changed'): self.verify()

    def test_build_options_are_recorded_and_test_harness_variables_do_not_replace_them(self):
        self.environment['RUSTFLAGS'] = '--cfg original'
        artifacts = self.build()
        self.environment['RUSTFLAGS'] = '--cfg changed'
        with self.assertRaises(business.ArtifactError): self.verify()
        executable = self.target / 'debug/deps/component-business'
        executable.parent.mkdir(parents=True)
        executable.touch()
        self.assertEqual(self.verify(test_executable=executable), artifacts)
        receipt = json.loads((self.root / business.RECEIPT).read_text())
        self.assertEqual(receipt['build_environment']['RUSTFLAGS'], '--cfg original')

    def test_corrupt_partial_and_incomplete_receipts_are_rejected(self):
        for replacement in ['{', '{}', '{"schema":1}']:
            with self.subTest(receipt=replacement):
                self.build()
                (self.root / business.RECEIPT).write_text(replacement)
                with self.assertRaises(business.ArtifactError): self.verify()
        self.build()
        path = self.root / business.RECEIPT
        receipt = json.loads(path.read_text())
        del receipt['artifacts']['wasm']
        path.write_text(json.dumps(receipt))
        with self.assertRaises(business.ArtifactError): self.verify()

    def test_explicit_destination_cannot_replace_source_or_follow_symlinks(self):
        source = self.root / 'apps/crawler-lab/dylib/do-not-overwrite.so'
        source.write_bytes(b'original source fixture')
        self.environment['CRAWLER_TEST_DYLIB'] = str(source)
        with self.assertRaises(business.ArtifactError): self.build()
        self.assertEqual(source.read_bytes(), b'original source fixture')
        alias = self.root / 'alias.so'; alias.symlink_to(source)
        self.environment['CRAWLER_TEST_DYLIB'] = str(alias)
        with self.assertRaises(business.ArtifactError): self.build()
        self.assertEqual(source.read_bytes(), b'original source fixture')

    def test_live_builder_lock_causes_verification_to_fail_without_waiting(self):
        self.build()
        with business.receipt_lock(self.root, True):
            with self.assertRaisesRegex(business.ArtifactError, 'already in progress'): self.verify()

    def test_generated_targets_do_not_become_source_inputs(self):
        artifacts = self.build()
        generated = self.root / 'apps/crawler-lab/wasm/target/generated'
        generated.mkdir(parents=True)
        (generated / 'new-object.o').write_bytes(b'irrelevant cache')
        self.assertEqual(self.verify(), artifacts)

    def test_later_cargo_output_replacement_does_not_modify_the_verified_copy(self):
        artifacts = self.build()
        receipt = json.loads((self.root / business.RECEIPT).read_text())
        compiled = Path(receipt['native_build_path'])
        self.assertNotEqual(str(compiled), artifacts['dylib']['path'])
        compiled.write_bytes(b'another Cargo invocation with different features')
        self.assertEqual(self.verify(), artifacts)
        self.assertEqual(Path(artifacts['dylib']['path']).read_bytes(), b'compiled-native')

    def test_verification_never_creates_missing_build_state(self):
        with self.assertRaises(business.ArtifactError): self.verify()
        self.assertFalse((self.root / business.LOCK).exists())
        self.assertFalse((self.root / business.RECEIPT).exists())

    def test_interrupted_atomic_copy_staging_is_not_a_source_input(self):
        artifacts = self.build()
        staging = self.root / 'apps/crawler-lab/wasm/.parrot-business-staging'
        (staging / 'interrupted-component').write_bytes(b'incomplete generated output')
        self.assertEqual(self.verify(), artifacts)

    def test_wrong_package_artifact_cannot_substitute_for_the_selected_library(self):
        original = self.run_tool
        def wrong_package(command, *args, **kwargs):
            result = original(command, *args, **kwargs)
            if '--message-format=json' in command:
                event = json.loads(result.stdout)
                event['package_id'] = 'another-package'
                result.stdout = json.dumps(event) + '\n'
            return result
        with patch.object(business, 'run', side_effect=wrong_package):
            with self.assertRaisesRegex(business.ArtifactError, 'exactly one'): self.build()
        self.assertFalse((self.root / business.RECEIPT).exists())

    def test_receipt_fifo_fails_without_opening_it(self):
        self.build()
        path = self.root / business.RECEIPT
        path.unlink()
        os.mkfifo(path)
        with self.assertRaises(business.ArtifactError): self.verify()

    def test_transitive_local_dependency_is_hashed_and_graph_races_fail(self):
        directory = self.root / 'helpers/business-helper'
        directory.mkdir(parents=True)
        source = directory / 'lib.rs'; source.write_text('helper business')
        original = self.metadata
        def with_helper(wasm):
            metadata = original(wasm)
            metadata['packages'].append(dict(id='helper', name='business-helper', source=None,
                                              manifest_path=str(directory / 'Cargo.toml')))
            metadata['resolve']['nodes'][1]['deps'].append(dict(pkg='helper', dep_kinds=[dict(kind='build')]))
            metadata['resolve']['nodes'].append(dict(id='helper', deps=[]))
            return metadata
        with patch.object(self, 'metadata', side_effect=with_helper):
            self.build()
            source.write_text('changed helper business')
            with self.assertRaises(business.ArtifactError): self.verify()
        calls = [0]
        def changing_graph(wasm):
            calls[0] += 1
            return original(wasm) if calls[0] <= 2 else with_helper(wasm)
        with patch.object(self, 'metadata', side_effect=changing_graph):
            with self.assertRaisesRegex(business.ArtifactError, 'source graph'): self.build()
        self.assertFalse((self.root / business.RECEIPT).exists())


if __name__ == '__main__':
    unittest.main(verbosity=2)
