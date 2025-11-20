"""Current Cargo output, rather than a valid stale program, must be exercised."""
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import run_integration as web


class RunnerBuildContracts(unittest.TestCase):
    def test_full_entry_runs_and_hashes_the_current_custom_target(self):
        with tempfile.TemporaryDirectory(prefix="ws build ") as temporary, ExitStack() as stack:
            repository = Path(temporary)
            app = repository / "apps/websearch"
            (app / "jvm").mkdir(parents=True)
            (app / "erlang").mkdir()
            shutil.copyfile(web.APP / "build.sh", app / "build.sh")
            (app / "jvm/build.sh").write_text("exit 0\n")
            (app / "jvm/test.sh").write_text("exit 0\n")
            for name in ("ray", "jieba"):
                (repository / (name + ".py")).write_text("")
            for name in ("interop/jvm/target/parrot-protocol-jvm-0.1.0.jar",
                         "interop/erlang/parrot_gw.beam", "apps/websearch/erlang/frontier.beam",
                         "apps/websearch/jvm/target/websearch-jvm-1.0.0.jar",
                         "apps/websearch/python/tokenizer.py", "interop/python/parrot_protocol/ray_gw.py"):
                path = repository / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("owned component fixture\n")
            current = repository / "custom target/debug/websearch"
            stale = repository / "target/debug/websearch"
            for path, marker in ((current, "current-ran"), (stale, "stale-ran")):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("#!" + sys.executable + "\nfrom pathlib import Path\n"
                                + "Path(" + repr(str(repository / marker)) + ").touch()\n"
                                + "print('business-ok')\n")
                path.chmod(0o700)
            receipt = dict(reason="compiler-artifact", target=dict(name="websearch", kind=["bin"],
                src_path=str(app / "src/main.rs")), profile=dict(test=False), executable=str(current))
            tools = repository / "tools"
            tools.mkdir()
            for name in ("cargo", "make", "erl", "erlc", "java", "mvn", "jar", "unzip"):
                path = tools / name
                path.write_text("#!" + sys.executable + "\n"
                                + ("print(" + repr(json.dumps(receipt)) + ")\n" if name == "cargo" else ""))
                path.chmod(0o700)
            logs = repository / "logs"
            logs.mkdir()
            class BusinessFixture:
                def __init__(self, root, binary, resources):
                    self.binary = binary
                    self.passed = []
                    self.token = "fixture"
                    self.corpus = SimpleNamespace(base="owned-fixture", requests={}, blocked=[])
                    self.runtime_proxy = SimpleNamespace(blocked=[])

                def run(self):
                    result = subprocess.run([self.binary], capture_output=True, text=True, timeout=10)
                    web.require(result.returncode == 0 and result.stdout == "business-ok\n", "business failed")

                def run_faults(self):
                    pass

            stack.enter_context(patch.object(web, "REPO", repository))
            stack.enter_context(patch.object(web, "APP", app))
            process = web.Process
            stack.enter_context(patch.object(web, "Process", side_effect=
                lambda root, name, argv, **options: process(root, name, argv, cwd=repository, **options)))
            stack.enter_context(patch.object(web, "Suite", BusinessFixture))
            stack.enter_context(patch.object(web.tempfile, "mkdtemp", return_value=str(logs)))
            stack.enter_context(patch.object(sys, "argv", ["run_integration.py"]))
            stack.enter_context(patch.dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                                           PYTHONPATH=str(repository), CARGO_TARGET_DIR=str(current.parents[1])))
            self.assertEqual(web.main(), 0)
            self.assertTrue((repository / "current-ran").is_file(), "current Cargo artifact was not executed")
            self.assertFalse((repository / "stale-ran").exists(), "stale default executable was run")
            report = json.loads((logs / "result.json").read_text())
            self.assertEqual(report["binary"], str(current))
            self.assertEqual(report["artifact_sha256"][str(current)], hashlib.sha256(current.read_bytes()).hexdigest())

    def test_invalid_cargo_receipts_fail_instead_of_falling_back(self):
        with tempfile.TemporaryDirectory() as temporary:
            executable = Path(temporary) / "websearch"
            executable.write_text("fixture")
            executable.chmod(0o700)
            valid = dict(reason="compiler-artifact", target=dict(name="websearch", kind=["bin"],
                src_path=str(web.APP / "src/main.rs")), profile=dict(test=False), executable=str(executable))
            invalid = ["", json.dumps(dict(valid, executable=None)),
                       json.dumps(dict(valid, executable="relative/websearch")),
                       json.dumps(dict(valid, executable=str(executable) + "-missing")),
                       json.dumps(dict(valid, profile=dict(test=True))),
                       json.dumps(dict(valid, target=dict(valid["target"], kind=["example"]))),
                       json.dumps(dict(valid, target=dict(valid["target"], src_path="other/src/main.rs"))),
                       json.dumps(valid) + "\n" + json.dumps(valid)]
            for receipt in invalid:
                with self.subTest(receipt=receipt), self.assertRaisesRegex(AssertionError, "Cargo"):
                    web.built_application(receipt)
            executable.chmod(0o600)
            with self.assertRaisesRegex(AssertionError, "Cargo"):
                web.built_application(json.dumps(valid))


if __name__ == "__main__":
    unittest.main()
