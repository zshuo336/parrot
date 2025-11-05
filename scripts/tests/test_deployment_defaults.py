"""Check resolved development deployments with Docker Compose v2 (no daemon needed)."""
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]


def resolve(relative, prefix=None, platform=None):
    env = dict(os.environ)
    env.pop("WS_PLATFORM", None)
    if platform is not None:
        env["WS_PLATFORM"] = platform
    env.pop("WS_REGISTRY_PREFIX", None)
    if prefix is not None:
        env["WS_REGISTRY_PREFIX"] = prefix
    result = subprocess.run(
        ["docker", "compose", "-f", str(ROOT / relative), "--profile", "*", "config", "--format", "json"],
        env=env, capture_output=True, text=True, timeout=30, check=True,
    )
    return json.loads(result.stdout)["services"]


class DeploymentDefaultsTests(unittest.TestCase):
    def test_websearch_projects_do_not_share_container_names_or_subnets(self):
        configurations = []
        for project in ["parrot-contract-a", "parrot-contract-b"]:
            result = subprocess.run(["docker", "compose", "--project-name", project,
                "-f", str(ROOT / "apps/websearch/deploy/docker-compose.yml"),
                "config", "--format", "json"], capture_output=True, text=True, check=True, timeout=30)
            configurations.append(json.loads(result.stdout))
        for config in configurations:
            for service in config["services"].values():
                self.assertNotIn("container_name", service, "fixed names defeat Compose project isolation")
                for network in service["networks"].values():
                    self.assertNotIn("ipv4_address", network or {})
            for network in config["networks"].values():
                self.assertFalse(network.get("ipam", {}).get("config"), "fixed subnet prevents independent deployments")
        names = [{n["name"] for n in config["networks"].values()} for config in configurations]
        self.assertTrue(names[0].isdisjoint(names[1]))

    def test_ray_containers_reap_orphaned_descendants(self):
        for path, name in [("deploy/parrot-stack/docker-compose.yml", "ray-gw"),
                           ("apps/websearch/deploy/docker-compose.yml", "ray")]:
            with self.subTest(path=path):
                self.assertIs(resolve(path)[name].get("init"), True)

    def test_websearch_app_forwards_default_termination_signals(self):
        self.assertIs(resolve("apps/websearch/deploy/docker-compose.yml")["app"].get("init"), True)

    def test_stack_ray_uses_prebuilt_gateway_without_startup_package_install(self):
        ray = resolve("deploy/parrot-stack/docker-compose.yml")["ray-gw"]
        self.assertEqual(ray.get("build", {}).get("dockerfile"), "deploy/images/gw-ray.Dockerfile")
        self.assertEqual(ray["command"], ["python3", "-m", "parrot_protocol.ray_gw", "9851"])
        for volume in ray.get("volumes", []):
            target = volume["target"].rstrip("/")
            self.assertFalse(target in ("", "/gw") or target.startswith("/gw/parrot_protocol"),
                             "a host mount must not replace the tested protocol package")

    def test_app_platform_is_explicit_and_binary_selection_follows_build_target(self):
        for platform in [None, "", "linux/arm64", "linux/amd64"]:
            with self.subTest(platform=platform):
                app = resolve("apps/websearch/deploy/docker-compose.yml", platform=platform)["app"]
                self.assertEqual(app.get("platform"), platform or "linux/arm64")
                self.assertEqual(app["build"]["args"].get("TRIPLE", "auto"), "auto")

    def test_wrapper_arguments_survive_compose_variable_interpolation(self):
        sys.path.insert(0, str(ROOT / "apps/websearch/tests"))
        from test_compose import ComposeWrapperTests
        args = ["--data", "literal ${HOME} $HOME $$ `whoami` ' quote"]
        result, _, resolved, _ = ComposeWrapperTests().run_wrapper("up", *args, resolve=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNotNone(resolved)
        # Compose config serializes literal dollars as $$ for round-trip safety;
        # Compose removes this escaping when constructing the container argv.
        self.assertEqual(resolved["services"]["app"]["command"][-2:],
                         [arg.replace("$", "$$") for arg in args])

    def check_ports(self, services, expected):
        actual = {}
        for name, service in services.items():
            for port in service.get("ports", []):
                self.assertEqual(port.get("host_ip"), "127.0.0.1", (name, port))
                actual[name] = (int(port["published"]), port["target"])
        self.assertEqual(actual, expected)

    def test_stack_preserves_peer_bind_but_limits_host_publications(self):
        services = resolve("deploy/parrot-stack/docker-compose.yml")
        self.check_ports(services, {
            **{f"parrot-{n}": (19800 + n, 9801) for n in range(1, 4)},
            "ray-gw": (19851, 9851),
        })
        for n in range(1, 4):
            self.assertEqual(services[f"parrot-{n}"]["environment"]["PARROT_BIND"], "0.0.0.0:9801")

    def test_checked_in_mesh_only_publishes_loopback(self):
        services = resolve("deploy/parrot-mesh/docker-compose.mesh.yml")
        self.check_ports(services, {f"parrot-{n}": (21800 + n, 9801) for n in range(1, 6)})

    def test_websearch_only_publishes_local_web(self):
        services = resolve("apps/websearch/deploy/docker-compose.yml")
        self.check_ports(services, {"app": (8188, 8080)})

    def test_default_and_explicit_empty_registry_use_official_images(self):
        for prefix in [None, ""]:
            with self.subTest(prefix=prefix):
                services = resolve("apps/websearch/deploy/docker-compose.yml", prefix)
                for name, service in services.items():
                    self.assertEqual(service["build"]["args"]["REGISTRY_PREFIX"], "", name)

    def test_registry_mirror_is_an_explicit_choice(self):
        services = resolve("apps/websearch/deploy/docker-compose.yml", "mirror.example/org/")
        for name, service in services.items():
            self.assertEqual(service["build"]["args"]["REGISTRY_PREFIX"], "mirror.example/org/", name)


if __name__ == "__main__":
    unittest.main()
