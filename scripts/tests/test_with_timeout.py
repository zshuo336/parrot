"""Actual deadline, exit-status and owned descendant contracts for the CLI wrapper."""
from pathlib import Path
import errno
import os
import select
import signal
import socket
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
WRAPPER = ROOT / 'scripts/with_timeout.sh'


def loopback_bind_available():
    try:
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
        return True
    except OSError as error:
        if error.errno in (errno.EPERM, errno.EACCES):
            return False
        raise


class TimeoutContracts(unittest.TestCase):
    def command(self, duration, source, *args):
        return ['bash', str(WRAPPER), str(duration), sys.executable, '-c', source, *map(str, args)]

    def invoke(self, duration, source, *args):
        return subprocess.run(self.command(duration, source, *args),
                              capture_output=True, text=True, timeout=15)

    def test_ordinary_exit_status_is_preserved(self):
        for code in [0, 23, 124]:
            with self.subTest(code=code):
                result = self.invoke(5, f'raise SystemExit({code})')
                self.assertEqual(result.returncode, code, result.stderr)
                self.assertNotIn('timed out', result.stderr)

    def test_native_signal_is_not_misreported_as_deadline_expiry(self):
        for sig in [signal.SIGTERM, signal.SIGKILL]:
            with self.subTest(signal=sig):
                result = self.invoke(5, f'import os; os.kill(os.getpid(), {int(sig)})')
                self.assertEqual(result.returncode, 128 + sig, result.stderr)
                self.assertNotIn('timed out', result.stderr)

    def test_deadline_remains_failure_when_term_handler_exits_successfully(self):
        result = self.invoke(1, '''import signal, sys, time
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
print('READY', flush=True)
time.sleep(60)
''')
        self.assertIn('READY', result.stdout)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn('timed out', result.stderr)

    def test_ignored_term_is_killed_and_output_pipe_closes(self):
        result = self.invoke(1, '''import signal, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
print('READY', flush=True)
time.sleep(60)
''')
        self.assertIn('READY', result.stdout)
        self.assertEqual(result.returncode, 124, result.stderr)

    @unittest.skipUnless(loopback_bind_available(),
                         'loopback listeners are unavailable in this environment')
    def test_successful_leader_still_retires_pipe_holding_descendant(self):
        server = '''import socket, time
s = socket.socket()
s.bind(('127.0.0.1', 0))
s.listen()
print(s.getsockname()[1], flush=True)
time.sleep(60)
'''
        result = self.invoke(5, '''import subprocess, sys
child = subprocess.Popen([sys.executable, '-c', sys.argv[1]], stdout=subprocess.PIPE, text=True)
print(child.stdout.readline().strip(), flush=True)
''', server)
        self.assertEqual(result.returncode, 0, result.stderr)
        port = int(result.stdout.strip())
        with self.assertRaises(ConnectionRefusedError):
            with socket.create_connection(('127.0.0.1', port), timeout=2):
                self.fail('descendant service survived successful leader retirement')

    def test_invalid_deadlines_never_launch_command(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'launched'
            for duration in ['nan', 'inf', '-1', 'invalid']:
                with self.subTest(duration=duration):
                    result = self.invoke(duration, 'from pathlib import Path; import sys; Path(sys.argv[1]).touch()', marker)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertFalse(marker.exists())

    def test_wrapper_termination_retires_child_and_preserves_signal_status(self):
        with subprocess.Popen(self.command(60, '''import time
print('READY', flush=True)
time.sleep(60)
'''), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as wrapper:
            try:
                self.assertTrue(select.select([wrapper.stdout], [], [], 10)[0], 'child never became ready')
                self.assertEqual(wrapper.stdout.readline().strip(), 'READY')
                os.kill(wrapper.pid, signal.SIGTERM)
                _, stderr = wrapper.communicate(timeout=10)
                self.assertEqual(wrapper.returncode, 143, stderr)
            finally:
                if wrapper.poll() is None:
                    wrapper.kill()


if __name__ == '__main__':
    unittest.main()
