"""Exercise real processes, sockets and cleanup through the production launcher."""
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


class LauncherContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='ws-owner-test-', dir='/tmp')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.app = self.root/'app'
        self.app.write_text('#!/bin/sh\nexec '+shlex.quote(sys.executable)+' '+shlex.quote(str(FIXTURE))+' app "$@"\n')
        self.app.chmod(0o700)
        self.children = []
        self.addCleanup(self.cleanup)

    def cleanup(self):
        for child, directory in self.children:
            if child.poll() is None: child.kill()
            child.communicate(timeout=15)
            self.retired(directory)

    def start(self, case='success', mode='direct', arguments=(), extra=None):
        directory = self.root/str(len(self.children))
        directory.mkdir()
        env = dict(os.environ, WS_TEST_CASE=case, WS_TEST_RECORDS=str(directory), WS_TEST_APP=str(self.app),
                   WS_RUN_DIR=str(directory/'run'), WS_STARTUP_TIMEOUT='3', WS_TIMEOUT='10')
        # Test configuration is independent of an ambient launcher session.
        for name in ['WS_SKIP_BUILD', 'WS_BIN', 'WS_PROFILE', 'WS_DATA', 'WS_PORT', 'WS_SITES', 'WS_DEPTH', 'WS_RAY_HTTP_PROXY', 'WS_BUILD_TIMEOUT']:
            env.pop(name, None)
        env.update(extra or {})
        child = subprocess.Popen([sys.executable, FIXTURE, 'driver', mode, *arguments], env=env,
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.children.append((child, directory))
        return child, directory

    def retired(self, directory):
        for path in directory.glob('*.json'):
            if path.name == 'app-args.json': continue
            record = json.loads(path.read_text())
            end = time.monotonic()+8
            while True:
                try:
                    with socket.socket() as sock: sock.bind(('127.0.0.1', record['port']))
                    break
                except OSError:
                    if time.monotonic() >= end: self.fail(f'owned descendant still holds listener: {record}')
                    time.sleep(0.03)
            state = subprocess.run(['ps', '-p', str(record['descendant']), '-o', 'stat='], capture_output=True, text=True, timeout=3).stdout.strip()
            self.assertTrue(not state or state.startswith('Z'), f'live descendant: {record}')

    def finish(self, child, directory, code=0):
        output, _ = child.communicate(timeout=20)
        self.assertEqual(child.returncode, code, output)
        result = json.loads((directory/'run/result.json').read_text())
        self.assertEqual(result['exit_code'], code)
        self.assertTrue(all(c['retired'] and c['guardian_exit'] is not None for c in result['children']))
        self.retired(directory)
        return output, result

    def ready(self, child, directory):
        end = time.monotonic()+5
        while time.monotonic() < end:
            self.assertIsNone(child.poll())
            if (directory/'app-args.json').exists() and len(list(directory.glob('*.json'))) >= 4:
                time.sleep(0.3)
                return
            time.sleep(0.02)
        self.fail('fixture did not start all owned processes')

    def test_failed_build_never_starts_validation_application_or_gateways(self):
        child, directory = self.start('build-fails')
        output, result = self.finish(child, directory, 17)
        self.assertEqual([c['name'] for c in result['children']], ['build'])
        self.assertFalse((directory/'app-args.json').exists())
        self.assertNotIn('WEBSEARCH_READY=', output)

    def test_dead_application_logs_do_not_establish_readiness(self):
        output, result = self.finish(*self.start('dead-ready'), code=1)
        self.assertFalse(result['ready'])
        self.assertNotIn('WEBSEARCH_READY=', output)

    def test_bad_or_trickling_http_cannot_pass_or_escape_startup_deadline(self):
        for case in ['bad-health', 'trickle']:
            output, result = self.finish(*self.start(case, extra={'WS_STARTUP_TIMEOUT':'0.5'}), code=1)
            self.assertFalse(result['ready'])
            self.assertNotIn('WEBSEARCH_READY=', output)

    def test_direct_and_registry_keep_literal_arguments_and_exact_data_path(self):
        data = self.root/"data space ' $(not-a-command) 中文"
        remote = '/gateway/remote "quote" \\ 中文'
        for mode in ['direct', 'registry']:
            flags = ['--insecure-development'] if mode == 'registry' else []
            child, directory = self.start(mode=mode, arguments=[*flags, '--data', str(data), '--index-data', remote, '--port', '0', 'https://example.test/中文?q=$x'])
            output, result = self.finish(child, directory)
            self.assertTrue(result['ready'])
            self.assertIn('WEBSEARCH_READY=', output)
            argv = json.loads((directory/'app-args.json').read_text())
            self.assertEqual(argv[argv.index('--data')+1], str(data))
            self.assertEqual(argv[argv.index('--index-data')+1], remote)
            self.assertIn('https://example.test/中文?q=$x', argv)
            self.assertEqual('--insecure-development' in argv, mode == 'registry')

    def test_gateway_failure_and_application_failure_propagate(self):
        self.finish(*self.start('gateway-crash'), code=8)
        self.finish(*self.start('app-fails'), code=23)

    def test_option_values_are_not_gateway_overrides_or_help(self):
        for value in ['jvm=index', 'erl=index', 'ray=index', '-h']:
            child, directory = self.start(arguments=['--data', value, '--', 'https://example.test/'])
            _, result = self.finish(child, directory)
            self.assertTrue(result['ready'])
            argv = json.loads((directory/'app-args.json').read_text())
            self.assertEqual(argv[argv.index('--data')+1], str(Path(value).absolute()))
            self.assertEqual(argv[-2:], ['--', 'https://example.test/'])

    def test_real_gateway_and_bind_overrides_are_still_rejected(self):
        for args in [['jvm=127.0.0.1:1234'], ['--bind', '127.0.0.1:0'], ['--validate-only']]:
            _, result = self.finish(*self.start(arguments=args), code=2)
            self.assertEqual(result['children'], [])
            self.assertFalse(result['ready'])

    def test_signal_retires_groups_and_preserves_an_unrelated_process(self):
        outsider = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(60)'])
        try:
            child, directory = self.start('signal')
            self.ready(child, directory)
            child.send_signal(signal.SIGTERM)
            time.sleep(0.1)
            child.send_signal(signal.SIGTERM)
            self.finish(child, directory, 143)
            self.assertIsNone(outsider.poll())
        finally:
            outsider.terminate(); outsider.wait(timeout=5)

    def test_signal_drains_application_before_stopping_its_gateways(self):
        child, directory = self.start('signal-drain')
        self.ready(child, directory)
        child.send_signal(signal.SIGTERM)
        _, result = self.finish(child, directory, 143)
        self.assertEqual(json.loads((directory/'run/app-drained.json').read_text()),
                         dict(erl=True, jvm=True, ray=True))
        self.assertEqual(next(c for c in result['children'] if c['name']=='app')['exit_code'], 0)

    def test_sigkill_owner_eof_retires_including_inline_ray_descendants(self):
        child, directory = self.start('signal')
        self.ready(child, directory)
        child.kill(); child.communicate(timeout=5)
        self.retired(directory)

    def test_two_runs_have_disjoint_logs_data_and_listener_ports(self):
        a, da = self.start(); b, db = self.start(mode='registry')
        self.finish(a, da); self.finish(b, db)
        def ports(d): return {json.loads(p.read_text())['port'] for p in d.glob('*.json') if p.name != 'app-args.json'}
        self.assertFalse(ports(da) & ports(db))
        self.assertNotEqual(json.loads((da/'app-args.json').read_text()), json.loads((db/'app-args.json').read_text()))

    def test_existing_run_directory_is_preserved(self):
        directory = self.root/'existing'; directory.mkdir()
        (directory/'result.json').write_text('user data')
        child, _ = self.start(extra={'WS_RUN_DIR': str(directory)})
        output, _ = child.communicate(timeout=5)
        self.assertNotEqual(child.returncode, 0, output)
        self.assertEqual((directory/'result.json').read_text(), 'user data')

    def test_serve_only_starts_only_jvm_and_requires_explicit_data(self):
        child, directory = self.start(arguments=['--serve-only', '--data', str(self.root/'index')])
        _, result = self.finish(child, directory)
        self.assertEqual([c['name'] for c in result['children']], ['validate', 'jvm', 'app'])
        output, result = self.finish(*self.start(arguments=['--serve-only']), code=2)
        self.assertEqual(result['children'], [])
        self.assertNotIn('WEBSEARCH_READY=', output)

    def test_invalid_launcher_configuration_cannot_create_a_run(self):
        for extra in [{'WS_PROFILE':'typo'}, {'WS_SKIP_BUILD':'maybe'}, {'WS_BIN':str(self.app)},
                      {'WS_STARTUP_TIMEOUT':'nan'}, {'WS_TIMEOUT':'inf'}, {'WS_RAY_HTTP_PROXY':'invalid'}]:
            child, directory = self.start(extra=extra)
            output, _ = child.communicate(timeout=5)
            self.assertEqual(child.returncode, 2, output)
            self.assertFalse((directory/'run').exists())

    def test_cleanup_error_is_not_hidden_by_a_late_signal(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location('websearch_cleanup_test', FIXTURE.parents[1]/'deploy/launcher.py')
        module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        class BrokenChild:
            name = 'broken'
            def begin_stop(self): pass
            def finish_stop(self): raise OSError('retirement failed')
            def state(self): return {'retired':False}
        owner = module.Launcher('direct', [])
        def execute():
            owner.children.append(BrokenChild())
            owner.signum = signal.SIGTERM
        owner.execute = execute
        from unittest.mock import patch
        with patch.dict(os.environ, {'WS_RUN_DIR':str(self.root/'cleanup-run')}):
            self.assertEqual(owner.run(), 1)
        terminal = json.loads((self.root/'cleanup-run/result.json').read_text())
        self.assertIn('retirement failed', terminal['detail'])
        self.assertEqual(terminal['exit_code'], 1)


if __name__ == '__main__': unittest.main()
