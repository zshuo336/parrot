"""Exercise launcher routing and failure with a fake Cargo, never a stale binary."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

LAUNCHER = Path(__file__).resolve().parents[1] / "run.sh"

class LauncherTests(unittest.TestCase):
    def run_launcher(self, exit_code):
        with tempfile.TemporaryDirectory(prefix="actix launcher ") as directory:
            root = Path(directory)
            cargo = root / "cargo"
            cargo.write_text("#!/usr/bin/env python3\nimport json,os,sys\nfrom pathlib import Path\nPath(os.environ['CAPTURE']).write_text(json.dumps({'args':sys.argv[1:],'cwd':os.getcwd(),'target':os.environ['CARGO_TARGET_DIR']}))\nsys.exit(int(os.environ['FAKE_EXIT']))\n")
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=str(root)+os.pathsep+os.environ['PATH'], CAPTURE=str(root/'capture.json'), FAKE_EXIT=str(exit_code), CARGO_TARGET_DIR=str(root/'custom target'))
            result = subprocess.run(['bash', str(LAUNCHER), str(root/'report with spaces.md')], cwd=root, env=env, capture_output=True, timeout=10)
            observed = json.loads((root/'capture.json').read_text())
            self.assertEqual(observed['args'], ['run', '--locked', '--release', '--bin', 'actix-bench', '--', str(root/'report with spaces.md')])
            self.assertEqual(observed['cwd'], str(LAUNCHER.parent))
            self.assertEqual(observed['target'], str(root/'custom target'))
            return result.returncode

    def test_custom_target_and_spaced_report_passed_to_cargo(self):
        self.assertEqual(self.run_launcher(0), 0)

    def test_build_or_run_failure_is_not_hidden_by_a_stale_executable(self):
        self.assertEqual(self.run_launcher(19), 19)

if __name__ == '__main__':
    unittest.main()
