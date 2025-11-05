"""Validate mesh generation and failed-probe cleanup without a Docker daemon."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


class MeshGeneratorTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        source = Path(__file__).resolve().parents[2] / "deploy/parrot-mesh/gen-mesh.sh"
        self.script = self.directory / "gen-mesh.sh"
        shutil.copyfile(source, self.script)
        self.output = self.directory / "docker-compose.mesh.yml"
        self.env = dict(os.environ)
        for key in ["PARROT_MESH_NET", "PARROT_MESH_BASE_PORT"]:
            self.env.pop(key, None)

    def run_generator(self, *args, env=None, functions=None):
        command = ["bash", str(self.script), *args]
        if functions is not None:
            command = ["bash", "-c", functions + '\nsource "$0" "$@"', str(self.script), *args]
        return subprocess.run(command, env=env or self.env,
                              capture_output=True, text=True, timeout=60)

    def test_every_pair_has_an_explicit_seed_and_dns_target(self):
        for count in [1, 2, 5, 12]:
            result = self.run_generator(str(count))
            self.assertEqual(result.returncode, 0, result.stderr)
            text = self.output.read_text()
            seeds = re.findall(r'PARROT_SEEDS: "(.*)"', text)
            self.assertEqual(len(seeds), count)
            for index, value in enumerate(seeds, 1):
                expected = [f"parrot-{peer}=parrot-{peer}:9801" for peer in range(1, index)]
                self.assertEqual(value.split(",") if value else [], expected)
            for index in range(1, count + 1):
                self.assertIn(f"      - parrot-{index}=parrot-{index}:9801\n", text)
            self.assertNotIn("ipv4_address", text)
            self.assertEqual(text.count("condition: service_healthy"), count)

    def test_invalid_input_never_overwrites_existing_compose(self):
        self.output.write_text("preserve existing deployment")
        for value in ["0", "250", "-1", "01", "a", "1+1", "999999999999", "x[$(touch injected)]"]:
            self.assertEqual(self.run_generator(value).returncode, 2, value)
        for key, value in [("PARROT_MESH_BASE_PORT", "65531"),
                           ("PARROT_MESH_BASE_PORT", "1+1"),
                           ("PARROT_MESH_NET", "300.0.0.0/16"),
                           ("PARROT_MESH_NET", "172.28.0.1/16"),
                           ("PARROT_MESH_NET", "172.28.0.0/30"),
                           ("PARROT_MESH_NET", "172.28.0.0/16\nservices:")]:
            self.assertEqual(self.run_generator("5", env={**self.env, key: value}).returncode, 2)
        self.assertEqual(self.run_generator("5", "unknown").returncode, 2)
        self.assertEqual(self.output.read_text(), "preserve existing deployment")
        self.assertFalse((self.directory / "injected").exists())

    def test_custom_network_does_not_keep_addresses_from_default_subnet(self):
        result = self.run_generator("5", env={**self.env, "PARROT_MESH_NET": "10.88.4.0/24"})
        self.assertEqual(result.returncode, 0, result.stderr)
        text = self.output.read_text()
        self.assertIn("subnet: 10.88.4.0/24", text)
        self.assertNotIn("172.28.", text)

    def test_generated_host_ports_are_loopback_and_preserve_custom_base(self):
        for count, base in [(1, 21800), (12, 31000)]:
            result = self.run_generator(str(count), env={**self.env, "PARROT_MESH_BASE_PORT": str(base)})
            self.assertEqual(result.returncode, 0, result.stderr)
            text = self.output.read_text()
            ports = re.findall(r'^      - "([^"]*:9801)"$', text, re.MULTILINE)
            self.assertEqual(ports, [f"127.0.0.1:{base + n}:9801" for n in range(1, min(count, 5) + 1)])
            self.assertEqual(text.count("PARROT_BIND: 0.0.0.0:9801"), count)

    def test_failed_probe_always_tears_down_and_preserves_failure_status(self):
        # Avoid a freshly created executable's platform scanning delay while
        # preserving the generator's real shell error/trap behavior.
        functions = r'''docker() {
  printf '%s\n' "$*" >> "$FAKE_DOCKER_LOG"
  case "$*" in *" run "*) return 23 ;; esac
}
export -f docker
'''
        log = self.directory / "calls"
        result = self.run_generator("2", "probe", functions=functions,
                                    env={**self.env, "FAKE_DOCKER_LOG": str(log)})
        self.assertEqual(result.returncode, 23, result.stderr)
        calls = log.read_text().splitlines()
        self.assertEqual(len(calls), 3)
        self.assertIn("--wait --wait-timeout 120", calls[0])
        self.assertTrue(calls[-1].endswith(" down"), calls)
        self.assertTrue(self.output.exists())


if __name__ == "__main__":
    unittest.main()
