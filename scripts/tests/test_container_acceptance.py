"""Exercise verifier failure paths; Docker responses are controlled, not a daemon."""
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import verify_container
import verify_ray_image
from check_packaged_protocol import protocol_files


def completed(args, stdout="", stderr="", code=0):
    return subprocess.CompletedProcess(args, code, stdout, stderr)


class ContainerAcceptanceTests(unittest.TestCase):
    def test_packaged_protocol_checker_rejects_missing_changed_and_extra_inputs(self):
        source = verify_ray_image.ROOT / "interop/python/parrot_protocol"
        checker = verify_ray_image.ROOT / "scripts/check_packaged_protocol.py"
        for defect in [None, "missing", "changed", "extra", "symlink", "root-symlink", "absent"]:
            with self.subTest(defect=defect), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                package = root / "image/parrot_protocol"
                shutil.copytree(source, package, ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
                manifest = root / "expected.json"
                manifest.write_text(json.dumps(protocol_files(source)))
                if defect == "missing":
                    (package / "ray_hosts.py").unlink()
                elif defect == "changed":
                    with (package / "wire.py").open("a") as out:
                        out.write("\n# stale image implementation\n")
                elif defect == "extra":
                    (package / "unexpected.py").write_text("# extra implementation\n")
                elif defect == "symlink":
                    (package / "wire.py").unlink()
                    (package / "wire.py").symlink_to(source / "wire.py")
                elif defect == "root-symlink":
                    external = root / "external-package"
                    package.rename(external)
                    package.symlink_to(external, target_is_directory=True)
                elif defect == "absent":
                    shutil.rmtree(package)
                result = subprocess.run([sys.executable, str(checker), "--root", str(package),
                                         "--manifest", str(manifest)], capture_output=True, text=True, timeout=10,
                                        env=dict(os.environ, PYTHONPATH=str(source.parent)))
                # A valid host package is deliberately available in PYTHONPATH.
                if defect is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(str(package / "__init__.py"), result.stdout)
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertNotIn("RAY_IMAGE_PROTOCOL_OK", result.stdout)

    def test_real_harness_imports_packaged_code_and_missing_image_cannot_fall_back(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = root / "image/parrot_protocol"
            shutil.copytree(verify_ray_image.ROOT / "interop/python/parrot_protocol", package,
                            ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
            harness = root / "harness"
            verify_ray_image.prepare_harness(harness, package.parent)
            code = ("import run_ray_lifecycle, run_ray_shutdown, parrot_protocol; "
                    "from pathlib import Path; print(Path(parrot_protocol.__file__).resolve())")
            env = dict(os.environ, PYTHONPATH=str(harness / "apps/test_support"))
            for present in [True, False]:
                if not present:
                    (package / "wire.py").unlink()
                    shutil.rmtree(package / "__pycache__", ignore_errors=True)
                result = subprocess.run([sys.executable, "-c", code], cwd=harness,
                                        env=env, capture_output=True, text=True, timeout=10)
                if present:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.strip(), str((package / "__init__.py").resolve()))
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("parrot_protocol.wire", result.stderr)

    def test_packaged_owner_runs_without_checkout_and_missing_dependency_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            harness = root / 'harness'
            verify_ray_image.prepare_harness(harness)
            tool = harness / 'tools/parrot-obs'
            script = """import sys,time,json
from pathlib import Path
sys.path.insert(0,sys.argv[1])
from process_owner import OwnedChild
owner=OwnedChild('packaged',[sys.executable,'-c','pass'],Path(sys.argv[2]))
try:
 deadline=time.monotonic()+5
 while owner.code is None:
  owner.poll()
  if time.monotonic()>deadline:raise TimeoutError('packaged child')
  time.sleep(.01)
 owner.finish_stop()
 if not owner.retired or owner.guardian_exit is None:raise RuntimeError('incomplete receipt')
 print('PACKAGED_OWNER_RETIRED')
finally:
 owner.finish_stop()
"""
            environment = dict(os.environ, PYTHONDONTWRITEBYTECODE='1',
                               PYTHONPATH=str(verify_ray_image.ROOT / 'tools/parrot-obs'))
            for present in [True, False]:
                if not present:
                    (tool / 'process_session.py').unlink()
                result = subprocess.run([sys.executable, '-c', script, str(tool), str(root / 'owner.log')],
                                        cwd=root, env=environment, capture_output=True, text=True, timeout=15)
                if present:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn('PACKAGED_OWNER_RETIRED', result.stdout)
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('process_session.py', result.stderr)

    def test_ray_acceptance_does_not_mount_repository_protocol_sources(self):
        mounts = []
        executed = []

        def command(*args, **kwargs):
            if args[:2] == ("docker", "run") and "--mount" in args:
                self.assertIn("--init", args, "Ray's orphaned descendants require a container reaper")
                executed.append(args[-1])
                mount = args[args.index("--mount") + 1]
                fields = dict(part.split("=", 1) for part in mount.split(",") if "=" in part)
                source = Path(fields["src"])
                mounts.append(source)
                self.assertNotEqual(source.resolve(), verify_ray_image.ROOT.resolve())
                self.assertFalse((source / ".git").exists())
                package = source / "interop/python/parrot_protocol"
                self.assertTrue(package.is_symlink(), "protocol source must come from the image")
                self.assertEqual(str(package.readlink()), "/gw/parrot_protocol")
                self.assertTrue((source / "apps/test_support/run_ray_environments.py").is_file())
                self.assertTrue((source / "apps/test_support/run_ray_shutdown.py").is_file())
                self.assertTrue((source / "apps/test_support/owned_process.py").is_file())
                self.assertTrue((source / "apps/test_support/run_ray_network_shutdown.py").is_file())
                self.assertTrue((source / "apps/test_support/run_ray_gateway_retirement.py").is_file())
                self.assertTrue((source / "apps/test_support/run_ray_artifact_recovery.py").is_file())
                self.assertTrue((source / "apps/test_support/run_ray_setup_hooks.py").is_file())
                self.assertTrue((source / "apps/test_support/run_ray_task_retirement.py").is_file())
                self.assertTrue((source / "apps/test_support/run_ray_job_cancellation.py").is_file())
                self.assertTrue((source / "apps/test_support/run_ray_job_output.py").is_file())
                self.assertTrue((source / "tools/parrot-obs/process_owner.py").is_file())
                self.assertTrue((source / "tools/parrot-obs/gateway_worker.py").is_file())
            return completed(args, "OK\nRAY_IMAGE_PROTOCOL_OK\nRAY_SHUTDOWN_OK\nRAY_NETWORK_OK\nRAY_RETIREMENT_OK blocked:\n"
                             "RAY_ENV_OK failed environment setup has no factory effects and permits immediate name reuse\n"
                             "RAY_LIFECYCLE_OK failed native pool retires created prefix\nRAY_RECOVERY_OK driver:\nRAY_HOOK_OK callable:\nRAY_TASK_OK\nRAY_JOB_CANCEL_OK guard-shutdown:\nRAY_JOB_OUTPUT_OK startup-error:\n")

        with patch.object(verify_ray_image, "run", side_effect=command), contextlib.redirect_stdout(io.StringIO()):
            verify_ray_image.verify()
        self.assertTrue(mounts, "actual runtime tests were never selected")
        self.assertIn("apps/test_support/run_ray_shutdown.py", executed)
        self.assertIn("apps/test_support/run_ray_network_shutdown.py", executed)
        self.assertIn("apps/test_support/run_ray_gateway_retirement.py", executed)
        self.assertIn("apps/test_support/run_ray_artifact_recovery.py", executed)
        self.assertIn("apps/test_support/run_ray_setup_hooks.py", executed)
        self.assertIn("apps/test_support/run_ray_task_retirement.py", executed)
        self.assertIn("apps/test_support/run_ray_job_cancellation.py", executed)
        self.assertIn("apps/test_support/run_ray_job_output.py", executed)
        self.assertTrue(all(not path.exists() for path in mounts), "owned harness was not cleaned up")

    def test_failed_protocol_gate_stops_later_tests_and_cleans_owned_resources(self):
        calls = []
        def command(*args, **kwargs):
            calls.append(args)
            if args[:2] == ("docker", "run"):
                self.assertIn("scripts/check_packaged_protocol.py", args)
                raise RuntimeError("packaged protocol missing")
            return completed(args)
        with patch.object(verify_ray_image, "run", side_effect=command), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "packaged protocol missing"):
                verify_ray_image.verify()
        self.assertEqual(sum(c[:2] == ("docker", "run") for c in calls), 1)
        self.assertEqual(calls[-2][:3], ("docker", "container", "rm"))
        self.assertEqual(calls[-1][:3], ("docker", "image", "rm"))

    def test_unavailable_daemon_during_cleanup_does_not_report_success(self):
        def docker(*args, **kwargs):
            if args[:2] == ("image", "inspect"):
                return completed(args, "sha256:fixture")
            if args[:2] == ("network", "create"):
                raise subprocess.TimeoutExpired(["docker", *args], 30)
            if args[:2] == ("network", "rm"):
                return completed(args, stderr="Cannot connect to Docker daemon", code=1)
            raise AssertionError(args)
        with patch.object(verify_container, "docker", side_effect=docker), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "Cannot connect to Docker daemon") as error:
                verify_container.verify("fixture")
        self.assertIsInstance(error.exception.__context__, subprocess.TimeoutExpired)

    def test_network_create_receipt_timeout_still_removes_owned_network(self):
        calls = []
        live = set()

        def docker(*args, **kwargs):
            calls.append(args)
            if args[:2] == ("image", "inspect"):
                return completed(args, "sha256:fixture")
            if args[:2] == ("network", "create"):
                live.add(args[2])  # daemon committed, CLI receipt was lost
                raise subprocess.TimeoutExpired(["docker", *args], 30)
            if args[:2] == ("network", "rm"):
                self.assertIn(args[2], live)
                live.remove(args[2])
                return completed(args)
            raise AssertionError(f"unexpected Docker operation: {args}")

        with patch.object(verify_container, "docker", side_effect=docker), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(subprocess.TimeoutExpired):
                verify_container.verify("fixture")
        self.assertFalse(live, "network survived a lost create receipt")
        self.assertEqual(len(calls), 3)

    def test_network_create_failure_without_side_effect_preserves_original_error(self):
        def docker(*args, **kwargs):
            if args[:2] == ("image", "inspect"):
                return completed(args, "sha256:fixture")
            if args[:2] == ("network", "create"):
                raise RuntimeError("creation denied")
            if args[:2] == ("network", "rm"):
                return completed(args, stderr="Error response from daemon: No such network: " + args[2], code=1)
            raise AssertionError(args)
        with patch.object(verify_container, "docker", side_effect=docker), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "^creation denied$"):
                verify_container.verify("fixture")

    def test_failed_image_inspection_does_not_remove_unattempted_resources(self):
        with patch.object(verify_container, "docker", side_effect=RuntimeError("missing image")) as docker:
            with self.assertRaisesRegex(RuntimeError, "missing image"):
                verify_container.verify("fixture")
        self.assertEqual(docker.call_count, 1)


if __name__ == "__main__":
    unittest.main()
