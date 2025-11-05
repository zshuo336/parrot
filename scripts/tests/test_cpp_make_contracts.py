"""Exercise C++ recipe discovery and failure propagation through real Make."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
MAKEFILE = Path(os.environ.get('PARROT_MAKEFILE', ROOT / 'Makefile'))
MAKE = shutil.which(os.environ.get('PARROT_MAKE', 'make'))
SHIM = '''import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
with open(os.environ['PARROT_CPP_MAKE_EVENTS'], 'a') as stream:
    stream.write(json.dumps([name, *sys.argv[1:]]) + '\\n')
if os.environ['PARROT_CPP_MAKE_FAIL'] in [name, *sys.argv[1:]]:
    sys.exit(23)
if name == 'clang++':
    path = pathlib.Path(sys.argv[sys.argv.index('-o') + 1])
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text('#!/bin/sh\\nexit 0\\n')
    path.chmod(0o755)
'''


class CppMakeContracts(unittest.TestCase):
    def run_entry(self, target, fail=''):
        self.assertIsNotNone(MAKE, 'GNU Make is required')
        retained = os.environ.get('PARROT_CPP_MAKE_EVIDENCE')
        if retained:
            directory = Path(tempfile.mkdtemp(prefix=self._testMethodName + '-' + target + '-', dir=retained))
        else:
            temporary = tempfile.TemporaryDirectory(prefix='parrot-cpp-make-')
            self.addCleanup(temporary.cleanup)
            directory = Path(temporary.name)
        shutil.copy2(MAKEFILE, directory / 'Makefile')
        (directory / 'interop/cpp-lite').mkdir(parents=True)
        bins = directory / 'bin'
        bins.mkdir()
        for name in ['clang++', 'python3']:
            path = bins / name
            path.write_text('#!' + sys.executable + '\n' + SHIM)
            path.chmod(0o755)
        events_path = directory / 'events.jsonl'
        env = dict(os.environ, PATH=str(bins) + os.pathsep + os.environ['PATH'],
            PARROT_CPP_MAKE_EVENTS=str(events_path), PARROT_CPP_MAKE_FAIL=fail or 'never-a-command')
        for name in ['MAKEFLAGS', 'MFLAGS', 'GNUMAKEFLAGS', 'MAKELEVEL', 'MAKEOVERRIDES']:
            env.pop(name, None)
        result = subprocess.run([MAKE, '--no-print-directory', target], cwd=directory, env=env,
            capture_output=True, text=True, timeout=15)
        (directory / 'make.stdout').write_text(result.stdout)
        (directory / 'make.stderr').write_text(result.stderr)
        events = [json.loads(line) for line in events_path.read_text().splitlines()] if events_path.exists() else []
        return result, events

    def python_entries(self, events):
        return [event[1] for event in events if event[0] == 'python3']

    def test_both_entries_run_cli_before_transport(self):
        for target in ['cpp', 'check-cpp']:
            with self.subTest(target=target):
                result, events = self.run_entry(target)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.python_entries(events),
                    ['interop/cpp-lite/test_interop_cli.py', 'interop/cpp-lite/test_transport.py'])
                cli = next(event for event in events if 'interop/cpp-lite/test_interop_cli.py' in event)
                self.assertEqual(cli[2], '--binary')
                self.assertTrue(cli[3].endswith('/target/cpp-lite/test_interop'), cli)

    def test_cli_failure_stops_both_entries_before_transport(self):
        for target in ['cpp', 'check-cpp']:
            with self.subTest(target=target):
                result, events = self.run_entry(target, 'interop/cpp-lite/test_interop_cli.py')
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.python_entries(events), ['interop/cpp-lite/test_interop_cli.py'])

    def test_compiler_failure_stops_both_entries_before_tests(self):
        for target in ['cpp', 'check-cpp']:
            with self.subTest(target=target):
                result, events = self.run_entry(target, 'clang++')
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.python_entries(events), [])

    def test_transport_failure_propagates(self):
        for target in ['cpp', 'check-cpp']:
            with self.subTest(target=target):
                result, events = self.run_entry(target, 'interop/cpp-lite/test_transport.py')
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.python_entries(events),
                    ['interop/cpp-lite/test_interop_cli.py', 'interop/cpp-lite/test_transport.py'])


if __name__ == '__main__':
    unittest.main()
