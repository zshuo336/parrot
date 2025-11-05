"""Real Ray isolation and signal cleanup. Requires the benchmark dependencies."""

import json
import hashlib
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import psutil

ROOT = Path(__file__).resolve().parents[1]


def alive(identity):
    pid, created = identity
    try:
        p = psutil.Process(pid)
        return (
            p.create_time() == created
            and p.is_running()
            and p.status() != psutil.STATUS_ZOMBIE
        )
    except psutil.NoSuchProcess:
        return False


class RealOwnership(unittest.TestCase):
    def wait_for(self, predicate, processes, seconds=90):
        deadline = time.monotonic() + seconds
        while True:
            result = predicate()
            if result:
                return result
            for p in processes:
                self.assertIsNone(p.poll(), "owned process exited before condition")
            self.assertLess(time.monotonic(), deadline, "condition deadline")
            time.sleep(0.05)

    def test_sigterm_closes_only_owned_cluster(self):
        shared_token = Path.home() / ".ray/auth_token"
        token_before = (
            hashlib.sha256(shared_token.read_bytes()).hexdigest()
            if shared_token.exists()
            else None
        )
        with tempfile.TemporaryDirectory(
            prefix="prs-", dir="/tmp"
        ) as sentinel_dir, tempfile.TemporaryDirectory(
            prefix="prt-", dir="/tmp"
        ) as test_dir:
            sentinel_root = Path(sentinel_dir)
            root = Path(test_dir)
            sentinel = None
            launcher = None
            with (root / "sentinel.log").open("w") as sentinel_log, (
                root / "benchmark.log"
            ).open("w") as benchmark_log:
                try:
                    sentinel = subprocess.Popen(
                        [
                            sys.executable,
                            str(Path(__file__).with_name("sentinel.py")),
                            str(sentinel_root),
                        ],
                        stdout=sentinel_log,
                        stderr=subprocess.STDOUT,
                        start_new_session=True,
                    )
                    self.wait_for(
                        lambda: (sentinel_root / "heartbeat.json").exists(), [sentinel]
                    )
                    initial = json.loads((sentinel_root / "heartbeat.json").read_text())
                    launcher = subprocess.Popen(
                        [
                            str(ROOT / "run.sh"),
                            "--check",
                            "--cpus",
                            "4",
                            "--object-store-bytes",
                            "134217728",
                        ],
                        env=dict(
                            os.environ, PYTHON=sys.executable, RAY_ADDRESS="127.0.0.1:1"
                        ),
                        stdout=benchmark_log,
                        stderr=subprocess.STDOUT,
                        start_new_session=True,
                    )
                    log = root / "benchmark.log"
                    self.wait_for(
                        lambda: "CONTRACT system-wide-monotonic-clock PASS"
                        in log.read_text(),
                        [sentinel, launcher],
                        120,
                    )
                    driver = int(
                        re.search(
                            r"RAY_SESSION_READY driver_pid=(\d+)", log.read_text()
                        ).group(1)
                    )
                    owned = []
                    for p in [
                        psutil.Process(driver),
                        *psutil.Process(driver).children(recursive=True),
                    ]:
                        try:
                            owned.append((p.pid, p.create_time()))
                        except psutil.NoSuchProcess:
                            pass
                    self.assertGreater(len(owned), 3, "no real Ray cluster observed")
                    launcher.send_signal(signal.SIGTERM)
                    self.assertEqual(launcher.wait(timeout=120), 143, log.read_text())
                    deadline = time.monotonic() + 30
                    while any(alive(p) for p in owned):
                        self.assertLess(
                            time.monotonic(), deadline, "owned Ray process survived"
                        )
                        time.sleep(0.05)
                    previous = json.loads(
                        (sentinel_root / "heartbeat.json").read_text()
                    )["sequence"]
                    self.wait_for(
                        lambda: json.loads(
                            (sentinel_root / "heartbeat.json").read_text()
                        )["sequence"]
                        > previous,
                        [sentinel],
                    )
                    final = json.loads((sentinel_root / "heartbeat.json").read_text())
                    self.assertGreater(final["sequence"], initial["sequence"])
                    self.assertEqual(final["identity"], initial["identity"])
                    self.assertTrue(alive(tuple(initial["identity"])))
                    token_after = (
                        hashlib.sha256(shared_token.read_bytes()).hexdigest()
                        if shared_token.exists()
                        else None
                    )
                    self.assertEqual(
                        token_before, token_after, "shared Ray credential was changed"
                    )
                    print(
                        "REAL_OWNERSHIP_PASS owned_processes="
                        + str(len(owned))
                        + " sentinel_advanced="
                        + str(final["sequence"] - initial["sequence"]),
                        flush=True,
                    )
                except BaseException:
                    print(
                        "SENTINEL LOG\n" + (root / "sentinel.log").read_text(),
                        file=sys.stderr,
                    )
                    print(
                        "BENCHMARK LOG\n" + (root / "benchmark.log").read_text(),
                        file=sys.stderr,
                    )
                    raise
                finally:
                    if launcher is not None and launcher.poll() is None:
                        launcher.send_signal(signal.SIGTERM)
                        try:
                            launcher.wait(timeout=120)
                        except subprocess.TimeoutExpired:
                            os.killpg(launcher.pid, signal.SIGKILL)
                            launcher.wait(timeout=10)
                    (sentinel_root / "stop").touch()
                    if sentinel is not None:
                        try:
                            sentinel.wait(timeout=45)
                        except subprocess.TimeoutExpired:
                            os.killpg(sentinel.pid, signal.SIGKILL)
                            sentinel.wait(timeout=10)
                    if sentinel is not None and sentinel.returncode != 0:
                        raise RuntimeError((root / "sentinel.log").read_text())


if __name__ == "__main__":
    unittest.main(verbosity=2)
