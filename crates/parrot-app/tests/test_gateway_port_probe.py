"""Real socket contracts: bounded waiting must never mask a live listener."""
import errno
import importlib.util
from pathlib import Path
import socket
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('ownership', Path(__file__).with_name('run_gateway_ownership.py'))
ownership = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ownership)


class PortProbeContracts(unittest.TestCase):
    def test_exit_observations_preserve_live_command_and_exec_updates_it(self):
        observed = {}
        ownership.remember_identity(observed, 123, 1.5, ['java', 'ParrotGatewayMain'], 123)
        for unavailable in [[], ['<unavailable during process exit>']]:
            ownership.remember_identity(observed, 123, 1.5, unavailable, 123)
            self.assertEqual(observed[(123, 1.5)]['command'], ['java', 'ParrotGatewayMain'])
        ownership.remember_identity(observed, 123, 1.5, ['new-executable'], 123)
        self.assertEqual(observed[(123, 1.5)]['command'], ['new-executable'])

    def test_reused_pid_cannot_inherit_an_old_command(self):
        observed = {}
        ownership.remember_identity(observed, 123, 1.5, ['old-executable'], 123)
        ownership.remember_identity(observed, 123, 2.5, [], 123)
        self.assertEqual(len(observed), 2)
        self.assertEqual(observed[(123, 2.5)]['command'], [])
        ownership.remember_identity(observed, 123, 2.5, ['new-executable'], 123)
        self.assertEqual(observed[(123, 2.5)]['command'], ['new-executable'])
        self.assertEqual(observed[(123, 1.5)]['command'], ['old-executable'])

    def listener(self):
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.addCleanup(listener.close)
        listener.bind(('127.0.0.1', 0))
        listener.listen()
        return listener, (int(socket.AF_INET), *listener.getsockname())

    def test_live_listener_fails_even_after_retries(self):
        _, endpoint = self.listener()
        errors, history = ownership.probe_retired_listeners([endpoint], timeout=.05, interval=.005)
        self.assertEqual(len(errors), 1)
        self.assertEqual(errors[0]['errno'], errno.EADDRINUSE)
        self.assertGreaterEqual(history[0]['failed_attempts'], 1)
        self.assertNotIn('rebound_after_seconds', history[0])

    def test_observed_failure_is_retained_after_actual_release(self):
        listener, endpoint = self.listener()
        # First prove the exact listener is rejected, then release it while a
        # second bounded probe is actively retrying. No assumed TIME_WAIT cause.
        errors, _ = ownership.probe_retired_listeners([endpoint], timeout=0)
        self.assertEqual(errors[0]['errno'], errno.EADDRINUSE)
        # Control only the backoff schedule; both failed and successful binds
        # remain real kernel operations. Closing happens after EADDRINUSE.
        with mock.patch.object(ownership.time, 'sleep', side_effect=lambda _: listener.close()):
            errors, history = ownership.probe_retired_listeners([endpoint], timeout=3, interval=.005)
        self.assertEqual(errors, [])
        self.assertEqual(len(history), 1)
        self.assertGreater(history[0]['failed_attempts'], 0)
        self.assertIn('rebound_after_seconds', history[0])

    def test_non_address_in_use_error_is_terminal(self):
        # An unavailable documentation address gives an OS error independent
        # of port occupancy.
        errors, history = ownership.probe_retired_listeners([(int(socket.AF_INET), '192.0.2.123', 0)], timeout=.1)
        self.assertEqual(len(errors), 1)
        self.assertNotEqual(errors[0]['errno'], errno.EADDRINUSE)
        self.assertEqual(history[0]['failed_attempts'], 1)
        self.assertNotIn('rebound_after_seconds', history[0])

    def test_closed_listener_requires_a_successful_bind(self):
        listener, endpoint = self.listener()
        listener.close()
        errors, history = ownership.probe_retired_listeners([endpoint], timeout=0)
        self.assertEqual(errors, [])
        self.assertEqual(history, [])


if __name__ == '__main__':
    unittest.main()
