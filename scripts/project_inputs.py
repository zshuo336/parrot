#!/usr/bin/env python3
"""Export current Git-selected validation inputs, including dirty submodules.

Python 3.9+, standard library only. Selection includes tracked and nonignored
untracked files, except historical docs/review evidence. It is not a proof of
complete build/runtime dependency closure: validate the exported tree separately.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import subprocess
import sys


EXCLUSIONS = ('docs/review',)
SCHEMA = 'parrot-project-inputs-v1'


class InputError(ValueError):
    pass


def git(root, *arguments):
    result = subprocess.run(['git', '-C', str(root), *arguments],
                            capture_output=True, check=True)
    return result.stdout


def valid_name(name):
    if not isinstance(name, str) or not name or '\\' in name:
        raise InputError('invalid portable input path: ' + repr(name))
    parts = PurePosixPath(name).parts
    if (PurePosixPath(name).is_absolute() or any(p in {'.', '..', '.git'} for p in parts)
            or PurePosixPath(name).as_posix() != name):
        raise InputError('invalid portable input path: ' + repr(name))
    try:
        name.encode('utf-8')
    except UnicodeError as error:
        raise InputError('input paths must be UTF-8: ' + repr(name)) from error
    return name


def excluded(name):
    return any(name == prefix or name.startswith(prefix + '/') for prefix in EXCLUSIONS)


def regular_file(root, name):
    valid_name(name)
    current = root
    for part in PurePosixPath(name).parts:
        current = current / part
        if current.is_symlink():
            raise InputError('symlink input is unsupported: ' + name)
    if not stat.S_ISREG(current.stat().st_mode):
        raise InputError('input is not a regular file: ' + name)
    return current


def discover(root):
    root = Path(root).resolve()
    selected = set()

    def repository(directory, prefix=''):
        top = Path(os.fsdecode(git(directory, 'rev-parse', '--show-toplevel')).rstrip('\n')).resolve()
        if top != directory:
            raise InputError('uninitialized or invalid Git submodule: ' + (prefix or '.'))
        submodules = set()
        for entry in git(directory, 'ls-files', '--stage', '-z').split(b'\0'):
            if not entry:
                continue
            header, path = entry.split(b'\t', 1)
            mode, _, stage = header.split()
            name = valid_name(os.fsdecode(path))
            if stage != b'0':
                raise InputError('unresolved Git conflict: ' + prefix + name)
            if mode == b'160000':
                submodules.add(name)
        pathspecs = [':(top,literal,exclude)' + name[len(prefix):]
                     for name in EXCLUSIONS if name.startswith(prefix)]
        names = git(directory, 'ls-files', '--cached', '--others', '--exclude-standard', '-z',
                    '--', '.', *pathspecs)
        for raw in sorted(set(names.split(b'\0')) - {b''}):
            name = os.fsdecode(raw)
            full = prefix + name
            if excluded(full):
                continue
            valid_name(full)
            if name in submodules:
                child = directory / name
                if child.is_symlink() or not child.is_dir():
                    raise InputError('uninitialized or invalid Git submodule: ' + full)
                repository(child, full + '/')
            else:
                regular_file(root, full)
                selected.add(full)

    repository(root)
    if not selected:
        raise InputError('no project input files found')
    return sorted(selected)


def identity(path):
    sha = hashlib.sha256()
    size = 0
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            sha.update(block)
            size += len(block)
    return {'sha256': sha.hexdigest(), 'bytes': size,
            'executable': bool(path.stat().st_mode & 0o111)}


def no_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise InputError('duplicate manifest key: ' + key)
        result[key] = value
    return result


def load_manifest(path):
    with Path(path).open(encoding='utf-8') as source:
        manifest = json.load(source, object_pairs_hook=no_duplicate_keys)
    if (not isinstance(manifest, dict) or manifest.get('schema') != SCHEMA
            or manifest.get('exclusions') != list(EXCLUSIONS)
            or not isinstance(manifest.get('files'), dict) or not manifest['files']):
        raise InputError('invalid input manifest schema or selection policy')
    for name, item in manifest['files'].items():
        valid_name(name)
        if excluded(name):
            raise InputError('manifest includes excluded evidence: ' + name)
        if (not isinstance(item, dict) or set(item) != {'sha256', 'bytes', 'executable'}
                or type(item['bytes']) is not int or item['bytes'] < 0
                or type(item['executable']) is not bool
                or not isinstance(item['sha256'], str) or len(item['sha256']) != 64
                or any(c not in '0123456789abcdef' for c in item['sha256'])):
            raise InputError('invalid file identity: ' + name)
    return manifest


def verify(root, manifest, selection=False, exact=False):
    root = Path(root).resolve()
    files = manifest['files']
    if selection:
        actual = set(discover(root))
    elif exact:
        actual = set()
        for current, directories, names in os.walk(root, followlinks=False):
            for name in directories:
                if (Path(current) / name).is_symlink():
                    raise InputError('symlink directory in export: ' + str(Path(current) / name))
            for name in names:
                actual.add((Path(current) / name).relative_to(root).as_posix())
    else:
        actual = set(files)
    if actual != set(files):
        raise InputError('input set mismatch; missing=' + repr(sorted(set(files) - actual))
                         + '; extra=' + repr(sorted(actual - set(files))))
    for name, expected in files.items():
        if identity(regular_file(root, name)) != expected:
            raise InputError('input identity mismatch: ' + name)


def export(root, output):
    root, output = Path(root).resolve(), Path(output).absolute()
    names = discover(root)
    # A fresh directory avoids merging an old checkout into the new snapshot.
    output.mkdir(parents=True, exist_ok=False)
    source = output / 'source'
    source.mkdir()
    files = {}
    for name in names:
        original = regular_file(root, name)
        expected = identity(original)
        destination = source / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(original, destination)
        destination.chmod(0o755 if expected['executable'] else 0o644)
        if identity(destination) != expected:
            raise InputError('input changed during export: ' + name)
        files[name] = expected
    manifest = {'schema': SCHEMA, 'exclusions': list(EXCLUSIONS), 'files': files}
    # Detect selection, byte or executable changes during the whole copy.
    verify(root, manifest, selection=True)
    verify(source, manifest, exact=True)
    (output / 'manifest.json').write_text(json.dumps(manifest, indent=2, sort_keys=True) + '\n',
                                          encoding='utf-8')
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_subparsers(dest='action', required=True)
    create = actions.add_parser('export', help='create a fresh source/ and manifest.json')
    create.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    create.add_argument('--output', type=Path, required=True)
    check = actions.add_parser('verify', help='check bytes and executable status against manifest')
    check.add_argument('--root', type=Path, required=True)
    check.add_argument('--manifest', type=Path, required=True)
    mode = check.add_mutually_exclusive_group()
    mode.add_argument('--selection', action='store_true', help='also rediscover current Git inputs')
    mode.add_argument('--exact', action='store_true', help='also reject all extra files in exported tree')
    args = parser.parse_args()
    try:
        if args.action == 'export':
            manifest = export(args.root, args.output)
        else:
            manifest = load_manifest(args.manifest)
            verify(args.root, manifest, selection=args.selection, exact=args.exact)
        print(json.dumps({'status': 'ok', 'files': len(manifest['files']),
                          'action': args.action}))
    except (ValueError, OSError, subprocess.CalledProcessError, UnicodeError) as error:
        print(str(error), file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(os.fsdecode(error.stderr), file=sys.stderr, end='')
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
