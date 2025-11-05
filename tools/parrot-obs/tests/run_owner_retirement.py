#!/usr/bin/env python3
"""Actual gateway process trees must retire, not just the recorded guardians.

Requires built gateways, parrot-obs and Ray. Keeps private logs, process creation
identities and terminal records; failures are not skips, including under -O.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import traceback

from ray._private.services import psutil

ROOT = Path(__file__).resolve().parents[3]
TOOL = ROOT / 'tools/parrot-obs'
spec = importlib.util.spec_from_file_location('manager', TOOL / 'gateway_manager.py')
manager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manager)


def require(condition, detail):
    if not condition:
        raise AssertionError(detail)


class Session:
    def __init__(self, evidence, name):
        self.evidence = evidence
        self.name = name
        self.directory = Path(tempfile.mkdtemp(prefix='pg-owner-', dir='/tmp'))
        # Deliberately hostile ambient identity/address must not retarget the
        # local demonstration's configured nodes or join a different cluster.
        self.environment = dict(os.environ, GW_RUN_DIR=str(self.directory),
                                GW_ERL_PORT='0', GW_JVM_PORT='0', GW_RAY_PORT='0',
                                GW_JVM_NODE='search-owned', PYTHON=sys.executable,
                                PARROT_NODE_ID='ambient-wrong', RAY_ADDRESS='127.0.0.1:1')
        self.config = manager.config_from_environment(self.directory, self.environment)
        manager.write_json(self.directory / 'config.json', self.config)
        self.log = (self.directory / 'manager.log').open('w')
        self.process = subprocess.Popen([sys.executable, str(TOOL / 'gateway_manager.py'),
                                         '_serve', str(self.directory / 'config.json')],
                                        env=self.environment, stdout=self.log, stderr=subprocess.STDOUT)
        self.identity = psutil.Process(self.process.pid)
        self.observed = {}
        self.state = None
        self.retired = False

    def sample(self):
        try:
            children = self.identity.children(recursive=True)
        except psutil.NoSuchProcess:
            children = []
        for child in children:
            try:
                created = child.create_time()
                try:
                    command = child.cmdline()
                except psutil.AccessDenied:
                    command = ['<command unavailable during sampling>']
                self.observed[(child.pid, created)] = dict(
                    pid=child.pid, created=created, command=command, group=os.getpgid(child.pid))
            except (psutil.NoSuchProcess, ProcessLookupError):
                pass

    def ready(self):
        deadline = time.monotonic() + 130
        while True:
            self.sample()
            state = manager.request(self.directory, 'status')
            if state and state['phase'] == 'running':
                self.state = state
                self.sample()
                require(any('raylet' in ' '.join(row['command']) for row in self.observed.values()),
                        'did not observe the actual local Ray runtime')
                return state
            require(self.process.poll() is None, 'manager exited during startup: ' + (self.directory / 'manager.log').read_text())
            require(time.monotonic() < deadline, 'manager startup deadline')
            time.sleep(.025)

    def cli(self, *args, succeeds=True):
        result = subprocess.run(['bash', str(TOOL / 'gw.sh'), *args], cwd='/tmp',
                                env=self.environment, text=True, capture_output=True, timeout=150)
        with (self.directory / 'public-cli.log').open('a') as log:
            log.write(f'{args!r} exit={result.returncode}\n' + result.stdout + result.stderr)
        require((result.returncode == 0) == succeeds, result.stdout + result.stderr)
        return result

    def business(self):
        nodes = [f'{name}@{self.state["nodes"][name]}=127.0.0.1:{port}'
                 for name, port in sorted(self.state['ports'].items())]
        result = subprocess.run([str(ROOT / 'target/debug/parrot-obs'), 'ask', '--rounds=2',
                                 *nodes, '--insecure-development'], text=True, capture_output=True, timeout=45)
        (self.directory / 'business.log').write_text(result.stdout + result.stderr)
        require(result.returncode == 0, result.stdout + result.stderr)
        # Also exercise real Ray artifact deployment and lifecycle. This existing
        # helper uses asserts; invoke its body without optimization in a child
        # so the outer runner's -O can never remove its checks.
        result = subprocess.run([sys.executable, '-c',
                                'import sys;sys.path.insert(0,sys.argv[1]);from ray_registry_contract import check_ray_registry;check_ray_registry(int(sys.argv[2]))',
                                str(TOOL / 'tests'), str(self.state['ports']['ray'])],
                                env={k:v for k,v in os.environ.items() if k != 'PYTHONOPTIMIZE'},
                                text=True, capture_output=True, timeout=100)
        (self.directory / 'ray-business.log').write_text(result.stdout + result.stderr)
        require(result.returncode == 0 and 'RAY_REGISTRY_OK' in result.stdout,
                result.stdout + result.stderr)
        self.sample()

    def finish(self, expected=0):
        deadline = time.monotonic() + 20
        while self.process.poll() is None:
            self.sample()
            require(time.monotonic() < deadline, 'manager did not exit')
            time.sleep(.025)
        code = self.process.wait(timeout=5)
        live = []
        while time.monotonic() < deadline:
            live = []
            for (pid, created), row in self.observed.items():
                try:
                    process = psutil.Process(pid)
                    if process.create_time() == created and process.status() != psutil.STATUS_ZOMBIE:
                        live.append(row)
                except psutil.NoSuchProcess:
                    pass
            if not live and not manager.owner_active(self.directory):
                break
            time.sleep(.025)
        errors = []
        for name, port in (self.state or {}).get('ports', {}).items():
            with socket.socket() as listener:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                try:
                    listener.bind(('127.0.0.1', port))
                except OSError as error:
                    errors.append(dict(name=name, port=port, error=str(error)))
        result = dict(name=self.name, directory=str(self.directory), exit_code=code,
                      state=self.state, observed=list(self.observed.values()),
                      live_after_exit=live, port_errors=errors,
                      lease_held=manager.owner_active(self.directory))
        saved = json.loads((self.directory / 'state.json').read_text())
        result['terminal_state'] = saved
        (self.evidence / (self.name + '.json')).write_text(json.dumps(result, indent=2) + '\n')
        require(not live and not errors and not result['lease_held'], 'owned resources did not retire: ' + str(self.directory))
        self.retired = True
        require(code == expected, f'{self.name} actual manager exit {code}, expected {expected}')
        if expected != -signal.SIGKILL:
            require(saved['phase'] == ('failed' if expected else 'stopped'),
                    'manager exited without completing terminal state: ' + str(saved))
            require(len(saved.get('guardian_exits', {})) == 3, 'guardians not actually reaped')
            require(not (self.directory / 'control.sock').exists(), 'terminal control socket retained')

    def close(self):
        try:
            if self.process.poll() is None:
                self.process.terminate()
                self.finish()
            elif not self.retired:
                self.finish(expected=self.process.returncode)
        finally:
            self.log.close()


def run(evidence):
    sessions, failures, passed = [], [], []
    try:
        normal = Session(evidence, 'normal')
        sessions.append(normal)
        normal.ready()
        initial = normal.cli('up').stdout
        require(normal.cli('up').stdout == initial, 'repeated up changed endpoints')
        normal.business()
        normal.cli('down')
        normal.finish()
        passed.append('public-lifecycle-and-business')

        killed = Session(evidence, 'manager-kill')
        sessions.append(killed)
        killed.ready()
        killed.process.kill()
        killed.finish(expected=-signal.SIGKILL)
        passed.append('manager-sigkill')

        crashed = Session(evidence, 'ray-kill')
        sessions.append(crashed)
        crashed.ready()
        # The inline runtime's guardian is a direct child of our still-live
        # manager. Its actual command-line and creation time identify it.
        candidates = [row for row in crashed.observed.values()
                      if 'gateway_worker.py' in ' '.join(row['command'])
                      and '--watch-group' not in row['command']]
        ray_pid = next(row['group'] for row in crashed.observed.values()
                       if row['command'] and Path(row['command'][0]).name == 'raylet')
        row = next(row for row in candidates if row['pid'] == ray_pid)
        target = psutil.Process(ray_pid)
        require(target.create_time() == row['created'], 'Ray identity changed')
        target.kill()
        crashed.finish(expected=1)
        passed.append('ray-runtime-sigkill')

        first = Session(evidence, 'concurrent-one')
        sessions.append(first)
        second = Session(evidence, 'concurrent-two')
        sessions.append(second)
        first.ready()
        second.ready()
        require(set(first.state['ports'].values()).isdisjoint(second.state['ports'].values()),
                'concurrent gateway listeners collided')
        dirs = []
        for item in [first, second]:
            dirs.append(next(line for line in (item.directory / 'ray.log').read_text().splitlines()
                             if line.startswith('PARROT_RAY_DIRECTORY=')))
        require(len(set(dirs)) == 2, 'concurrent clusters share logs')
        first.cli('down')
        first.finish()
        require(second.process.poll() is None, 'stopping one owner killed the other')
        second.business()
        second.cli('down')
        second.finish()
        passed.append('concurrent-ray-clusters-and-unrelated-owner-protection')
    except Exception as error:
        failures.append(str(error))
        traceback.print_exc()
    finally:
        for session in sessions:
            try:
                session.close()
            except Exception as error:
                failures.append('cleanup ' + session.name + ': ' + str(error))
        (evidence / 'result.json').write_text(json.dumps(dict(passed=passed, failed=failures), indent=2) + '\n')
    require(not failures and len(passed) == 4, 'actual owner retirement matrix failed: ' + str(failures))
    print('OWNED_RUNTIME_RETIREMENT_PASS: public lifecycle, real business, manager/runtime SIGKILL, concurrent private clusters and descendant retirement', flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    evidence = args.output or Path(tempfile.mkdtemp(prefix='pg-retirement-evidence-', dir='/tmp'))
    if args.output:
        evidence.mkdir(parents=True, exist_ok=False)
    print('OWNER_RETIREMENT_ARTIFACTS=' + str(evidence), flush=True)
    run(evidence)
