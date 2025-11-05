#!/usr/bin/env python3
"""Build an independent oracle, then run one owned local Ray cluster."""
import argparse
import hashlib
import os
from pathlib import Path
import signal
import secrets
import subprocess
import sys
import tempfile

from bench_plan import Plan

ROOT = Path(__file__).resolve().parent


def interrupted(number, _frame):
    raise SystemExit(128 + number)


def run(command, directory, environment):
    process = None
    try:
        previous = signal.pthread_sigmask(
            signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM}
        )
        try:
            # This launcher is single-threaded. Restore the child's signal mask so
            # its SIGTERM handler can close Ray before escalation kills its group.
            process = subprocess.Popen(
                list(map(str, command)),
                cwd=directory,
                env=environment,
                start_new_session=True,
                preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_SETMASK, previous),
            )
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous)
        code = process.wait(timeout=6 * 3600)
        if code:
            raise subprocess.CalledProcessError(code, command)
    finally:
        previous = signal.pthread_sigmask(
            signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM}
        )
        try:
            if process is not None:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=90)
                    except subprocess.TimeoutExpired:
                        pass
                # Even an exited leader may leave owned descendants in its group.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=10)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument(
        "--check",
        action="store_true",
        help="behavior contracts plus all twenty small scenarios",
    )
    modes.add_argument(
        "--full", action="store_true", help="all twenty full workloads (default)"
    )
    parser.add_argument(
        "--scale",
        default="1",
        help="explicit positive divisor, default 1; historical ratio is 85.9375",
    )
    parser.add_argument(
        "--cpus",
        type=int,
        default=15,
        help="Ray logical CPU resource capacity (not an OS CPU limit)",
    )
    parser.add_argument("--object-store-bytes", type=int, default=1_500_000_000)
    args = parser.parse_args()
    try:
        plan = Plan(args.check, args.scale)
    except (ValueError, ZeroDivisionError) as error:
        parser.error(str(error))
    if not (4 if args.check else 7) <= args.cpus <= 1024:
        parser.error("CPU capacity must be >=4 for check or >=7 for full, and <=1024")
    if not 80 * 1024 * 1024 <= args.object_store_bytes <= 64 * 1024**3:
        parser.error("object store must be between 80 MiB and 64 GiB")
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    # Short path also fits macOS Unix-domain socket path limits.
    with tempfile.TemporaryDirectory(prefix="prb-", dir="/tmp") as temporary:
        work = Path(temporary)
        token_path = work / "auth_token"
        descriptor = os.open(token_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w") as token_file:
            token_file.write(secrets.token_hex(32))
        environment = dict(
            os.environ, RAY_AUTH_MODE="token", RAY_AUTH_TOKEN_PATH=str(token_path)
        )
        environment.pop("RAY_AUTH_TOKEN", None)

        def execute(command):
            run(command, work, environment)

        mode = "check" if args.check else "full"
        execute(
            [
                "rustc",
                "--edition=2021",
                "-O",
                "-D",
                "warnings",
                ROOT / "oracle.rs",
                "-o",
                work / "oracle",
            ]
        )
        execute(
            [
                sys.executable,
                ROOT / "ray_bench.py",
                "keys",
                mode,
                str(plan.scale),
                work / "keys",
            ]
        )
        execute([work / "oracle", work / "keys", work / "expected.txt"])
        print(
            "ORACLE_SHA256="
            + hashlib.sha256((work / "expected.txt").read_bytes()).hexdigest(),
            flush=True,
        )
        print(
            f"RAY_RESOURCES cpus={args.cpus} object_store_bytes={args.object_store_bytes} mode={mode} scale={plan.scale}",
            flush=True,
        )
        execute(
            [
                sys.executable,
                ROOT / "ray_bench.py",
                "run",
                mode,
                str(plan.scale),
                work / "expected.txt",
                "--session-dir",
                work / "ray",
                "--cpus",
                args.cpus,
                "--object-store-bytes",
                args.object_store_bytes,
            ]
        )


if __name__ == "__main__":
    main()
