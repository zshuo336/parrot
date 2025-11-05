#!/usr/bin/env python3
"""Compile privately, prepare independent answers, then run actual BEAM workloads."""
import argparse
import hashlib
import os
from pathlib import Path
import signal
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent


def run(command, directory):
    process = None
    try:
        previous = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
        try:
            process = subprocess.Popen(list(map(str, command)), cwd=directory, start_new_session=True)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous)
        code = process.wait()
        if code:
            raise subprocess.CalledProcessError(code, command)
    finally:
        previous = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
        try:
            if process is not None:
                if process.poll() is None:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                process.wait(timeout=10)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument('--check', action='store_true', help='failure contracts and all twenty small scenarios')
    modes.add_argument('--full', action='store_true', help='all twenty full workloads (default)')
    parser.add_argument('--scale', type=int, default=1, help='explicit iteration divisor; 1 preserves Rust work counts')
    args = parser.parse_args()
    if not 1 <= args.scale <= 1000000:
        parser.error('--scale must be between 1 and 1000000')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    with tempfile.TemporaryDirectory(prefix='parrot-erlang-bench-') as temporary:
        work = Path(temporary)

        def execute(command):
            run(command, work)

        execute(['erlc', '-Werror', '-o', work, ROOT / 'bench_runtime.erl',
                 ROOT / 'parrot_bench.erl', ROOT / 'bench_contracts.erl'])
        execute(['rustc', '--edition=2021', '-O', '-D', 'warnings', ROOT / 'oracle.rs', '-o', work / 'oracle'])
        if args.check:
            execute(['erl', '-noshell', '-pa', work, '-s', 'bench_contracts', 'main', '-s', 'init', 'stop'])
        mode = 'contract' if args.check else 'full'
        execute(['escript', ROOT / 'run_bench.escript', work, 'keys', mode, str(args.scale), work / 'keys'])
        execute([work / 'oracle', work / 'keys', work / 'expected.term'])
        print('ORACLE_SHA256=' + hashlib.sha256((work / 'expected.term').read_bytes()).hexdigest(), flush=True)
        execute(['escript', ROOT / 'run_bench.escript', work, 'run', mode, str(args.scale), work / 'expected.term'])


if __name__ == '__main__':
    main()
