#!/usr/bin/env python3
"""Exercise native runtime dependency protection from independent workspaces.

Full execution requires Linux GNU and a C compiler. --resolution-only verifies
Cargo resolution, explicitly without claiming compiled/runtime coverage. Logs,
generated consumers and every intermediate lockfile are retained under a new
--output-dir. Cargo uses two build jobs, including metadata/update invocations.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import time


MARKER = 'parrot-native-progress'
REMOTE_MARKER = 'parrot-deferred-mpsc'
CASES = [
    ('parrot-abi', [], False),
    ('parrot-app', [], False),
    ('parrot-app', ['host'], False),
    ('parrot-node', [], False),
    ('parrot-abi', ['loader'], True),
    ('parrot-app', ['dylib'], True),
    ('parrot-node', ['dylib'], True),
]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class Check:
    def __init__(self, args):
        self.args = args
        self.root = args.root.resolve()
        self.output = args.output_dir.resolve()
        self.output.mkdir(parents=True, exist_ok=False)
        self.env = dict(os.environ, CARGO_BUILD_JOBS='2')
        self.env['CARGO_TARGET_DIR'] = str(args.target_dir.resolve())
        self.commands = []
        self.resolutions = []

    def run(self, name, consumer, arguments, expected=0):
        command = ['cargo', *arguments]
        if self.args.offline:
            command.append('--offline')
        base = self.output / name
        started = time.monotonic()
        timed_out = False
        with base.with_suffix('.stdout').open('x') as stdout, base.with_suffix('.stderr').open('x') as stderr:
            process = subprocess.Popen(command, cwd=consumer, env=self.env,
                                       stdout=stdout, stderr=stderr, start_new_session=True)
            try:
                code = process.wait(timeout=1800)
            except subprocess.TimeoutExpired:
                timed_out = True
                os.killpg(process.pid, signal.SIGKILL)
                code = process.wait()
        row = dict(name=name, cwd=str(consumer), command=command, exit=code,
                   timeout=timed_out, seconds=time.monotonic() - started)
        self.commands.append(row)
        self.save('running')
        lock = consumer / 'Cargo.lock'
        if lock.exists():
            shutil.copy2(lock, self.output / (name + '.Cargo.lock'))
        print(json.dumps(row), flush=True)
        require(not timed_out and code == expected,
                f'{name}: expected exit {expected}, got {code}; see {base}.stderr')
        return base.with_suffix('.stdout').read_text(), base.with_suffix('.stderr').read_text()

    def save(self, status, error=None):
        report = dict(status=status, scope='resolution' if self.args.resolution_only else 'compiled-and-runtime',
                      cargo_build_jobs=2, commands=self.commands, resolutions=self.resolutions, error=error)
        (self.output / 'results.json').write_text(json.dumps(report, indent=2) + '\n')

    def manifest(self, consumer, package, features, patch, native, dependency_path=None):
        dependency = {"path": str(dependency_path or self.root / 'crates' / package), "features": features}
        # JSON strings/arrays are valid TOML values; construct only known keys.
        text = ('[package]\nname="native-consumer-contract"\nversion="0.0.0"\nedition="2021"\n'
                '[workspace]\n[dependencies]\n' + package + ' = { path = '
                + json.dumps(dependency['path']) + ', features = ' + json.dumps(features) + ' }\n')
        if patch:
            text += '\n[patch.crates-io]\ntokio = { path = ' + json.dumps(str(self.root / 'vendor/tokio')) + ' }\n'
        if native:
            text += ('\n[features]\ndefault=["dylib"]\ndylib=[]\n'
                     '[dev-dependencies]\ntokio={version="1",features=["full"]}\n'
                     'libloading="0.8.9"\nparking_lot="0.12.5"\ntempfile="3"\n')
        (consumer / 'Cargo.toml').write_text(text)

    def consumer(self, name, package, features, patch, native):
        consumer = self.output / 'consumers' / name
        (consumer / 'src').mkdir(parents=True)
        self.manifest(consumer, package, features, patch, native)
        if package == 'parrot-abi':
            body = 'assert_eq!(parrot_abi::AbiStr::of_str("external").len, 8);'
            if native:
                body += '\nlet _loader = parrot_abi::DylibLoader;'
        elif package == 'parrot-app':
            body = ('let manifest = parrot_app::AppManifest::from_toml('
                    '"name = \\"external\\"\\nversion = \\"1.0.0\\"\\ncomponents = []\\n").unwrap();\n'
                    'assert_eq!(manifest.name, "external");')
            if native:
                body += '\nassert_eq!(parrot_app::host::dylib::DylibReply(vec![42]).0, vec![42]);'
        else:
            body = 'assert_eq!(parrot_node::builtin_manifest_components_from(Some("echo")), vec!["echo"]);'
            if native:
                body += '\nassert!(parrot_node::NativeTrust::default().is_empty());'
        (consumer / 'src/main.rs').write_text('fn main() {\n' + body + '\n}\n')
        if native:
            tests = consumer / 'tests'
            (tests / 'fixtures').mkdir(parents=True)
            source = self.root / 'crates/parrot-app/tests'
            for relative in ['native_runtime_progress.rs', 'fixtures/native_unload.c']:
                shutil.copy2(source / relative, tests / relative)
        return consumer

    def metadata(self, name, consumer, native, patch, pure_abi=False, locked=False, remote=False):
        required = ({MARKER} if native else set()) | ({REMOTE_MARKER} if remote else set())
        stdout, stderr = self.run(name, consumer, ['metadata', '--format-version', '1']
                                  + (['--locked'] if locked else []),
                                  expected=101 if required and not patch else 0)
        if required and not patch:
            require(any(marker in stderr for marker in required) and 'does not have that feature' in stderr,
                    f'{name}: failure must identify the missing capability, not a network/compiler error')
            return
        data = json.loads(stdout)
        packages = [p for p in data['packages'] if p['name'] == 'tokio']
        if pure_abi:
            require(not packages, f'{name}: pure ABI unexpectedly acquired Tokio')
        else:
            require(len(packages) == 1, f'{name}: expected exactly one Tokio identity')
            tokio = packages[0]
            if patch:
                require(tokio['source'] is None and Path(tokio['manifest_path']).resolve()
                        == self.root / 'vendor/tokio/Cargo.toml', f'{name}: local patch was not selected')
                if required:
                    node = next(n for n in data['resolve']['nodes'] if n['id'] == tokio['id'])
                    require(required <= set(node['features']), f'{name}: required runtime capabilities missing')
            else:
                require((tokio['source'] or '').startswith('registry+'), f'{name}: expected registry Tokio')
        self.resolutions.append(dict(name=name, tokio=[dict(id=p['id'], version=p['version'],
                                                         source=p['source']) for p in packages]))

    def execute(self):
        if not self.args.resolution_only:
            require(platform.system() == 'Linux' and platform.libc_ver()[0] == 'glibc',
                    'full native contracts require Linux GNU; use --resolution-only elsewhere')
        builds = []
        for package, features, native in CASES:
            for patch in [False, True]:
                name = package + '-' + ('-'.join(features) or 'default') + ('-patched' if patch else '-registry')
                consumer = self.consumer(name, package, features, patch, native)
                remote = package != 'parrot-abi'
                self.metadata(name + '-resolve', consumer, native, patch,
                              pure_abi=package == 'parrot-abi' and not native, remote=remote)
                # Compile each supported consumer with its required runtime configuration.
                if patch == (native or remote):
                    builds.append((name, consumer, native))

        # Model an existing loader consumer upgrading Parrot in place. Seed the
        # complete dependency graph with the previous, unguarded ABI manifest;
        # never mutate production sources. Keep its package path unchanged.
        legacy = self.output / 'legacy-abi'
        shutil.copytree(self.root / 'crates/parrot-abi/src', legacy / 'src')
        shutil.copytree(self.root / 'crates/parrot-abi/benches', legacy / 'benches')
        guarded = (self.root / 'crates/parrot-abi/Cargo.toml').read_text()
        requirement = ', "tokio/' + MARKER + '"'
        require(guarded.count(requirement) == 1, 'loader capability must occur exactly once')
        (legacy / 'Cargo.toml').write_text(guarded.replace(requirement, ''))
        consumer = self.consumer('newer-registry-lock', 'parrot-abi', ['loader'], False, True)
        self.manifest(consumer, 'parrot-abi', ['loader'], False, True, legacy)
        manifest = consumer / 'Cargo.toml'
        manifest.write_text(manifest.read_text().replace('tokio={version="1"', 'tokio={version="=1.53.2"'))
        stdout, _ = self.run('newer-lock-seed', consumer, ['metadata', '--format-version', '1'])
        tokio = [p for p in json.loads(stdout)['packages'] if p['name'] == 'tokio']
        require(len(tokio) == 1 and tokio[0]['version'] == '1.53.2'
                and (tokio[0]['source'] or '').startswith('registry+'), 'seed must use real registry 1.53.2')
        (legacy / 'Cargo.toml').write_text(guarded)
        self.manifest(consumer, 'parrot-abi', ['loader'], False, True, legacy)
        self.metadata('newer-lock-unpatched', consumer, True, False)
        self.manifest(consumer, 'parrot-abi', ['loader'], True, True, legacy)
        # An existing lock isn't repaired by merely adding [patch]. Preserve
        # the failure and then perform the documented explicit selection.
        _, stderr = self.run('newer-lock-patched-not-selected', consumer,
                             ['metadata', '--format-version', '1', '--locked'], expected=101)
        missing_capability = MARKER in stderr and 'does not have that feature' in stderr
        locked_change = '--locked' in stderr and ('lock file' in stderr or 'lockfile' in stderr)
        require(missing_capability or locked_change,
                'locked migration must fail on capability or required lock update, not an unrelated error')
        patched_versions = {package['version'] for resolution in self.resolutions
                            for package in resolution['tokio'] if package['source'] is None}
        require(len(patched_versions) == 1, 'patched consumers must agree on one runtime version')
        patched_version = patched_versions.pop()
        self.run('newer-lock-select-patch', consumer, ['update', '-p', 'tokio', '--precise', patched_version])
        self.metadata('newer-lock-selected', consumer, True, True, locked=True)

        if not self.args.resolution_only:
            for name, consumer, native in builds:
                self.run(name + '-run', consumer, ['run', '--locked'])
                if native:
                    stdout, _ = self.run(name + '-progress', consumer,
                                         ['test', '--locked', '--test', 'native_runtime_progress'])
                    require('test result: ok. 8 passed; 0 failed; 0 ignored;' in stdout,
                            f'{name}: all eight actual native contracts must execute')
            self.run('newer-lock-selected-run', consumer=self.output / 'consumers/newer-registry-lock',
                     arguments=['run', '--locked'])
        self.save('passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--target-dir', type=Path, default=Path('target'))
    parser.add_argument('--offline', action='store_true')
    parser.add_argument('--resolution-only', action='store_true')
    checker = Check(parser.parse_args())
    try:
        checker.execute()
    except Exception as error:
        checker.save('failed', str(error))
        raise


if __name__ == '__main__':
    main()
