"""Exercise the deployment wrapper without starting containers or building artifacts."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class ComposeWrapperTests(unittest.TestCase):
    def run_wrapper(self, *args, mode="ready", resolve=False, platform=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app = root / "apps/websearch"
            deploy = app / "deploy"
            deploy.mkdir(parents=True)
            images = root / "deploy/images"
            images.mkdir(parents=True)
            (app / "build.sh").write_text('touch "$TEST_ROOT/prebuilt"\n')
            (images / "prep-jvm-libs.sh").write_text("exit 0\n")
            (deploy / "docker-compose.yml").write_text('services: {app: {image: example, command: ["--port", "8080", "--data", "/data"]}}\n')
            shutil.copyfile(Path(__file__).parents[1] / "deploy/compose.sh", deploy / "compose.sh")
            shutil.copyfile(Path(__file__).parents[1] / "deploy/compose_config.py", deploy / "compose_config.py")
            # Export functions instead of executing newly written fake binaries.
            functions = r'''
cargo() {
  printf '%s\n' "$*" > "$TEST_ROOT/cargo"
  [ "$TEST_MODE" != build-fail ] || return 17
}
sleep() { SECONDS=$((SECONDS + 500)); }
exec() { "$@"; exit $?; }
docker() {
  python3 -c 'import json,os,sys; f=open(os.environ["TEST_ROOT"]+"/calls","a"); f.write(json.dumps(sys.argv[1:])+"\n")' "$@"
  case "$*" in
    *"config --format json"*) printf '%s\n' '{"services":{"app":{"image":"example","command":["--port","8080","--data","/data"],"ports":[{"host_ip":"127.0.0.1","published":"8188","target":8080}]}}}' ;;
    *"run --rm --no-deps app --validate-only"*) [ "$TEST_MODE" != validate-fail ] || return 2 ;;
    *"ps --all --quiet app"*) echo 'owned-app-id' ;;
    *"port app"*) [ "$TEST_MODE" = no-publication ] || echo '127.0.0.1:8188' ;;
    *"stop -t 75 app"*) [ "$TEST_MODE" != stop-fail ] || return 27 ;;
    *"up -d --no-build"*)
      [ "$TEST_MODE" != up-fail ] || return 23
      while [ "$#" -gt 0 ]; do
        if [ "$1" = -f ]; then
          shift
          if [ "$1" != "$TEST_ROOT/apps/websearch/deploy/docker-compose.yml" ]; then cp "$1" "$TEST_ROOT/override.json"; fi
        fi
        shift
      done ;;
    *logs*) if [ "$TEST_MODE" = ready ] || [ "$TEST_MODE" = health-invalid ] || [ "$TEST_MODE" = no-publication ]; then printf '%s\n' '浏览器打开'; fi ;;
    *inspect*) if [ "$TEST_MODE" = exited ]; then echo 'false 2026-10-10T00:00:00Z'; else echo 'true 2026-10-10T00:00:00Z'; fi ;;
    *exec*)
      [ "$TEST_MODE" != exec-fail ] || return 31
      if [ "$TEST_MODE" = health-invalid ]; then echo '<html>unavailable</html>';
      else echo '{"docs":0,"terms":0,"postings":0,"queries":0,"segments":0}'; fi ;;
  esac
}
export -f cargo docker sleep exec
source "$0" "$@"
'''
            env = dict(os.environ, TEST_ROOT=str(root), TEST_MODE=mode)
            env.pop("WS_PLATFORM", None)
            if platform is not None:
                env["WS_PLATFORM"] = platform
            result = subprocess.run(["/bin/bash", "-c", functions, str(deploy / "compose.sh"), *args],
                                    env=env, capture_output=True, text=True, timeout=30)
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()] if (root / "calls").exists() else []
            for call in calls:
                for i, value in enumerate(call[:-1]):
                    if value == "-f" and call[i + 1] != str(deploy / "docker-compose.yml"):
                        self.assertFalse(Path(call[i + 1]).exists(), "temporary override was not removed")
            override = json.loads((root / "override.json").read_text()) if (root / "override.json").exists() else None
            if resolve and override is not None:
                resolved = subprocess.run(["docker", "compose", "-f", str(root / "override.json"), "config", "--format", "json"],
                                          capture_output=True, text=True, check=True, timeout=30)
                override = json.loads(resolved.stdout)
            if platform in ["linux/386", "darwin/arm64", "linux/amd64;echo bad"]:
                self.assertFalse((root / "prebuilt").exists(), "unsupported platform started expensive prebuild")
            cargo = (root / "cargo").read_text() if (root / "cargo").exists() else ""
            self.assertEqual(sorted(p.name for p in deploy.iterdir()), ["compose.sh", "compose_config.py", "docker-compose.yml"])
            return result, calls, override, cargo

    def test_platform_selects_matching_rust_target(self):
        for platform, triple in [(None, "aarch64"), ("", "aarch64"),
                                 ("linux/arm64", "aarch64"), ("linux/amd64", "x86_64")]:
            with self.subTest(platform=platform):
                result, _, _, cargo = self.run_wrapper("up", platform=platform)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--target " + triple + "-unknown-linux-musl", cargo)

    def test_override_replaces_defaults_instead_of_creating_invalid_duplicate_cli(self):
        result, _, override, _ = self.run_wrapper("up", "--data", "/data/tenant one", "--port", "9090")
        self.assertEqual(result.returncode, 0, result.stderr)
        command = override["services"]["app"]["command"]
        for option, value in [("--data", "/data/tenant one"), ("--port", "9090")]:
            self.assertEqual(command.count(option), 1, command)
            self.assertEqual(command[command.index(option)+1], value)
        self.assertEqual(override["services"]["app"]["ports"],
                         [{"host_ip": "127.0.0.1", "published": "8188", "target": 9090}])
        self.assertEqual(override["services"]["app"]["environment"]["WS_WEB_LISTEN_PORT"], "9090")

    def test_ready_log_without_valid_business_health_must_not_pass(self):
        result, _, _, _ = self.run_wrapper("up", mode="health-invalid")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("✓ Web", result.stdout)

    def test_unsupported_platform_fails_before_prebuild(self):
        for platform in ["linux/386", "darwin/arm64", "linux/amd64;echo bad"]:
            with self.subTest(platform=platform):
                result, calls, _, cargo = self.run_wrapper("up", platform=platform)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(cargo, "")
                self.assertEqual(calls, [])

    def test_ready_app_forwards_literal_arguments_and_reports_host_port(self):
        extra = ["--sites", "7", "--data", "a path ' with $symbols"]
        result, calls, override, cargo = self.run_wrapper("up", *extra)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(override["services"]["app"]["command"], ["--port", "8080", *[arg.replace("$", "$$") for arg in extra]])
        self.assertIn("127.0.0.1:8188", result.stdout)
        self.assertIn("--locked", cargo)
        self.assertIn(["logs", "--since", "2026-10-10T00:00:00Z", "--tail", "200", "owned-app-id"], calls)

    def test_validation_failure_does_not_launch_persistent_services(self):
        result, calls, _, _ = self.run_wrapper("up", "--sites", "7", "--sites", "8", mode="validate-fail")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertTrue(any("--validate-only" in call for call in calls))
        self.assertFalse(any("up" in call for call in calls))

    def test_missing_port_publication_cannot_print_success(self):
        result, _, _, _ = self.run_wrapper("up", mode="no-publication")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("✓ Web", result.stdout)

    def test_logs_and_stats_are_scoped_to_the_compose_project(self):
        for action in ["logs", "stats"]:
            result, calls, _, _ = self.run_wrapper(action)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(calls), 1)
            self.assertEqual(calls[0][0], "compose")
            self.assertIn("app", calls[0])
            self.assertNotIn("ws-app", calls[0])

    def test_readiness_timeout_fails_without_success_message(self):
        result, _, _, _ = self.run_wrapper("up", mode="timeout")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertNotIn("✓ Web", result.stdout)

    def test_exited_app_fails_without_success_message(self):
        result, _, _, _ = self.run_wrapper("up", mode="exited")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertNotIn("✓ Web", result.stdout)

    def test_down_stops_application_before_destroying_gateway_dependencies(self):
        result, calls, _, _ = self.run_wrapper("down", "-v")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[3:] for call in calls],
                         [["stop", "-t", "75", "app"], ["down", "-v", "--remove-orphans"]])

    def test_failed_application_stop_does_not_destroy_gateways(self):
        result, calls, _, _ = self.run_wrapper("down", mode="stop-fail")
        self.assertEqual(result.returncode, 27)
        self.assertFalse(any("down" in call for call in calls))

    def test_build_failure_propagates(self):
        result, calls, _, _ = self.run_wrapper("up", mode="build-fail")
        self.assertEqual(result.returncode, 17)
        self.assertEqual(calls, [])

    def test_compose_failure_propagates(self):
        result, _, _, _ = self.run_wrapper("up", "--sites", "7", mode="up-fail")
        self.assertEqual(result.returncode, 23)
        self.assertNotIn("✓ Web", result.stdout)

    def test_search_passes_encoded_url_as_data_to_one_container_command(self):
        query = "中文 ' & $(printf injected)"
        result, calls, _, _ = self.run_wrapper("search", query)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        from urllib.parse import quote
        self.assertEqual(calls[0][-1], "/search?q=" + quote(query))
        script = calls[0][calls[0].index("-c") + 1]
        self.assertIn('$1', script)
        self.assertIn("wget", script)
        self.assertNotIn(query, script)

    def test_search_failure_is_not_hidden(self):
        result, _, _, _ = self.run_wrapper("search", "hello", mode="exec-fail")
        self.assertEqual(result.returncode, 31)

    def test_search_shell_selects_curl_or_wget_and_preserves_url(self):
        _, calls, _, _ = self.run_wrapper("search", "中文 ' & $(printf injected)")
        script, path = calls[0][calls[0].index("-c") + 1], calls[0][-1]
        url = "http://localhost:9123" + path
        for available in ["yes", "no"]:
            functions = r'''
command() { if [ "$*" = '-v curl' ]; then [ "$CURL_AVAILABLE" = yes ]; else builtin command "$@"; fi; }
curl() { printf 'curl\n%s\n' "$@"; }
wget() { printf 'wget\n%s\n' "$@"; }
exec() { "$@"; exit $?; }
'''
            result = subprocess.run(["/bin/bash", "-c", functions + script, "sh", path],
                                    env=dict(os.environ, CURL_AVAILABLE=available, WS_WEB_LISTEN_PORT="9123"),
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.splitlines()[-1], url)
            self.assertEqual(result.stdout.splitlines()[0], "curl" if available == "yes" else "wget")
            arguments = result.stdout.splitlines()
            self.assertIn("--max-time" if available == "yes" else "-T", arguments)
            self.assertIn("5", arguments)


if __name__ == "__main__":
    unittest.main()
