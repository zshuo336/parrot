"""Failure-path checks for the integration harness (no language runtimes needed)."""
from contextlib import ExitStack
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from run_integration import Corpus, Process, Suite, require, until
from owned_process import _exit_observer


class RunnerContracts(unittest.TestCase):
    def test_owned_group_cleanup_preserves_unrelated_process(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            outsider = Process(root, "outsider", [sys.executable, "-c", "import time; time.sleep(60)"])
            marker = root / "ready"
            owner = Process(root, "owner", [sys.executable, "-c", """
import os, signal, subprocess, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
open(sys.argv[1], 'w').write(str(child.pid))
time.sleep(60)
""", marker])
            try:
                until(marker.exists, [owner], seconds=5)
                self.assertEqual(os.getpgid(int(marker.read_text())), owner.child.pid)
                descendant = int(marker.read_text())
                owner.stop(grace=0.1)
                self.assertIsNotNone(owner.child.returncode)

                def descendant_exited():
                    state = subprocess.run(
                        ["ps", "-p", str(descendant), "-o", "stat="],
                        capture_output=True, text=True, timeout=5,
                    ).stdout.strip()
                    # A zombie has exited; its OS parent is responsible for reap.
                    return not state or state.startswith("Z")

                until(descendant_exited, seconds=5)
                outsider.alive()
                owner.stop()  # idempotent; does not signal a later process
            finally:
                owner.stop(grace=0.1)
                outsider.stop(grace=0.1)

    def test_readiness_failure_retains_exited_child_identity_until_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            process = Process(Path(directory), "failed", [sys.executable, "-c", "raise SystemExit(7)"])
            try:
                process.child.wait(timeout=5)
                with self.assertRaisesRegex(AssertionError, "exited 7"):
                    process.alive()
                self.assertEqual(_exit_observer()(process.child.pid), 7)
            finally:
                # A failing old implementation may already have reaped it.
                # Never send a group signal after independent reaping.
                if not isinstance(process.child, subprocess.Popen):
                    process.stop()

    def test_stop_signals_group_before_reaping_and_never_signals_it_again(self):
        with tempfile.TemporaryDirectory() as directory:
            process = Process(Path(directory), "done", [sys.executable, "-c", "raise SystemExit(19)"])
            process.child.wait(timeout=5)
            real_killpg = os.killpg

            def checked(pgid, sig):
                self.assertEqual((pgid, sig), (process.child.pid, signal.SIGKILL))
                self.assertEqual(_exit_observer()(pgid), 19)
                return real_killpg(pgid, sig)

            try:
                with patch("owned_process.os.killpg", side_effect=checked) as kill:
                    process.stop()
                    process.stop()
                self.assertEqual(kill.call_count, 1)
                with self.assertRaises(ChildProcessError):
                    _exit_observer()(process.child.pid)
            finally:
                if not isinstance(process.child, subprocess.Popen):
                    process.stop()

    def test_failed_cleanup_is_retryable_without_losing_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            process = Process(Path(directory), "done", [sys.executable, "-c", "raise SystemExit(23)"])
            process.child.wait(timeout=5)
            try:
                with patch("owned_process.os.killpg", side_effect=PermissionError("denied")), \
                        patch("owned_process._darwin_only_zombie_leader", return_value=False):
                    with self.assertRaises(PermissionError):
                        process.stop()
                self.assertFalse(process.stopped)
                self.assertEqual(_exit_observer()(process.child.pid), 23)
                process.stop()
                self.assertTrue(process.stopped)
            finally:
                if not isinstance(process.child, subprocess.Popen):
                    process.stop()

    def test_graceful_owner_shutdown_and_closed_stdin(self):
        with tempfile.TemporaryDirectory() as directory:
            process = Process(Path(directory), "graceful", [sys.executable, "-c", """
import signal, sys, time
if sys.stdin.read() != '': raise SystemExit(9)
def stop(*args):
    print('owner-cleanup-complete', flush=True)
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
print('ready', flush=True)
while True: time.sleep(1)
"""])
            try:
                until(lambda: "ready" in process.text(), [process], seconds=5)
                process.stop(grace=5)
                self.assertEqual(process.child.returncode, 0)
                self.assertIn("owner-cleanup-complete", process.text())
            finally:
                if not isinstance(process.child, subprocess.Popen):
                    process.stop(grace=0.1)

    def test_dead_process_fails_readiness_even_if_probe_would_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            process = Process(Path(directory), "failed", [sys.executable, "-c", "raise SystemExit(7)"])
            try:
                process.child.wait(timeout=5)
                with self.assertRaisesRegex(AssertionError, "exited 7"):
                    until(lambda: True, [process], seconds=1)
            finally:
                process.stop()

    def test_wrong_documents_cannot_pass_even_with_nonempty_results(self):
        suite = object.__new__(Suite)
        suite.token = "unique"
        suite.corpus = type("Fixture", (), {"base": "http://127.0.0.1:1234"})()
        with patch("run_integration.results", return_value=(["http://wrong.test/"], ["0.42"])):
            with self.assertRaisesRegex(AssertionError, "search URLs"):
                suite.assert_search(1, ["/root"])
        with patch("run_integration.results", return_value=([suite.corpus.base + "/root"] * 2, ["0.42"] * 2)):
            with self.assertRaises(AssertionError):
                suite.assert_search(1, ["/root"])

    def test_automatic_application_port_uses_the_owned_listener(self):
        import socket
        with tempfile.TemporaryDirectory() as directory, ExitStack() as stack:
            root = Path(directory)
            executable = root / "application"
            executable.write_text("#!" + sys.executable + "\n" + """
import socket, sys
port = int(sys.argv[sys.argv.index('--port') + 1])
if port != 0: raise SystemExit(9)
with socket.socket() as listener:
    listener.bind(('127.0.0.1', port)); listener.listen()
    print('PARROT_WEBSEARCH_PORT=' + str(listener.getsockname()[1]), flush=True)
    connection, _ = listener.accept()
    with connection: connection.sendall(b'owned-listener')
""")
            executable.chmod(0o700)
            suite = Suite(root, executable, stack)
            with patch("run_integration.available_port", side_effect=AssertionError("released-port race")):
                app, port = suite.app(root / "data", {})
            self.assertGreater(port, 0)
            with socket.create_connection(('127.0.0.1', port), timeout=5) as client:
                self.assertEqual(client.recv(64), b'owned-listener')
            self.assertEqual(app.child.wait(timeout=5), 0)

    def test_automatic_port_preserves_startup_failure_without_waiting_for_readiness(self):
        with tempfile.TemporaryDirectory() as directory, ExitStack() as stack:
            root = Path(directory)
            executable = root / "application"
            executable.write_text("#!" + sys.executable + "\nraise SystemExit(7)\n")
            executable.chmod(0o700)
            suite = Suite(root, executable, stack)
            app, port = suite.app(root / "data", {})
            self.assertEqual(port, 0)
            self.assertEqual(app.child.wait(timeout=5), 7)
            with self.assertRaisesRegex(AssertionError, "exited 7"):
                app.alive()

    def test_proxy_rejects_outside_corpus_and_records_it(self):
        import urllib.request
        import urllib.error
        with ExitStack() as stack:
            corpus = Corpus(stack, "testtoken")
            # Direct request with absolute target has the same HTTP proxy shape.
            import http.client
            client = http.client.HTTPConnection("127.0.0.1", corpus.port)
            stack.callback(client.close)
            client.request("GET", "http://outside.invalid/unexpected")
            self.assertEqual(client.getresponse().status, 502)
            with self.assertRaisesRegex(AssertionError, "unexpected/outbound"):
                corpus.assert_requests({})

    def test_assertions_remain_active_under_python_optimization(self):
        # Assertions intentionally use require(), not the stripped assert opcode.
        with self.assertRaisesRegex(AssertionError, "business mismatch"):
            require(False, "business mismatch")


if __name__ == "__main__":
    unittest.main()
