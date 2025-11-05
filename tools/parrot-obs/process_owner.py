"""Owned process groups shared by local application launchers."""
from __future__ import annotations
import contextlib
import json
import importlib.util
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
WORKER = Path(__file__).with_name('gateway_worker.py')
_spec = importlib.util.spec_from_file_location('_parrot_process_session', Path(__file__).with_name('process_session.py'))
session = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(session)


class OwnedChild:
    """A fresh, unreaped guardian pins the only PGID this object may signal."""
    def __init__(self, name, command, log, *, cwd=ROOT, environment=None, marker=None,
                 wait_for_output=True, python_entrypoint=False):
        if signal.getsignal(signal.SIGCHLD) == signal.SIG_IGN:
            raise RuntimeError('owned guardian cannot use SIGCHLD=SIG_IGN')
        self.retirement_error = None
        self.name, self.log = name, Path(log)
        self.command = [str(arg) for arg in command]
        self.port = self.code = self.failure = None
        self.retired = False
        self.buffer = bytearray()
        self.process = None
        self.guardian_exit = None
        self.channel, child = socket.socketpair()
        try:
            self.process = subprocess.Popen(
                [sys.executable, str(WORKER), str(child.fileno())],
                pass_fds=(child.fileno(),), start_new_session=True,
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, close_fds=True,
            )
            child.close()
            self.channel.settimeout(3)
            self.channel.sendall(json.dumps(dict(
                command=self.command, cwd=str(cwd), env=environment or {},
                log=str(self.log), marker=marker, port=0, wait_for_output=wait_for_output,
                python_entrypoint=python_entrypoint,
            )).encode() + b'\n')
            self.channel.setblocking(False)
        except BaseException:
            child.close()
            self.begin_stop()
            self.finish_stop()
            raise

    def poll(self):
        if self.retired:
            return
        while select.select([self.channel], [], [], 0)[0]:
            try:
                data = self.channel.recv(4096)
            except OSError as error:
                self.failure = f'guardian control: {error}'
                return
            if not data:
                self.failure = 'guardian control closed unexpectedly'
                return
            self.buffer.extend(data)
            if len(self.buffer) > 8192:
                self.failure = 'guardian response exceeds limit'
                return
            while b'\n' in self.buffer:
                raw, _, rest = self.buffer.partition(b'\n')
                self.buffer = bytearray(rest)
                event = json.loads(raw)
                if event.get('event') == 'ready':
                    self.port = event['port']
                elif event.get('event') == 'exited':
                    self.code = event['code']
                else:
                    self.failure = str(event)

    def begin_stop(self):
        if not self.retired:
            with contextlib.suppress(OSError):
                self.channel.sendall(b'stop\n')

    def request_termination(self):
        """Signal the owned direct child while keeping its guardian observable.

        Unlike group retirement, this allows the owner to await the child's
        acknowledgement before stopping its dependencies. Not for inline jobs.
        """
        if not self.retired:
            self.channel.sendall(b'terminate\n')

    def finish_stop(self):
        if self.retirement_error is not None:
            raise RuntimeError(self.retirement_error)
        if self.retired:
            return
        errors = []
        try:
            if self.process is not None:
                deadline = session.retirement_deadline()
                try:
                    session.final_signal(self.process)
                except Exception as error:
                    errors.append(f'guardian signal: {error}')
                try:
                    self.guardian_exit = session.wait_retired(self.process, deadline)
                except Exception as error:
                    errors.append(f'guardian session retirement: {error}')
            if errors:
                self.retirement_error = '; '.join(errors)
                raise RuntimeError(self.retirement_error)
            self.retired = True
        finally:
            self.channel.close()

    def state(self):
        return dict(name=self.name, command=self.command, port=self.port,
                    exit_code=self.code, failure=self.failure, retired=self.retired,
                    guardian_exit=self.guardian_exit, retirement_error=self.retirement_error,
                    guardian_pid=self.process.pid if self.process is not None else None)

    def text(self):
        # The guardian rotates each log at 2 MiB. No unbounded pipe buffering.
        return self.log.read_text(errors='replace') if self.log.exists() else ''
