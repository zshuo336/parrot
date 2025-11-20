#!/usr/bin/env python3
"""Run a command with a deadline, retiring its owned process group before reaping."""
import argparse
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'apps/test_support'))
from owned_process import OwnedProcess, _darwin_only_zombie_leader


def seconds(value):
    try:
        duration = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError('seconds must be a finite nonnegative number') from error
    if not math.isfinite(duration) or duration < 0:
        raise argparse.ArgumentTypeError('seconds must be a finite nonnegative number')
    return duration


def run(command, duration):
    interrupted = None

    def remember_signal(signum, _frame):
        nonlocal interrupted
        interrupted = signum

    previous = {sig: signal.signal(sig, remember_signal)
                for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        with OwnedProcess(command) as child:
            deadline = time.monotonic() + duration
            while True:
                if interrupted is not None:
                    return 128 + interrupted
                code = child.poll()
                if code is not None:
                    return code if code >= 0 else 128 - code
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                time.sleep(min(.01, remaining))

            # The unreaped session leader pins this process-group identity even
            # if it exits during the grace period. close() sends the final KILL.
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            except PermissionError:
                if not (sys.platform == 'darwin' and child.poll() is not None
                        and _darwin_only_zombie_leader(child.pid)):
                    raise
            try:
                child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
            print(f'[with_timeout] command timed out (>{duration:g}s)', file=sys.stderr)
            return 124
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('seconds', type=seconds)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if not args.command:
        parser.error('a command is required')
    try:
        return run(args.command, args.seconds)
    except FileNotFoundError as error:
        print(f'[with_timeout] {error}', file=sys.stderr)
        return 127
    except OSError as error:
        print(f'[with_timeout] {error}', file=sys.stderr)
        return 126


if __name__ == '__main__':
    raise SystemExit(main())
