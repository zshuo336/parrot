#!/usr/bin/env python3
"""Own one local crawler demonstration, including every launched process group."""
from __future__ import annotations

import argparse
import importlib.util
import json
import math
import os
from pathlib import Path
import shutil
import signal
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'tools/parrot-obs'))
from process_owner import OwnedChild


class RunFailure(Exception):
    def __init__(self, message, code=1):
        super().__init__(message)
        self.code = code if code > 0 else 128 - code


class Interrupted(RunFailure):
    def __init__(self, signum):
        super().__init__(f'interrupted by signal {signum}', 128 + signum)


def arguments(argv):
    parser = argparse.ArgumentParser(
        description='Run isolated local Erlang/Ray/JVM gateways and the Rust crawler. '
                    'LAB_MODE=manifest|direct|registry (default manifest); --skip-ts omits the TS query. '
                    'Registry runs four runtimes because its JVM has no listener.',
        allow_abbrev=False,
    )
    bounds = {'pages': (0, 1000000, 500), 'depth': (0, 65535, 2),
              'fanout': (0, 256, 3), 'batch': (1, 256, 32), 'wait': (1, 3600, 60)}
    for name, (_, _, default) in bounds.items():
        parser.add_argument('--' + name, type=int, default=default)
    parser.add_argument('--skip-ts', action='store_true')
    parser.add_argument('--manifest', type=Path)
    for name in ('hub-dylib', 'trust-native-sha256', 'hub-wasm', 'hub-sha256'):
        parser.add_argument('--' + name)

    flags = [arg.split('=', 1)[0] for arg in argv if arg.startswith('--')]
    if len(flags) != len(set(flags)):
        parser.error('duplicate option')
    args = parser.parse_args(argv)
    for name, (minimum, maximum, _) in bounds.items():
        if not minimum <= getattr(args, name) <= maximum:
            parser.error(f'--{name} must be in {minimum}..{maximum}')
    native = args.hub_dylib is not None or args.trust_native_sha256 is not None
    wasm = args.hub_wasm is not None or args.hub_sha256 is not None
    if native and wasm:
        parser.error('select only one hub form')
    if native or wasm:
        path, digest = (args.hub_dylib, args.trust_native_sha256) if native else (args.hub_wasm, args.hub_sha256)
        if not path or not digest or len(digest) != 64 or any(c not in '0123456789abcdefABCDEF' for c in digest):
            parser.error('hub requires a file and exactly 64 hexadecimal SHA-256 digits')
    args.mode = os.environ.get('LAB_MODE', 'manifest')
    if args.mode not in ('manifest', 'direct', 'registry'):
        parser.error('LAB_MODE must be manifest, direct or registry')
    if args.mode == 'direct' and '--wait' in flags:
        parser.error('--wait requires LAB_MODE=registry or manifest')
    if args.manifest is not None and args.mode != 'manifest':
        parser.error('--manifest requires LAB_MODE=manifest')
    args.explicit_workload = {name for name in bounds if '--' + name in flags}
    return args


def timeout_setting(name, default):
    raw = os.environ.get(name, str(default))
    try:
        value = float(raw) if len(raw) <= 64 else float('nan')
    except ValueError:
        value = float('nan')
    if not math.isfinite(value) or not 0.1 <= value <= 86400:
        raise RunFailure(f'{name} must be finite seconds in 0.1..86400', 2)
    return value


class Launcher:
    def __init__(self, args):
        self.args = args
        self.children = []
        self.signum = None
        self.directory = None
        self.ray_directory = None
        self.ts = 'not-run'
        self.startup_timeout = timeout_setting('LAB_STARTUP_TIMEOUT', 120)
        self.deadline = time.monotonic() + timeout_setting('LAB_TIMEOUT', 1800)

    def signal(self, signum, _frame):
        # Record interruption; do not raise between Popen and ownership transfer.
        if self.signum is None:
            self.signum = signum

    def spawn(self, name, command, **kwargs):
        child = OwnedChild(name, command, self.directory / (name + '.log'), **kwargs)
        self.children.append(child)
        return child

    def wait(self, condition, *, required=(), seconds=None):
        deadline = min(self.deadline, time.monotonic() + seconds) if seconds is not None else self.deadline
        while True:
            if self.signum is not None:
                raise Interrupted(self.signum)
            for child in self.children:
                child.poll()
                if child.failure:
                    raise RunFailure(f'{child.name}: {child.failure}')
            for child in required:
                if child.code is not None:
                    raise RunFailure(f'{child.name} exited before the scenario completed: {child.code}')
            if condition():
                return
            if time.monotonic() >= deadline:
                raise RunFailure('launcher deadline exceeded')
            time.sleep(0.02)

    def completed(self, child, *, required=()):
        self.wait(lambda: child.code is not None, required=required)
        if child.code:
            raise RunFailure(f'{child.name} exited {child.code}; see {child.log}', child.code)

    def prepare(self):
        binary = Path(os.environ.get('PARROT_LAB_BIN', 'target/debug/crawler-lab'))
        self.binary = (ROOT / binary).resolve()
        if not self.binary.is_file() or not os.access(self.binary, os.X_OK):
            raise RunFailure(f'missing executable {self.binary}; build crawler-lab first')
        for tool in ['erl', 'java'] + ([] if self.args.skip_ts or self.args.mode == 'registry' else ['npm', 'node']):
            if shutil.which(tool) is None:
                raise RunFailure(f'missing required tool: {tool}')
        if self.args.mode == 'manifest':
            return
        for artifact in [ROOT / 'apps/crawler-lab/erlang/frontier.beam',
                         ROOT / 'apps/crawler-lab/jvm/target/crawler-lab-jvm-1.0.0.jar']:
            if not artifact.is_file():
                raise RunFailure(f'missing component artifact {artifact}; run make build-crawler-components')
        spec = importlib.util.spec_from_file_location('crawler_gateway_config', ROOT / 'tools/parrot-obs/gateway_manager.py')
        manager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(manager)
        # Configuration checks artifacts before any gateway is started. Ignore
        # ambient obs ports/node IDs: every listener allocates its own port.
        self.gateway_specs = manager.config_from_environment(
            self.directory, dict(os.environ, GW_ERL_PORT='0', GW_JVM_PORT='0',
                                 GW_RAY_PORT='0', GW_JVM_NODE='jvm-search-1'))['gateways']

    def gateways(self, target=None):
        children = []
        for spec in self.gateway_specs:
            command = list(spec['command'])
            environment = dict(spec['env'], PARROT_NODE_ID=spec['node'])
            marker = spec['marker'] if target is None else None
            if spec['name'] == 'ray':
                environment.update(RAY_ADDRESS='local', RAY_USAGE_STATS_ENABLED='0')
                command = [sys.executable, str(Path(__file__).with_name('ray_gateway.py')),
                           str(self.ray_directory), target or '']
            elif target and spec['name'] == 'erl':
                command[-1] = f'parrot_gw:main(["0", "parrot={target}"]).'
            elif target and spec['name'] == 'jvm':
                command.append('parrot=' + target)
            children.append(self.spawn(spec['name'], command, environment=environment, marker=marker,
                                       wait_for_output=False, python_entrypoint=spec['name'] == 'ray'))
        return children

    def workload(self):
        args = self.args
        result = []
        for name in ('pages', 'depth', 'fanout', 'batch'):
            if args.mode != 'manifest' or name in args.explicit_workload:
                result.extend(['--' + name, str(getattr(args, name))])
        for flag in ('hub_dylib', 'trust_native_sha256', 'hub_wasm', 'hub_sha256'):
            value = getattr(args, flag)
            if value is not None:
                result.extend(['--' + flag.replace('_', '-'), value])
        return result

    def execute(self):
        self.prepare()
        if self.args.mode == 'manifest':
            return self.execute_manifest()
        if self.args.mode == 'registry':
            app = self.spawn('app', [self.binary, '--insecure-development', '--bind', '127.0.0.1:0',
                                   '--wait', str(self.args.wait), *self.workload()], marker='PARROT_CRAWLER_PORT=')
            self.wait(lambda: app.port is not None, required=[app], seconds=self.startup_timeout)
            gateways = self.gateways(f'127.0.0.1:{app.port}')
        else:
            gateways = self.gateways()
            self.wait(lambda: all(child.port is not None for child in gateways),
                      required=gateways, seconds=self.startup_timeout)
            print('LAB_NODES=' + json.dumps({child.name: child.port for child in gateways}), flush=True)
            endpoints = [f'{child.name}=127.0.0.1:{child.port}' for child in gateways]
            app = self.spawn('app', [self.binary, '--insecure-development', *endpoints, *self.workload()])
        self.completed(app, required=gateways)
        output = app.text()
        print(output, end='', flush=True)
        if 'crawler-lab PASS（四运行时全链集成）' not in output:
            raise RunFailure('application exited successfully without its completed scenario marker')
        if self.args.skip_ts:
            self.ts = 'skipped-by-request'
        elif self.args.mode == 'registry':
            self.ts = 'unavailable-in-registry'
            print('TS query requires direct mode; registry completed four runtimes.', flush=True)
        else:
            build = self.spawn('typescript-build', ['npm', 'run', 'build'], cwd=ROOT / 'interop/typescript-lite')
            self.completed(build, required=gateways)
            port = next(child.port for child in gateways if child.name == 'jvm')
            query = self.spawn('typescript-query', ['node', '--test', '--test-reporter=spec',
                               'tests/integration/crawler_client.test.mjs'],
                               cwd=ROOT / 'interop/typescript-lite', environment={'LAB_JVM': f'127.0.0.1:{port}'})
            self.completed(query, required=gateways)
            output = query.text()
            print(output, end='', flush=True)
            if '[ts-user] search parrot+actor' not in output:
                raise RunFailure('TypeScript query exited without completing the actual user query')
            self.ts = 'passed'

    def execute_manifest(self):
        ready, done = self.directory / 'query-ready.json', self.directory / 'query-done.txt'
        manifest = self.args.manifest or ROOT / 'apps/crawler-lab/crawler.app.toml'
        command = [self.binary, '--insecure-development', '--manifest', manifest,
                   '--wait', str(self.args.wait), *self.workload()]
        if not self.args.skip_ts:
            command.extend(['--query-ready', ready, '--query-done', done])
        app = self.spawn('app', command, wait_for_output=False)
        if self.args.skip_ts:
            self.ts = 'skipped-by-request'
        else:
            self.wait(ready.exists, required=[app])
            state = json.loads(ready.read_text())
            print('LAB_NODES=' + json.dumps({'application': int(state['endpoint'].rsplit(':', 1)[1])}), flush=True)
            build = self.spawn('typescript-build', ['npm', 'run', 'build'], cwd=ROOT / 'interop/typescript-lite')
            self.completed(build, required=[app])
            query = self.spawn('typescript-query', ['node', '--test', '--test-reporter=spec',
                               'tests/integration/crawler_client.test.mjs'],
                               cwd=ROOT / 'interop/typescript-lite',
                               environment={'LAB_JVM': state['endpoint'], 'LAB_SEARCH_PATH': state['path'],
                                            'LAB_EMPTY': '1' if state['pages'] == 0 else '0',
                                            'LAB_EXPECTED_HITS': json.dumps(state['expected_hits'])})
            self.completed(query, required=[app])
            output = query.text()
            print(output, end='', flush=True)
            if '[ts-user] search parrot+actor' not in output:
                raise RunFailure('TypeScript query did not complete')
            self.ts = 'passed'
            # Commit success only after the actual TS process exits zero.
            temporary = done.with_suffix('.tmp')
            temporary.write_text('ok\n')
            temporary.replace(done)
        self.completed(app)
        output = app.text()
        print(output, end='', flush=True)
        if 'crawler-lab PASS（四运行时全链集成）' not in output:
            raise RunFailure('application lacks completed business and teardown marker')

    def run(self):
        code, detail = 1, ''
        try:
            requested = os.environ.get('LAB_RUN_DIR')
            if requested:
                directory = Path(requested).absolute()
                directory.mkdir(mode=0o700, exist_ok=False)
                self.directory = directory
            else:
                self.directory = Path(tempfile.mkdtemp(prefix='parrot-lab-', dir='/tmp'))
            # Ray appends long AF_UNIX socket names. Keep its private root short
            # even when a user-selected log directory has a long Unicode path.
            self.ray_directory = Path(tempfile.mkdtemp(prefix='pl-ray-', dir='/tmp'))
            print(f'LAB_RUN_DIR={self.directory}', flush=True)
            print(f'LAB_RAY_DIR={self.ray_directory}', flush=True)
            self.execute()
            code = 0
        except RunFailure as error:
            code, detail = error.code, str(error)
        except Exception as error:
            code, detail = 1, str(error)
        finally:
            # A second signal cannot interrupt retirement. All guardians remain
            # unreaped until the final signal to their fresh process group.
            for child in self.children:
                child.begin_stop()
            if self.children:
                time.sleep(2.2)
            for child in self.children:
                try:
                    child.finish_stop()
                except Exception as error:
                    code = 1
                    detail += f'; cleanup {child.name}: {error}'
            if self.signum is not None and code == 0:
                code, detail = 128 + self.signum, f'interrupted by signal {self.signum}'
            if self.directory is not None and self.directory.is_dir():
                result = dict(exit_code=code, detail=detail, mode=self.args.mode, ts=self.ts,
                              ray_directory=str(self.ray_directory), children=[child.state() for child in self.children])
                try:
                    (self.directory / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
                except OSError as error:
                    code, detail = 1, f'{detail}; cannot write terminal result: {error}'
        if detail:
            print('crawler-lab launcher: ' + detail, file=sys.stderr, flush=True)
        if code == 0:
            print(f'CRAWLER_LAB_LAUNCHER_PASS mode={self.args.mode} ts={self.ts}', flush=True)
        return code


def main(argv=None):
    try:
        launcher = Launcher(arguments(sys.argv[1:] if argv is None else argv))
    except RunFailure as error:
        print('crawler-lab launcher: ' + str(error), file=sys.stderr)
        return error.code
    signal.signal(signal.SIGTERM, launcher.signal)
    signal.signal(signal.SIGINT, launcher.signal)
    return launcher.run()


if __name__ == '__main__':
    raise SystemExit(main())
