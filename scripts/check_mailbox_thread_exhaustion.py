#!/usr/bin/env python3
"""Run mailbox or native join thread-failure contracts in a bounded Linux container."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, help='already built Linux test binary')
    parser.add_argument('--image', default='ubuntu:24.04')
    parser.add_argument('--release', action='store_true', help='build the optimized test executable')
    parser.add_argument('--native-join', action='store_true', help='run the library native join retry contract')
    args = parser.parse_args()
    native_case = 'thread::native_budget::pid_exhaustion_contract::thread_exhaustion_retries_original_native_join'
    target = 'parrot' if args.native_join else 'test_mailbox_thread_exhaustion'
    binary = args.binary
    if binary is None:
        command = [
            'cargo', 'test', '-p', 'parrot', '--all-features',
            '--no-run', '--message-format=json',
        ]
        command.extend(['--lib'] if args.native_join else ['--test', target])
        if args.release:
            command.append('--release')
        build = subprocess.run(command, cwd=Path(__file__).resolve().parents[1],
                               capture_output=True, text=True)
        sys.stderr.write(build.stderr)
        if build.returncode:
            sys.stdout.write(build.stdout)
            return build.returncode
        artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith('{')]
        binaries = [item['executable'] for item in artifacts
                    if item.get('reason') == 'compiler-artifact'
                    and item.get('target', {}).get('name') == target
                    and item.get('executable')]
        if len(binaries) != 1:
            raise ValueError(f'expected exactly one {target} test executable')
        binary = Path(binaries[0])
    binary = binary.resolve(strict=True)
    container = 'parrot-mailbox-exhaustion-' + uuid.uuid4().hex
    try:
        result = subprocess.run([
            'docker', 'run', '--rm', '--init', '--name', container, '--pids-limit=64',
            '--ulimit', 'core=0:0', '--ulimit', 'nofile=65536:65536',
            '--network=none', '-v', str(binary.parent) + ':/tests:ro',
            '--entrypoint', '/tests/' + binary.name, args.image, '--ignored', '--test-threads=1',
            *(['--exact', native_case, '--nocapture'] if args.native_join else []),
        ], capture_output=True, text=True, timeout=120)
    finally:
        subprocess.run(['docker', 'rm', '--force', container],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    if result.returncode:
        return result.returncode
    actual = re.findall(r'^test (\S+) \.\.\. ok$', result.stdout, re.MULTILINE)
    if args.native_join:
        if actual != [native_case] or not re.search(
                r'^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; \d+ filtered out;',
                result.stdout, re.MULTILINE):
            raise ValueError('the native join retry contract did not execute')
        return 0
    expected = {
        'thread_exhaustion_closes_' + kind for kind in ('mpsc', 'spsc', 'ring')
    } | {
        'thread_exhaustion_preserves_' + kind + '_failures' for kind in ('mpsc', 'spsc', 'ring')
    } | {
        'thread_exhaustion_rejects_' + kind for kind in ('mpsc', 'spsc', 'ring')
    } | {
        'thread_exhaustion_cancels_failed_tokio_job'
    }
    if len(actual) != len(expected) or set(actual) != expected:
        raise ValueError('missing, duplicate or unexpected mailbox exhaustion contracts')
    if not re.search(r'^test result: ok\. 10 passed; 0 failed; 0 ignored; 0 measured; '
                     r'0 filtered out;', result.stdout, re.MULTILINE):
        raise ValueError('mailbox exhaustion suite did not execute all ten contracts')
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1)
