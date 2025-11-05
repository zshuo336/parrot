"""Run with the image's CPython 3.11 environment, before gateway acceptance."""
import importlib.metadata
import logging
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RayImageRuntimeTests(unittest.TestCase):
    def test_installed_dependencies_match_both_locks(self):
        for filename in ["requirements-ray-build.lock", "requirements-ray.lock"]:
            lock = (ROOT / "deploy/images" / filename).read_text()
            selected = re.findall(r"^([A-Za-z0-9_.-]+)==([^\s;]+)", lock, re.MULTILINE)
            self.assertTrue(selected)
            for name, version in selected:
                with self.subTest(package=name):
                    self.assertEqual(importlib.metadata.version(name), version)

    def test_installed_dependency_metadata_is_consistent(self):
        result = subprocess.run([sys.executable, "-m", "pip", "check"],
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_jieba_executes_tokenization(self):
        import jieba
        self.assertEqual(jieba.lcut("南京市长江大桥"), ["南京市", "长江大桥"])

    def test_ray_pip_runtime_can_construct_its_installer(self):
        from ray.runtime_env import RuntimeEnv
        from ray._private.runtime_env.pip import PipProcessor
        with tempfile.TemporaryDirectory() as root:
            # Ray imports virtualenv here even when the gateway itself is in a
            # venv. Importing Ray alone never exercises this capability.
            PipProcessor(root, RuntimeEnv(pip=["offline-fixture==1"]), logging.getLogger(__name__))

    def test_virtualenv_can_create_an_offline_environment(self):
        with tempfile.TemporaryDirectory() as root:
            environment = Path(root) / "isolated"
            result = subprocess.run([sys.executable, "-m", "virtualenv", "--no-download",
                                     "--no-periodic-update", "--app-data", str(Path(root) / "app-data"),
                                     str(environment)], capture_output=True, text=True, timeout=60)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            result = subprocess.run([str(environment / "bin/python"), "-m", "pip", "--version"],
                                    capture_output=True, text=True, timeout=20)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn(str(environment), result.stdout)


if __name__ == "__main__":
    unittest.main()
