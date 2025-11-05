#!/usr/bin/env python3
"""Real process fixture for ownership tests; no Parrot business claims."""
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve()
PASS = 'crawler-lab PASS（四运行时全链集成）'


def listener():
    sock = socket.socket()
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(('127.0.0.1', 0))
    sock.listen()
    return sock


def main():
    role = sys.argv[1]
    scenario = os.environ.get('LAUNCHER_CASE', 'success')
    if role == 'driver':
        spec = importlib.util.spec_from_file_location('launcher', ROOT / 'apps/crawler-lab/deploy/launcher.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        class FixtureLauncher(module.Launcher):
            def prepare(self):
                self.binary = Path(os.environ['LAUNCHER_APP'])

            def gateways(self, target=None):
                children = []
                for name in ['erl', 'jvm', 'ray']:
                    child = self.spawn(name, [sys.executable, HERE, 'gateway', name],
                                       marker='READY=', wait_for_output=False,
                                       python_entrypoint=scenario.startswith('inline') and name == 'ray')
                    children.append(child)
                return children

        module.Launcher = FixtureLauncher
        return module.main(sys.argv[2:])
    if role == 'gateway':
        name = sys.argv[2]
        if scenario.startswith('inline') and name == 'ray':
            try:
                os.setpgrp()
            except PermissionError:
                if os.getpgrp() != os.getpid():
                    raise
            if scenario == 'inline-signal':
                signal.signal(signal.SIGTERM, lambda *_: os._exit(17))
        if scenario == 'partial' and name == 'ray':
            time.sleep(0.3)
            return 7
        sock = listener()
        # Even when the direct child exits, this descendant holds stdout and the
        # listening socket and ignores TERM. The group owner must retire both.
        read, write = os.pipe()
        descendant = subprocess.Popen([sys.executable, str(HERE), 'descendant', str(write)],
                                      pass_fds=(sock.fileno(), write))
        os.close(write)
        if os.read(read, 1) != b'R':
            raise RuntimeError('descendant did not start')
        os.close(read)
        record = Path(os.environ['LAUNCHER_RECORDS']) / (name + '.json')
        record.write_text(json.dumps(dict(pid=os.getpid(), descendant=descendant.pid,
                                         port=sock.getsockname()[1], group=os.getpgrp())))
        if scenario == 'bad' and name == 'ray':
            print('READY=invalid', flush=True)
        elif scenario != 'never':
            print('READY=' + str(sock.getsockname()[1]), flush=True)
        if scenario == 'crash' and name == 'ray':
            time.sleep(0.4)
            return 8
        time.sleep(90)
        return 0
    if role == 'descendant':
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        os.write(int(sys.argv[2]), b'R')
        os.close(int(sys.argv[2]))
        time.sleep(90)
        return 0
    if role == 'app':
        if '--bind' in sys.argv:
            sock = listener()
            print('PARROT_CRAWLER_PORT=' + str(sock.getsockname()[1]), flush=True)
            time.sleep(0.5)
        if scenario in ('signal', 'inline-signal', 'crash', 'timeout'):
            time.sleep(90)
        if scenario == 'app-fail':
            print(PASS, flush=True)
            return 23
        if scenario == 'no-pass':
            return 0
        if scenario == 'tail':
            for _ in range(15000):
                print('output padding ' + 'x' * 200)
        print(PASS, flush=True)
        return 0
    if role == 'npm':
        return 24 if scenario == 'ts-build' else 0
    if role == 'node':
        if scenario != 'ts-no-query':
            print('[ts-user] search parrot+actor', flush=True)
        return 25 if scenario == 'ts-fail' else 0
    raise ValueError(role)


if __name__ == '__main__':
    raise SystemExit(main())
