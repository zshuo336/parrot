#!/usr/bin/env python3
"""Own an isolated local websearch application and its gateway process groups."""
from __future__ import annotations
import importlib.util
import asyncio
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import sys
import tempfile
import time
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[3]
APP = ROOT / 'apps/websearch'
sys.path.insert(0, str(ROOT / 'tools/parrot-obs'))
from process_owner import OwnedChild


class Failure(Exception):
    def __init__(self, message, code=1):
        super().__init__(message)
        self.code = code if code > 0 else 128 - code


def seconds(name, default=None):
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        value = float(raw) if len(raw) <= 64 else float('nan')
    except ValueError:
        value = float('nan')
    if not math.isfinite(value) or not 0.1 <= value <= 86400:
        raise Failure(f'{name} must be finite seconds in 0.1..86400', 2)
    return value


class Launcher:
    def __init__(self, mode, arguments):
        if mode not in ('direct', 'registry'):
            raise Failure('launcher mode must be direct or registry', 2)
        self.mode, self.arguments = mode, list(arguments)
        self.children = []
        self.directory = self.ray_directory = None
        self.signum = None
        self.ready = False
        self.web_port = None
        self.startup_timeout = seconds('WS_STARTUP_TIMEOUT', 180)
        self.build_timeout = seconds('WS_BUILD_TIMEOUT', 1200)
        limit = seconds('WS_TIMEOUT')
        self.deadline = time.monotonic() + limit if limit is not None else math.inf
        self.profile = os.environ.get('WS_PROFILE', 'release')
        if self.profile not in ('dev', 'release'):
            raise Failure('WS_PROFILE must be dev or release', 2)
        skip = os.environ.get('WS_SKIP_BUILD', '0')
        if skip not in ('0', '1'):
            raise Failure('WS_SKIP_BUILD must be 0 or 1', 2)
        self.skip_build = skip == '1'
        if os.environ.get('WS_BIN') and not self.skip_build:
            raise Failure('WS_BIN requires explicit WS_SKIP_BUILD=1', 2)
        self.ray_proxy = os.environ.get('WS_RAY_HTTP_PROXY')
        if self.ray_proxy is not None:
            try:
                proxy = urlsplit(self.ray_proxy)
                valid = len(self.ray_proxy) <= 4096 and proxy.scheme in ('http', 'https') and proxy.hostname and proxy.port != 0
            except ValueError:
                valid = False
            if not valid:
                raise Failure('WS_RAY_HTTP_PROXY must be an HTTP(S) proxy URL', 2)

    def signal(self, signum, _frame):
        # Do not throw between process creation and ownership transfer.
        if self.signum is None:
            self.signum = signum

    def spawn(self, name, command, **kwargs):
        child = OwnedChild(name, command, self.directory / (name + '.log'), **kwargs)
        self.children.append(child)
        return child

    def observe(self, required=()):
        if self.signum is not None:
            raise Failure(f'interrupted by signal {self.signum}', 128 + self.signum)
        for child in self.children:
            child.poll()
            if child.failure:
                raise Failure(f'{child.name}: {child.failure}')
        for child in required:
            if child.code is not None:
                raise Failure(f'{child.name} exited unexpectedly: {child.code}; see {child.log}', child.code or 1)

    def wait(self, condition, *, required=(), deadline=None):
        deadline = min(self.deadline, deadline if deadline is not None else math.inf)
        while True:
            self.observe(required)
            if time.monotonic() >= deadline:
                raise Failure('websearch launcher deadline exceeded')
            if condition():
                self.observe(required)
                if time.monotonic() >= deadline:
                    raise Failure('websearch launcher deadline exceeded')
                return
            time.sleep(0.02)

    def completed(self, child):
        self.wait(lambda: child.code is not None, deadline=time.monotonic()+self.build_timeout)
        if child.code:
            raise Failure(f'{child.name} exited {child.code}; see {child.log}', child.code)

    def prepare(self):
        if self.skip_build:
            default = ROOT / 'target' / ('debug' if self.profile == 'dev' else 'release') / 'websearch'
            self.binary = Path(os.environ.get('WS_BIN', str(default))).resolve()
            print(f'Build explicitly skipped; executable: {self.binary}', flush=True)
        else:
            build = self.spawn('build-rust', ['cargo', 'build', '--locked', '-p', 'websearch',
                                           '--profile', self.profile, '--message-format=json'])
            self.completed(build)
            executables = []
            for line in build.text().splitlines():
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if (event.get('reason') == 'compiler-artifact' and
                        event.get('target', {}).get('name') == 'websearch' and event.get('executable')):
                    executables.append(event['executable'])
            if len(executables) != 1:
                raise Failure('successful build did not identify exactly one websearch executable')
            self.binary = Path(executables[0]).resolve()
            # Cargo's artifact path handles configured target directories. Never
            # choose an unrelated old release binary after building dev.
            for name, command in [
                ('build-gateways', ['make', 'build-jvm', 'build-erl']),
                ('build-search', ['bash', str(APP / 'jvm/build.sh')]),
                ('build-frontier', ['erlc', '-o', str(APP / 'erlang'), str(APP / 'erlang/frontier.erl')]),
            ]:
                self.completed(self.spawn(name, command))
        if not self.binary.is_file() or not os.access(self.binary, os.X_OK):
            raise Failure(f'missing executable: {self.binary}')
        for tool in ['java', 'erl']:
            if shutil.which(tool) is None:
                raise Failure(f'missing required tool: {tool}')
        for artifact in [APP / 'jvm/target/websearch-jvm-1.0.0.jar', APP / 'erlang/frontier.beam']:
            if not artifact.is_file():
                raise Failure(f'missing component artifact: {artifact}')
        self.completed(self.spawn('python-check', [sys.executable, '-c', 'import ray, jieba']))
        spec = importlib.util.spec_from_file_location('websearch_gateway_config', ROOT / 'tools/parrot-obs/gateway_manager.py')
        manager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(manager)
        self.gateway_specs = manager.config_from_environment(
            self.directory, dict(os.environ, GW_ERL_PORT='0', GW_JVM_PORT='0', GW_RAY_PORT='0', GW_JVM_NODE='jvm-search-1'))['gateways']

    def application_arguments(self):
        args = list(self.arguments)
        # Classify tokens, not their contents: a data directory may be named
        # jvm=index or -h. Rust still owns value and duplicate validation.
        flags = []
        index = 0
        value_options = {'--pages', '--sites', '--maxdepth', '--port', '--data',
                         '--bind', '--wait', '--node-root', '--crawl-origin', '--index-data'}
        while index < len(args):
            value = args[index]
            if value == '--':
                break
            flags.append(value)
            index += 2 if value in value_options else 1
        if '--help' in flags or '-h' in flags:
            validation = self.spawn('validate', [self.binary, '--validate-only', *args])
            self.completed(validation)
            print(validation.text(), end='', flush=True)
            return None
        if any(value in ('--bind', '--validate-only') or value.startswith(('erl=', 'ray=', 'jvm=')) for value in flags):
            raise Failure('launcher owns gateway endpoints and --bind; use the registry entrypoint for reverse registration', 2)
        self.serve_only = '--serve-only' in flags
        if '--data' in flags:
            i = args.index('--data')
            if i + 1 >= len(args) or not args[i+1] or args[i+1].startswith('--'):
                raise Failure('--data requires a path', 2)
            self.data = Path(args[i+1]).absolute()
            args[i+1] = str(self.data)
        else:
            explicit = os.environ.get('WS_DATA')
            if not explicit and ('--keep' in flags or self.serve_only):
                raise Failure('--keep and --serve-only require --data or WS_DATA', 2)
            self.data = Path(explicit).absolute() if explicit else self.directory / 'data'
            args = ['--data', str(self.data), *args]
        for option, variable in [('--port', 'WS_PORT'), ('--sites', 'WS_SITES'), ('--maxdepth', 'WS_DEPTH')]:
            if option not in flags and (variable in os.environ or option == '--port'):
                args = [option, os.environ.get(variable, '8080'), *args]
        if self.mode == 'registry':
            args = ['--bind', '127.0.0.1:0', *args]
        # The Rust CLI remains the authority for all numeric, URL, duplicate and
        # option validation. This process exits before any storage or listener.
        self.completed(self.spawn('validate', [self.binary, '--validate-only', *args]))
        return args

    def gateways(self, target=None):
        children = []
        for spec in self.gateway_specs:
            if self.serve_only and spec['name'] != 'jvm':
                continue
            command = list(spec['command'])
            environment = dict(spec['env'], WS_DATA=str(self.data), PARROT_NODE_ID=spec['node'])
            if spec['name'] == 'ray':
                command = [sys.executable, str(Path(__file__).with_name('ray_gateway.py')), str(self.ray_directory), target or '']
                if self.ray_proxy is not None:
                    for key in ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy']:
                        environment[key] = self.ray_proxy
            elif target and spec['name'] == 'erl':
                command[-1] = f'parrot_gw:main(["0", "parrot={target}"]).'
            elif target and spec['name'] == 'jvm':
                command.append('parrot=' + target)
            children.append(self.spawn(spec['name'], command, environment=environment,
                                       marker=spec['marker'] if target is None else None,
                                       wait_for_output=False, python_entrypoint=spec['name'] == 'ray'))
        return children

    def healthy(self, app, deadline):
        if app.port is None:
            return False
        try:
            remaining = min(deadline, self.deadline) - time.monotonic()
            if remaining <= 0:
                return False
            # One absolute probe budget covers connect, headers and body. A
            # trickling response cannot reset a socket timeout indefinitely.
            async def request():
                reader, writer = await asyncio.open_connection('127.0.0.1', app.port, limit=65536)
                try:
                    writer.write(b'GET /stats HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n')
                    await writer.drain()
                    headers = (await reader.readuntil(b'\r\n\r\n')).split(b'\r\n')
                    if headers[0].split()[:2] != [b'HTTP/1.1', b'200']:
                        return None
                    lengths = [line.split(b':', 1)[1].strip() for line in headers[1:] if line.lower().startswith(b'content-length:')]
                    if len(lengths) != 1 or not lengths[0].isdigit() or len(lengths[0]) > 5:
                        return None
                    length = int(lengths[0])
                    if length > 65536:
                        return None
                    return json.loads(await reader.readexactly(length))
                finally:
                    writer.close()
                    await writer.wait_closed()
            async def bounded():
                return await asyncio.wait_for(request(), timeout=min(1, remaining))
            stats = asyncio.run(bounded())
            valid = isinstance(stats, dict) and all(type(stats.get(key)) is int and 0 <= stats[key] <= 2**64-1
                                                   for key in ['docs', 'terms', 'postings', 'queries', 'segments'])
            if valid:
                self.web_port = app.port
            return valid
        except (OSError, ValueError, asyncio.TimeoutError, asyncio.IncompleteReadError, asyncio.LimitOverrunError):
            return False

    def execute(self):
        self.prepare()
        args = self.application_arguments()
        if args is None:
            return
        deadline = time.monotonic() + self.startup_timeout
        if self.mode == 'registry':
            app = self.spawn('app', [self.binary, *args], marker='PARROT_WEBSEARCH_PORT=')
            port = None
            def bound():
                nonlocal port
                match = re.search(r'^PARROT_WEBSEARCH_REGISTRY_PORT=(\d+)$', app.text(), re.M)
                if match:
                    port = int(match.group(1))
                    if not 1 <= port <= 65535:
                        raise Failure('application reported an invalid registry port')
                    return True
                return False
            self.wait(bound, required=[app], deadline=deadline)
            gateways = self.gateways(f'127.0.0.1:{port}')
        else:
            gateways = self.gateways()
            self.wait(lambda: all(child.port is not None for child in gateways), required=gateways, deadline=deadline)
            endpoints = [f'{child.name}=127.0.0.1:{child.port}' for child in gateways]
            app = self.spawn('app', [self.binary, *endpoints, *args], marker='PARROT_WEBSEARCH_PORT=')
        self.wait(lambda: self.healthy(app, deadline), required=[app, *gateways], deadline=deadline)
        self.ready = True
        print(f'WEBSEARCH_READY=http://127.0.0.1:{self.web_port}', flush=True)
        print(f'Web ready; data: {self.data}; logs: {self.directory}', flush=True)
        # Keep supervising all gateways after startup, and wait for this exact
        # application child. A gateway failure cannot leave a false-ready owner.
        self.wait(lambda: app.code is not None, required=gateways)
        if app.code:
            raise Failure(f'application exited {app.code}; see {app.log}', app.code)

    def run(self):
        code, detail = 1, ''
        try:
            requested = os.environ.get('WS_RUN_DIR')
            if requested:
                directory = Path(requested).absolute()
                directory.mkdir(mode=0o700, exist_ok=False)
                self.directory = directory
            else:
                self.directory = Path(tempfile.mkdtemp(prefix='websearch-', dir='/tmp'))
            self.ray_directory = Path(tempfile.mkdtemp(prefix='ws-ray-', dir='/tmp'))
            print(f'WS_RUN_DIR={self.directory}', flush=True)
            self.execute()
            code = 0
        except Failure as error:
            code, detail = error.code, str(error)
        except Exception as error:
            code, detail = 1, str(error)
        finally:
            # Keep the gateways alive until the application has acknowledged
            # its final batch and snapshot. The guardian remains the group
            # owner throughout; final group cleanup still handles descendants.
            for app in (child for child in self.children if child.name == 'app'):
                try:
                    if app.code is None:
                        app.request_termination()
                        deadline = time.monotonic() + 65
                        while app.code is None and not app.failure and time.monotonic() < deadline:
                            app.poll()
                            time.sleep(0.02)
                        if app.code is None or app.failure:
                            raise Failure('application did not acknowledge shutdown: ' + str(app.failure))
                    if app.code and code == 0:
                        code, detail = 1, f'application shutdown failed: {app.code}'
                except Exception as error:
                    code, detail = 1, f'{detail}; application drain: {error}'
            for child in self.children:
                child.begin_stop()
            if self.children:
                time.sleep(2.2)
            for child in self.children:
                try:
                    child.finish_stop()
                except Exception as error:
                    code, detail = 1, f'{detail}; cleanup {child.name}: {error}'
            if self.signum is not None and code == 0:
                code, detail = 128+self.signum, f'interrupted by signal {self.signum}'
            if self.directory is not None:
                result = dict(exit_code=code, detail=detail, mode=self.mode, ready=self.ready,
                              binary=str(self.binary) if hasattr(self, 'binary') else None,
                              web_port=self.web_port, build='explicitly-skipped' if self.skip_build else self.profile,
                              ray_directory=str(self.ray_directory), children=[c.state() for c in self.children])
                try:
                    (self.directory/'result.json').write_text(json.dumps(result, indent=2)+'\n')
                except OSError as error:
                    code, detail = 1, f'{detail}; cannot write terminal result: {error}'
        if detail:
            print('websearch launcher: ' + detail, file=sys.stderr, flush=True)
        return code


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if not argv or argv[1:] in (['--help'], ['-h']):
        print('run.sh [websearch arguments] | deploy/run-registry.sh [websearch arguments]\n'
              'WS_PROFILE=dev|release; WS_SKIP_BUILD=1 explicitly uses existing artifacts; WS_BIN requires it.\n'
              'WS_DATA/--data selects persistent storage; default data and logs belong to a new private run.\n'
              'WS_PORT defaults to 8080 (0 allocates a port); WS_STARTUP_TIMEOUT defaults to 180 seconds.\n'
              'WS_TIMEOUT optionally bounds the whole run. Ctrl-C retires the owned process groups.\n'
              'Use --data with --keep or --serve-only. See apps/websearch/README.md for application options.')
        return 0
    try:
        launcher = Launcher(argv[0], argv[1:])
    except Failure as error:
        print('websearch launcher: ' + str(error), file=sys.stderr)
        return error.code
    signal.signal(signal.SIGTERM, launcher.signal)
    signal.signal(signal.SIGINT, launcher.signal)
    return launcher.run()


if __name__ == '__main__':
    raise SystemExit(main())
