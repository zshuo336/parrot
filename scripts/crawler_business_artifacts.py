#!/usr/bin/env python3
"""Build and verify current-source crawler Dylib/Wasm test artifacts.

The receipt proves local build freshness, not publisher identity. Verification
never invokes Cargo or another build. Explicit native paths are repository-relative;
relative CARGO_TARGET_DIR follows the build command's invocation directory.
"""
import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
RECEIPT = Path('apps/crawler-lab/.business-artifacts.json')
LOCK = Path('apps/crawler-lab/.business-artifacts.lock')
REQUIRED_ROOTS = {'apps/crawler-lab/core', 'apps/crawler-lab/dylib',
                  'apps/crawler-lab/wasm', 'apps/crawler-lab/src',
                  'crates/parrot-abi', 'crates/parrot-wasm/wit'}
RECIPES = ['Cargo.toml', 'Cargo.lock', 'Makefile', 'apps/crawler-lab/Cargo.toml',
           'apps/crawler-lab/crawler.app.toml',
           'scripts/crawler_business_artifacts.py', 'scripts/build_crawler_wasm.sh']
BUILD_ENV = ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC', 'RUSTC_WRAPPER',
             'RUSTC_WORKSPACE_WRAPPER', 'RUSTUP_TOOLCHAIN', 'CARGO_BUILD_TARGET']


class ArtifactError(RuntimeError):
    pass


def digest(path):
    with Path(path).open('rb') as source:
        result = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def selected_native(root, environment):
    value = environment.get('CRAWLER_TEST_DYLIB')
    if not value:
        return None
    path = root / value
    if path.is_symlink():
        raise ArtifactError('explicit native output must not be a symlink')
    return path.resolve()


@contextmanager
def receipt_lock(root, exclusive):
    path = root / LOCK
    if path.is_symlink() or (path.exists() and not path.is_file()):
        raise ArtifactError('business lock must be a regular file')
    try:
        opened = path.open('a+b' if exclusive else 'rb')
    except OSError as error:
        raise ArtifactError('missing/unavailable business build state; rebuild required') from error
    with opened as lock:
        try:
            fcntl.flock(lock, (fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH) | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ArtifactError('business artifact build/verification already in progress') from error
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def relative(root, path):
    try:
        return str(Path(path).resolve().relative_to(root))
    except ValueError as error:
        raise ArtifactError('local source dependency is outside the repository: ' + str(path)) from error


def local_dependencies(root, metadata, package_name):
    packages = {p['id']: p for p in metadata['packages']}
    roots = [p['id'] for p in packages.values() if p['name'] == package_name and p['source'] is None]
    if len(roots) != 1:
        raise ArtifactError('ambiguous/missing Cargo package: ' + package_name)
    nodes = {n['id']: n for n in metadata['resolve']['nodes']}
    pending, visited, paths = list(roots), set(), set()
    while pending:
        identity = pending.pop()
        if identity in visited:
            continue
        visited.add(identity)
        package = packages[identity]
        if package['source'] is None:
            directory = relative(root, Path(package['manifest_path']).parent)
            # The application's directory also owns generated multilingual builds
            # and receipt state. Its Rust sources are a separate input root.
            paths.add('apps/crawler-lab/src' if directory == 'apps/crawler-lab' else directory)
        for dep in nodes[identity]['deps']:
            if any(kind['kind'] != 'dev' for kind in dep['dep_kinds']):
                pending.append(dep['pkg'])
    return paths


def source_inputs(root, roots):
    if not REQUIRED_ROOTS.issubset(roots):
        raise ArtifactError('receipt is missing required source roots')
    paths = set(RECIPES)
    if (root / 'apps/crawler-lab/build.rs').exists():
        paths.add('apps/crawler-lab/build.rs')
    for source_root in roots:
        directory = root / source_root
        if not directory.is_dir() or relative(root, directory) != source_root:
            raise ArtifactError('invalid source root: ' + source_root)
        for current, directories, files in os.walk(directory, followlinks=False):
            directories[:] = sorted(d for d in directories if d not in {'target', '.git', '__pycache__', '.parrot-business-staging'})
            for directory_name in directories:
                if (Path(current) / directory_name).is_symlink():
                    raise ArtifactError('source directory symlinks require an explicit source root')
            for name in sorted(files):
                path = Path(current) / name
                rel = str(path.relative_to(root))
                if rel == 'apps/crawler-lab/wasm/crawler-hub.wasm':
                    continue  # generated component, verified separately below
                if path.is_symlink() or not path.is_file():
                    raise ArtifactError('source inputs must be regular files: ' + rel)
                paths.add(rel)
    return {path: digest(root / path) for path in sorted(paths)}


def cargo_configs(root, environment):
    home = Path(environment.get('CARGO_HOME', str(Path.home() / '.cargo'))).resolve()
    paths = {home / name for name in ['config', 'config.toml']}
    for ancestor in [root, *root.parents]:
        paths.update(ancestor / '.cargo' / name for name in ['config', 'config.toml'])
        paths.update(ancestor / name for name in ['rust-toolchain', 'rust-toolchain.toml'])
    # Nested manifests can also have local Cargo configuration.
    for directory in [root / 'apps', root / 'apps/crawler-lab', root / 'apps/crawler-lab/wasm']:
        paths.update(directory / '.cargo' / name for name in ['config', 'config.toml'])
    return {str(p): digest(p) if p.is_file() else None for p in sorted(paths)}


def environment_inputs(environment):
    # Include profile/target/compiler overrides without capturing credentials.
    keys = set(BUILD_ENV) | {k for k in environment if k.startswith(('CARGO_PROFILE_', 'CARGO_TARGET_'))
                             and k not in {'CARGO_TARGET_DIR', 'CARGO_TARGET_TMPDIR'}}
    return {key: environment.get(key) for key in sorted(keys)}


def run(command, root, environment, capture=False):
    return subprocess.run(command, cwd=root, env=environment, check=True, text=True,
                          stdout=subprocess.PIPE if capture else sys.stderr)


def cargo_artifact(command, root, environment, package_name, target_name, package_identity, binary=False):
    try:
        result = run(command + ['--message-format=json'], root, environment, capture=True)
    except subprocess.CalledProcessError as error:
        for line in (error.stdout or '').splitlines():
            event = json.loads(line)
            if event.get('reason') == 'compiler-message':
                print(event['message'].get('rendered', ''), file=sys.stderr, end='')
        raise
    candidates = []
    for line in result.stdout.splitlines():
        event = json.loads(line)
        if event.get('reason') == 'compiler-message':
            print(event['message'].get('rendered', ''), file=sys.stderr, end='')
        if event.get('reason') == 'compiler-artifact' and event['package_id'] == package_identity and event['target']['name'] == target_name:
            if binary and 'bin' in event['target']['crate_types'] and event.get('executable'):
                candidates.append(Path(event['executable']).resolve())
            elif not binary and 'cdylib' in event['target']['crate_types']:
                candidates.extend(Path(p).resolve() for p in event['filenames']
                                  if Path(p).suffix in {'.dylib', '.so', '.dll', '.wasm'})
    if len(candidates) != 1 or not candidates[0].is_file() or candidates[0].stat().st_size == 0:
        raise ArtifactError('Cargo did not produce exactly one ' + package_name + ' artifact')
    return candidates[0]


def atomic_copy(source, destination, executable=False):
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = destination.parent / '.parrot-business-staging'
    staging.mkdir(exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=destination.name + '.', dir=staging)
    try:
        with os.fdopen(descriptor, 'wb') as out, source.open('rb') as incoming:
            shutil.copyfileobj(incoming, out)
            out.flush()
            os.fsync(out.fileno())
        os.chmod(temporary, 0o755 if executable else 0o644)
        os.replace(temporary, destination)
    finally:
        Path(temporary).unlink(missing_ok=True)


def build(root=ROOT, environment=None):
    root = Path(root).resolve()
    environment = dict(os.environ if environment is None else environment)
    # Cargo will run at root. Preserve the caller's relative target-dir meaning.
    if environment.get('CARGO_TARGET_DIR'):
        environment['CARGO_TARGET_DIR'] = str(Path(environment['CARGO_TARGET_DIR']).resolve())
    cargo = environment.get('CARGO', 'cargo')
    with receipt_lock(root, True):
        (root / RECEIPT).unlink(missing_ok=True)
        native_metadata = json.loads(run([cargo, 'metadata', '--locked', '--format-version', '1'], root, environment, True).stdout)
        wasm_metadata = json.loads(run([cargo, 'metadata', '--locked', '--format-version', '1', '--manifest-path', 'apps/crawler-lab/wasm/Cargo.toml'], root, environment, True).stdout)
        roots = sorted(REQUIRED_ROOTS | local_dependencies(root, native_metadata, 'crawler-lab-dylib')
                       | local_dependencies(root, wasm_metadata, 'crawler-lab-wasm')
                       | local_dependencies(root, native_metadata, 'crawler-lab'))
        application_identity = next(p['id'] for p in native_metadata['packages'] if p['name'] == 'crawler-lab' and p['source'] is None)
        native_identity = next(p['id'] for p in native_metadata['packages'] if p['name'] == 'crawler-lab-dylib' and p['source'] is None)
        wasm_identity = next(p['id'] for p in wasm_metadata['packages'] if p['name'] == 'crawler-lab-wasm' and p['source'] is None)
        inputs = source_inputs(root, roots)
        configs = cargo_configs(root, environment)
        env_inputs = environment_inputs(environment)
        target_directory = Path(native_metadata['target_directory']).resolve()
        for source_root in roots:
            directory = root / source_root
            if target_directory.is_relative_to(directory) and 'target' not in target_directory.relative_to(directory).parts:
                raise ArtifactError('Cargo target directory must be outside component sources or in a target subdirectory')
        selected = selected_native(root, environment)
        if selected is not None:
            if selected.suffix not in {'.so', '.dylib', '.dll'}:
                raise ArtifactError('explicit native output must have a native-library extension')
            for source_root in roots:
                directory = root / source_root
                if selected.is_relative_to(directory) and 'target' not in selected.relative_to(directory).parts:
                    raise ArtifactError('explicit native output must not overwrite a source directory')
        versions = {name: run([tool, '--version'], root, environment, True).stdout.strip()
                    for name, tool in [('cargo', cargo), ('rustc', environment.get('RUSTC', 'rustc')),
                                       ('wasm-tools', 'wasm-tools')]}
        application = cargo_artifact([cargo, 'build', '--locked', '-p', 'crawler-lab', '--bin', 'crawler-lab'], root, environment, 'crawler-lab', 'crawler-lab', application_identity, binary=True)
        application_output = target_directory / 'parrot-business' / digest(application) / application.name
        atomic_copy(application, application_output, executable=True)
        native_target = target_directory / 'parrot-business-build/native'
        wasm_target = target_directory / 'parrot-business-build/wasm'
        native = cargo_artifact([cargo, 'build', '--locked', '--target-dir', str(native_target), '-p', 'crawler-lab-dylib', '--lib'], root, environment, 'crawler-lab-dylib', 'crawler_lab_dylib', native_identity)
        raw_wasm = cargo_artifact([cargo, 'build', '--locked', '--manifest-path', 'apps/crawler-lab/wasm/Cargo.toml', '--target-dir', str(wasm_target), '--target', 'wasm32-unknown-unknown', '--release'], root, environment, 'crawler-lab-wasm', 'crawler_lab_wasm', wasm_identity)
        native_build_path = native
        layout = native.relative_to(native_target).parts
        if len(layout) < 2 or layout[-2] != 'debug':
            raise ArtifactError('unexpected Cargo native artifact layout')
        test_platform_directory = target_directory.joinpath(*layout[:-2])
        output = root / 'apps/crawler-lab/wasm/crawler-hub.wasm'
        descriptor, temporary = tempfile.mkstemp(prefix='crawler-component-', suffix='.wasm')
        os.close(descriptor)
        try:
            run(['wasm-tools', 'component', 'new', str(raw_wasm), '-o', temporary], root, environment)
            run(['wasm-tools', 'validate', temporary], root, environment)
            atomic_copy(Path(temporary), output)
        finally:
            Path(temporary).unlink(missing_ok=True)
        mutable_outputs = {native_build_path} | {test_platform_directory / profile / native.name for profile in ['debug', 'release']}
        if selected in mutable_outputs:
            raise ArtifactError('CRAWLER_TEST_DYLIB must not name a mutable Cargo compiler output')
        destination = selected or target_directory / 'parrot-business' / digest(native) / native.name
        atomic_copy(native, destination)
        native = destination
        current_native = json.loads(run([cargo, 'metadata', '--locked', '--format-version', '1'], root, environment, True).stdout)
        current_wasm = json.loads(run([cargo, 'metadata', '--locked', '--format-version', '1', '--manifest-path', 'apps/crawler-lab/wasm/Cargo.toml'], root, environment, True).stdout)
        current_roots = sorted(REQUIRED_ROOTS | local_dependencies(root, current_native, 'crawler-lab-dylib')
                               | local_dependencies(root, current_wasm, 'crawler-lab-wasm')
                               | local_dependencies(root, current_native, 'crawler-lab'))
        if current_roots != roots or Path(current_native['target_directory']).resolve() != target_directory:
            raise ArtifactError('Cargo source graph or target directory changed during the build')
        if source_inputs(root, roots) != inputs or cargo_configs(root, environment) != configs:
            raise ArtifactError('source/build configuration changed during the build')
        receipt = dict(schema=1, repository=str(root), source_roots=roots, inputs=inputs,
                       cargo_configs=configs, build_environment=env_inputs, tools=versions,
                       target_directory=str(target_directory),
                       native_build_path=str(native_build_path), wasm_build_path=str(raw_wasm),
                       test_platform_directory=str(test_platform_directory),
                       artifacts={name: dict(path=str(path), sha256=digest(path), size_bytes=path.stat().st_size)
                                  for name, path in [('application', application_output), ('dylib', native), ('wasm', output)]})
        descriptor, temporary = tempfile.mkstemp(prefix='.business-receipt-', dir=root / RECEIPT.parent)
        try:
            with os.fdopen(descriptor, 'w') as out:
                json.dump(receipt, out, indent=2, sort_keys=True)
                out.write('\n'); out.flush(); os.fsync(out.fileno())
            os.replace(temporary, root / RECEIPT)
        finally:
            Path(temporary).unlink(missing_ok=True)
        return receipt['artifacts']


def verify(root=ROOT, environment=None, test_executable=None):
    root = Path(root).resolve()
    environment = dict(os.environ if environment is None else environment)
    with receipt_lock(root, False):
        try:
            receipt_path = root / RECEIPT
            if not receipt_path.is_file() or receipt_path.stat().st_size > 1024 * 1024:
                raise ArtifactError('missing/non-regular/oversized business receipt; rebuild required')
            receipt = json.loads(receipt_path.read_text())
            if receipt['schema'] != 1 or receipt['repository'] != str(root):
                raise ArtifactError('receipt belongs to another repository or schema')
            if source_inputs(root, receipt['source_roots']) != receipt['inputs']:
                raise ArtifactError('business source inputs changed; rebuild required')
            if cargo_configs(root, environment) != receipt['cargo_configs']:
                raise ArtifactError('business Cargo configuration changed; rebuild required')
            # Cargo injects compiler/profile variables into test processes. The
            # receipt records the component build options; test executable location
            # binds those consumers to the actual target directory instead.
            if not test_executable and environment_inputs(environment) != receipt['build_environment']:
                raise ArtifactError('business build configuration changed; rebuild required')
            target = Path(receipt['target_directory'])
            if test_executable:
                executable = Path(test_executable).resolve()
                platform_directory = Path(receipt['test_platform_directory'])
                if (not executable.is_file() or not executable.is_relative_to(target)
                        or executable.parent.parent.parent != platform_directory):
                    raise ArtifactError('test executable belongs to a different Cargo target directory')
            elif environment.get('CARGO_TARGET_DIR') and Path(environment['CARGO_TARGET_DIR']).resolve() != target:
                raise ArtifactError('receipt belongs to a different Cargo target directory')
            artifacts = receipt['artifacts']
            if set(artifacts) != {'application', 'dylib', 'wasm'}:
                raise ArtifactError('receipt must contain the application and both business artifacts')
            selected = selected_native(root, environment)
            if selected is not None and str(selected) != artifacts['dylib']['path']:
                raise ArtifactError('receipt does not describe CRAWLER_TEST_DYLIB')
            for name, artifact in artifacts.items():
                path = Path(artifact['path'])
                if (not path.is_absolute() or not path.is_file()
                        or path.stat().st_size != artifact['size_bytes']
                        or digest(path) != artifact['sha256']
                        or (name == 'application' and not os.access(path, os.X_OK))):
                    raise ArtifactError(name + ' artifact missing or changed; rebuild required')
            return artifacts
        except (KeyError, TypeError, ValueError, OSError) as error:
            raise ArtifactError('invalid/missing business receipt; run make build-crawler-business') from error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['build', 'verify'])
    parser.add_argument('--test-executable', type=Path)
    args = parser.parse_args()
    try:
        artifacts = build() if args.operation == 'build' else verify(test_executable=args.test_executable)
        print(json.dumps(artifacts, sort_keys=True))
    except (ArtifactError, subprocess.CalledProcessError, OSError, ValueError, KeyError) as error:
        print('crawler business artifacts: ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
