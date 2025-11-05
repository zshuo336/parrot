"""Test production control flow with real subprocesses and listening descendants.

Only component commands/preflight are substituted. Actual polyglot business is
separately exercised by run-lab.sh; these fixtures do not certify that business.
"""
import json
import os
from pathlib import Path
import shlex
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest

FIXTURE = Path(__file__).with_name('launcher_fixture.py')
LAUNCHER = FIXTURE.parents[1] / 'deploy/launcher.py'


class LauncherContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='crawler-owner-test-', dir='/tmp')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        for name, role in [('app', 'app'), ('npm', 'npm'), ('node', 'node')]:
            path = self.bin / name
            path.write_text('#!/bin/sh\nexec ' + shlex.quote(sys.executable) + ' ' +
                            shlex.quote(str(FIXTURE)) + ' ' + role + ' "$@"\n')
            path.chmod(0o700)
        self.children = []
        self.runs = []
        self.addCleanup(self.cleanup)

    def cleanup(self):
        for process, _, _ in self.children:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=15)
        # A failed assertion must still clean up any test-owned process group.
        # Never signal an old stored PGID: EOF to its live guardian owns cleanup.
        for directory in self.runs:
            self.retired(directory)

    def start(self, case='success', *, mode='direct', args=(), extra=None):
        directory = self.root / str(len(self.runs))
        directory.mkdir()
        self.runs.append(directory)
        environment = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
                           LAB_RUN_DIR=str(directory / 'run'), LAB_MODE=mode,
                           LAB_STARTUP_TIMEOUT='3', LAB_TIMEOUT='12', LAUNCHER_CASE=case,
                           LAUNCHER_APP=str(self.bin / 'app'), LAUNCHER_RECORDS=str(directory))
        if extra:
            environment.update(extra)
        process = subprocess.Popen([sys.executable, str(FIXTURE), 'driver', *args],
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   env=environment, text=True)
        self.children.append((process, directory, environment))
        return process, directory

    def finish(self, process, directory, code=0):
        output, _ = process.communicate(timeout=20)
        self.assertEqual(process.returncode, code, output)
        self.assertEqual('CRAWLER_LAB_LAUNCHER_PASS' in output, code == 0, output)
        result = json.loads((directory / 'run/result.json').read_text())
        self.assertEqual(result['exit_code'], code)
        self.assertTrue(all(child['retired'] for child in result['children']))
        self.retired(directory)
        return output, result

    def records(self, directory):
        return [json.loads(path.read_text()) for path in directory.glob('*.json')]

    def retired(self, directory):
        for record in self.records(directory):
            deadline = time.monotonic() + 8
            while True:
                try:
                    with socket.socket() as sock:
                        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                        sock.bind(('127.0.0.1', record['port']))
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        self.fail('descendant still owns listener: ' + str(record))
                    time.sleep(0.03)

    def ready(self, directory):
        deadline = time.monotonic() + 5
        while len(self.records(directory)) != 3:
            self.assertLess(time.monotonic(), deadline, 'gateways did not start')
            time.sleep(0.03)
        # Production writes LAB_NODES only after consuming all ready events;
        # the fixture app's log exists only after application spawn.
        while not (directory / 'run/app.log').exists():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.03)

    def test_two_concurrent_runs_have_distinct_ports_and_logs(self):
        first = self.start()
        second = self.start()
        for process, directory in [first, second]:
            _, result = self.finish(process, directory)
            self.assertEqual(result['ts'], 'passed')
        ports = [record['port'] for _, directory in [first, second] for record in self.records(directory)]
        self.assertEqual(len(ports), 6)
        self.assertEqual(len(set(ports)), 6)

    def test_registry_reports_four_runtimes_and_real_app_port(self):
        process, directory = self.start(mode='registry')
        _, result = self.finish(process, directory)
        self.assertEqual(result['ts'], 'unavailable-in-registry')
        self.assertGreater(result['children'][0]['port'], 0)

    def test_python_gateway_cannot_move_away_from_its_session_owner(self):
        _, result = self.finish(*self.start('inline'))
        self.assertTrue(all(child['retired'] for child in result['children']))

    def test_sigkill_with_fatal_runtime_term_handler_still_retires_ignoring_descendant(self):
        process, directory = self.start('inline-signal')
        self.ready(directory)
        process.kill()
        output, _ = process.communicate(timeout=10)
        self.assertEqual(process.returncode, -signal.SIGKILL)
        self.assertNotIn('CRAWLER_LAB_LAUNCHER_PASS', output)
        self.retired(directory)

    def test_explicit_ts_skip_is_not_ts_success(self):
        process, directory = self.start(args=['--skip-ts'])
        _, result = self.finish(process, directory)
        self.assertEqual(result['ts'], 'skipped-by-request')

    def test_app_failure_overrides_even_a_pass_marker(self):
        self.finish(*self.start('app-fail'), code=23)

    def test_application_without_completed_marker_is_failure(self):
        self.finish(*self.start('no-pass'), code=1)

    def test_ts_build_failure_is_failure(self):
        self.finish(*self.start('ts-build'), code=24)

    def test_ts_query_failure_is_failure(self):
        self.finish(*self.start('ts-fail'), code=25)

    def test_ts_without_actual_query_is_failure(self):
        self.finish(*self.start('ts-no-query'), code=1)

    def test_partial_start_failure_retires_previous_groups(self):
        self.finish(*self.start('partial'), code=1)

    def test_invalid_readiness_is_failure(self):
        self.finish(*self.start('bad'), code=1)

    def test_startup_deadline_retires_silent_gateways(self):
        self.finish(*self.start('never', extra={'LAB_STARTUP_TIMEOUT': '0.5'}), code=1)

    def test_workload_deadline_retires_all_groups(self):
        self.finish(*self.start('timeout', extra={'LAB_TIMEOUT': '1'}), code=1)

    def test_gateway_exit_is_detected_even_if_descendant_holds_stdout(self):
        output, _ = self.finish(*self.start('crash'), code=1)
        self.assertIn('exited before the scenario completed: 8', output)

    def test_final_output_is_drained_before_success(self):
        output, _ = self.finish(*self.start('tail'))
        self.assertIn('crawler-lab PASS（四运行时全链集成）', output)

    def test_term_and_repeated_int_retire_descendants(self):
        for signum in [signal.SIGTERM, signal.SIGINT]:
            process, directory = self.start('signal')
            self.ready(directory)
            process.send_signal(signum)
            time.sleep(0.2)
            process.send_signal(signum)
            self.finish(process, directory, code=128 + signum)

    def test_sigkill_control_eof_retires_descendants_and_preserves_unrelated_process(self):
        unrelated = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
        try:
            process, directory = self.start('signal')
            self.ready(directory)
            process.kill()
            output, _ = process.communicate(timeout=10)
            self.assertEqual(process.returncode, -signal.SIGKILL)
            self.assertNotIn('CRAWLER_LAB_LAUNCHER_PASS', output)
            self.retired(directory)
            self.assertIsNone(unrelated.poll())
        finally:
            unrelated.terminate()
            unrelated.wait(timeout=5)

    def test_existing_directory_is_never_modified(self):
        existing = self.root / 'existing'
        existing.mkdir()
        (existing / 'result.json').write_text('user owned result\n')
        process, directory = self.start(extra={'LAB_RUN_DIR': str(existing)})
        output, _ = process.communicate(timeout=5)
        self.assertNotEqual(process.returncode, 0, output)
        self.assertEqual((existing / 'result.json').read_text(), 'user owned result\n')
        self.assertEqual(list(existing.iterdir()), [existing / 'result.json'])
        self.assertEqual(self.records(directory), [])

    def test_invalid_options_and_help_have_no_run_directory_or_children(self):
        cases = [(['--help'], {}, 0), (['--pages', '-1'], {}, 2),
                 (['--batch', '0'], {}, 2), (['--wait', '3'], {}, 2),
                 (['--skip-ts', '--skip-ts'], {}, 2), (['--pag', '2'], {}, 2),
                 ([], {'LAB_MODE': 'typo'}, 2), ([], {'LAB_TIMEOUT': 'nan'}, 2)]
        digest = 'a' * 64
        cases.extend((options, {}, 2) for options in [
            ['--hub-dylib', 'x'], ['--hub-wasm', 'x'],
            ['--trust-native-sha256', digest], ['--hub-sha256', digest],
            ['--hub-dylib', 'x', '--hub-sha256', digest],
            ['--hub-wasm', 'x', '--hub-sha256', 'xyz'],
            ['--hub-dylib', '', '--trust-native-sha256', digest],
            ['--hub-dylib', 'x', '--trust-native-sha256', digest, '--hub-wasm', 'y', '--hub-sha256', digest],
        ])
        for args, extra, code in cases:
            process, directory = self.start(args=args, extra=extra)
            output, _ = process.communicate(timeout=5)
            self.assertEqual(process.returncode, code, output)
            self.assertFalse((directory / 'run').exists())
            self.assertEqual(self.records(directory), [])


if __name__ == '__main__':
    unittest.main()
