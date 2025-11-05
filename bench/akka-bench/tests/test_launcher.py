import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = Path(__file__).with_name('tool_fixture.py')


class LauncherContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='akka-launcher-contract-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.events = self.root / 'events.jsonl'
        self.pids = self.root / 'pids.json'
        self.environment = dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ['PATH'],
                                BENCH_EVENTS=str(self.events), BENCH_PIDS=str(self.pids),
                                BENCH_FIXTURE=str(FIXTURE), TMPDIR=str(self.root))
        for name in ['javac', 'rustc', 'java']:
            tool = self.root / name
            tool.write_text('#!' + sys.executable + '\n' + FIXTURE.read_text())
            tool.chmod(0o755)

    def records(self):
        return [json.loads(line) for line in self.events.read_text().splitlines()] if self.events.exists() else []

    def run_launcher(self, *args, **overrides):
        result = subprocess.run([sys.executable, str(ROOT / 'run_bench.py'), *args],
                                env=dict(self.environment, **overrides), stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True, timeout=15)
        self.assertEqual(list(self.root.glob('parrot-akka-bench-*')), [])
        return result

    def test_private_strict_compilation_and_order(self):
        result = self.run_launcher('--check')
        self.assertEqual(result.returncode, 0, result.stdout)
        rows = self.records()
        self.assertEqual([r['stage'] for r in rows], ['javac', 'rustc', 'contracts', 'keys', 'oracle', 'runtime'])
        self.assertIn('-Werror', rows[0]['args'])
        self.assertNotIn('-nowarn', rows[0]['args'])
        self.assertIn('warnings', rows[1]['args'])
        self.assertIn('-O', rows[1]['args'])
        code = Path(rows[0]['args'][rows[0]['args'].index('-d') + 1])
        self.assertEqual(code.parent, self.root)
        self.assertFalse(code.exists())

    def test_compile_failure_prevents_running_anything(self):
        result = self.run_launcher('--check', BENCH_FAIL='javac')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([r['stage'] for r in self.records()], ['javac'])

    def test_oracle_failure_prevents_benchmark(self):
        result = self.run_launcher('--full', BENCH_FAIL='oracle')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('runtime', [r['stage'] for r in self.records()])

    def test_runtime_failure_is_not_a_passing_launcher(self):
        result = self.run_launcher('--full', BENCH_FAIL='runtime')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.records()[-1]['stage'], 'runtime')

    def test_invalid_arguments_fail_before_compilation(self):
        for args in [('--dispatcher-threads', '0'), ('--dispatcher-threads', '-1'), ('--dispatcher-threads', '1025'),
                     ('--unknown',), ('--full', '--check')]:
            result = self.run_launcher(*args)
            self.assertEqual(result.returncode, 2, result.stdout)
        self.assertEqual(self.records(), [])

    def test_sigterm_retires_active_tool_group_and_private_directory(self):
        process = subprocess.Popen([sys.executable, str(ROOT / 'run_bench.py'), '--check'],
                                   env=dict(self.environment, BENCH_BLOCK='javac'),
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 5
            while not self.pids.exists():
                self.assertIsNone(process.poll())
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.01)
            pids = json.loads(self.pids.read_text())
            process.send_signal(signal.SIGTERM)
            output, _ = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 128 + signal.SIGTERM, output)
            for pid in pids:
                deadline = time.monotonic() + 5
                while self.alive(pid):
                    self.assertLess(time.monotonic(), deadline, f'owned tool survived: {pid}')
                    time.sleep(.01)
            self.assertEqual(list(self.root.glob('parrot-akka-bench-*')), [])
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
            process.communicate(timeout=10)

    @staticmethod
    def alive(pid):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        proc = Path(f'/proc/{pid}/stat')
        if proc.exists():
            return proc.read_text().rsplit(')', 1)[1].strip().split()[0] != 'Z'
        state = subprocess.run(['ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True)
        return bool(state.stdout.strip()) and not state.stdout.strip().startswith('Z')


if __name__ == '__main__':
    unittest.main(verbosity=2)
