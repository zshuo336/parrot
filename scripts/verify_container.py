#!/usr/bin/env python3
"""Build and verify the Linux node image in an isolated, owned Docker network.

Requires Docker Desktop/Engine and Python 3.9+. No host ports are published.
Use --skip-build only to deliberately validate an already-built --image.
"""

import argparse
import json
from pathlib import Path
import subprocess
import time
import uuid


def require(condition, message):
    # Keep acceptance checks active even when Python is run with -O.
    if not condition:
        raise RuntimeError(message)


def docker(*args, timeout=30, check=True):
    result = subprocess.run(
        ["docker", *args], text=True, capture_output=True, timeout=timeout
    )
    if check and result.returncode:
        raise RuntimeError(
            f"docker {args}: exit {result.returncode}\n"
            f"{result.stdout}\n{result.stderr}"
        )
    return result


def inspect(name):
    return json.loads(docker("inspect", name).stdout)[0]


def verify(image):
    network = "parrot-verify-" + uuid.uuid4().hex[:12]
    containers = []
    created_network = False
    try:
        identity = docker("image", "inspect", "--format", "{{.Id}}", image).stdout.strip()
        print(f"Verifying {image} ({identity})", flush=True)
        # Record the attempt first: the daemon may commit creation even if the
        # CLI times out before its receipt. Removal tolerates a confirmed miss.
        created_network = True
        docker("network", "create", network)
        for name, seeds in [
            ("node-a", None),
            ("node-b", "node-a=node-a:9801"),
            ("unjoined", None),
        ]:
            full = network + "-" + name
            args = [
                "run", "-d", "--name", full, "--network", network,
                "--network-alias", name, "--label", "parrot.verify=" + network,
                "-e", "PARROT_NODE_ID=" + name,
                "-e", "PARROT_BIND=0.0.0.0:9801",
                "-e", "PARROT_SECURITY_MODE=insecure-development",
            ]
            if seeds:
                args += ["-e", "PARROT_SEEDS=" + seeds]
            # Record before creating: a client timeout can occur after creation.
            containers.append(full)
            docker(*args, image)

        deadline = time.monotonic() + 120
        while True:
            states = [inspect(name)["State"] for name in containers]
            if all(s.get("Health", {}).get("Status") == "healthy" for s in states):
                break
            require(all(s["Running"] for s in states), f"node exited: {states}")
            require(time.monotonic() < deadline, f"healthcheck deadline: {states}")
            time.sleep(1)
        for name in containers:
            require(
                docker("exec", name, "id", "-u").stdout.strip() == "1000",
                f"{name} must run as UID 1000",
            )
        print("PASS: three non-root containers passed image HEALTHCHECK", flush=True)

        probe = network + "-probe"
        containers.append(probe)
        result = docker(
            "run", "--name", probe, "--network", network,
            "--entrypoint", "/usr/local/bin/parrot-probe", image,
            "node-a=node-a:9801", "node-b=node-b:9801", "--full", "--insecure-development", timeout=150,
        )
        print(result.stdout, flush=True)
        require("parrot-probe FULL PASS" in result.stdout, "missing full probe result")

        negative = network + "-negative-probe"
        containers.append(negative)
        result = docker(
            "run", "--name", negative, "--network", network,
            "--entrypoint", "/usr/local/bin/parrot-probe", image,
            "node-a=node-a:9801", "unjoined=unjoined:9801", "--smoke", "--insecure-development",
            timeout=60, check=False,
        )
        print(result.stdout + result.stderr, flush=True)
        require(result.returncode == 1, "unjoined routing must fail with exit 1")
        require(
            "PASS P1 echo on unjoined" in result.stdout
            and "P2 cross-route node-a -> unjoined" in result.stderr
            and "RouteUnreachable" in result.stderr,
            "negative probe must fail at routing, not startup, DNS or direct echo",
        )

        for name in containers[:3]:
            docker("stop", "--time", "15", name, timeout=30)
            state = inspect(name)["State"]
            require(
                state["ExitCode"] == 0 and not state["OOMKilled"],
                f"SIGTERM must shut down cleanly: {name}: {state}",
            )
        print("PASS: SIGTERM exit 0; isolated-node routing rejected", flush=True)
    finally:
        failures = []
        # Diagnostics must not prevent cleanup of the other owned resources.
        for name in containers:
            try:
                result = docker("logs", name, check=False)
                if result.stdout or result.stderr:
                    print(f"CONTAINER LOG {name}\n{result.stdout}{result.stderr}", flush=True)
            except (OSError, subprocess.TimeoutExpired) as error:
                failures.append(f"logs {name}: {error}")
        for name in reversed(containers):
            try:
                result = docker("rm", "-f", name, check=False)
                if result.returncode and "No such container" not in result.stderr:
                    failures.append(result.stderr)
            except (OSError, subprocess.TimeoutExpired) as error:
                failures.append(f"remove {name}: {error}")
        if created_network:
            try:
                result = docker("network", "rm", network, check=False)
                if result.returncode and "No such network" not in result.stderr:
                    failures.append(result.stderr)
            except (OSError, subprocess.TimeoutExpired) as error:
                failures.append(f"remove network {network}: {error}")
        require(not failures, f"owned resource cleanup/diagnostics failed: {failures}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", default="parrot:verification")
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()
    if not args.skip_build:
        root = Path(__file__).resolve().parent.parent
        result = docker(
            "build", "--progress=plain", "-f", str(root / "deploy/Dockerfile"),
            "-t", args.image, str(root), timeout=1800,
        )
        print(result.stdout + result.stderr, flush=True)
    verify(args.image)


if __name__ == "__main__":
    main()
