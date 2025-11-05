#!/usr/bin/env python3
"""Exercise the real prebuilt-app Dockerfile on Linux with Docker and a C compiler.

Uses a private build context and uniquely named images/containers. No host ports,
shared source artifacts, existing containers or builder caches are removed.
"""
import json
import os
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import tempfile
import uuid


ROOT = Path(__file__).resolve().parents[1]


def run(*args, timeout=300, check=True):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f"{args}: exit {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


def verify():
    if platform.system() != "Linux":
        raise RuntimeError("run this native image acceptance on Linux")
    selected = {"x86_64": ("amd64", "x86_64", 183), "aarch64": ("arm64", "aarch64", 62)}
    if platform.machine() not in selected:
        raise RuntimeError(f"unsupported host architecture: {platform.machine()}")
    arch, rust_arch, wrong_machine = selected[platform.machine()]
    identity = "parrot-app-verify-" + uuid.uuid4().hex[:12]
    image = identity + ":test"
    container = identity + "-run"
    cleanup_errors = []
    try:
        with tempfile.TemporaryDirectory(prefix=identity) as directory:
            context = Path(directory)
            for filename in ["gw-app.Dockerfile", "gw-app.Dockerfile.dockerignore", "validate_app_binary.py"]:
                dest = context / "deploy/images" / filename
                dest.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / "deploy/images" / filename, dest)
            # Reproduce the repository's competing root ignore policy as well.
            shutil.copyfile(ROOT / ".dockerignore", context / ".dockerignore")
            source = context / "probe.c"
            source.write_text('#include <stdio.h>\nint main(void) { puts("PARROT_APP_IMAGE_OK"); return 0; }\n')
            binary = context / f"target/{rust_arch}-unknown-linux-musl/release/websearch"
            binary.parent.mkdir(parents=True)
            # Deliberately use a regular dynamic native ELF too: the gate must
            # check its bytes, not infer libc or machine from the directory name.
            run(os.environ.get("CC", "cc"), str(source), "-o", str(binary))
            original = binary.read_bytes()
            for triple in ["auto", ""]:
                if triple == "":
                    native = context / "target/release/websearch"
                    native.parent.mkdir(parents=True)
                    native.write_bytes(original)
                build = run("docker", "build", "--platform", "linux/" + arch,
                            "--build-arg", "TRIPLE=" + triple, "-f", str(context / "deploy/images/gw-app.Dockerfile"),
                            "-t", image, str(context), timeout=600)
                print(build.stdout + build.stderr, flush=True)
                metadata = json.loads(run("docker", "image", "inspect", image).stdout)[0]
                if (metadata["Os"], metadata["Architecture"]) != ("linux", arch):
                    raise RuntimeError("built image platform mismatch")
                result = run("docker", "run", "--rm", "--name", container, "--network", "none", image, timeout=30)
                if result.stdout.strip() != "PARROT_APP_IMAGE_OK":
                    raise RuntimeError(f"unexpected application output: {result.stdout!r}")
                print(f"PASS actual image build and native execution: TRIPLE={triple!r}", flush=True)
            changed = bytearray(original)
            struct.pack_into("<H", changed, 18, wrong_machine)
            binary.write_bytes(changed)
            rejected = run("docker", "build", "--platform", "linux/" + arch,
                           "-f", str(context / "deploy/images/gw-app.Dockerfile"), "-t", image,
                           str(context), timeout=600, check=False)
            if rejected.returncode == 0 or "does not match image architecture" not in rejected.stderr + rejected.stdout:
                raise RuntimeError(f"wrong-machine build did not fail at ELF gate: {rejected.stdout}\n{rejected.stderr}")
            print("PASS mislabeled binary rejected by real Docker build", flush=True)
    finally:
        for args in [("container", "rm", "-f", container), ("image", "rm", "-f", image)]:
            try:
                result = run("docker", *args, timeout=30, check=False)
                if result.returncode and "No such" not in result.stderr:
                    cleanup_errors.append(result.stderr)
            except (OSError, subprocess.TimeoutExpired) as error:
                cleanup_errors.append(str(error))
        if cleanup_errors:
            raise RuntimeError(f"cleanup incomplete for owned resources {identity}: {cleanup_errors}")


if __name__ == "__main__":
    verify()
