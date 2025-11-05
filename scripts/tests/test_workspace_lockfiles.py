"""Real Git/Cargo fixtures catch source-distribution lockfile omissions."""
import os
from pathlib import Path
import json
import shutil
import subprocess
import tempfile
import unittest


GATE = Path(__file__).resolve().parents[1] / 'check_workspace_lockfiles.py'


class WorkspaceLockfiles(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='parrot-lockfiles-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.env = dict(os.environ, CARGO_BUILD_JOBS='2', CARGO_NET_OFFLINE='true')
        self.run_command('git', 'init', '-q')
        self.package('.', 'main_fixture')

    def run_command(self, *command):
        return subprocess.run(command, cwd=self.root, env=self.env, check=True,
                              text=True, capture_output=True, timeout=30)

    def package(self, directory, name):
        path = self.root / directory
        (path / 'src').mkdir(parents=True)
        (path / 'src/lib.rs').write_text('pub fn example() {}\n')
        (path / 'Cargo.toml').write_text(
            '[package]\nname="' + name + '"\nversion="0.1.0"\nedition="2021"\n'
            '[workspace]\n')
        self.run_command('cargo', 'generate-lockfile', '--offline', '--manifest-path',
                         str(path / 'Cargo.toml'))
        return path

    def gate(self):
        return subprocess.run(['python3', str(GATE), '--root', str(self.root)],
                              env=self.env, text=True, capture_output=True, timeout=30)

    def test_discovers_new_workspace_without_allowlist(self):
        self.package('new-tool', 'tool_fixture')
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ['Cargo.lock', 'new-tool/Cargo.lock'])

    def test_missing_standalone_lock_rejects_clean_source_copy(self):
        path = self.package('new-tool', 'tool_fixture')
        (path / 'Cargo.lock').unlink()
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('missing workspace lockfile: new-tool/Cargo.lock', result.stderr)
        self.assertEqual(result.stdout, '')

    def test_ignored_existing_lock_is_not_valid_source_input(self):
        self.package('new-tool', 'tool_fixture')
        (self.root / '.gitignore').write_text('**/Cargo.lock\n!/Cargo.lock\n')
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('git-ignored workspace lockfile: new-tool/Cargo.lock', result.stderr)

    def test_tracked_lock_does_not_hide_bad_ignore_rule(self):
        self.run_command('git', 'add', 'Cargo.lock')
        (self.root / '.gitignore').write_text('**/Cargo.lock\n')
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('git-ignored workspace lockfile: Cargo.lock', result.stderr)

    def test_workspace_members_share_one_lockfile(self):
        member = self.root / 'member'
        (member / 'src').mkdir(parents=True)
        (member / 'src/lib.rs').write_text('')
        (member / 'Cargo.toml').write_text('[package]\nname="member"\nversion="0.1.0"\n')
        manifest = self.root / 'Cargo.toml'
        manifest.write_text(manifest.read_text().replace('[workspace]', '[workspace]\nmembers=["member"]'))
        self.run_command('cargo', 'generate-lockfile', '--offline')
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ['Cargo.lock'])

    def test_review_and_vendor_snapshots_are_not_product_workspaces(self):
        for directory in ['docs/review/history', 'vendor/example']:
            path = self.package(directory, 'snapshot')
            (path / 'Cargo.lock').unlink()
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ['Cargo.lock'])

    def audit_fixture(self):
        scripts = self.root / 'scripts'
        scripts.mkdir()
        shutil.copy2(GATE, scripts / GATE.name)
        shutil.copy2(GATE.with_name('check_rust_dependencies.sh'), scripts / 'check_rust_dependencies.sh')
        binary = self.root / 'bin'
        binary.mkdir()
        cargo = binary / 'cargo'
        real_cargo = shutil.which('cargo')
        self.assertIsNotNone(real_cargo)
        cargo.write_text(
            '#!/usr/bin/env python3\nimport json, os, sys\n'
            'if sys.argv[1] != "audit":\n'
            ' os.execv(' + repr(real_cargo) + ', [' + repr(real_cargo) + '] + sys.argv[1:])\n'
            'with open(os.environ["AUDIT_EVENTS"], "a") as output:\n'
            ' output.write(json.dumps(sys.argv[1:]) + "\\n")\n'
            'sys.exit(int(os.environ.get("AUDIT_EXIT", "0")))\n')
        cargo.chmod(0o755)
        self.env.update(PATH=str(binary) + os.pathsep + self.env['PATH'],
                        AUDIT_EVENTS=str(self.root / 'audit-events.jsonl'))

    def audit(self):
        return subprocess.run(['bash', 'scripts/check_rust_dependencies.sh', '--json'],
                              cwd=self.root, env=self.env, text=True,
                              capture_output=True, timeout=30)

    def test_audit_discovers_all_workspaces_and_preserves_failure(self):
        self.package('new-tool', 'tool_fixture')
        self.audit_fixture()
        result = self.audit()
        self.assertEqual(result.returncode, 0, result.stderr)
        events = self.root / 'audit-events.jsonl'
        rows = [json.loads(line) for line in events.read_text().splitlines()]
        self.assertEqual(rows, [
            ['audit', '--file', lock, '--deny', 'unsound', '--deny', 'unmaintained', '--json']
            for lock in ['Cargo.lock', 'new-tool/Cargo.lock']])
        events.unlink()
        self.env['AUDIT_EXIT'] = '45'
        result = self.audit()
        self.assertEqual(result.returncode, 45, result.stderr)
        self.assertEqual(len(events.read_text().splitlines()), 1)

    def test_discovery_failure_prevents_every_audit(self):
        self.audit_fixture()
        (self.root / 'Cargo.lock').unlink()
        result = self.audit()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('missing workspace lockfile', result.stderr)
        self.assertFalse((self.root / 'audit-events.jsonl').exists())


if __name__ == '__main__':
    unittest.main()
