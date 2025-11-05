#!/usr/bin/env python3
"""Run pinned upstream Tokio suites and a minimal sync consumer with saved evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import tempfile
import time
import uuid

SUITES = {'io_async_fd': {'Linux': 27, 'Darwin': 24},
          'sync_mpsc': {'Linux': 107, 'Darwin': 107},
          'sync_oneshot': {'Linux': 22, 'Darwin': 22}}
MINIMAL = 'parrot-minimal-sync-contract'
COMPLETED = 'PARROT_MINIMAL_SYNC_PARKING_LOT_CONTRACT_PASSED'


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def save(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, indent=2)


def verify_resolution(data, root, minimal):
    packages = [item for item in data['packages'] if item['name'] == 'tokio']
    require(len(packages) == 1, 'expected exactly one resolved Tokio identity')
    package = packages[0]
    require(package['source'] is None and Path(package['manifest_path']).resolve() == root / 'vendor/tokio/Cargo.toml'
            and 'parrot-native-progress' in package['features'], 'actual vendored Tokio patch was not selected')
    nodes = [item for item in data['resolve']['nodes'] if item['id'] == package['id']]
    require(len(nodes) == 1, 'resolved Tokio feature node missing or duplicated')
    features = set(nodes[0]['features'])
    require({'sync', 'parking_lot'} <= features, 'sync/parking_lot feature coverage missing')
    if minimal:
        require(not features & {'full', 'rt', 'net', 'macros', 'fs', 'process', 'signal', 'time'},
                'minimal sync consumer accidentally enabled runtime or full features')
    else:
        require({'full', 'test-util'} <= features, 'upstream tests lack required runtime/test features')
    require(data['resolve']['root'] is not None, 'consumer root package missing')
    return dict(package=package['id'], version=package['version'], features=sorted(features),
                consumer=data['resolve']['root'])


def verify_suite(log, code, expected):
    declared = re.findall(r'^running (\d+) tests?$', log, re.M)
    summaries = re.findall(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; '
                           r'(\d+) measured; (\d+) filtered out;', log, re.M)
    passed = re.findall(r'^test .+ \.\.\. ok$', log, re.M)
    require(code == 0 and declared == [str(expected)] and
            summaries == [('ok', str(expected), '0', '0', '0', '0')] and len(passed) == expected,
            'upstream test suite is incomplete, filtered, ignored or failed')


def build_artifacts(log, package, expected, root, target, profile, minimal=False):
    result = {}
    for line in log.splitlines():
        if not line.startswith('{'):
            continue
        item = json.loads(line)
        if item.get('reason') != 'compiler-artifact' or item.get('package_id') != package or not item.get('executable'):
            continue
        name = item['target']['name']
        require(name in expected and name not in result, 'unexpected or duplicate consumer executable')
        expected_source = (root / 'scripts/fixtures/tokio-minimal/src/main.rs' if minimal else
                           root / 'vendor/tokio/tests' / (name + '.rs'))
        binary = Path(item['executable']).resolve()
        require(Path(item['target']['src_path']).resolve() == expected_source and
                item['target']['kind'] == (['bin'] if minimal else ['test']) and
                item['profile']['test'] is (not minimal) and binary.is_file() and
                binary.is_relative_to(target / profile), 'consumer executable source/profile/ownership differs')
        result[name] = binary
    require(set(result) == set(expected), 'compiled consumer targets are missing')
    return result


class Check:
    def __init__(self, args):
        self.root = args.root.resolve(strict=True)
        self.output = args.output_dir.resolve()
        self.target = args.target_dir.resolve()
        require(not self.output.is_relative_to(self.root / 'vendor/tokio') and
                not self.output.is_relative_to(self.target), 'evidence must be separate from source and build cache')
        self.output.mkdir(parents=True, exist_ok=False)
        self.env = dict(os.environ, CARGO_BUILD_JOBS='2', CARGO_TARGET_DIR=str(self.target))
        self.offline = args.offline
        self.rows = []
        self.resolutions = {}
        self.artifacts = []
        self.inputs = {}

    def capture(self):
        files = set()
        for directory in ['vendor/tokio', 'scripts/fixtures/tokio-upstream', 'scripts/fixtures/tokio-minimal']:
            base = self.root / directory
            require(base.is_dir(), 'runtime source or pinned consumer missing: ' + directory)
            for parent, dirs, names in os.walk(base):
                dirs[:] = [name for name in dirs if name not in ['target', '.git']]
                for name in names:
                    path = Path(parent) / name
                    require(path.is_file() and not path.is_symlink(), 'nonregular runtime input')
                    files.add(path.relative_to(self.root).as_posix())
        for name in ['scripts/check_tokio_upstream.py', 'scripts/tests/test_tokio_upstream.py',
                     '.github/workflows/ci.yml', 'Makefile']:
            require((self.root / name).is_file(), 'runtime gate input missing: ' + name)
            files.add(name)
        return {name: sha(self.root / name) for name in sorted(files)}

    def run(self, name, command, timeout=1800):
        print('START', name, flush=True)
        started, expired = time.monotonic(), False
        stdout, stderr = [self.output / (name + suffix) for suffix in ['.stdout', '.stderr']]
        with stdout.open('xb') as out, stderr.open('xb') as err:
            process = subprocess.Popen(command, cwd=self.root, env=self.env, stdout=out,
                                       stderr=err, start_new_session=True)
            try:
                code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                expired = True
                os.killpg(process.pid, signal.SIGKILL)
                code = process.wait()
            except BaseException:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise
        row = dict(name=name, command=command, exit=code, watchdog=expired,
                   seconds=time.monotonic() - started, stdout_sha256=sha(stdout), stderr_sha256=sha(stderr))
        self.rows.append(row)
        save(self.output / (name + '.json'), row)
        require(code == 0 and not expired, 'runtime contract command failed: ' + name)
        require(self.capture() == self.inputs, 'runtime gate changed source or pinned locks: ' + name)
        print('PASS', name, flush=True)
        return stdout.read_text()

    def cargo(self, name, arguments):
        return self.run(name, ['cargo', *arguments, '--locked'] + (['--offline'] if self.offline else []))

    def save_binary(self, name, binary):
        target = self.output / 'binaries' / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(binary, target)
        digest = sha(target)
        require(digest == sha(binary), 'consumer executable changed while copying')
        self.artifacts.append(dict(name=name, path=target.relative_to(self.output).as_posix(), sha256=digest))
        return target

    def execute(self):
        system = platform.system()
        require(system in ['Linux', 'Darwin'], 'upstream AsyncFd gate currently supports Linux and macOS')
        self.inputs = self.capture()
        save(self.output / 'source-inputs.json', self.inputs)
        for tool in ['cargo', 'rustc']:
            self.run(tool + '-version', [tool, '--version'], timeout=60)
        manifests = {name: str(self.root / 'scripts/fixtures' / ('tokio-' + name) / 'Cargo.toml')
                     for name in ['minimal', 'upstream']}
        for name, manifest in manifests.items():
            data = json.loads(self.cargo('metadata-' + name, ['metadata', '--manifest-path', manifest, '--format-version', '1']))
            self.resolutions[name] = verify_resolution(data, self.root, name == 'minimal')
        require(self.resolutions['minimal']['version'] == self.resolutions['upstream']['version'],
                'consumer Tokio versions disagree')
        for profile in ['debug', 'release']:
            flags = ['--release'] if profile == 'release' else []
            text = self.cargo('upstream-build-' + profile, ['test', '--manifest-path', manifests['upstream'],
                              '--tests', '--no-run', '--message-format=json', *flags])
            binaries = build_artifacts(text, self.resolutions['upstream']['consumer'], SUITES,
                                       self.root, self.target, profile)
            for name, binary in sorted(binaries.items()):
                saved = self.save_binary(name + '-' + profile, binary)
                log = self.run(name + '-' + profile, [str(saved), '--test-threads=2'], timeout=300)
                verify_suite(log, 0, SUITES[name][system])
            text = self.cargo('minimal-build-' + profile, ['build', '--manifest-path', manifests['minimal'],
                              '--bin', MINIMAL, '--message-format=json', *flags])
            binaries = build_artifacts(text, self.resolutions['minimal']['consumer'], [MINIMAL],
                                       self.root, self.target, profile, minimal=True)
            saved = self.save_binary('minimal-' + profile, binaries[MINIMAL])
            log = self.run('minimal-' + profile, [str(saved)], timeout=60)
            require(log.splitlines() == [COMPLETED], 'minimal sync/parking_lot runtime contract did not complete')
        require(len(self.rows) == 16 and self.capture() == self.inputs and len(self.artifacts) == 8 and
                all(sha(self.output / item['path']) == item['sha256'] for item in self.artifacts),
                'runtime gate stage/source/artifact selection changed')
        save(self.output / 'verified.json', dict(status='verified', platform=system, stages=16, cargo_build_jobs=2,
             upstream_tests_per_profile=sum(counts[system] for counts in SUITES.values()),
             minimal_sync_profiles=2, resolutions=self.resolutions, artifacts=self.artifacts,
             source_unchanged=True, test_sources='actual-vendored-upstream', commands=self.rows))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    root = Path(__file__).resolve().parents[1]
    parser.add_argument('--root', type=Path, default=root)
    default_output = os.environ.get('PARROT_TOKIO_OUTPUT')
    parser.add_argument('--output-dir', type=Path, default=Path(default_output) if default_output else
                        Path(tempfile.gettempdir()) / ('parrot-tokio-contracts-' + uuid.uuid4().hex))
    parser.add_argument('--target-dir', type=Path, default=root / 'target/tokio-contracts')
    parser.add_argument('--offline', action='store_true')
    args = parser.parse_args()
    check = Check(args)
    try:
        check.execute()
    except BaseException as error:
        save(check.output / 'failed.json', dict(error=str(error), commands=check.rows,
             artifacts=check.artifacts, resolutions=check.resolutions, source_inputs=check.inputs))
        raise
    print('Tokio upstream and minimal sync/parking_lot contracts verified: ' + str(check.output))


if __name__ == '__main__':
    main()
