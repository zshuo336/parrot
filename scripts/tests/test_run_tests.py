"""Execute the workspace entrypoint with command probes, including an unrelated cwd."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ENTRYPOINT = Path(__file__).resolve().parents[1] / 'run_tests.sh'
RESOURCE_DEFAULTS = {
    'CARGO_BUILD_JOBS': '2',
    'CARGO_INCREMENTAL': '0',
    'CARGO_PROFILE_TEST_DEBUG': '0',
    'CARGO_TERM_COLOR': 'never',
}


class TestEntrypoint(unittest.TestCase):
    def check_entrypoint(self, prepare, overrides=None):
        with tempfile.TemporaryDirectory(prefix='parrot entrypoint ') as temporary:
            directory = Path(temporary)
            root = directory / 'checkout with spaces'
            scripts = root / 'scripts'
            scripts.mkdir(parents=True)
            entrypoint = scripts / 'run_tests.sh'
            shutil.copyfile(ENTRYPOINT, entrypoint)
            caller = directory / 'unrelated caller'
            caller.mkdir()
            binaries = directory / 'bin'
            binaries.mkdir()
            log = directory / 'commands.jsonl'
            driver = directory / 'record.py'
            driver.write_text(
                'import json, os, pathlib, sys\n'
                'record = {"tool": sys.argv[1], "cwd": os.getcwd(), "args": sys.argv[2:]}\n'
                'record["resources"] = {key: os.environ.get(key) for key in '
                f'{list(RESOURCE_DEFAULTS)!r}'
                '}\n'
                'with pathlib.Path(os.environ["PARROT_TEST_LOG"]).open("a") as stream:\n'
                '    stream.write(json.dumps(record) + "\\n")\n'
                'sys.exit(0 if os.getcwd() == os.environ["PARROT_TEST_ROOT"] else 43)\n'
            )
            for name in ['make', 'cargo']:
                executable = binaries / name
                executable.write_text(
                    '#!/bin/sh\n'
                    f'exec "$PARROT_TEST_PYTHON" "$PARROT_TEST_DRIVER" {name} "$@"\n'
                )
                executable.chmod(0o755)
            environment = dict(os.environ, PATH=str(binaries) + os.pathsep + os.environ['PATH'],
                               PARROT_TEST_PYTHON=sys.executable, PARROT_TEST_LOG=str(log),
                               PARROT_TEST_DRIVER=str(driver),
                               PARROT_TEST_ROOT=str(root.resolve()),
                               PARROT_PREPARE_CRAWLER='1' if prepare else '0')
            for variable in RESOURCE_DEFAULTS:
                environment.pop(variable, None)
            environment.update(overrides or {})
            result = subprocess.run(['bash', str(entrypoint)], cwd=caller, env=environment,
                                    text=True, capture_output=True, timeout=10)
            commands = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(result.returncode, 0, result.stderr + repr(commands))
            self.assertEqual([command['tool'] for command in commands],
                             ['make', 'cargo'] if prepare else ['cargo'])
            self.assertTrue(all(command['cwd'] == str(root.resolve()) for command in commands))
            for command in commands:
                self.assertEqual(command['resources'], dict(RESOURCE_DEFAULTS, **(overrides or {})))
            if prepare:
                self.assertEqual(commands[0]['args'], ['--no-print-directory', 'build-crawler-business'])
            self.assertEqual(commands[-1]['args'],
                             ['test', '--locked', '--workspace', '--all-targets', '--lib',
                              '--bins', '--tests', '--benches'])

    def test_preparation_and_tests_use_checkout_from_unrelated_directory(self):
        self.check_entrypoint(prepare=True)

    def test_skipping_preparation_still_tests_checkout_from_unrelated_directory(self):
        self.check_entrypoint(prepare=False)

    def test_caller_resource_overrides_reach_both_preparation_and_cargo(self):
        self.check_entrypoint(prepare=True, overrides={
            'CARGO_BUILD_JOBS': '3',
            'CARGO_INCREMENTAL': '1',
            'CARGO_PROFILE_TEST_DEBUG': '1',
            'CARGO_TERM_COLOR': 'always',
        })


if __name__ == '__main__':
    unittest.main()
