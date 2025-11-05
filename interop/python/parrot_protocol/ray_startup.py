"""Keep Ray local service ports outside automatic client-port allocation.

Ray 2.51.2's Node._get_unused_port can exhaust its random retries near 65535
and return the initial ephemeral port even when it is in allocated_ports.
Even a valid selection inside the OS ephemeral range can be reused by an
outbound connection before an agent binds. Select automatic service ports
outside the actual OS range, excluding Ray's full reserved set. Explicit
user ports remain unchanged; another explicit bind can still compete.
Assign ObjectManager/NodeManager explicit distinct ports before raylet.
Wrap these private compatibility seams only during this gateway's ray.init;
never retry a partially started cluster. No Ray installation is modified.
The gateway owns initialization in its process; unrelated concurrent ray.init
calls in that same process are not supported.
"""
from contextlib import contextmanager
from pathlib import Path
import errno
import secrets
import signal
import socket
import subprocess
import sys
import threading
import warnings

_INITIALIZATION = threading.Lock()


def ephemeral_port_range():
    """Read the active TCP client range once per gateway initialization.

    Refuse an unknown range rather than assume defaults on a configured host.
    Local Ray gateway startup currently supports Linux and macOS.
    """
    try:
        if sys.platform == 'linux':
            with Path('/proc/sys/net/ipv4/ip_local_port_range').open() as source:
                raw = source.read(129)
        elif sys.platform == 'darwin':
            raw = subprocess.run(
                ['/usr/sbin/sysctl', '-n', 'net.inet.ip.portrange.first',
                 'net.inet.ip.portrange.last'],
                capture_output=True, text=True, timeout=5, check=True,
            ).stdout
        else:
            raise RuntimeError('automatic Ray service port selection requires Linux or macOS')
        values = raw.split()
        if len(raw) > 128 or len(values) != 2 or any(not value.isascii() or not value.isdecimal() for value in values):
            raise ValueError('invalid TCP ephemeral port range')
        first, last = map(int, values)
        if not 1 <= first <= last <= 65535:
            raise ValueError('invalid TCP ephemeral port range')
        return first, last
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        raise RuntimeError('cannot determine OS TCP ephemeral port range') from error


def guarded_allocator(original):
    first, last = ephemeral_port_range()

    def allocate(node, allocated_ports=None):
        excluded = set(allocated_ports or ())
        candidate = original(node, excluded)
        if (type(candidate) is int and 1024 <= candidate <= 65535
                and not first <= candidate <= last and candidate not in excluded):
            return candidate
        candidates = [port for port in range(1024, 65536)
                      if not first <= port <= last and port not in excluded]
        # Sample without replacement, with a fixed cap even on a busy host.
        # Do not use bind(0): it selects from the very range we must exclude.
        for _ in range(min(128, len(candidates))):
            index = secrets.randbelow(len(candidates))
            candidate = candidates[index]
            candidates[index] = candidates[-1]
            candidates.pop()
            listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            try:
                listener.bind(('', candidate))
                return candidate
            except OSError as error:
                if error.errno != errno.EADDRINUSE:
                    raise
            finally:
                listener.close()
        raise RuntimeError('Ray startup could not select a distinct non-ephemeral internal port')
    return allocate


def guarded_raylet_start(original):
    def start(node, *args, **kwargs):
        params = node._ray_params
        # Cached agent ports are selected but not yet listening. The kernel's
        # automatic port-0 allocation cannot see those logical reservations.
        # Include explicit worker/service ports, and preserve all explicit input.
        params.update_pre_selected_port()
        for name in ('object_manager_port', 'node_manager_port'):
            if getattr(params, name) in (None, 0):
                selected = node._get_unused_port(params.reserved_ports)
                setattr(params, name, selected)
                params.update_pre_selected_port()
        return original(node, *args, **kwargs)
    return start


@contextmanager
def guarded_ports(node_type):
    with _INITIALIZATION:
        original = node_type._get_unused_port
        original_start = node_type.start_raylet
        allocator = guarded_allocator(original)
        node_type.start_raylet = guarded_raylet_start(original_start)
        node_type._get_unused_port = allocator
        try:
            yield
        finally:
            node_type._get_unused_port = original
            node_type.start_raylet = original_start


def initialize(ray_module, kwargs):
    from ray._private.node import Node
    from ray._private import ray_constants
    from .ray_artifacts import collect_orphaned_snapshots
    recovery = collect_orphaned_snapshots()
    if recovery['errors']:
        warnings.warn('snapshot recovery retained directories with filesystem errors; '
                      'inspect collect_orphaned_snapshots() before retrying', RuntimeWarning, stacklevel=2)
    # Ray installs its own SIGTERM -> SystemExit(signum) policy during init.
    # Restore an embedding application's explicit callback/ignore disposition,
    # including on initialization failure. Leave Ray's default policy alone
    # when the caller did not install one. Python only permits handler writes
    # from the main thread.
    previous = signal.getsignal(signal.SIGTERM) if threading.current_thread() is threading.main_thread() else None
    owns_signal = previous == signal.SIG_IGN or callable(previous)
    try:
        with guarded_ports(Node):
            # Ray snapshots this option at import, then consumes it when
            # creating the driver's CoreWorker. Its native logger otherwise
            # resets SIGTERM to SIG_DFL during shutdown, overriding even a
            # restored Python callback. Scope the private option to this init,
            # under the same lock as our other Ray compatibility seams. Do not
            # set the environment: subprocess workers retain native diagnostics.
            has_native_option = hasattr(ray_constants, 'RAY_DISABLE_FAILURE_SIGNAL_HANDLER')
            native_disabled = getattr(ray_constants, 'RAY_DISABLE_FAILURE_SIGNAL_HANDLER', None)
            try:
                if owns_signal and has_native_option:
                    ray_constants.RAY_DISABLE_FAILURE_SIGNAL_HANDLER = True
                elif owns_signal:
                    warnings.warn(
                        'This Ray version cannot preserve SIGTERM throughout native shutdown; '
                        'use the locked Ray 2.59 runtime for repeated-TERM cleanup guarantees',
                        RuntimeWarning, stacklevel=2)
                return ray_module.init(**kwargs)
            finally:
                if has_native_option:
                    ray_constants.RAY_DISABLE_FAILURE_SIGNAL_HANDLER = native_disabled
    finally:
        if owns_signal:
            signal.signal(signal.SIGTERM, previous)


@contextmanager
def graceful_sigterm():
    """CLI termination policy; initialize() also guards native Ray ownership.

    Older Ray builds without the native-driver option cannot honor this policy
    throughout shutdown; initialize() reports that compatibility limitation.
    """
    if threading.current_thread() is not threading.main_thread():
        yield
        return
    previous = signal.getsignal(signal.SIGTERM)
    stopping = False

    def terminate(_signum, _frame):
        nonlocal stopping
        if not stopping:
            stopping = True
            raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    try:
        yield
    finally:
        signal.signal(signal.SIGTERM, previous)
