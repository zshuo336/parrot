"""Build gate contracts using isolated stub tools, never user build artifacts."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class BuildGate(unittest.TestCase):
    def run_build(self, failing_tool):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app = root / "apps/websearch"
            (app / "jvm").mkdir(parents=True)
            (app / "erlang").mkdir()
            (app / "jvm/build.sh").write_text("exit 0\n")
            shutil.copyfile(Path(__file__).parents[1] / "build.sh", app / "build.sh")
            cargo_args = root / "cargo-args"
            # Exported shell functions avoid executing freshly created binaries
            # (macOS may asynchronously scan those for tens of seconds).
            functions = r'''cargo() {
  printf '%s\n' "$@" > "$TEST_CARGO_ARGS"
  [ "$TEST_FAIL_TOOL" != cargo ] || return 17
}
erlc() { [ "$TEST_FAIL_TOOL" != erlc ] || return 17; }
python3() { [ "$TEST_FAIL_TOOL" != python3 ] || return 17; }
export -f cargo erlc python3
source "$0" "$@"
'''
            env = dict(os.environ, TEST_CARGO_ARGS=str(cargo_args), TEST_FAIL_TOOL=failing_tool or "none")
            result = subprocess.run(["/bin/bash", "-c", functions, str(app / "build.sh"), "--profile", "dev"], env=env, capture_output=True, text=True, timeout=60)
            args = cargo_args.read_text().splitlines()
            self.assertIn("--locked", args)
            self.assertEqual(args[-2:], ["--profile", "dev"])
            return result

    def test_missing_python_dependency_is_failure(self):
        result = self.run_build("python3")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("构建完成", result.stdout)

    def test_cargo_failure_propagates(self):
        self.assertEqual(self.run_build("cargo").returncode, 17)

    def test_erlang_failure_propagates(self):
        self.assertEqual(self.run_build("erlc").returncode, 17)

    def test_success_requires_all_steps(self):
        self.assertEqual(self.run_build(None).returncode, 0)


if __name__ == "__main__":
    unittest.main()
