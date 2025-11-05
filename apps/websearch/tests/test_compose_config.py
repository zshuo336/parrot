"""Argument replacement and real Compose-model semantics, without a daemon."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'deploy/compose_config.py'
spec = importlib.util.spec_from_file_location('compose_config', SCRIPT)
config = importlib.util.module_from_spec(spec)
spec.loader.exec_module(config)


def model():
    return {'services': {'app': {'image': 'example', 'command': ['--port', '8080', '--data', '/data'],
        'ports': [{'host_ip': '127.0.0.1', 'published': '0', 'target': 8080},
                  {'host_ip': '127.0.0.1', 'published': '9000', 'target': 9000}]}}}


class ComposeConfigurationTests(unittest.TestCase):
    def test_port_replacement_preserves_random_host_port_and_other_publications(self):
        app = config.configure(model(), ['--port', '9090'])['services']['app']
        self.assertEqual(app['command'], ['--data', '/data', '--port', '9090'])
        self.assertEqual(app['ports'], [
            {'host_ip': '127.0.0.1', 'published': '0', 'target': 9090},
            {'host_ip': '127.0.0.1', 'published': '9000', 'target': 9000}])
        self.assertEqual(app['environment']['WS_WEB_LISTEN_PORT'], '9090')

    def test_duplicate_user_options_survive_for_real_cli_rejection(self):
        command = config.configure(model(), ['--data', '/data/a', '--data', '/data/b'])['services']['app']['command']
        self.assertEqual(command, ['--port', '8080', '--data', '/data/a', '--data', '/data/b'])
        self.assertNotIn('/data', command)

    def test_option_like_values_and_seed_separator_do_not_override_other_options(self):
        app = config.configure(model(), ['--data', '-h', '--', '--port', '9090'])['services']['app']
        self.assertEqual(app['command'], ['--port', '8080', '--data', '-h', '--', '--port', '9090'])
        self.assertEqual(app['ports'][0]['target'], 8080)

    def test_literal_dollars_are_escaped_only_once_for_compose(self):
        original = model()
        original['services']['app']['command'] += ['--node-root', '/already/$$HOME']
        app = config.configure(original, ['--data', '/data/${HOME} $$ `id`'])['services']['app']
        self.assertEqual(app['command'], ['--port', '8080', '--node-root', '/already/$$HOME',
                                         '--data', '/data/$${HOME} $$$$ `id`'])

    def test_invalid_container_ports_and_non_running_modes_are_rejected(self):
        for args in [['--port'], ['--port', '0'], ['--port', '65536'], ['--port', 'bad'],
                     ['--port', '１２'], ['--help'], ['--validate-only'], ['--data', '--serve-only']]:
            with self.subTest(args=args), self.assertRaises(ValueError):
                config.configure(model(), args)

    def test_health_requires_all_nonnegative_integer_business_fields(self):
        valid = dict(docs=0, terms=0, postings=0, queries=0, segments=0)
        self.assertTrue(config.healthy(valid))
        for key in valid:
            missing = dict(valid); missing.pop(key)
            self.assertFalse(config.healthy(missing))
            for invalid in [True, None, '0', -1, 0.1]:
                self.assertFalse(config.healthy(dict(valid, **{key: invalid})))
        for value in [[], None, '<html>ready</html>']:
            self.assertFalse(config.healthy(value))

    def test_optimized_health_entrypoint_still_rejects_bad_responses(self):
        for value, expected in [(dict(docs=0, terms=0, postings=0, queries=0, segments=0), 0),
                                (dict(docs=True, terms=0, postings=0, queries=0, segments=0), 2), ({}, 2)]:
            result = subprocess.run([sys.executable, '-O', str(SCRIPT), 'health'],
                input=json.dumps(value), text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, expected, result.stderr)


if __name__ == '__main__':
    unittest.main()
