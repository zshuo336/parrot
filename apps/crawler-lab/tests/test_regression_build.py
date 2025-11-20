"""Exercise build selection with real owned commands and distinct executables.

Compiler shims isolate Cargo output selection; they do not certify polyglot business.
"""
import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'apps/crawler-lab/deploy'))
import regression
from launcher import Launcher, arguments


class RegressionBuildContracts(unittest.TestCase):
    def test_missing_unrelated_or_ambiguous_build_outputs_cannot_use_a_stale_binary(self):
        with tempfile.TemporaryDirectory(prefix='crawler-build-events-', dir='/tmp') as directory:
            root = Path(directory)
            binary = root / 'crawler-lab'
            binary.write_text('#!/bin/sh\nexit 0\n')
            binary.chmod(0o700)
            event = dict(reason='compiler-artifact',
                         target=dict(name='crawler-lab', kind=['bin'],
                                     src_path=str(root / 'apps/crawler-lab/src/main.rs')),
                         profile=dict(test=False), executable=str(binary))
            raw = json.dumps(event)
            with patch.object(regression, 'ROOT', root):
                self.assertEqual(regression.built_application(raw), binary)
                for output in ['', json.dumps(dict(reason='build-finished', success=True)),
                               raw + '\n' + raw,
                               raw.replace('"test": false', '"test": true'),
                               raw.replace('/apps/crawler-lab/src/main.rs', '/other/src/main.rs'),
                               raw.replace('"kind": ["bin"]', '"kind": ["lib"]')]:
                    with self.subTest(output=output), self.assertRaises(regression.RunFailure):
                        regression.built_application(output)
                binary.chmod(0o600)
                with self.assertRaises(regression.RunFailure):
                    regression.built_application(raw)
                binary.unlink()
                with self.assertRaises(regression.RunFailure):
                    regression.built_application(raw)

    def test_golden_runs_the_selected_build_in_a_custom_target_directory(self):
        with tempfile.TemporaryDirectory(prefix='crawler-build-selection-', dir='/tmp') as directory:
            root = Path(directory)
            selected = root / 'custom target/debug/crawler-lab'
            selected.parent.mkdir(parents=True)
            selected.write_text('#!' + sys.executable + '\nprint(' +
                                repr(regression.GOLDEN.read_text()) + ')\n')
            selected.chmod(0o700)
            stale = root / 'target/debug/crawler-lab'
            stale.parent.mkdir(parents=True)
            stale.write_text(selected.read_text())
            stale.chmod(0o700)
            bindir = root / 'bin'
            bindir.mkdir()
            (bindir / 'make').write_text('#!/bin/sh\nexit 0\n')
            (bindir / 'make').chmod(0o700)
            record = root / 'cargo-arguments.json'
            event = dict(reason='compiler-artifact',
                         target=dict(name='crawler-lab', kind=['bin'],
                                     src_path=str(root / 'apps/crawler-lab/src/main.rs')),
                         profile=dict(test=False), executable=str(selected))
            cargo = bindir / 'cargo'
            cargo.write_text('#!' + sys.executable + '\nimport json, pathlib, sys\n' +
                             'pathlib.Path(' + repr(str(record)) + ').write_text(json.dumps(sys.argv[1:]))\n' +
                             'print(' + repr(json.dumps(event)) + ')\n')
            cargo.chmod(0o700)
            launched = []

            def execute_selected(launcher):
                path = Path(os.environ['PARROT_LAB_BIN'])
                launched.append(path)
                child = launcher.spawn('app', [path])
                launcher.completed(child)

            environment = dict(PATH=str(bindir) + os.pathsep + os.environ['PATH'],
                               CARGO=str(cargo), CARGO_TARGET_DIR=str(selected.parents[1]),
                               LAB_MODE='direct', LAB_RUN_DIR=str(root / 'run'), LAB_TIMEOUT='15')
            with patch.dict(os.environ, environment), patch.object(regression, 'ROOT', root), \
                    patch.object(Launcher, 'execute', execute_selected):
                runner = regression.Regression(arguments(['--skip-ts']))
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    code = runner.run()
            self.assertEqual(code, 0, (root / 'run/result.json').read_text())
            self.assertEqual(launched, [selected])
            command = json.loads(record.read_text())
            self.assertIn('--message-format=json', command)
            self.assertIn('--locked', command)
            self.assertEqual(command[command.index('--bin') + 1], 'crawler-lab')


if __name__ == '__main__':
    unittest.main()
