import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = Path(__file__).with_name("tool_fixture.py")


class LauncherContracts(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ray-launcher-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.events = self.root / "events.jsonl"
        self.pids = self.root / "pids.json"
        for name in ["run.sh", "run_bench.py", "bench_plan.py"]:
            shutil.copy2(ROOT / name, self.root / name)
        shutil.copy2(FIXTURE, self.root / "ray_bench.py")
        tool = self.root / "rustc"
        tool.write_text("#!" + sys.executable + "\n" + FIXTURE.read_text())
        tool.chmod(0o755)
        self.environment = dict(
            os.environ,
            PATH=str(self.root) + os.pathsep + os.environ["PATH"],
            PYTHON=sys.executable,
            BENCH_EVENTS=str(self.events),
            BENCH_PIDS=str(self.pids),
            BENCH_FIXTURE=str(FIXTURE),
        )

    def records(self):
        return (
            [json.loads(line) for line in self.events.read_text().splitlines()]
            if self.events.exists()
            else []
        )

    def cleaned(self):
        for row in self.records():
            self.assertTrue(Path(row["cwd"]).name.startswith("prb-"))
            self.assertFalse(Path(row["cwd"]).exists())

    def run_launcher(self, *args, **overrides):
        result = subprocess.run(
            [str(self.root / "run.sh"), *args],
            env=dict(self.environment, **overrides),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=20,
        )
        self.cleaned()
        return result

    def test_private_reference_before_runtime_and_explicit_configuration(self):
        result = self.run_launcher(
            "--check",
            "--scale",
            "85.9375",
            "--cpus",
            "4",
            "--object-store-bytes",
            "134217728",
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        rows = self.records()
        self.assertEqual(
            [r["stage"] for r in rows], ["rustc", "keys", "oracle", "runtime"]
        )
        self.assertIn("warnings", rows[0]["args"])
        self.assertIn("-O", rows[0]["args"])
        self.assertIn("1375/16", rows[1]["args"])
        self.assertEqual(rows[-1]["args"][rows[-1]["args"].index("--cpus") + 1], "4")
        self.assertEqual(len({r["cwd"] for r in rows}), 1)
        for r in rows:
            self.assertEqual(r["auth_mode"], "token")
            self.assertEqual(
                Path(r["auth_path"]).resolve(),
                (Path(r["cwd"]) / "auth_token").resolve(),
            )
            self.assertEqual(r["auth_permissions"], 0o600)
            self.assertFalse(r["auth_token_present"])

    def test_default_workload_unscaled(self):
        result = self.run_launcher()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(self.records()[1]["args"][:3], ["keys", "full", "1"])

    def test_compile_failure_stops_pipeline(self):
        result = self.run_launcher("--check", BENCH_FAIL="rustc")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([r["stage"] for r in self.records()], ["rustc"])

    def test_oracle_failure_prevents_runtime(self):
        result = self.run_launcher("--check", BENCH_FAIL="oracle")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.records()[-1]["stage"], "oracle")

    def test_runtime_exit_not_hidden_by_output(self):
        result = self.run_launcher("--check", BENCH_FAIL="runtime")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("control failure", result.stdout)

    def test_bad_arguments_before_any_tool(self):
        for args in [
            ("--scale", "0"),
            ("--scale", "nan"),
            ("--scale", "1/0"),
            ("--scale", "1000001"),
            ("--cpus", "6"),
            ("--check", "--cpus", "3"),
            ("--object-store-bytes", "1"),
            ("--full", "--check"),
            ("--unknown",),
        ]:
            self.assertEqual(self.run_launcher(*args).returncode, 2)
        self.assertEqual(self.records(), [])

    def test_finished_leader_does_not_leave_child(self):
        result = self.run_launcher("--check", BENCH_ORPHAN="runtime")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assert_retired(json.loads(self.pids.read_text()))

    def test_sigterm_retires_active_tool_and_child(self):
        process = subprocess.Popen(
            [str(self.root / "run.sh"), "--check"],
            env=dict(self.environment, BENCH_BLOCK="runtime"),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        try:
            deadline = time.monotonic() + 10
            while not self.pids.exists():
                self.assertIsNone(process.poll())
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.01)
            pids = json.loads(self.pids.read_text())
            process.send_signal(signal.SIGTERM)
            output, _ = process.communicate(timeout=15)
            self.assertEqual(process.returncode, 143, output)
            self.assert_retired(pids)
            self.cleaned()
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
            process.communicate(timeout=15)

    def assert_retired(self, pids):
        for pid in pids:
            deadline = time.monotonic() + 5
            while self.alive(pid):
                self.assertLess(
                    time.monotonic(), deadline, f"owned process survived: {pid}"
                )
                time.sleep(0.01)

    @staticmethod
    def alive(pid):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        proc = Path(f"/proc/{pid}/stat")
        if proc.exists():
            return proc.read_text().rsplit(")", 1)[1].strip().split()[0] != "Z"
        result = subprocess.run(
            ["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True
        )
        return bool(result.stdout.strip()) and not result.stdout.strip().startswith("Z")


if __name__ == "__main__":
    unittest.main(verbosity=2)
