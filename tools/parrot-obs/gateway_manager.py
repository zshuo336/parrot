#!/usr/bin/env python3
"""Owned, Unix-only lifecycle for the three demonstration gateways."""
from __future__ import annotations
import argparse
from collections import deque
import contextlib
import fcntl
import hashlib
import json
import importlib.util
import os
from pathlib import Path
import selectors
import signal
import socket
import stat
import subprocess
import sys
import time
import threading

ROOT = Path(__file__).resolve().parents[2]
WORKER = Path(__file__).with_name('gateway_worker.py')
_spec = importlib.util.spec_from_file_location('_parrot_process_session', Path(__file__).with_name('process_session.py'))
session = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(session)
START_TIMEOUT = 120.0
CONTROL_LIMIT = 4096


def run_directory():
    suffix = hashlib.sha256(str(ROOT).encode()).hexdigest()[:12]
    return Path(os.environ.get('GW_RUN_DIR', f'/tmp/parrot-obs-{os.getuid()}-{suffix}'))


def private_directory(path):
    path = Path(path).absolute()
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError(f'run directory must be a private owned directory: {path}')
    if len(os.fsencode(path / 'control.sock')) > 100:
        raise ValueError('run directory path is too long for a portable Unix socket')
    return path


def lock_file(path):
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        os.close(fd)
        raise ValueError('unsafe lock file')
    return os.fdopen(fd, 'r+')


def owner_active(directory):
    with lock_file(directory / 'owner.lock') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return False
        except BlockingIOError:
            return True


def write_json(path, value):
    temporary = path.with_name(path.name + '.tmp')
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as output:
        json.dump(value, output)
        output.write('\n')
    os.replace(temporary, path)


def _request(directory, op, timeout=3):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(timeout)
        try:
            client.connect(str(directory / 'control.sock'))
        except (FileNotFoundError, ConnectionRefusedError):
            return None
        client.sendall(json.dumps({'op': op}).encode() + b'\n')
        response = bytearray()
        while b'\n' not in response:
            chunk = client.recv(4096)
            if not chunk:
                return None
            response.extend(chunk)
            if len(response) > 65536:
                raise ValueError('control reply exceeds limit')
        return json.loads(bytes(response).split(b'\n', 1)[0])


def request(directory, op, timeout=3):
    try:
        return _request(directory, op, timeout)
    except (ConnectionResetError, BrokenPipeError):
        # Owner closure is reconciled against its lease / actual process exit.
        # It is not an acknowledgement that startup or retirement succeeded.
        return None


def config_from_environment(directory, environment=None):
    environment = os.environ if environment is None else environment
    def port(name, default):
        raw = environment.get(name, str(default))
        if not raw.isascii() or not raw.isdigit() or not 0 <= int(raw) <= 65535:
            raise ValueError(f'{name} must be an integer in 0..65535')
        return int(raw)
    erl_port = port('GW_ERL_PORT', 19871)
    jvm_port = port('GW_JVM_PORT', 19872)
    ray_port = port('GW_RAY_PORT', 19873)
    nonzero = [p for p in [erl_port, jvm_port, ray_port] if p]
    if len(set(nonzero)) != len(nonzero):
        raise ValueError('gateway ports must be distinct (or 0 for dynamic allocation)')
    node = environment.get('GW_JVM_NODE', 'jvm-search-1')
    if not node or len(node) > 128 or not all(c.isascii() and (c.isalnum() or c in '._-') for c in node):
        raise ValueError('GW_JVM_NODE must be 1..128 ASCII letters, digits, dot, underscore or hyphen')
    jar = ROOT / 'interop/jvm/target/parrot-protocol-jvm-0.1.0.jar'
    for path in [jar, ROOT / 'interop/jvm/target/cp.txt', ROOT / 'interop/erlang/parrot_gw.beam', ROOT / 'interop/erlang/parrot_trace.beam', ROOT / 'interop/erlang/parrot_component_runtime.beam', ROOT / 'interop/erlang/parrot_beam_snapshot.beam', ROOT / 'interop/erlang/parrot_beam_guest.beam', ROOT / 'interop/erlang/parrot_beam_host.beam']:
        if not path.is_file():
            raise ValueError(f'missing gateway artifact: {path}; run make check-jvm check-erl')
    classpath = (ROOT / 'interop/jvm/target/cp.txt').read_text().strip()
    # Resolve relative classpath entries in the same directory as the old JVM entrypoint.
    cwd = ROOT / 'interop/jvm/target'
    classpath = os.pathsep.join(str((cwd / p).resolve()) if not Path(p).is_absolute() else p for p in classpath.split(os.pathsep))
    specs = [
        dict(name='erl', node='erl-gw-1', port=erl_port, marker='PARROT_ERL_PORT=', cwd=str(ROOT), env={'PARROT_NODE_ID': 'erl-gw-1'},
             command=['erl', '-noinput', '-noshell', '-pa', str(ROOT / 'interop/erlang'), '-eval', f'parrot_gw:main(["{erl_port}"]).']),
        dict(name='jvm', node=node, port=jvm_port, marker='PARROT_JVM_PORT=', cwd=str(ROOT), env={'PARROT_NODE_ID': node},
             command=['java', '-cp', f'{jar}{os.pathsep}{classpath}', 'parrot.protocol.jvm.ParrotGatewayMain', str(jvm_port), f'node={node}', '86400']),
        dict(name='ray', node='ray-gw-1', port=ray_port, marker='RAY_GW_PORT=', cwd=str(ROOT),
             env={'PYTHONPATH': str(ROOT / 'interop/python'), 'PARROT_NODE_ID': 'ray-gw-1',
                  'RAY_ADDRESS': 'local', 'RAY_USAGE_STATS_ENABLED': '0'},
             python_entrypoint=True,
             command=[sys.executable, str(Path(__file__).with_name('ray_gateway.py')), str(ray_port)]),
    ]
    return {'directory': str(directory), 'gateways': specs, 'startup_timeout': START_TIMEOUT}


def digest(config):
    return hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest()


def snapshot(config, phase, ports, detail=''):
    return {'phase': phase, 'config': digest(config), 'ports': ports.copy(), 'detail': detail,
            'nodes': {entry['name']: entry['node'] for entry in config['gateways']}}


def serve(config):
    directory = private_directory(config['directory'])
    # Never auto-reap our guardians: their unreaped leaders pin every PGID until cleanup.
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)
    interrupted = False
    def interrupt(*_):
        nonlocal interrupted
        interrupted = True
    signal.signal(signal.SIGTERM, interrupt)
    signal.signal(signal.SIGINT, interrupt)
    with lock_file(directory / 'owner.lock') as lease:
        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        endpoint = directory / 'control.sock'
        with contextlib.suppress(FileNotFoundError):
            endpoint.unlink()
        selector = selectors.DefaultSelector()
        listener = socket.socket(socket.AF_UNIX)
        workers = []
        ports = {}
        state = snapshot(config, 'starting', ports)
        stop_client = None
        failed = False

        def persist_retirement():
            nonlocal failed
            try:
                write_json(directory / 'state.json', state)
            except OSError as error:
                # Diagnostic storage must never prevent signal/wait/close.
                failed = True
                state['phase'] = 'failed'
                detail = f'state persistence failed: {error}'
                if detail not in state['detail']:
                    state['detail'] = '; '.join(filter(None, [state['detail'], detail]))

        try:
            listener.bind(str(endpoint)); os.chmod(endpoint, 0o600)
            listener.listen(8); listener.setblocking(False)
            selector.register(listener, selectors.EVENT_READ, 'client')
            write_json(directory / 'state.json', state)
            for spec in config['gateways']:
                parent, child = socket.socketpair()
                try:
                    process = subprocess.Popen([sys.executable, str(WORKER), str(child.fileno())],
                                               pass_fds=(child.fileno(), lease.fileno()), start_new_session=True,
                                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                               stderr=subprocess.DEVNULL, close_fds=True)
                except BaseException:
                    parent.close(); raise
                finally:
                    child.close()
                record = {'process': process, 'channel': parent, 'name': spec['name'], 'buffer': bytearray()}
                workers.append(record)
                parent.settimeout(3)
                parent.sendall(json.dumps(dict(spec, log=str(directory / (spec['name'] + '.log')),
                                               ownership_fd=lease.fileno())).encode() + b'\n')
                parent.setblocking(False)
                selector.register(parent, selectors.EVENT_READ, record)
            deadline = time.monotonic() + config['startup_timeout']
            while not interrupted:
                if state['phase'] == 'starting' and time.monotonic() >= deadline:
                    raise TimeoutError('gateway readiness deadline expired')
                for key, _ in selector.select(0.1):
                    if key.data == 'client':
                        client, _ = listener.accept()
                        read_deadline = time.monotonic() + 0.25
                        try:
                            data = bytearray()
                            while b'\n' not in data:
                                remaining = read_deadline - time.monotonic()
                                if remaining <= 0: raise TimeoutError('control read deadline expired')
                                client.settimeout(remaining)
                                chunk = client.recv(1024)
                                if not chunk: break
                                data.extend(chunk)
                                if len(data) > CONTROL_LIMIT: raise ValueError('control request too large')
                            message = json.loads(bytes(data).split(b'\n', 1)[0])
                            if not isinstance(message, dict): raise ValueError('control message must be an object')
                            op = message.get('op')
                            if op == 'stop':
                                stop_client = client
                                interrupted = True
                                break
                            reply = state if op == 'status' else {'phase': 'error', 'detail': 'unknown control operation'}
                            client.sendall(json.dumps(reply).encode() + b'\n')
                        except (OSError, ValueError, TypeError):
                            pass
                        finally:
                            if client is not stop_client: client.close()
                    else:
                        record = key.data
                        data = record['channel'].recv(4096)
                        if not data: raise RuntimeError(f"{record['name']} guardian exited")
                        record['buffer'].extend(data)
                        if len(record['buffer']) > 8192: raise RuntimeError('worker response exceeds limit')
                        while b'\n' in record['buffer']:
                            raw, _, rest = record['buffer'].partition(b'\n')
                            record['buffer'] = bytearray(rest)
                            event = json.loads(raw)
                            if event['event'] != 'ready':
                                raise RuntimeError(f"{record['name']} gateway failed: {event}")
                            ports[record['name']] = event['port']
                            state = snapshot(config, 'running' if len(ports) == len(workers) else 'starting', ports)
                            write_json(directory / 'state.json', state)
        except BaseException as error:
            failed = True
            state = snapshot(config, 'failed', ports, str(error))
        finally:
            state = snapshot(config, 'stopping', ports, state.get('detail', ''))
            persist_retirement()
            for record in workers:
                with contextlib.suppress(OSError): record['channel'].sendall(b'stop\n')
            # Guardians also handle control EOF, including manager SIGKILL.
            if workers: time.sleep(2.2)
            deadline = session.retirement_deadline()
            for record in workers:
                # Verify unreaped ownership; never signal enumerated or saved IDs.
                try:
                    session.final_signal(record['process'])
                except Exception as error:
                    failed = True
                    state['detail'] += f"; signal {record['name']}: {error}"
            retired = {}
            for record in workers:
                # One budget for the whole group of gateways. Keep each identity
                # pinned until its complete session is observed retired.
                try:
                    retired[record['name']] = session.wait_retired(record['process'], deadline)
                except Exception as error:
                    failed = True
                    state['detail'] += f"; wait {record['name']}: {error}"
                finally:
                    record['channel'].close()
            state = snapshot(config, 'failed' if failed else 'stopped', ports, state.get('detail', ''))
            state['guardian_exits'] = retired
            persist_retirement()
            if stop_client:
                with contextlib.suppress(OSError): stop_client.sendall(json.dumps(state).encode() + b'\n')
                stop_client.close()
            selector.close(); listener.close()
            with contextlib.suppress(FileNotFoundError): endpoint.unlink()
        return state


def start(config):
    directory = private_directory(config['directory'])
    with lock_file(directory / 'launch.lock') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        current = request(directory, 'status')
        if current and current.get('config') != digest(config):
            raise RuntimeError('different gateway configuration is active; stop this owned session first')
        process = None
        if current is None:
            if owner_active(directory):
                raise RuntimeError('gateway owner is active but control endpoint is unavailable')
            write_json(directory / 'config.json', config)
            logfd = os.open(directory / 'manager.log', os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW, 0o600)
            with os.fdopen(logfd, 'ab') as output:
                process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '_serve', str(directory / 'config.json')],
                                           stdin=subprocess.DEVNULL, stdout=output, stderr=output,
                                           start_new_session=True, close_fds=True)
                threading.Thread(target=process.wait, daemon=True).start()
        deadline = time.monotonic() + config['startup_timeout'] + 10
        while time.monotonic() < deadline:
            current = request(directory, 'status')
            if current and current['phase'] == 'running':
                return current
            if process is not None and process.poll() is not None:
                saved = json.loads((directory / 'state.json').read_text()) if (directory / 'state.json').is_file() else {}
                raise RuntimeError('gateway startup failed: ' + saved.get('detail', 'owner exited'))
            if process is None and not owner_active(directory):
                raise RuntimeError('gateway owner exited during startup')
            time.sleep(0.1)
        raise TimeoutError('gateway owner did not finish startup; inspect its private state/logs')


def stop(directory):
    directory = private_directory(directory)
    try:
        reply = request(directory, 'stop', timeout=20)
    except (ConnectionResetError, BrokenPipeError):
        reply = None
    deadline = time.monotonic() + 20
    while owner_active(directory):
        if time.monotonic() >= deadline: raise TimeoutError('owner is still retiring gateways; no PIDs were signalled by the client')
        time.sleep(0.05)
    if reply and reply['phase'] == 'failed': raise RuntimeError(reply['detail'])
    return {'phase': 'stopped'}


def main():
    if len(sys.argv) == 3 and sys.argv[1] == '_serve':
        result = serve(json.loads(Path(sys.argv[2]).read_text()))
        raise SystemExit(1 if result['phase'] == 'failed' else 0)
    parser = argparse.ArgumentParser(description='Own three development gateway process groups via a private control socket.')
    parser.add_argument('command', choices=['up', 'down', 'status', 'log'], nargs='?', default='status')
    parser.add_argument('gateway', nargs='?', choices=['erl', 'jvm', 'ray', 'all'], default='all')
    args = parser.parse_args()
    directory = private_directory(run_directory())
    if args.command == 'up':
        value = start(config_from_environment(directory))
        nodes = ' '.join(f'{name}@{value["nodes"][name]}=127.0.0.1:{port}' for name, port in sorted(value['ports'].items()))
        print('NODES=' + json.dumps(nodes)); print('Use parrot-obs with --insecure-development for these demo gateways.')
    elif args.command == 'down': print(json.dumps(stop(directory)))
    elif args.command == 'status':
        value = request(directory, 'status')
        if not value:
            if owner_active(directory): raise RuntimeError('owner active but control endpoint unavailable')
            print(json.dumps({'phase': 'stopped'})); return
        print(json.dumps(value))
        if value['phase'] != 'running': raise RuntimeError(value['phase'])
    else:
        for name in (['erl', 'jvm', 'ray'] if args.gateway == 'all' else [args.gateway]):
            path = directory / (name + '.log')
            print(f'[{name}]')
            if path.is_file():
                with path.open(errors='replace') as source: print(''.join(deque(source, maxlen=20)), end='')
            else: print('(no owned gateway log)')


if __name__ == '__main__':
    try: main()
    except (OSError, ValueError, RuntimeError, TimeoutError) as error:
        print(f'gateway manager: {error}', file=sys.stderr)
        raise SystemExit(1)
