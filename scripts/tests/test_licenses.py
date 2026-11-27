"""License regressions mutate real files; acceptance is not a matching string search."""
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest import mock

if sys.version_info < (3, 11):
    raise unittest.SkipTest('License checker requires Python 3.11+; mandatory CI runs it on 3.11.')

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_licenses as checker
import project_inputs


ROOT = Path(__file__).resolve().parents[2]


class LicenseContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        manifest_path = os.environ.get('PARROT_LICENSE_MANIFEST')
        if manifest_path:
            manifest = project_inputs.load_manifest(manifest_path)
            project_inputs.verify(ROOT, manifest, exact=True)
            cls.selected = sorted(manifest['files'])
        else:
            cls.selected = project_inputs.discover(ROOT)
        cls.inputs = set(checker.LICENSE_COPIES) | set(checker.THIRD_PARTY)
        cls.inputs.update(name for name in cls.selected
                          if Path(name).name in {'Cargo.toml', 'pyproject.toml',
                                                  'package.json', 'package-lock.json'}
                          or name.endswith('pom.xml'))

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.selection = set(self.inputs)
        for name in self.inputs:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, path)

    def replace(self, name, old, new):
        path = self.root / name
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))

    def check(self):
        return checker.evaluate(self.root, self.selection)

    def test_current_project_and_third_party_notices_are_consistent(self):
        report = self.check()
        self.assertTrue(report['success'])
        self.assertEqual(report['license'], 'AGPL-3.0-only')
        self.assertIn('crates/parrot/Cargo.toml', report['inputs'])

    def test_new_own_package_is_discovered_and_rejected_without_license(self):
        path = self.root / 'new-component/Cargo.toml'
        path.parent.mkdir()
        path.write_text('[package]\nname = "new-component"\nversion = "0.1.0"\n')
        self.selection.add('new-component/Cargo.toml')
        with self.assertRaisesRegex(ValueError, 'incorrect Rust license: new-component'):
            self.check()

    def test_only_version_cannot_be_replaced_with_mit_later_or_dual_license(self):
        name = 'crates/parrot/Cargo.toml'
        original = (self.root / name).read_text()
        for license in ['MIT', 'AGPL-3.0-or-later', 'AGPL-3.0-only OR MIT']:
            with self.subTest(license=license):
                (self.root / name).write_text(original.replace(checker.SPDX, license))
                with self.assertRaisesRegex(ValueError, 'incorrect Rust license'):
                    self.check()

    def test_wrong_rust_distribution_file_is_rejected(self):
        self.replace('crates/parrot/Cargo.toml', '../../LICENSE', '../../vendor/tokio/LICENSE')
        with self.assertRaisesRegex(ValueError, 'Rust license-file must resolve'):
            self.check()

    def test_python_pep639_license_file_cannot_be_omitted(self):
        self.replace('interop/python/pyproject.toml', 'license-files = ["LICENSE"]', 'license-files = []')
        with self.assertRaisesRegex(ValueError, 'missing Python license-files'):
            self.check()

    def test_npm_lockfile_cannot_retain_old_root_license(self):
        name = 'interop/typescript-lite/package-lock.json'
        self.replace(name, '"license": "AGPL-3.0-only"', '"license": "MIT"')
        with self.assertRaisesRegex(ValueError, 'incorrect npm lockfile root license'):
            self.check()

    def test_maven_jar_must_include_the_actual_project_license(self):
        self.replace('interop/jvm/pom.xml', '<targetPath>META-INF</targetPath>', '<targetPath>wrong</targetPath>')
        with self.assertRaisesRegex(ValueError, 'missing Maven packaged license resource'):
            self.check()

    def test_dependency_pom_cannot_implicitly_package_an_unlicensed_jar(self):
        self.replace('apps/websearch/jvm/config-dependencies.pom.xml',
                     '<packaging>pom</packaging>', '')
        with self.assertRaisesRegex(ValueError, 'missing Maven packaged license resource'):
            self.check()

    def test_license_copy_change_or_missing_selection_is_rejected(self):
        self.replace('interop/python/LICENSE', 'GNU AFFERO', 'ALTERED')
        with self.assertRaisesRegex(ValueError, 'nonstandard or stale license text'):
            self.check()
        self.selection.remove('docs/LICENSE.md')
        with self.assertRaisesRegex(ValueError, 'required license input not selected'):
            self.check()

    def test_third_party_text_cannot_be_relicensed(self):
        (self.root / 'vendor/tokio/LICENSE').write_bytes((self.root / 'LICENSE').read_bytes())
        with self.assertRaisesRegex(ValueError, 'third-party license changed'):
            self.check()

    def test_malformed_or_duplicate_metadata_fails(self):
        name = 'interop/typescript-lite/package.json'
        self.replace(name, '"license": "AGPL-3.0-only",',
                     '"license": "MIT", "license": "AGPL-3.0-only",')
        with self.assertRaisesRegex(ValueError, 'duplicate manifest key'):
            self.check()

    def test_historical_evidence_and_vendor_package_are_not_relicensed(self):
        for name in ['docs/review/old/Cargo.toml', 'vendor/dependency/Cargo.toml']:
            path = self.root / name
            path.parent.mkdir(parents=True)
            path.write_text('[package]\nname = "old"\nlicense = "MIT"\n')
            self.selection.add(name)
        self.assertTrue(self.check()['success'])

    def add_dependency_manifests(self):
        dependencies = {
            'node_modules/plain/package.json': '{"name":"plain","license":"MIT"}',
            'interop/typescript-lite/node_modules/typescript/package.json':
                '{"name":"typescript","license":"Apache-2.0"}',
            'apps/websearch/node_modules/@third-party/helper/package.json':
                '{"name":"@third-party/helper","license":"BSD-3-Clause"}',
            'apps/websearch/node_modules/native/Cargo.toml':
                '[package]\nname = "native"\nlicense = "MIT"\n',
            'apps/websearch/node_modules/python/pyproject.toml':
                '[project]\nname = "dependency"\nlicense = "MIT"\n',
            'apps/websearch/node_modules/java/pom.xml': '<dependency-not-a-project/>',
        }
        for name, text in dependencies.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
            self.selection.add(name)
        return dependencies

    def test_installed_dependencies_keep_their_licenses_and_are_not_own_packages(self):
        original = self.check()
        dependencies = self.add_dependency_manifests()
        report = self.check()
        self.assertEqual(report['counts'], original['counts'])
        self.assertEqual(report['inputs'], original['inputs'])
        for name, text in dependencies.items():
            self.assertEqual((self.root / name).read_text(), text)

    def test_installed_dependencies_do_not_hide_invalid_own_package(self):
        self.add_dependency_manifests()
        path = self.root / 'apps/node_modules-tools/package.json'
        path.parent.mkdir(parents=True)
        path.write_text('{"name":"own-tools","license":"MIT"}')
        self.selection.add(path.relative_to(self.root).as_posix())
        with self.assertRaisesRegex(ValueError, 'incorrect npm license: apps/node_modules-tools/'):
            self.check()

    def test_frozen_cli_accepts_dependencies_but_still_rejects_tampering(self):
        self.add_dependency_manifests()
        manifest = self.root.parent / (self.root.name + '-dependencies.json')
        self.addCleanup(manifest.unlink, missing_ok=True)
        manifest.write_text(json.dumps({
            'schema': project_inputs.SCHEMA, 'exclusions': list(project_inputs.EXCLUSIONS),
            'files': {name: project_inputs.identity(self.root / name) for name in self.selection}}))
        arguments = ['check_licenses', '--root', str(self.root), '--manifest', str(manifest)]
        with contextlib.redirect_stdout(io.StringIO()), mock.patch.object(sys, 'argv', arguments):
            self.assertEqual(checker.main(), 0)
        self.replace('interop/typescript-lite/node_modules/typescript/package.json', 'Apache-2.0', 'MIT')
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            with mock.patch.object(sys, 'argv', arguments):
                self.assertEqual(checker.main(), 1)
        self.assertIn('input identity mismatch', errors.getvalue())

    def test_frozen_cli_rejects_tampered_input_even_when_license_still_matches(self):
        manifest = self.root.parent / (self.root.name + '-manifest.json')
        self.addCleanup(manifest.unlink, missing_ok=True)
        manifest.write_text(json.dumps({
            'schema': project_inputs.SCHEMA, 'exclusions': list(project_inputs.EXCLUSIONS),
            'files': {name: project_inputs.identity(self.root / name) for name in self.selection}}))
        self.replace('crates/parrot/Cargo.toml', 'name = "parrot"', 'name = "tampered"')
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            with mock.patch.object(sys, 'argv', ['check_licenses', '--root', str(self.root),
                                                         '--manifest', str(manifest)]):
                self.assertEqual(checker.main(), 1)
        self.assertIn('input identity mismatch', errors.getvalue())


if __name__ == '__main__':
    unittest.main()
