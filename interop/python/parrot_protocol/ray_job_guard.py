"""Observe owner loss outside the Ray driver's interpreter and native calls.

This process never imports Ray. It owns one Popen child, relays bounded control
frames without blocking, and reaps that child after EOF or a protocol failure.
An independent group member watches this guard's private lifetime socket. If
the guard itself dies, that member kills its own group, including a driver
stalled in native code. Worker leases still authorize snapshot removal.
"""
import os
import select
import signal
import socket
import struct
import subprocess
import sys

from .ray_job import MAX_CONTROL_BYTES, encode, receive
from .ray_snapshot_lease import hold_snapshot_lease

GRACE_SECONDS = 1
KILL_WAIT_SECONDS = 3


def watch_guard(descriptor, control):
    """Hold source and group identity until verified retirement or guard EOF.

    Never signal a saved PID/PGID. This process stays in the fresh group made
    by DeploymentJob, and the driver never inherits our private control socket.
    """
    connection = socket.socket(fileno=descriptor)
    os.set_inheritable(descriptor, False)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    hold_snapshot_lease(control)
    disarmed = False
    try:
        connection.sendall(b'R')
        disarmed = connection.recv(1) == b'D'
    finally:
        connection.close()
        if not disarmed:
            os.killpg(os.getpgrp(), signal.SIGKILL)


def arm_watchdog(control):
    connection, child = socket.socketpair()
    process = None
    try:
        process = subprocess.Popen(
            [sys.executable, '-m', 'parrot_protocol.ray_job_guard',
             '--watch', str(child.fileno()), control],
            pass_fds=(child.fileno(),), stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        child.close()
        connection.settimeout(KILL_WAIT_SECONDS)
        if connection.recv(1) != b'R':
            raise RuntimeError('deployment guard watchdog failed to arm')
        return process, connection
    except BaseException:
        connection.close()
        if process is not None:
            process.kill()
            process.wait(timeout=KILL_WAIT_SECONDS)
        raise
    finally:
        child.close()


def disarm_watchdog(process, connection):
    """Only called after the driver has been reaped (or was never spawned)."""
    try:
        connection.sendall(b'D')
        status = process.wait(timeout=KILL_WAIT_SECONDS)
        if status != 0:
            raise RuntimeError('deployment guard watchdog exited: ' + str(status))
    finally:
        connection.close()


def retire(process, connection):
    """Request driver shutdown, then kill only our still-owned child if stuck."""
    connection.close()
    try:
        process.wait(timeout=GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        # A kernel-stuck process may outlast this budget. Its snapshot lease
        # remains authoritative; a timeout must never authorize deleting source.
        process.wait(timeout=KILL_WAIT_SECONDS)


def supervise(descriptor):
    owner = socket.socket(fileno=descriptor)
    child_connection = child_end = process = None
    watchdog = watchdog_connection = None
    try:
        config = receive(owner)
        if config is None:
            return  # Owner disappeared before requesting a process.
        if not isinstance(config, dict) or set(config) != {'address', 'namespace', 'node', 'root', 'runtime_env'}:
            raise ValueError('invalid deployment job configuration')
        if os.getpid() != os.getpgrp() or os.getsid(0) != os.getpid():
            raise RuntimeError('deployment guard requires its own session')
        # Protect the bootstrap/import window before the driver can take its
        # own lease. This process releases its lease only on process exit.
        hold_snapshot_lease(config['runtime_env']['env_vars']['PARROT_SNAPSHOT_CONTROL'])
        pending = encode(config)
        watchdog, watchdog_connection = arm_watchdog(config['runtime_env']['env_vars']['PARROT_SNAPSHOT_CONTROL'])
        child_connection, child_end = socket.socketpair()
        process = subprocess.Popen(
            [sys.executable, '-m', 'parrot_protocol.ray_job', str(child_end.fileno()), '--driver'],
            pass_fds=(child_end.fileno(),), stdin=subprocess.DEVNULL,
        )
        child_end.close(); child_end = None
        owner.setblocking(False)
        child_connection.setblocking(False)
        received = bytearray()
        response = b''
        receipt_complete = False
        expected = None
        while True:
            if process.poll() is not None:
                raise RuntimeError('deployment job driver exited unexpectedly: ' + str(process.returncode))
            writes = ([child_connection] if pending else []) + ([owner] if response else [])
            readable, writable, _ = select.select([owner, child_connection, watchdog_connection], writes, [], .1)
            # Always handle owner loss before making further startup progress.
            if owner in readable:
                try:
                    command = owner.recv(1)
                except ConnectionResetError:
                    return  # Linux may reset a peer closed with unread data.
                if command:
                    raise ValueError('unexpected deployment job command')
                return
            if watchdog_connection in readable:
                raise RuntimeError('deployment guard watchdog exited before driver retirement')
            if child_connection in readable:
                chunk = child_connection.recv(65536)
                if not chunk:
                    raise EOFError('deployment job driver closed its control socket')
                if receipt_complete:
                    raise ValueError('duplicate deployment job startup receipt')
                received.extend(chunk)
                if expected is None and len(received) >= 4:
                    length, = struct.unpack('!I', received[:4])
                    if not 0 < length <= MAX_CONTROL_BYTES:
                        raise ValueError('invalid deployment job control length')
                    expected = 4 + length
                if expected is not None:
                    if len(received) > expected:
                        raise ValueError('extra deployment job startup data')
                    if len(received) == expected:
                        response = bytes(received)
                        received.clear()
                        receipt_complete = True
            if child_connection in writable and pending:
                pending = pending[child_connection.send(pending):]
            if owner in writable and response:
                try:
                    response = response[owner.send(response):]
                except (BrokenPipeError, ConnectionResetError):
                    return  # The caller disappeared while receiving startup.
    finally:
        owner.close()
        if child_end is not None:
            child_end.close()
        retired = False
        try:
            if process is not None:
                retire(process, child_connection)
            elif child_connection is not None:
                child_connection.close()
            retired = True
        finally:
            if watchdog is not None:
                if retired:
                    disarm_watchdog(watchdog, watchdog_connection)
                else:
                    # Failed reap is never permission to disarm protection.
                    watchdog_connection.close()


if __name__ == '__main__':
    if len(sys.argv) != 4 or sys.argv[1] != '--watch':
        raise SystemExit('private deployment guard watchdog')
    watch_guard(int(sys.argv[2]), sys.argv[3])
