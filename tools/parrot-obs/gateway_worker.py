#!/usr/bin/env python3
"""Private group guardian. Parent owns the socket, not a recorded PID.

The guardian is the session/group leader. It remains unreaped by its parent
until the parent's last group signal, preventing process-group ID reuse.
EOF from the manager independently retires the group after a bounded grace.
"""
from __future__ import annotations
import json
import logging
from logging.handlers import RotatingFileHandler
import os
import runpy
import select
import signal
import socket
import subprocess
import sys
import threading
import time

GRACE = 2.0


def watch_inline_group(lifetime, ready, control):
    """Stay in the inline runtime's group even if it replaces signal handlers.

    This live member pins its own group identity. It never uses a stored PID.
    The parent alone holds the non-inheritable lifetime writer.
    """
    signal.signal(signal.SIGTERM, lambda *_: None)
    signal.signal(signal.SIGINT, lambda *_: None)
    os.write(ready, b'R')
    os.close(ready)
    while os.read(lifetime, 1):
        pass
    os.close(lifetime)
    # Our duplicate socket keeps the endpoint open even after the inline
    # runtime dies without unwinding. Notify the owner explicitly; otherwise
    # it could keep advertising the dead gateway as running until shutdown.
    try:
        control.sendall(b'{"event":"failed","detail":"inline runtime lifetime ended"}\n')
    except OSError:
        pass
    os.killpg(os.getpgrp(), signal.SIGTERM)
    time.sleep(GRACE)
    # Keep a live group member until the launcher's final kill. If the
    # launcher died, EOF instead gives us responsibility for that last kill.
    while True:
        try:
            readable, _, _ = select.select([control], [], [], 0.25)
            if readable and not control.recv(1024):
                break
        except OSError:
            break
    os.killpg(os.getpgrp(), signal.SIGKILL)


def retire_group(control):
    # Our new-session identity was assigned by Popen, never loaded from disk.
    assert os.getpid() == os.getpgrp()
    try:
        os.killpg(os.getpid(), signal.SIGTERM)
    except ProcessLookupError:
        pass
    time.sleep(GRACE)
    # Stay as the live group anchor until the manager's final signal. If the
    # manager died, socket EOF retires the group without consulting any PID file.
    while True:
        try:
            readable, _, _ = select.select([control], [], [], 0.25)
            if readable and not control.recv(1024):
                break
        except OSError:
            break
    os.killpg(os.getpid(), signal.SIGKILL)


def run(control):
    signal.signal(signal.SIGTERM, lambda *_: None)
    signal.signal(signal.SIGINT, lambda *_: None)
    control.settimeout(5)
    buf = bytearray()
    while b'\n' not in buf:
        chunk = control.recv(4096)
        if not chunk:
            return
        buf.extend(chunk)
        if len(buf) > 262144:
            raise ValueError('worker configuration exceeds limit')
    cfg = json.loads(bytes(buf).split(b'\n', 1)[0])
    if 'command_os_bytes' in cfg:
        cfg['command'] = [os.fsdecode(bytes(arg)) for arg in cfg['command_os_bytes']]
    control.settimeout(1)
    send_lock = threading.Lock()

    def report(message):
        with send_lock:
            try:
                control.sendall(json.dumps(message).encode() + b'\n')
            except OSError:
                pass

    logger = logging.getLogger('gateway')
    logger.setLevel(logging.INFO)
    handler = RotatingFileHandler(cfg['log'], maxBytes=2*1024*1024, backupCount=2)
    handler.setFormatter(logging.Formatter('%(message)s'))
    logger.addHandler(handler)
    inline = cfg.get('python_entrypoint', False)
    if inline:
        # Ray on macOS calls setpgrp() during local startup. Run this trusted
        # entrypoint IN the session leader so that it cannot leave our group.
        # The control thread remains the owner even if the Python main returns.
        read_fd, write_fd = os.pipe()
        output = os.fdopen(read_fd, 'rb')
        os.dup2(write_fd, 1)
        os.dup2(write_fd, 2)
        os.close(write_fd)
        os.chdir(cfg['cwd'])
        os.environ.update(cfg.get('env', {}))
        # Updating os.environ alone does not update an already-running Python
        # interpreter's module path as exec of the command would.
        sys.path[:0] = [os.path.dirname(os.path.abspath(cfg['command'][1]))] + [
            path for path in os.environ.get('PYTHONPATH', '').split(os.pathsep) if path]
    else:
        process = subprocess.Popen(cfg['command'], cwd=cfg['cwd'],
                                   env=dict(os.environ, **cfg.get('env', {})),
                                   stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, close_fds=True)
        output = process.stdout

    def consume():
        pending = bytearray()
        dropping = False
        ready = False
        startup_bytes = 0
        startup_limit = cfg.get("startup_output_limit")
        while True:
            data = os.read(output.fileno(), 4096)
            if not data:
                if pending:
                    logger.info(pending.decode('utf8', errors='replace'))
                break
            for part in data.splitlines(keepends=True):
                # A ready line ends the startup budget. A single pipe read may
                # also contain arbitrary post-ready diagnostics; do not charge
                # those bytes to startup before inspecting that line.
                if not ready and startup_limit is not None:
                    startup_bytes += len(part)
                    if startup_bytes > startup_limit:
                        report({'event': 'failed', 'detail': 'startup output exceeds limit'})
                        return
                newline = part.endswith(b'\n')
                if not dropping:
                    pending.extend(part)
                    if len(pending) > 8192:
                        logger.info(pending[:8192].decode('utf8', errors='replace') + ' [line truncated]')
                        pending.clear()
                        dropping = True
                    elif newline:
                        line = pending.decode('utf8', errors='replace').rstrip('\r\n')
                        logger.info(line)
                        pending.clear()
                        if not ready and cfg.get('marker') and line.startswith(cfg['marker']):
                            raw = line[len(cfg['marker']):].strip()
                            try:
                                port = int(raw)
                                if not 1 <= port <= 65535 or (cfg['port'] and port != cfg['port']):
                                    raise ValueError('unexpected bound port')
                                report({'event': 'ready', 'port': port})
                                ready = True
                            except ValueError:
                                report({'event': 'failed', 'detail': 'invalid readiness port'})
                if newline:
                    dropping = False

    output_done = threading.Event()
    output_errors = []

    def consume_owned():
        try:
            consume()
        except Exception as error:
            output_errors.append(str(error))
        finally:
            output_done.set()

    threading.Thread(target=consume_owned, daemon=True).start()
    if inline:
        lifetime_read, lifetime_write = os.pipe()
        ready_read, ready_write = os.pipe()
        inherited = (lifetime_read, ready_write, control.fileno())
        if cfg.get('ownership_fd') is not None:
            # A persistent manager's lease must outlive an inline runtime that
            # replaces SIGTERM handling and exits before group retirement.
            inherited += (cfg['ownership_fd'],)
        watchdog = subprocess.Popen(
            [sys.executable, __file__, '--watch-group', str(lifetime_read), str(ready_write), str(control.fileno())],
            pass_fds=inherited, close_fds=True,
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        os.close(lifetime_read)
        os.close(ready_write)
        if not select.select([ready_read], [], [], 5)[0] or os.read(ready_read, 1) != b'R':
            raise RuntimeError('inline group watchdog did not start')
        os.close(ready_read)

        def supervise_inline():
            try:
                while True:
                    readable, _, _ = select.select([control], [], [], 0.1)
                    if readable:
                        data = control.recv(1024)
                        if not data or b'stop' in data:
                            break
            finally:
                # The watchdog survives a runtime's fatal SIGTERM handler and
                # completes the final group kill even after launcher SIGKILL.
                os.close(lifetime_write)

        threading.Thread(target=supervise_inline, daemon=True).start()
        code = 0
        try:
            sys.argv = cfg['command'][1:]
            runpy.run_path(sys.argv[0], run_name='__main__')
        except SystemExit as error:
            code = error.code if isinstance(error.code, int) else (0 if error.code is None else 1)
        except BaseException as error:
            report({'event': 'failed', 'detail': repr(error)})
            code = 1
        sys.stdout.flush()
        sys.stderr.flush()
        report({'event': 'exited', 'code': code})
        # Do not exit/reap the group anchor before the owner's final signal.
        while True:
            time.sleep(1)

    exited = False
    while True:
        readable, _, _ = select.select([control], [], [], 0.1)
        if readable:
            data = control.recv(1024)
            if not data or b'stop' in data:
                return
            if b'terminate' in data:
                # Popen checks child exit before signalling. The live guardian
                # continues reporting exit and pins the group until retirement.
                process.terminate()
        result = process.poll()
        if (result is not None and not exited and
                (not cfg.get('wait_for_output', False) or output_done.is_set())):
            # Reaping the gateway is safe: this guardian still pins the group.
            if output_errors:
                report({'event': 'failed', 'detail': 'output read failed: ' + output_errors[0]})
            else:
                report({'event': 'exited', 'code': result})
            exited = True


if __name__ == '__main__':
    if sys.argv[1] == '--watch-group':
        watch_inline_group(int(sys.argv[2]), int(sys.argv[3]), socket.socket(fileno=int(sys.argv[4])))
        raise SystemExit(0)
    channel = socket.socket(fileno=int(sys.argv[1]))
    try:
        run(channel)
    except Exception as error:
        try:
            channel.sendall(json.dumps({'event': 'failed', 'detail': str(error)}).encode() + b'\n')
        except OSError:
            pass
    finally:
        retire_group(channel)
