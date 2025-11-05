"""Failure and ownership contracts for real-image acceptance (no Docker needed)."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('images', Path(__file__).resolve().parents[1]/'verify_websearch_images.py')
images = importlib.util.module_from_spec(spec)
spec.loader.exec_module(images)


def result(value='', code=0, error=''):
    return subprocess.CompletedProcess([], code, value, error)


class AcceptanceContracts(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name)
        self.acceptance = images.Acceptance(self.path, 'provided:images', True)

    def test_timeout_preserves_bytes_and_text_diagnostics(self):
        for stdout, stderr in [('partial', None), (b'partial', 'error'), (None, b'error')]:
            with patch.object(images.subprocess, 'run', side_effect=subprocess.TimeoutExpired('docker', 1, stdout, stderr)):
                with self.assertRaisesRegex(RuntimeError, 'timed out'):
                    self.acceptance.run('docker')
            event = self.acceptance.events[-1]
            self.assertTrue((self.path/event['log']).read_bytes())

    def test_network_committed_without_receipt_is_still_cleaned(self):
        calls = []
        def docker(*args, **kwargs):
            calls.append(args)
            if args[:2] == ('network', 'create'):
                raise RuntimeError('lost receipt after daemon committed network')
            if args[:2] == ('network', 'inspect'):
                return result(json.dumps([{'Labels': {'parrot.websearch.verify': self.acceptance.identity}}]))
            return result()
        self.acceptance.docker = docker
        with self.assertRaisesRegex(RuntimeError, 'lost receipt'):
            self.acceptance.create('a', 9090)
        self.acceptance.cleanup()
        self.assertIn(('network', 'rm', self.acceptance.identity+'-a-network'), calls)

    def test_foreign_objects_are_never_removed(self):
        self.acceptance.networks = ['foreign-network']
        self.acceptance.corpora = ['foreign-container']
        calls = []
        def docker(*args, **kwargs):
            calls.append(args)
            return result(json.dumps([{'Labels': {}, 'Config': {'Labels': {}}}]))
        self.acceptance.docker = docker
        with self.assertRaisesRegex(RuntimeError, 'foreign'):
            self.acceptance.cleanup()
        self.assertFalse(any('rm' in command for command in calls))
        self.assertEqual(len(self.acceptance.report['cleanup_errors']), 2)

    def test_log_failure_does_not_skip_project_teardown(self):
        project = {'name':'owned', 'stopped':False}
        self.acceptance.projects = [project]
        calls = []
        self.acceptance.compose = lambda *args, **kwargs: calls.append(args)
        with patch.object(self.acceptance, 'logs', side_effect=OSError('logs unavailable')):
            with self.assertRaisesRegex(RuntimeError, 'logs unavailable'):
                self.acceptance.cleanup()
        self.assertEqual(calls, [(project, 'stop', 'app'), (project, 'down', '-v', '--remove-orphans')])

    def test_cleanup_failure_prevents_passed_report(self):
        report = self.path/'report'
        with patch.object(sys, 'argv', ['verify', '--skip-build', '--image-prefix', 'provided', '--report-dir', str(report)]), \
             patch.object(images.Acceptance, 'exercise'), \
             patch.object(images.Acceptance, 'cleanup', side_effect=RuntimeError('network leaked')), \
             patch.object(images.signal, 'signal'):
            self.assertEqual(images.main(), 1)
        saved = json.loads((report/'result.json').read_text())
        self.assertEqual(saved['status'], 'failed')
        self.assertEqual(saved['cleanup_error'], 'network leaked')

    def test_caller_images_are_not_removed_on_skip_build(self):
        with patch.object(self.acceptance, 'docker') as docker:
            self.acceptance.cleanup()
        docker.assert_not_called()


if __name__ == '__main__':
    unittest.main()
