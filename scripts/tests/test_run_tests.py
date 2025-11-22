"""Static contracts for the bounded all-workspace test entrypoint."""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "run_tests.sh"


class RunTestsContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = SCRIPT.read_text(encoding="utf-8")

    def test_resource_defaults_are_overrideable_and_bounded(self):
        for variable, default in [
            ("CARGO_BUILD_JOBS", "2"),
            ("CARGO_INCREMENTAL", "0"),
            ("CARGO_PROFILE_TEST_DEBUG", "0"),
            ("CARGO_TERM_COLOR", "never"),
        ]:
            with self.subTest(variable=variable):
                self.assertRegex(
                    self.source,
                    rf'export {variable}="\$\{{{variable}:-{re.escape(default)}\}}"',
                )

    def test_workspace_command_is_locked_and_discovers_all_test_kinds(self):
        self.assertRegex(
            self.source,
            r"cargo test --locked --workspace --all-targets --lib --bins --tests --benches",
        )


if __name__ == "__main__":
    unittest.main()
