#!/usr/bin/env python3
"""List source workspace lockfiles, rejecting absent or git-ignored inputs.

By default Cargo locates workspace roots without resolving dependencies. Git's
source selection excludes generated trees; review snapshots and vendored code
are not independent product workspaces. --no-index also catches ignore rules
masked by an already tracked lockfile in a developer checkout.
--check-resolution additionally checks every workspace with cargo metadata
--locked before returning any audit input. CARGO_NET_OFFLINE is honored.
"""
import argparse
from pathlib import Path
import subprocess
import sys


def workspace_lockfiles(root, check_resolution=False):
    root = Path(root).resolve()
    selected = subprocess.run(
        ['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z'],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout.split('\0')
    manifests = sorted({p for p in selected if Path(p).name == 'Cargo.toml'
                        and not p.startswith(('docs/review/', 'vendor/'))})
    if not manifests:
        raise ValueError('no source Cargo manifests found')
    locks = set()
    for manifest in manifests:
        located = subprocess.run(
            ['cargo', 'locate-project', '--workspace', '--message-format', 'plain',
             '--manifest-path', str(root / manifest)], cwd=root,
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        workspace = Path(located).resolve()
        lock = workspace.with_name('Cargo.lock').relative_to(root)
        locks.add(lock.as_posix())
    errors = []
    for lock in sorted(locks):
        if not (root / lock).is_file():
            errors.append('missing workspace lockfile: ' + lock)
        ignored = subprocess.run(
            ['git', 'check-ignore', '--no-index', '--quiet', '--', lock], cwd=root,
            capture_output=True, text=True,
        )
        if ignored.returncode == 0:
            errors.append('git-ignored workspace lockfile: ' + lock)
        elif ignored.returncode != 1:
            raise subprocess.CalledProcessError(ignored.returncode, ignored.args,
                                                ignored.stdout, ignored.stderr)
    if errors:
        raise ValueError('\n'.join(errors))
    if check_resolution:
        for lock in sorted(locks):
            resolved = subprocess.run(
                ['cargo', 'metadata', '--locked', '--format-version', '1',
                 '--manifest-path', str(root / Path(lock).with_name('Cargo.toml'))],
                cwd=root, capture_output=True, text=True, timeout=120,
            )
            if resolved.returncode:
                errors.append('workspace lockfile resolution failed: ' + lock + '\n'
                              + resolved.stderr.strip())
        if errors:
            raise ValueError('\n'.join(errors))
    return sorted(locks)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--check-resolution', action='store_true',
                        help='require cargo metadata --locked to resolve every workspace')
    args = parser.parse_args()
    try:
        print('\n'.join(workspace_lockfiles(args.root, args.check_resolution)))
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(error, file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr, file=sys.stderr, end='')
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
