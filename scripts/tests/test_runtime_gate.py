"""Exercise the runtime gate with process failures and a real tiny Cargo package.

No workspace compilation or external service is needed. Rust, Clippy and Python
are required. PARROT_LIFECYCLE_GATE selects a saved gate for counterexamples.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
GATE = Path(os.environ.get('PARROT_LIFECYCLE_GATE', ROOT / 'scripts/check_typed_lifecycle.sh'))


class GateContracts(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='parrot-runtime-gate-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / 'scripts').mkdir()
        self.gate = self.root / 'scripts/check_typed_lifecycle.sh'
        shutil.copyfile(GATE, self.gate)
        self.environment = dict(os.environ, CARGO_TARGET_DIR=str(self.root / 'target'),
                                CARGO_NET_OFFLINE='true', CARGO_BUILD_JOBS='2',
                                CARGO_INCREMENTAL='0')
        self.environment.pop('PARROT_GATE_FAILURE', None)
        self.events = self.root / 'events.jsonl'

    def shim(self, failure=''):
        directory = self.root / 'bin'
        directory.mkdir()
        cargo = directory / 'cargo'
        cargo.write_text(
            '#!/usr/bin/env python3\n'
            'import json, os, sys\n'
            'with open(os.environ["PARROT_GATE_EVENTS"], "a") as out:\n'
            ' out.write(json.dumps({"args": sys.argv[1:], "cwd": os.getcwd()}) + "\\n")\n'
            'sys.exit(47 if sys.argv[1] == os.environ["PARROT_GATE_FAIL_COMMAND"] else 0)\n'
        )
        cargo.chmod(0o755)
        self.environment.update(PATH=str(directory) + os.pathsep + os.environ['PATH'],
                                PARROT_GATE_EVENTS=str(self.events),
                                PARROT_GATE_FAIL_COMMAND=failure)

    def run_gate(self, *args, failure=None):
        env = self.environment.copy()
        if failure:
            env['PARROT_GATE_FAILURE'] = failure
        # Invoking from outside the package is part of the public script contract.
        return subprocess.run(['sh', str(self.gate), *args], cwd=self.root.parent,
                              env=env, text=True, capture_output=True, timeout=120)

    def commands(self):
        return [json.loads(line) for line in self.events.read_text().splitlines()] if self.events.exists() else []

    def fixture(self):
        self.assertIsNotNone(shutil.which('cargo'), 'Cargo is required; this gate must not skip')
        (self.root / 'src').mkdir()
        (self.root / 'tests').mkdir()
        (self.root / 'Cargo.toml').write_text(
            '[package]\nname="parrot"\nversion="0.0.0"\nedition="2024"\n'
            '[features]\nparrot_envelope_legacy=[]\n')
        (self.root / 'src/lib.rs').write_text(
            '//! ```\n'
            '//! assert_ne!(std::env::var("PARROT_GATE_FAILURE").as_deref(), Ok("doc"), "DOC_FAILURE");\n'
            '//! ```\n'
            '#[test]\nfn library_contract() {\n'
            ' assert_ne!(std::env::var("PARROT_GATE_FAILURE").as_deref(), Ok("lib"), "LIB_FAILURE");\n'
            '}\n')
        # Deliberately use an arbitrary future target name. It is never added to
        # a shell allowlist, and can only run through package-wide discovery.
        (self.root / 'tests/future_runtime_contract.rs').write_text(
            '#[test]\nfn future_runtime_contract() {\n'
            ' assert_eq!(cfg!(feature="parrot_envelope_legacy"), std::env::var("PARROT_EXPECT_LEGACY").unwrap() == "true");\n'
            ' assert_eq!(cfg!(debug_assertions), std::env::var("PARROT_EXPECT_DEBUG").unwrap() == "true");\n'
            ' assert_ne!(std::env::var("PARROT_GATE_FAILURE").as_deref(), Ok("integration"), "INTEGRATION_FAILURE");\n'
            '}\n')
        result = subprocess.run(['cargo', 'generate-lockfile', '--offline'], cwd=self.root,
                                env=self.environment, text=True, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_historical_defaults_and_working_directory(self):
        self.shim()
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = self.commands()
        self.assertEqual([r['args'][0] for r in rows], ['test', 'clippy'])
        for row in rows:
            self.assertEqual(Path(row['cwd']).resolve(), self.root.resolve())
            self.assertIn('--locked', row['args'])
            self.assertIn('parrot_envelope_legacy', row['args'])
            self.assertNotIn('--release', row['args'])

    def test_invalid_arguments_fail_before_launching_cargo(self):
        self.shim()
        for args in [('fast',), ('debug', 'unknown'), ('debug', 'legacy', 'extra'),
                     ('',), ('debug', '')]:
            with self.subTest(args=args):
                result = self.run_gate(*args)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertEqual(self.commands(), [])

    def test_test_failure_prevents_lint_and_is_not_success(self):
        self.shim('test')
        result = self.run_gate('release', 'default')
        self.assertEqual(result.returncode, 47, result.stdout + result.stderr)
        self.assertEqual([r['args'][0] for r in self.commands()], ['test'])

    def test_clippy_failure_is_not_success(self):
        self.shim('clippy')
        result = self.run_gate('release', 'legacy')
        self.assertEqual(result.returncode, 47, result.stdout + result.stderr)
        rows = self.commands()
        self.assertEqual([r['args'][0] for r in rows], ['test', 'clippy'])
        for row in rows:
            self.assertIn('--release', row['args'])
            self.assertIn('parrot_envelope_legacy', row['args'])
        self.assertEqual(rows[-1]['args'][-3:], ['--', '-D', 'warnings'])

    def test_new_integration_contract_runs_in_all_four_modes(self):
        self.fixture()
        for profile in ['debug', 'release']:
            for envelope in ['default', 'legacy']:
                with self.subTest(profile=profile, envelope=envelope):
                    self.environment.update(PARROT_EXPECT_LEGACY=str(envelope == 'legacy').lower(),
                                            PARROT_EXPECT_DEBUG=str(profile == 'debug').lower())
                    result = self.run_gate(profile, envelope, failure='integration')
                    log = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 101, log)
                    self.assertIn('INTEGRATION_FAILURE', log)
                    self.assertIn('future_runtime_contract ... FAILED', log)
                    self.assertNotIn('could not compile', log)
                    result = self.run_gate(profile, envelope)
                    log = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, log)
                    self.assertIn('future_runtime_contract ... ok', log)
                    self.assertIn('library_contract ... ok', log)
                    self.assertIn('Doc-tests parrot', log)

    def test_library_and_documentation_failures_reach_the_caller(self):
        self.fixture()
        self.environment.update(PARROT_EXPECT_LEGACY='true', PARROT_EXPECT_DEBUG='true')
        for kind in ['lib', 'doc']:
            with self.subTest(kind=kind):
                result = self.run_gate(failure=kind)
                log = result.stdout + result.stderr
                self.assertEqual(result.returncode, 101, log)
                self.assertIn(kind.upper() + '_FAILURE', log)
                self.assertNotIn('could not compile', log)


if __name__ == '__main__':
    unittest.main()
