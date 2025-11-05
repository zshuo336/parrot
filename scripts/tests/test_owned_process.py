"""Real-process contracts for the Ray acceptance harness's cleanup ownership."""
import json
import errno
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "apps/test_support"))
from owned_process import OwnedProcess, _exit_observer, _darwin_only_zombie_leader, process_exited


class OwnedProcessTests(unittest.TestCase):
    def start(self, source, *args):
        child = OwnedProcess([sys.executable, "-c", source, *map(str, args)],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(child.close)
        return child

    def test_normal_and_native_exit_are_observed_repeatedly_before_reaping(self):
        for source, expected in [("pass", 0), ("raise SystemExit(17)", 17),
                                 ("import os;os._exit(23)", 23)]:
            with self.subTest(source=source):
                child = self.start(source)
                self.assertEqual(child.wait(5), expected)
                for _ in range(3):
                    self.assertEqual(child.poll(), expected)
                    # Independent kernel observation: cached Popen.returncode
                    # cannot make a reaping implementation pass this contract.
                    self.assertEqual(_exit_observer()(child.pid), expected)
                if sys.platform == 'darwin':
                    self.assertTrue(_darwin_only_zombie_leader(child.pid))
                child.close()
                self.assertEqual(child.returncode, expected)
                with self.assertRaises(ChildProcessError):
                    os.waitpid(child.pid, os.WNOHANG)

    def test_signal_exit_is_native_negative_status(self):
        child = self.start("import os,signal;os.kill(os.getpid(),signal.SIGTERM)")
        self.assertEqual(child.wait(5), -signal.SIGTERM)
        self.assertEqual(_exit_observer()(child.pid), -signal.SIGTERM)

    def test_read_only_exit_observer_includes_unreaped_zombie(self):
        child = self.start("import time;time.sleep(60)")
        self.assertFalse(process_exited(child.pid))
        child.send_signal(signal.SIGTERM)
        self.assertEqual(child.wait(5), -signal.SIGTERM)
        self.assertTrue(process_exited(child.pid))
        self.assertEqual(_exit_observer()(child.pid), -signal.SIGTERM)
        child.close()
        self.assertTrue(process_exited(child.pid))

    def test_timeout_keeps_live_owner_for_later_signal_and_cleanup(self):
        child = self.start("import time;time.sleep(60)")
        with self.assertRaises(subprocess.TimeoutExpired):
            child.wait(.05)
        self.assertIsNone(child.poll())
        child.send_signal(signal.SIGTERM)
        self.assertEqual(child.wait(5), -signal.SIGTERM)
        with patch("owned_process.os.kill") as kill:
            child.send_signal(signal.SIGTERM)
        kill.assert_not_called()

    def test_live_child_is_killed_and_reaped_on_close(self):
        child = self.start("import time;time.sleep(60)")
        child.close()
        self.assertTrue(child.retired)
        self.assertEqual(child.poll(), -signal.SIGKILL)
        with self.assertRaises(ChildProcessError):
            os.waitpid(child.pid, os.WNOHANG)

    def test_last_group_signal_precedes_reap_and_close_is_idempotent(self):
        child = self.start("raise SystemExit(19)")
        self.assertEqual(child.wait(5), 19)
        real_killpg = os.killpg

        def checked(pgid, sig):
            self.assertEqual((pgid, sig), (child.pid, signal.SIGKILL))
            self.assertEqual(_exit_observer()(pgid), 19)
            return real_killpg(pgid, sig)

        with patch("owned_process.os.killpg", side_effect=checked) as kill:
            child.close()
            child.close()
        self.assertEqual(kill.call_count, 1)

    def test_signal_failure_preserves_identity_for_retry(self):
        child = self.start("import time;time.sleep(60)")
        with patch("owned_process.os.killpg", side_effect=PermissionError("denied")):
            with self.assertRaises(PermissionError):
                child.close()
        self.assertFalse(child.retired)
        self.assertIsNone(_exit_observer()(child.pid))
        child.close()

    def test_external_reaping_fails_closed_without_signalling_old_group(self):
        child = OwnedProcess([sys.executable, "-c", "pass"])
        self.assertEqual(child._process.wait(timeout=5), 0)
        with patch("owned_process.os.killpg") as kill:
            with self.assertRaises(ChildProcessError):
                child.close()
        kill.assert_not_called()

    def test_ignored_sigchld_is_rejected_before_launch(self):
        with patch("owned_process.signal.getsignal", return_value=signal.SIG_IGN), \
                patch("owned_process.subprocess.Popen") as popen:
            with self.assertRaisesRegex(RuntimeError, "SIGCHLD"):
                OwnedProcess([sys.executable, "-c", "pass"])
        popen.assert_not_called()

    def test_unavailable_observer_is_rejected_before_launch(self):
        with patch("owned_process._exit_observer", side_effect=NotImplementedError), \
                patch("owned_process.subprocess.Popen") as popen:
            with self.assertRaises(NotImplementedError):
                OwnedProcess([sys.executable, "-c", "pass"])
        popen.assert_not_called()

    def test_context_retires_even_when_receipt_write_fails(self):
        child = self.start("import time;time.sleep(60)")
        with self.assertRaisesRegex(OSError, 'receipt'):
            with child:
                raise OSError('receipt write failed')
        self.assertTrue(child.retired)
        self.assertEqual(child.returncode, -signal.SIGKILL)

    def test_group_permission_failure_with_unknown_members_is_not_ignored(self):
        child = self.start("raise SystemExit(23)")
        self.assertEqual(child.wait(5), 23)
        with patch("owned_process.os.killpg", side_effect=PermissionError("denied")), \
                patch("owned_process._darwin_only_zombie_leader", return_value=False):
            with self.assertRaises(PermissionError):
                child.close()
        self.assertFalse(child.retired)
        self.assertEqual(_exit_observer()(child.pid), 23)

    def test_native_crash_leaves_descendant_unassisted_until_explicit_close(self):
        # A live descendant must keep serving after the driver crashes. A
        # watchdog that kills it on driver exit would falsify recovery tests.
        server = """import os,socket
s=socket.socket();s.bind(('127.0.0.1',0));s.listen()
print(s.getsockname()[1],flush=True)
c,_=s.accept()
while True:
 data=c.recv(1024)
 if not data:break
 c.sendall(data)
"""
        driver = """import json,os,subprocess,sys
from pathlib import Path
p=subprocess.Popen([sys.executable,'-c',sys.argv[1]],stdout=subprocess.PIPE,text=True)
port=int(p.stdout.readline())
Path(sys.argv[2]).write_text(json.dumps(dict(port=port,pid=p.pid,pgid=os.getpgrp())))
os._exit(23)
"""
        unrelated = self.start("import time;time.sleep(60)")
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / "ready.json"
            child = self.start(driver, server, receipt)
            self.assertEqual(child.wait(5), 23)
            data = json.loads(receipt.read_text())
            self.assertEqual(data["pgid"], child.pid)
            self.assertEqual(os.getpgid(data["pid"]), child.pid)
            if sys.platform == 'darwin':
                self.assertFalse(_darwin_only_zombie_leader(child.pid))
            with socket.create_connection(("127.0.0.1", data["port"]), timeout=5) as client:
                client.sendall(b"alive after native exit")
                self.assertEqual(client.recv(1024), b"alive after native exit")
                child.close()
                self.assertEqual(client.recv(1), b"")
            self.assertIsNone(unrelated.poll())
            self.assertEqual(child.returncode, 23)


class ProcessExitObservationTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == 'linux', 'requires Linux procfs read semantics')
    def test_real_procfs_read_after_child_reaping_is_exit(self):
        # Keep a real procfs file open, then retire our own child before its
        # read. The kernel produces ESRCH; no exception outcome is fabricated.
        child = subprocess.Popen([sys.executable, '-c',
            "import signal;print('ready',flush=True);signal.pause()"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        original_read = Path.read_text
        observed = []

        def read(path, *args, **kwargs):
            if path != Path(f'/proc/{child.pid}/stat'):
                return original_read(path, *args, **kwargs)
            with path.open('r') as stat:
                child.terminate()
                child.wait(timeout=5)
                try:
                    return stat.read()
                except ProcessLookupError as error:
                    observed.append(error.errno)
                    raise

        try:
            self.assertEqual(child.stdout.readline(), 'ready\n')
            with patch('owned_process.Path.read_text', autospec=True, side_effect=read):
                self.assertTrue(process_exited(child.pid))
            self.assertEqual(observed, [errno.ESRCH])
        finally:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
            child.stdout.close()
            child.stderr.close()

    def observe_linux_stat(self, result):
        pid = 712345

        def read(path, *args, **kwargs):
            self.assertEqual(path, Path(f'/proc/{pid}/stat'))
            if isinstance(result, BaseException):
                raise result
            return result

        def procfs(path):
            self.assertEqual(path, Path('/proc/self/stat'))
            return True

        with patch('owned_process.sys.platform', 'linux'), \
                patch('owned_process.os.kill') as kill, \
                patch('owned_process.os.waitpid', side_effect=AssertionError('observation must not reap')), \
                patch('owned_process.Path.is_file', autospec=True, side_effect=procfs), \
                patch('owned_process.Path.read_text', autospec=True, side_effect=read) as stat:
            try:
                return process_exited(pid)
            finally:
                kill.assert_called_once_with(pid, 0)
                stat.assert_called_once()

    def test_stat_disappearance_after_liveness_probe_is_exit(self):
        # procfs can return ESRCH from read(), after open()/kill(0) succeeded;
        # ENOENT alone does not cover a process disappearing during the read.
        for error in [FileNotFoundError(errno.ENOENT, 'gone'),
                      ProcessLookupError(errno.ESRCH, 'gone during stat read')]:
            with self.subTest(error=type(error).__name__):
                self.assertTrue(self.observe_linux_stat(error))

    def test_other_stat_errors_do_not_prove_exit(self):
        for error in [PermissionError(errno.EACCES, 'denied'), OSError(errno.EIO, 'read failed'),
                      InterruptedError(errno.EINTR, 'interrupted'),
                      NotADirectoryError(errno.ENOTDIR, 'invalid procfs path')]:
            with self.subTest(error=type(error).__name__):
                with self.assertRaises(type(error)) as raised:
                    self.observe_linux_stat(error)
                self.assertIs(raised.exception, error)

    def test_live_and_zombie_states_with_parentheses_in_comm(self):
        for state in ['R', 'S', 'D', 'T', 't', 'I', 'Z', 'X']:
            with self.subTest(state=state):
                stat = f'712345 (worker ) name) {state} 1 2 3 4 5\n'
                self.assertEqual(self.observe_linux_stat(stat), state in ('Z', 'X'))

    def test_procfs_absence_is_not_successful_exit_observation(self):
        with patch('owned_process.sys.platform', 'linux'), \
                patch('owned_process.os.kill') as kill, \
                patch('owned_process.Path.is_file', return_value=False), \
                patch('owned_process.Path.read_text') as stat:
            with self.assertRaisesRegex(RuntimeError, 'requires procfs'):
                process_exited(712345)
            kill.assert_called_once_with(712345, 0)
            stat.assert_not_called()

    def test_missing_liveness_probe_needs_no_procfs_read(self):
        with patch('owned_process.os.kill', side_effect=ProcessLookupError(errno.ESRCH, 'gone')) as kill, \
                patch('owned_process.Path.is_file') as procfs, \
                patch('owned_process.Path.read_text') as stat:
            self.assertTrue(process_exited(712345))
            kill.assert_called_once_with(712345, 0)
            procfs.assert_not_called()
            stat.assert_not_called()

    def test_denied_liveness_probe_is_not_evidence_of_exit(self):
        error = PermissionError(errno.EPERM, 'probe denied')
        with patch('owned_process.os.kill', side_effect=error) as kill, \
                patch('owned_process.Path.is_file') as procfs, \
                patch('owned_process.Path.read_text') as stat:
            with self.assertRaises(PermissionError) as raised:
                process_exited(712345)
            self.assertIs(raised.exception, error)
            kill.assert_called_once_with(712345, 0)
            procfs.assert_not_called()
            stat.assert_not_called()


if __name__ == "__main__":
    unittest.main()
