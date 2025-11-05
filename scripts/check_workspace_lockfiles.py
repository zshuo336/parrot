#!/usr/bin/env python3
"""List source workspace lockfiles, rejecting absent or git-ignored inputs.

Cargo locates workspace roots without building or resolving dependencies. Git's
source selection excludes generated trees; review snapshots and vendored code
are not independent product workspaces. --no-index also catches ignore rules
masked by an already tracked lockfile in a developer checkout.
"""
import argparse
from pathlib import Path
import subprocess
import sys


def workspace_lockfiles(root):
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
    return sorted(locks)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    try:
        print('\n'.join(workspace_lockfiles(args.root)))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(error, file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr, file=sys.stderr, end='')
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
