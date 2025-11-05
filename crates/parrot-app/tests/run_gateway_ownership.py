#!/usr/bin/env python3
"""Run actual host scenarios and independently verify descendant retirement.

Requires built JVM/BEAM components and Ray (make test-app-polyglot). No skips;
assertions remain effective with Python -O. Logs and identities are retained.
"""
import argparse
import errno
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import signal
import socket
import subprocess
import sys
import tempfile
import time

from ray._private.services import psutil

ROOT = Path(__file__).resolve().parents[3]


def require(value, detail):
    if not value:
        raise RuntimeError(detail)


def remember_identity(observed, pid, created, argv, group):
    key = (pid, created)
    # Linux zombies have an empty cmdline. An exit observation must not erase
    # the command independently observed while this exact process was alive.
    # A nonempty exec/rename observation still replaces the earlier command.
    if key not in observed or (argv and argv != ['<unavailable during process exit>']):
        observed[key] = dict(pid=pid, created=created, command=list(argv), group=group)


def bind_endpoint(endpoint):
    family, host, port = endpoint
    if family == socket.AF_INET6:
        address = ipaddress.IPv6Address(host)
        if address.ipv4_mapped is not None:
            return int(socket.AF_INET), str(address.ipv4_mapped), port
        # macOS psutil loses ffff for mapped sockets (getsockname retains it).
        # Keep the raw observation, but test the actual IPv4 bind namespace.
        if sys.platform == 'darwin' and host.startswith('::') and '.' in host:
            return int(socket.AF_INET), str(ipaddress.IPv4Address(host[2:])), port
    return family, host, port


def verify_port_probe(output):
    with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as occupied:
        occupied.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        occupied.bind(('::ffff:127.0.0.1', 0))
        occupied.listen()
        actual = occupied.getsockname()
        records = [connection for connection in psutil.Process(os.getpid()).net_connections(kind='tcp')
                   if connection.fd == occupied.fileno() and connection.status == psutil.CONN_LISTEN]
        require(len(records) == 1, 'unable to observe the probe listener')
        record = records[0]
        raw = int(record.family), record.laddr.ip, record.laddr.port
        family, host, port = bind_endpoint(raw)
        with socket.socket(family, socket.SOCK_STREAM) as probe:
            try:
                probe.bind((host, port))
            except OSError as error:
                require(error.errno == errno.EADDRINUSE, 'live probe failed for an unrelated address error: ' + str(error))
            else:
                raise RuntimeError('probe falsely accepted an occupied listener')
    with socket.socket(family, socket.SOCK_STREAM) as probe:
        probe.bind((host, port))
    (output / 'probe.json').write_text(json.dumps(dict(actual=actual, observed=raw,
        normalized=[family, host, port], occupied_rejected=True, closed_rebound=True), indent=2) + '\n')


def probe_retired_listeners(endpoints, timeout=65, interval=.1):
    """Require actual rebind, allowing a single bounded kernel-release window.

    EADDRINUSE may outlive a process (or reflect another user of the port).
    Never infer success from process absence; only a successful bind clears it.
    Other errors are terminal. Keep the first failure and recovery time for audit.
    """
    started = time.monotonic()
    deadline = started + timeout
    pending = set(endpoints)
    errors = {}
    history = {}
    while pending:
        for endpoint in sorted(pending):
            family, host, port = endpoint
            try:
                with socket.socket(family, socket.SOCK_STREAM) as listener:
                    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    if family == socket.AF_INET6:
                        listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
                    listener.bind((host, port))
            except OSError as error:
                record = dict(family=family, host=host, port=port,
                              errno=error.errno, error=str(error))
                errors[endpoint] = record
                row = history.setdefault(endpoint, dict(record, failed_attempts=0))
                row['failed_attempts'] += 1
                if error.errno != errno.EADDRINUSE:
                    pending.remove(endpoint)
            else:
                errors.pop(endpoint, None)
                pending.remove(endpoint)
                if endpoint in history:
                    history[endpoint]['rebound_after_seconds'] = time.monotonic() - started
        remaining = deadline - time.monotonic()
        if not pending or remaining <= 0:
            break
        time.sleep(min(interval, remaining))
    return [errors[key] for key in sorted(errors)], [history[key] for key in sorted(history)]


def run(output, cargo='cargo'):
    output.mkdir(parents=True, exist_ok=False)
    verify_port_probe(output)
    temporary = Path(tempfile.mkdtemp(prefix='ph-own-', dir='/tmp'))
    command = shlex.split(cargo) + ['test', '--locked', '-p', 'parrot-app', '--features', 'host',
               '--test', 'app_run_scenarios', '--test', 'mg_multiengine', '--', '--include-ignored']
    environment = dict(os.environ, TMPDIR=str(temporary), CARGO_INCREMENTAL='0',
                       RAY_ADDRESS='127.0.0.1:1', PARROT_NODE_ID='ambient-must-not-win')
    observed = {}
    sampling_errors = []
    listeners = set()
    listener_samples = {}
    monitor_error = None
    deadline = time.monotonic() + 600
    with (output / 'business.txt').open('w') as log:
        process = subprocess.Popen(command, cwd=ROOT, env=environment,
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        root = psutil.Process(process.pid)
        timed_out = False
        next_listener_sample = 0
        try:
            while process.poll() is None:
                try:
                    children = root.children(recursive=True)
                except psutil.NoSuchProcess:
                    children = []
                sample_listeners = time.monotonic() >= next_listener_sample
                if sample_listeners:
                    next_listener_sample = time.monotonic() + .25
                for child in children:
                    try:
                        created = child.create_time()
                        try:
                            argv = child.cmdline()
                        except psutil.AccessDenied:
                            argv = ['<unavailable during process exit>']
                        key = (child.pid, created)
                        remember_identity(observed, child.pid, created, argv, os.getpgid(child.pid))
                        if sample_listeners:
                            for connection in child.net_connections(kind='tcp'):
                                if connection.status == psutil.CONN_LISTEN:
                                    endpoint = (int(connection.family), connection.laddr.ip, connection.laddr.port)
                                    listeners.add(endpoint)
                                    listener_samples.setdefault(key, set()).add(endpoint)
                    except (psutil.NoSuchProcess, ProcessLookupError):
                        pass
                    except psutil.AccessDenied as error:
                        # macOS can deny socket inspection during process exit.
                        # Retain the diagnostic; require actual Ray listener evidence.
                        sampling_errors.append(dict(pid=child.pid, detail=str(error)))
                if time.monotonic() >= deadline:
                    timed_out = True
                    break
                time.sleep(.025)
        except Exception as error:
            monitor_error = repr(error)
        finally:
            # The unreaped cargo root pins this new group. Its test binaries
            # die with it; their guardians independently act on control EOF.
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        code = process.wait()
    def live_processes():
        live = []
        for (pid, created), row in observed.items():
            try:
                child = psutil.Process(pid)
                if child.create_time() == created and child.status() != psutil.STATUS_ZOMBIE:
                    live.append(row)
            except psutil.NoSuchProcess:
                pass
        return live
    end = time.monotonic() + 15
    live = live_processes()
    while live and time.monotonic() < end:
        time.sleep(.025)
        live = live_processes()
    ports = set()
    ray_directories = set()
    logs = output / 'gateway-logs'
    logs.mkdir()
    for source in temporary.rglob('gateway.log*'):
        if not source.is_file():
            continue
        destination = logs / source.parent.name
        destination.mkdir(exist_ok=True)
        shutil.copy2(source, destination / source.name)
        content = source.read_text(errors='replace')
        ports.update(int(value) for value in re.findall(r'^(?:RAY_GW_PORT|PARROT_JVM_PORT|PARROT_ERL_PORT)=(\d+)\s*$', content, re.M))
        ray_directories.update(re.findall(r'^PARROT_RAY_DIRECTORY=(.+)$', content, re.M))
    listeners.update((int(socket.AF_INET), '127.0.0.1', port) for port in ports)
    normalized_listeners = {bind_endpoint(endpoint) for endpoint in listeners}
    port_errors, port_probe_history = probe_retired_listeners(normalized_listeners)
    for identity, endpoints in listener_samples.items():
        observed[identity]['listeners'] = sorted(endpoints)
    ray_listener_evidence = [row for row in observed.values() if row.get('listeners')
                            and any(marker in ' '.join(row['command']) for marker in ['raylet', 'gcs_server'])]
    commands = [' '.join(row['command']) for row in observed.values()]
    coverage = {name: any(marker in command for command in commands)
                for name, marker in [('JVM', 'ParrotGatewayMain'), ('BEAM', 'beam.smp'), ('Ray', 'raylet')]}
    result = dict(command=command, exit_code=code, timed_out=timed_out,
                  temporary=str(temporary), observed=list(observed.values()),
                  live_after_exit=live, ports=sorted(ports), port_errors=port_errors,
                  port_probe_history=port_probe_history,
                  ray_directories=sorted(ray_directories), runtime_coverage=coverage,
                  sampling_errors=sampling_errors, listeners=sorted(listeners),
                  normalized_listeners=sorted(normalized_listeners),
                  monitor_error=monitor_error, ray_listener_processes=len(ray_listener_evidence))
    (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    require(not timed_out and not monitor_error and code == 0, 'actual business scenarios failed; see ' + str(output))
    require(all(coverage.values()) and len(ports) >= 3 and len(ray_directories) >= 3,
            'missing actual runtime/port/private-cluster evidence')
    require(len({row['group'] for row in ray_listener_evidence}) >= len(ray_directories),
            'missing actual listener evidence for a private Ray cluster')
    require(not live and not port_errors,
            'process retirement verification failed; see ' + str(output))
    print('RUST_GATEWAY_OWNERSHIP_PASS: actual JVM/BEAM/Ray business, cancellation, identities and listener retirement')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--cargo', default='cargo')
    args = parser.parse_args()
    destination = args.output or Path(tempfile.mkdtemp(prefix='parrot-host-evidence-')) / 'result'
    print('Gateway ownership evidence: ' + str(destination), flush=True)
    run(destination, args.cargo)
