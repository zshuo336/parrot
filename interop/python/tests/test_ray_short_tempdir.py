"""A private runtime alias must remain short even when the OS temporary root is long."""
from pathlib import Path
from unittest.mock import patch
import json
import os
import stat
import tempfile
import unittest

import ray_test_cluster


class RayShortTempdirTests(unittest.TestCase):
    def test_long_default_tempdir_cannot_overflow_the_owned_runtime_socket_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            long = root / ('long-temporary-root-' * 5)
            long.mkdir()
            calls, aliases, tokens = [], [], []
            class Ray:
                def is_initialized(self):
                    return False
                def shutdown(self):
                    calls.append('shutdown')
            def initialize(ray, options):
                calls.append('initialize')
                alias = Path(options['_temp_dir'])
                aliases.append(alias)
                token = Path(os.environ['RAY_AUTH_TOKEN_PATH'])
                tokens.append(token)
                socket_path = alias / 'session_2026-10-29_03-47-41_451571_1234567890/sockets/plasma_store'
                self.assertLessEqual(len(os.fsencode(socket_path)), 103,
                    'owned Ray runtime alias must fit the macOS socket path bound under a long TMPDIR')
                self.assertTrue(alias.is_symlink())
                self.assertEqual(alias.resolve(), (root / 'evidence/runtime').resolve())
                self.assertEqual(stat.S_IMODE(alias.parent.stat().st_mode), 0o700)
                self.assertEqual(stat.S_IMODE(token.stat().st_mode), 0o600)
            with patch.object(tempfile, 'tempdir', str(long)), \
                    patch.object(ray_test_cluster, 'initialize', initialize), \
                    patch.object(ray_test_cluster, 'refresh_authentication'), \
                    patch.object(ray_test_cluster, 'descendants', return_value=set()):
                with ray_test_cluster.owned_ray_cluster(Ray(), evidence=root / 'evidence'):
                    self.assertEqual(calls, ['initialize'])
            self.assertEqual(calls, ['initialize', 'shutdown'])
            self.assertFalse(aliases[0].exists())
            self.assertFalse(tokens[0].exists())
            self.assertFalse(aliases[0].parent.exists())
            receipt = json.loads((root / 'evidence/cluster-retirement.json').read_text())
            self.assertTrue(receipt['ok'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
