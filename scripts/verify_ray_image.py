#!/usr/bin/env python3
"""Build the actual Ray image and exercise offline environments inside it.

Requires Linux Docker. Uses only uniquely named owned resources and a read-only
test harness mount; no host ports or external container network are required.
"""
from pathlib import Path
import json
import shutil
import tempfile
import uuid

from verify_app_image import run
from check_packaged_protocol import protocol_files


ROOT = Path(__file__).resolve().parents[1]


def prepare_harness(destination, protocol_root=Path("/gw")):
    # Copy tests and expected hashes, never the implementation under test. The
    # existing harness imports through interop/python, so link that package to
    # the image location; missing image files cannot fall back to host sources.
    for relative in [
        "apps/test_support/run_ray_environments.py", "apps/test_support/run_ray_lifecycle.py",
        "apps/test_support/run_ray_shutdown.py",
        "apps/test_support/owned_process.py",
        "apps/test_support/run_ray_network_shutdown.py",
        "apps/test_support/run_ray_gateway_retirement.py",
        "apps/test_support/run_ray_artifact_recovery.py",
        "apps/test_support/run_ray_setup_hooks.py",
        "apps/test_support/run_ray_task_retirement.py",
        "apps/test_support/run_ray_job_cancellation.py",
        "apps/test_support/run_ray_job_output.py",
        "tools/parrot-obs/process_owner.py", "tools/parrot-obs/process_session.py", "tools/parrot-obs/gateway_worker.py",
        "apps/test_support/reply_proxy.py", "apps/websearch/tests/run_integration.py",
        "apps/websearch/tests/ray_gateway.py", "scripts/tests/test_ray_image_runtime.py",
        "scripts/check_packaged_protocol.py", "deploy/images/requirements-ray-build.lock",
        "deploy/images/requirements-ray.lock",
    ]:
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, target)
    package = destination / "interop/python/parrot_protocol"
    package.parent.mkdir(parents=True)
    package.symlink_to(protocol_root / "parrot_protocol", target_is_directory=True)
    (destination / "expected-protocol.json").write_text(
        json.dumps(protocol_files(ROOT / "interop/python/parrot_protocol"), sort_keys=True) + "\n")


def verify():
    identity = "parrot-ray-verify-" + uuid.uuid4().hex[:12]
    image = identity + ":test"
    container = identity + "-run"
    try:
        result = run("docker", "build", "-f", str(ROOT / "deploy/images/gw-ray.Dockerfile"),
                     "-t", image, str(ROOT), timeout=900)
        print(result.stdout + result.stderr, flush=True)
        with tempfile.TemporaryDirectory(prefix=identity) as directory:
            harness = Path(directory)
            prepare_harness(harness)
            for arguments, marker in [
                (["scripts/check_packaged_protocol.py", "--root", "/gw/parrot_protocol",
                  "--manifest", "expected-protocol.json"], "RAY_IMAGE_PROTOCOL_OK"),
                (["scripts/tests/test_ray_image_runtime.py"], "OK"),
                (["apps/test_support/run_ray_environments.py"],
                 "RAY_ENV_OK failed environment setup has no factory effects and permits immediate name reuse"),
                (["apps/test_support/run_ray_lifecycle.py"],
                 "RAY_LIFECYCLE_OK failed native pool retires created prefix"),
                (["apps/test_support/run_ray_shutdown.py"], "RAY_SHUTDOWN_OK"),
                (["apps/test_support/run_ray_network_shutdown.py"], "RAY_NETWORK_OK"),
                (["apps/test_support/run_ray_gateway_retirement.py"], "RAY_RETIREMENT_OK blocked:"),
                (["apps/test_support/run_ray_artifact_recovery.py"], "RAY_RECOVERY_OK driver:"),
                (["apps/test_support/run_ray_setup_hooks.py"], "RAY_HOOK_OK callable:"),
                (["apps/test_support/run_ray_task_retirement.py"], "RAY_TASK_OK"),
                (["apps/test_support/run_ray_job_cancellation.py"], "RAY_JOB_CANCEL_OK guard-shutdown:"),
                (["apps/test_support/run_ray_job_output.py"], "RAY_JOB_OUTPUT_OK startup-error:"),
            ]:
                result = run("docker", "run", "--rm", "--init", "--name", container, "--network", "none",
                             "--shm-size=256m", "--mount", f"type=bind,src={harness},dst=/repo,readonly",
                             "--workdir", "/repo", "-e", "RAY_USAGE_STATS_ENABLED=0", image,
                             "python3", "-u", *arguments, timeout=360)
                print(result.stdout + result.stderr, flush=True)
                if marker not in result.stdout + result.stderr:
                    raise RuntimeError(f"missing acceptance marker for {arguments[0]}")
    finally:
        failures = []
        for args in [("container", "rm", "-f", container), ("image", "rm", "-f", image)]:
            try:
                result = run("docker", *args, timeout=30, check=False)
                if result.returncode and "No such" not in result.stderr:
                    failures.append(result.stderr)
            except Exception as error:
                failures.append(str(error))
        if failures:
            raise RuntimeError(f"cleanup incomplete for owned resources {identity}: {failures}")


if __name__ == "__main__":
    verify()
