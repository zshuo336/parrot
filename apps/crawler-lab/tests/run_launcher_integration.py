#!/usr/bin/env python3
"""Actual polyglot launcher, process-tree retirement and port-release checks.

Ray is a required runtime; its bundled psutil supplies process identity checks.
All outputs are preserved in a fresh directory. This runner must finish and
write result.json; a launcher PASS alone does not certify resource retirement.
"""
import argparse
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
from process_observer import command_line

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / 'apps/crawler-lab/deploy/run-lab.sh'


def require(condition, message):
    if not condition:
        raise AssertionError(message)


class Run:
    def __init__(self, directory, *, mode='direct', pages=12, interrupt=None, bad_app=False, hub_options=(), expected_code=None, launch_command=None, signal_marker=None, extra_environment=None, application=None):
        self.directory = directory
        self.interrupt = interrupt
        self.sent = False
        self.raw_application = launch_command is not None
        self.signal_marker = signal_marker or 'LAB_NODES='
        self.listeners = set()
        self.sampling_errors = []
        self.next_listener_sample = 0
        self.identities = {}
        self.stdout = directory.with_suffix('.stdout.log')
        self.stream = self.stdout.open('w')
        environment = dict(os.environ, LAB_RUN_DIR=str(directory), LAB_MODE=mode,
                           LAB_TIMEOUT='150', LAB_STARTUP_TIMEOUT='90')
        # Deliberately incorrect ambient values must not redirect owned gateways
        # to another service or cause a shared Ray connection.
        environment.update(GW_ERL_PORT='1', GW_RAY_PORT='1', GW_JVM_PORT='1',
                           GW_JVM_NODE='ambient-must-not-win', RAY_ADDRESS='127.0.0.1:1')
        environment.update(extra_environment or {})
        if bad_app:
            environment['PARROT_LAB_BIN'] = '/usr/bin/false'
        elif application is not None:
            environment['PARROT_LAB_BIN'] = str(application)
        else:
            environment.pop('PARROT_LAB_BIN', None)
        workload = [] if pages is None else ['--pages', str(pages)]
        self.process = subprocess.Popen(launch_command or [str(SCRIPT), *workload, *hub_options], cwd=ROOT,
                                        stdout=self.stream, stderr=subprocess.STDOUT,
                                        env=environment)
        self.identity = psutil.Process(self.process.pid)
        self.started = time.monotonic()
        self.expected = -signal.SIGKILL if interrupt == signal.SIGKILL else (128 + interrupt if interrupt else (1 if bad_app else 0))

        if expected_code is not None:
            self.expected = expected_code

    def sample(self):
        sample_listeners = time.monotonic() >= self.next_listener_sample
        if sample_listeners:
            self.next_listener_sample = time.monotonic() + .25
        try:
            for child in [self.identity, *self.identity.children(recursive=True)]:
                try:
                    created = child.create_time()
                    try:
                        command = command_line(child)
                    except psutil.AccessDenied:
                        # macOS can deny command-line access during process
                        # teardown. Keep the identity for retirement checking.
                        command = ['<command unavailable during sampling>']
                    self.identities[(child.pid, created)] = dict(
                        pid=child.pid, created=created, command=command,
                        group=os.getpgid(child.pid))
                    if sample_listeners:
                        for connection in child.net_connections(kind='tcp'):
                            if connection.status == psutil.CONN_LISTEN:
                                self.listeners.add((int(connection.family), connection.laddr.ip, connection.laddr.port))
                except (psutil.NoSuchProcess, ProcessLookupError):
                    pass
                except psutil.AccessDenied as error:
                    self.sampling_errors.append(dict(pid=child.pid, detail=str(error)))
        except psutil.NoSuchProcess:
            pass
        text = self.stdout.read_text()
        if self.interrupt and not self.sent and self.signal_marker in text:
            self.process.send_signal(self.interrupt)
            self.sent = True
        require(time.monotonic() - self.started < 180, 'integration deadline: ' + str(self.directory))
        return self.process.poll() is not None

    def finish(self):
        code = self.process.wait(timeout=5)
        self.stream.close()
        text = self.stdout.read_text()
        deadline = time.monotonic() + 12
        while True:
            live = []
            for (pid, created), row in self.identities.items():
                try:
                    process = psutil.Process(pid)
                    if process.create_time() == created and process.status() != psutil.STATUS_ZOMBIE:
                        live.append(row)
                except psutil.NoSuchProcess:
                    pass
            if not live or time.monotonic() >= deadline:
                break
            time.sleep(.05)
        ports = {}
        for line in text.splitlines():
            if line.startswith('LAB_NODES='):
                ports.update(json.loads(line.split('=', 1)[1]))
        if (self.directory / 'result.json').exists():
            result = json.loads((self.directory / 'result.json').read_text())
            for child in result['children']:
                if child['port']:
                    ports[child['name']] = child['port']
        port_errors = []
        for name, port in ports.items():
            try:
                with socket.socket() as sock:
                    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    sock.bind(('127.0.0.1', port))
            except OSError as error:
                port_errors.append(dict(name=name, port=port, error=str(error)))
        # Include nested gateway and private Ray listeners, not just the front door.
        import ipaddress
        for family, host, port in sorted(self.listeners):
            if family == socket.AF_INET6:
                address = ipaddress.IPv6Address(host)
                if address.ipv4_mapped is not None:
                    family, host = socket.AF_INET, str(address.ipv4_mapped)
                elif sys.platform == 'darwin' and host.startswith('::') and '.' in host:
                    family, host = socket.AF_INET, host[2:]
            try:
                with socket.socket(family, socket.SOCK_STREAM) as sock:
                    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    sock.bind((host, port))
            except OSError as error:
                port_errors.append(dict(host=host, port=port, error=str(error)))
        record = dict(exit_code=code, signal=self.interrupt, signal_sent=self.sent,
                      observed_processes=list(self.identities.values()), live_after_exit=live,
                      ports=ports, port_errors=port_errors, listeners=sorted(self.listeners), sampling_errors=self.sampling_errors)
        self.directory.with_suffix('.terminal.json').write_text(json.dumps(record, indent=2) + '\n')
        require(not live and not port_errors, 'resources leaked: ' + json.dumps(record))
        require(code == self.expected, f'{self.directory}: expected {self.expected}, got {code}: {text[-3000:]}')
        marker = 'crawler-lab PASS（四运行时全链集成）' if self.raw_application else 'CRAWLER_LAB_LAUNCHER_PASS'
        require((marker in text) == (code == 0), text[-3000:])
        if self.interrupt:
            require(self.sent, 'did not actually inject signal')
        require(any('raylet' in ' '.join(row['command']) for row in self.identities.values()), 'no actual Ray runtime observed')
        return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    root = args.output or Path(tempfile.mkdtemp(prefix='crawler-launcher-real-', dir='/tmp'))
    if args.output:
        root.mkdir(parents=True, exist_ok=False)
    print('CRAWLER_LAUNCHER_ARTIFACTS=' + str(root), flush=True)
    cases = [
        [('direct', dict(pages=60))],
        [('registry', dict(mode='registry', pages=60))],
        [('concurrent-direct', {}), ('concurrent-registry', dict(mode='registry'))],
        [('app-failure', dict(bad_app=True))],
        [('term', dict(interrupt=signal.SIGTERM))],
        [('kill', dict(interrupt=signal.SIGKILL))],
    ]
    results, failures = {}, []
    for batch in cases:
        runs = []
        try:
            for name, options in batch:
                runs.append((name, Run(root / name, **options)))
            while True:
                completed = [run.sample() for _, run in runs]
                if all(completed):
                    break
                time.sleep(.025)
            for name, run in runs:
                results[name] = run.finish()
                print('CRAWLER_LAUNCHER_REAL_PASS ' + name, flush=True)
        except Exception as error:
            failures.append(str(error))
            print('CRAWLER_LAUNCHER_REAL_FAIL ' + str(error), flush=True)
            traceback.print_exc()
        finally:
            for _, run in runs:
                if run.process.poll() is None:
                    run.process.kill()
                run.process.wait(timeout=15)
                run.stream.close()
    (root / 'result.json').write_text(json.dumps(dict(passed=list(results), failed=failures), indent=2) + '\n')
    require(not failures and len(results) == 7, 'actual launcher matrix failed')


if __name__ == '__main__':
    main()
