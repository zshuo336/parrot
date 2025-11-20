"""Real Git and compiler fixtures for validation source export and verification."""
import json
import hashlib
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import tarfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / 'project_inputs.py'


class ProjectInputs(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='parrot-inputs-')
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.root = self.base / 'repo'
        self.root.mkdir()
        self.bundle = self.base / 'bundle'
        self.env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1',
                        GIT_CONFIG_GLOBAL=os.devnull, GIT_AUTHOR_NAME='Fixture',
                        GIT_AUTHOR_EMAIL='fixture@example.invalid', GIT_COMMITTER_NAME='Fixture',
                        GIT_COMMITTER_EMAIL='fixture@example.invalid')
        self.git('init', '-q')
        self.write('Cargo.toml', '[workspace]\n')
        self.write('.gitignore', 'target/\n__pycache__/\n')

    def git(self, *args, directory=None):
        return subprocess.run(['git', '-C', str(directory or self.root), *args],
                              env=self.env, capture_output=True, text=True,
                              timeout=30, check=True)

    def write(self, name, content, directory=None):
        path = (directory or self.root) / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding='utf-8')
        return path

    def run_script(self, *args):
        return subprocess.run([sys.executable, '-B', str(SCRIPT), *map(str, args)],
                              env=self.env, capture_output=True, text=True, timeout=30)

    def export(self):
        return self.run_script('export', '--root', self.root, '--output', self.bundle)

    def exported(self):
        result = self.export()
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads((self.bundle / 'manifest.json').read_text())

    def verify(self, *args, root=None):
        return self.run_script('verify', '--root', root or self.bundle / 'source',
                               '--manifest', self.bundle / 'manifest.json', *args)

    def submodule(self, name='docs', parent=None):
        origin = self.base / ('origin-' + name.replace('/', '-'))
        origin.mkdir()
        self.git('init', '-q', directory=origin)
        self.write('.gitignore', 'generated/\n', directory=origin)
        self.write('vectors/jvm-codecs.tsv', 'golden-vector\n', directory=origin)
        self.git('add', '.', directory=origin)
        self.git('commit', '-qm', 'fixture', directory=origin)
        self.git('-c', 'protocol.file.allow=always', 'submodule', 'add', '-q', str(origin), name,
                 directory=parent)
        return (parent or self.root) / name

    def test_dirty_tracked_new_files_and_executable_mode_are_preserved(self):
        path = self.write('src/main.rs', 'old\n')
        self.git('add', '.')
        path.write_text('dirty current bytes\n')
        executable = self.write('scripts/new tool.sh', '#!/bin/sh\nexit 0\n')
        executable.chmod(0o755)
        manifest = self.exported()
        self.assertEqual(set(manifest['files']),
                         {'.gitignore', 'Cargo.toml', 'src/main.rs', 'scripts/new tool.sh'})
        self.assertEqual((self.bundle / 'source/src/main.rs').read_text(), 'dirty current bytes\n')
        self.assertTrue(manifest['files']['scripts/new tool.sh']['executable'])
        self.assertEqual(self.verify('--exact').returncode, 0)
        self.assertEqual(self.verify('--selection', root=self.root).returncode, 0)

    def test_new_source_roots_vectors_vendor_and_ci_are_discovered(self):
        for name in ['new-tool/input.txt', 'bench/actix-bench/src/oracle.rs',
                     'docs/vectors/jvm-codecs.tsv', 'vendor/local/src/lib.rs',
                     '.github/workflows/ci.yml', '.cargo/config.toml']:
            self.write(name, name + '\n')
        manifest = self.exported()
        self.assertEqual(len(manifest['files']), 8)
        self.assertEqual(self.verify('--exact').returncode, 0)

    def test_historical_evidence_and_ignored_output_are_excluded(self):
        for name in ['docs/review/old/source/lib.rs', 'target/stale.rs', '__pycache__/x.pyc']:
            self.write(name, 'historical or generated\n')
        self.git('add', 'docs/review')
        self.write('docs/reviewer/current.md', 'current docs\n')
        manifest = self.exported()
        self.assertEqual(set(manifest['files']),
                         {'.gitignore', 'Cargo.toml', 'docs/reviewer/current.md'})

    def test_ignored_but_tracked_source_remains_an_input(self):
        self.write('target/required.txt', 'explicit tracked input\n')
        self.git('add', '-f', 'target/required.txt')
        self.assertIn('target/required.txt', self.exported()['files'])

    def test_dirty_submodule_files_and_untracked_vectors_are_included(self):
        docs = self.submodule()
        self.write('vectors/jvm-codecs.tsv', 'dirty vector\n', directory=docs)
        self.write('vectors/new.tsv', 'new vector\n', directory=docs)
        self.write('review/snapshot.rs', 'historical\n', directory=docs)
        self.write('generated/cache', 'ignored\n', directory=docs)
        files = self.exported()['files']
        self.assertIn('docs/vectors/new.tsv', files)
        self.assertNotIn('docs/review/snapshot.rs', files)
        self.assertNotIn('docs/generated/cache', files)
        self.assertEqual((self.bundle / 'source/docs/vectors/jvm-codecs.tsv').read_text(),
                         'dirty vector\n')

    def test_nested_submodule_is_recursed(self):
        docs = self.submodule()
        self.submodule('nested', parent=docs)
        self.assertIn('docs/nested/vectors/jvm-codecs.tsv', self.exported()['files'])

    def test_uninitialized_submodule_is_an_error(self):
        docs = self.submodule()
        self.git('submodule', 'deinit', '-f', 'docs')
        self.assertTrue(docs.is_dir())
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('uninitialized or invalid Git submodule', result.stderr)
        self.assertFalse((self.bundle / 'manifest.json').exists())

    def test_missing_tracked_file_is_an_error(self):
        path = self.write('src/required.rs', 'required\n')
        self.git('add', '.')
        path.unlink()
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('src/required.rs', result.stderr)
        self.assertFalse((self.bundle / 'manifest.json').exists())

    def test_staged_deletion_is_absent_from_current_selection(self):
        self.write('deleted.txt', 'obsolete\n')
        self.git('add', '.')
        self.git('commit', '-qm', 'before deletion')
        self.git('rm', '-q', 'deleted.txt')
        self.assertNotIn('deleted.txt', self.exported()['files'])

    def test_merge_conflicts_fail_discovery(self):
        self.write('conflict.txt', 'base\n')
        self.git('add', '.')
        self.git('commit', '-qm', 'base')
        self.git('checkout', '-qb', 'other')
        self.write('conflict.txt', 'other\n')
        self.git('commit', '-qam', 'other')
        self.git('checkout', '-q', '-')
        self.write('conflict.txt', 'ours\n')
        self.git('commit', '-qam', 'ours')
        merge = subprocess.run(['git', '-C', str(self.root), 'merge', 'other'],
                               env=self.env, capture_output=True, timeout=30)
        self.assertNotEqual(merge.returncode, 0)
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('unresolved Git conflict', result.stderr)

    def test_symlink_files_are_not_silently_dereferenced(self):
        (self.root / 'link').symlink_to(self.root / 'Cargo.toml')
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('symlink input is unsupported', result.stderr)

    def test_path_spaces_unicode_tabs_and_newlines_round_trip(self):
        names = ['space name.txt', '\u6d4b\u8bd5.txt', 'tab\tname.txt', 'line\nname.txt']
        for name in names:
            self.write(name, 'bytes for ' + name)
        files = self.exported()['files']
        for name in names:
            self.assertIn(name, files)
            self.assertEqual((self.bundle / 'source' / name).read_bytes(),
                             (self.root / name).read_bytes())
        self.assertEqual(self.verify('--exact').returncode, 0)

    def test_existing_export_is_never_overwritten(self):
        self.exported()
        manifest = (self.bundle / 'manifest.json').read_bytes()
        self.write('Cargo.toml', 'changed\n')
        self.assertNotEqual(self.export().returncode, 0)
        self.assertEqual((self.bundle / 'manifest.json').read_bytes(), manifest)

    def archive(self, output=None, includes=()):
        args = ['archive', '--bundle', self.bundle, '--output', output or self.base / 'inputs.tar.gz']
        for name in includes:
            args.extend(['--include', name])
        return self.run_script(*args)

    def test_portable_archive_preserves_manifest_bytes_modes_and_explicit_helpers(self):
        self.write('vendor/.__require_full.rs', '// real source, not metadata\n')
        script = self.write('scripts/run.sh', '#!/bin/sh\nexit 0\n')
        script.chmod(0o755)
        self.write('long/' + 'part/' * 30 + '\u6d4b\u8bd5.txt', 'long portable path\n')
        manifest = self.exported()
        self.write('verify.sh', '#!/bin/sh\nexit 0\n', directory=self.bundle).chmod(0o755)
        self.write('unselected/private.txt', 'not selected\n', directory=self.bundle)
        result = self.archive(includes=['verify.sh'])
        self.assertEqual(result.returncode, 0, result.stderr)
        unpacked = self.base / 'unpacked'
        unpacked.mkdir()
        with tarfile.open(self.base / 'inputs.tar.gz') as archive:
            self.assertEqual(set(archive.getnames()),
                             {'source/' + name for name in manifest['files']} | {'manifest.json', 'verify.sh'})
            for member in archive:
                self.assertTrue(member.isfile())
                self.assertEqual(member.uid, 0)
                self.assertEqual(member.gid, 0)
                self.assertEqual(member.mtime, 0)
                self.assertTrue(set(member.pax_headers) <= {'path'})
                path = unpacked / member.name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(archive.extractfile(member).read())
                path.chmod(member.mode)
        verified = self.run_script('verify', '--root', unpacked / 'source',
                                   '--manifest', unpacked / 'manifest.json', '--exact')
        self.assertEqual(verified.returncode, 0, verified.stderr)
        self.assertTrue((unpacked / 'verify.sh').stat().st_mode & 0o111)
        second = self.base / 'second.tar.gz'
        self.assertEqual(self.archive(second, includes=['verify.sh']).returncode, 0)
        self.assertEqual((self.base / 'inputs.tar.gz').read_bytes(), second.read_bytes())

    def test_archive_rejects_tampered_source_extras_and_existing_output(self):
        self.exported()
        original = (self.bundle / 'source/Cargo.toml').read_bytes()
        (self.bundle / 'source/Cargo.toml').write_bytes(b'x' * len(original))
        self.assertNotEqual(self.archive().returncode, 0)
        (self.bundle / 'source/Cargo.toml').write_bytes(original)
        extra = self.write('._Cargo.toml', 'metadata not in manifest', directory=self.bundle / 'source')
        result = self.archive()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('input set mismatch', result.stderr)
        extra.unlink()
        self.assertEqual(self.archive().returncode, 0)
        saved = (self.base / 'inputs.tar.gz').read_bytes()
        self.assertNotEqual(self.archive().returncode, 0)
        self.assertEqual((self.base / 'inputs.tar.gz').read_bytes(), saved)

    def test_archive_rejects_escaping_duplicate_reserved_and_symlink_helpers(self):
        self.exported()
        self.write('verify.sh', 'helper', directory=self.bundle)
        (self.bundle / 'linked.sh').symlink_to(self.bundle / 'verify.sh')
        for names in [['../outside'], ['manifest.json'], ['source/Cargo.toml'],
                      ['verify.sh', 'verify.sh'], ['linked.sh'], ['.git/config']]:
            with self.subTest(names=names):
                result = self.archive(includes=names)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('Traceback', result.stderr)
                self.assertFalse((self.base / 'inputs.tar.gz').exists())

    def test_archive_checks_archived_bytes_and_removes_partial_output_after_mutation(self):
        self.exported()
        implementation = runpy.run_path(str(SCRIPT))['archive']
        original = tarfile.TarFile.addfile
        def change_during_pack(archive, header, fileobj=None):
            if header.name == 'source/Cargo.toml':
                path = self.bundle / header.name
                path.write_bytes(b'x' * path.stat().st_size)
            return original(archive, header, fileobj)
        with mock.patch.object(tarfile.TarFile, 'addfile', change_during_pack):
            with self.assertRaisesRegex(ValueError, 'archive input identity mismatch: source/Cargo.toml'):
                implementation(self.bundle, self.base / 'inputs.tar.gz', [])
        self.assertFalse((self.base / 'inputs.tar.gz').exists())

    def test_same_length_content_tampering_is_detected(self):
        self.exported()
        path = self.bundle / 'source/Cargo.toml'
        path.write_bytes(b'x' * len(path.read_bytes()))
        result = self.verify('--exact')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('input identity mismatch: Cargo.toml', result.stderr)

    def build_target(self, output=None, root=None):
        return self.run_script('build-target', '--root', root or self.bundle / 'source',
                               '--manifest', self.bundle / 'manifest.json',
                               '--output', output or self.base / 'builds')

    def test_build_target_is_manifest_bound_fresh_and_refuses_existing_artifacts(self):
        manifest = self.exported()
        result = self.build_target()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads(result.stdout)
        digest = hashlib.sha256((self.bundle / 'manifest.json').read_bytes()).hexdigest()
        target = (self.base / 'builds' / digest).resolve()
        self.assertEqual(receipt, {'status': 'ok', 'files': len(manifest['files']),
                                  'action': 'build-target', 'target_dir': str(target),
                                  'manifest_sha256': digest, 'fresh': True})
        self.assertEqual(list(target.iterdir()), [])
        self.assertNotEqual(self.build_target().returncode, 0)
        marker = target / 'existing-artifact'
        marker.write_text('keep original artifact')
        self.assertNotEqual(self.build_target().returncode, 0)
        self.assertEqual(marker.read_text(), 'keep original artifact')

    def test_build_target_rejects_modified_inputs_and_in_source_output(self):
        self.exported()
        result = self.build_target(output=self.bundle / 'source/builds')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('outside the verified source tree', result.stderr)
        self.assertFalse((self.bundle / 'source/builds').exists())
        (self.bundle / 'source/Cargo.toml').write_text('changed source')
        result = self.build_target()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('input identity mismatch', result.stderr)
        self.assertFalse((self.base / 'builds').exists())

    def test_build_target_refuses_symlinked_existing_target(self):
        self.exported()
        digest = hashlib.sha256((self.bundle / 'manifest.json').read_bytes()).hexdigest()
        builds = self.base / 'builds'
        builds.mkdir()
        outside = self.base / 'outside'
        outside.mkdir()
        (builds / digest).symlink_to(outside, target_is_directory=True)
        self.assertNotEqual(self.build_target().returncode, 0)
        self.assertEqual(list(outside.iterdir()), [])

    def test_same_count_archived_cargo_source_uses_fresh_build_despite_old_mtimes(self):
        compiler = shutil.which('cargo')
        self.assertIsNotNone(compiler, 'cargo is required for the actual stale-build contract')
        self.write('Cargo.toml', '[package]\nname="fixture"\nversion="0.1.0"\nedition="2021"\n[workspace]\n')
        self.write('src/main.rs', 'fn main() { println!("111"); }\n')
        lock = subprocess.run([compiler, 'generate-lockfile', '--offline'], cwd=self.root,
                              env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(lock.returncode, 0, lock.stderr)
        first = self.exported()
        original_bundle = self.bundle
        source = original_bundle / 'source'
        shared = self.base / 'shared-target'

        def execute(target):
            result = subprocess.run([compiler, 'run', '--offline', '--locked', '--quiet',
                                     '--manifest-path', str(source / 'Cargo.toml'),
                                     '--target-dir', str(target)], env=self.env,
                                    capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            return result.stdout

        self.assertEqual(execute(shared), '111\n')
        old_target = self.build_target()
        self.assertEqual(old_target.returncode, 0, old_target.stderr)
        self.write('src/main.rs', 'fn main() { println!("222"); }\n')
        self.bundle = self.base / 'bundle-two'
        second = self.exported()
        self.assertEqual(set(first['files']), set(second['files']))
        self.assertNotEqual(first['files']['src/main.rs'], second['files']['src/main.rs'])
        self.assertEqual(self.archive().returncode, 0)
        with tarfile.open(self.base / 'inputs.tar.gz') as packed:
            for member in packed:
                path = original_bundle / member.name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(packed.extractfile(member).read())
                path.chmod(member.mode)
                os.utime(path, (member.mtime, member.mtime))
        self.bundle = original_bundle
        self.assertEqual(self.verify('--exact').returncode, 0)
        self.assertEqual((source / 'src/main.rs').stat().st_mtime_ns, 0)
        # Newer Cargo versions may detect this themselves. The isolated target
        # must return the new program regardless of the shared cache's decision.
        self.assertIn(execute(shared), {'111\n', '222\n'})
        result = self.build_target()
        self.assertEqual(result.returncode, 0, result.stderr)
        target = Path(json.loads(result.stdout)['target_dir'])
        self.assertNotEqual(target, Path(json.loads(old_target.stdout)['target_dir']))
        self.assertEqual(execute(target), '222\n')

    def test_executable_tampering_is_detected(self):
        self.exported()
        (self.bundle / 'source/Cargo.toml').chmod(0o755)
        self.assertNotEqual(self.verify('--exact').returncode, 0)

    def test_missing_and_extra_export_files_are_detected(self):
        self.exported()
        path = self.bundle / 'source/Cargo.toml'
        path.unlink()
        self.assertNotEqual(self.verify('--exact').returncode, 0)
        self.write('Cargo.toml', '[workspace]\n', directory=self.bundle / 'source')
        self.write('extra.txt', 'unaccounted\n', directory=self.bundle / 'source')
        result = self.verify('--exact')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('extra.txt', result.stderr)

    def test_added_current_source_invalidates_selection_verification(self):
        self.exported()
        self.write('new-source.rs', '// new input\n')
        result = self.verify('--selection', root=self.root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('new-source.rs', result.stderr)

    def test_exported_symlink_directory_cannot_escape_verification(self):
        self.exported()
        (self.bundle / 'source/extra').symlink_to(self.root, target_is_directory=True)
        self.assertNotEqual(self.verify('--exact').returncode, 0)

    def test_tracked_file_replaced_with_fifo_is_rejected_without_opening_it(self):
        path = self.write('pipe', 'original tracked file\n')
        self.git('add', 'pipe')
        path.unlink()
        os.mkfifo(self.root / 'pipe')
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.bundle / 'manifest.json').exists())

    def test_current_source_symlink_parent_is_rejected(self):
        self.write('src/required.rs', 'required\n')
        self.git('add', '.')
        external = self.base / 'external'
        (self.root / 'src').rename(external)
        (self.root / 'src').symlink_to(external, target_is_directory=True)
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('symlink input is unsupported', result.stderr)

    def test_malformed_identity_values_are_rejected(self):
        manifest = self.exported()
        path = self.bundle / 'manifest.json'
        for key, value in [('bytes', True), ('bytes', -1), ('sha256', 'z' * 64),
                           ('executable', 1)]:
            altered = json.loads(json.dumps(manifest))
            altered['files']['Cargo.toml'][key] = value
            path.write_text(json.dumps(altered))
            result = self.verify('--exact')
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('invalid file identity', result.stderr)

    def test_malformed_duplicate_or_escaping_manifest_is_rejected(self):
        manifest = self.exported()
        path = self.bundle / 'manifest.json'
        cases = ['{', '{"schema":"one","schema":"two"}', '[]']
        for name in ['../outside', '/absolute', './relative', 'a/../b', '.git/config']:
            altered = dict(manifest, files={name: manifest['files']['Cargo.toml']})
            cases.append(json.dumps(altered))
        for content in cases:
            with self.subTest(content=content):
                path.write_text(content)
                result = self.verify('--exact')
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('Traceback', result.stderr)

    def test_cross_directory_rust_inputs_compile_in_gitless_export(self):
        self.submodule()
        self.write('bench/actix-bench/src/oracle.rs', 'pub const ANSWER: usize = 42;\n')
        self.write('crates/fixture/src/main.rs',
                   '#[path="../../../bench/actix-bench/src/oracle.rs"] mod oracle;\n'
                   'fn main() { assert_eq!(oracle::ANSWER, 42);\n'
                   'assert_eq!(include_str!("../../../docs/vectors/jvm-codecs.tsv"), '
                   '"golden-vector\\n"); }\n')
        self.exported()
        source = self.bundle / 'source'
        self.assertFalse((source / '.git').exists())
        binary = self.bundle / 'fixture'
        compiler = shutil.which('rustc')
        self.assertIsNotNone(compiler, 'rustc is required for the actual compile contract')
        command = [compiler, str(source / 'crates/fixture/src/main.rs'), '-o', str(binary)]
        result = subprocess.run(command, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = subprocess.run([str(binary)], capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        for missing in ['bench/actix-bench/src/oracle.rs', 'docs/vectors/jvm-codecs.tsv']:
            path = source / missing
            saved = path.read_bytes()
            path.unlink()
            result = subprocess.run(command, capture_output=True, text=True, timeout=30)
            self.assertNotEqual(result.returncode, 0, missing)
            path.write_bytes(saved)


if __name__ == '__main__':
    unittest.main()
