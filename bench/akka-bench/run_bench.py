#!/usr/bin/env python3
"""Compile privately, prepare independent answers, then run actual Akka Typed workloads."""
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
    parser.add_argument('--dispatcher-threads', type=int, default=15, help='fixed Akka dispatcher thread count (default: 15)')
    args = parser.parse_args()
    if not 1 <= args.dispatcher_threads <= 1024:
        parser.error('--dispatcher-threads must be between 1 and 1024')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    with tempfile.TemporaryDirectory(prefix='parrot-akka-bench-') as temporary:
        work = Path(temporary)

        def execute(command):
            run(command, work)

        classpath = os.pathsep.join([str(ROOT / 'lib' / '*'), str(work)])
        execute(['javac', '-Xlint:all', '-Werror', '-cp', classpath, '-d', work,
                 *sorted((ROOT / 'src').glob('*.java'))])
        execute(['rustc', '--edition=2021', '-O', '-D', 'warnings', ROOT / 'oracle.rs', '-o', work / 'oracle'])
        java = ['java', '-Xms1g', '-Xmx1g', '-XX:+UseZGC',
                '-Dakka.actor.default-dispatcher.fork-join-executor.parallelism-min=' + str(args.dispatcher_threads),
                '-Dakka.actor.default-dispatcher.fork-join-executor.parallelism-max=' + str(args.dispatcher_threads),
                '-Dakka.actor.default-dispatcher.fork-join-executor.parallelism-factor=1.0', '-cp', classpath]
        print('DISPATCHER_THREADS=' + str(args.dispatcher_threads), flush=True)
        if args.check:
            execute([*java, 'BenchContracts'])
        mode = 'check' if args.check else 'full'
        execute([*java, 'AkkaBench', 'keys', mode, work / 'keys'])
        execute([work / 'oracle', work / 'keys', work / 'expected.txt'])
        print('ORACLE_SHA256=' + hashlib.sha256((work / 'expected.txt').read_bytes()).hexdigest(), flush=True)
        execute([*java, 'AkkaBench', 'run', mode, work / 'expected.txt'])


if __name__ == '__main__':
    main()
