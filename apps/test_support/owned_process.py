"""Observe native child exits without releasing the identity used for cleanup.

Unlike Popen, wait()/poll() do NOT reap. close() sends the last group signal,
then reaps the session leader. This lets crash-recovery tests inspect unassisted
descendant behavior after os._exit, without a watchdog changing the outcome.
Single-owner POSIX helper: do not use an external child reaper or SIGCHLD=IGN.
It owns the new session's process group, not descendants that create new groups.
"""
import ctypes
import errno
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def process_exited(pid):
    """Read-only fixture observation, including an unreaped zombie.

    This is not a process identity/ownership handle and must not authorize a
    later signal to that PID. kill(pid, 0) alone can treat zombies as live.
    """
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    if sys.platform == 'linux':
        if not Path('/proc/self/stat').is_file():
            raise RuntimeError('Linux exit observation requires procfs')
        try:
            fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        except (FileNotFoundError, ProcessLookupError):
            # A process can disappear after kill(0) or opening its procfs
            # file. Linux may then report ESRCH from read(), not ENOENT.
            # Permission, I/O and other errors must still propagate: they do
            # not prove that the process exited.
            return True
        return fields[0] in ('Z', 'X')
    if sys.platform == 'darwin':
        # Darwin's public proc_bsdshortinfo layout (sys/proc_info.h). Some
        # exiting zombies accept kill(0) while proc_pidinfo returns ESRCH.
        class ShortInfo(ctypes.Structure):
            _fields_ = [(name, ctypes.c_uint32) for name in ('pid','ppid','pgid','status')] + [
                ('comm', ctypes.c_char * 16)] + [
                (name, ctypes.c_uint32) for name in ('flags','uid','gid','ruid','rgid','svuid','svgid','reserved')]
        query = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True).proc_pidinfo
        query.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        query.restype = ctypes.c_int
        info = ShortInfo()
        ctypes.set_errno(0)
        count = query(pid, 13, 0, ctypes.byref(info), ctypes.sizeof(info))
        if not count and ctypes.get_errno() == errno.ESRCH:
            return True
        if count != ctypes.sizeof(info) or info.pid != pid:
            raise RuntimeError('cannot read Darwin process exit state')
        return info.status == 5  # SZOMB
    return False


def _exit_observer():
    if hasattr(os, "waitid"):
        def observe(pid):
            info = os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT | os.WNOHANG)
            if info is None or not info.si_pid:
                return None
            return _exit_code(info.si_code, info.si_status)
        return observe
    if sys.platform != "darwin":
        raise NotImplementedError("owned process requires non-reaping waitid")

    # CPython on macOS does not expose waitid. Use Darwin's public siginfo_t
    # ABI (sys/signal.h), not Linux's different layout or a reaping fallback.
    class SigInfo(ctypes.Structure):
        _fields_ = [
            ("signo", ctypes.c_int), ("errno", ctypes.c_int),
            ("code", ctypes.c_int), ("pid", ctypes.c_int),
            ("uid", ctypes.c_uint), ("status", ctypes.c_int),
            ("addr", ctypes.c_void_p), ("value", ctypes.c_void_p),
            ("band", ctypes.c_long), ("pad", ctypes.c_ulong * 7),
        ]

    waitid = ctypes.CDLL(None, use_errno=True).waitid
    waitid.argtypes = [ctypes.c_uint, ctypes.c_uint,
                       ctypes.POINTER(SigInfo), ctypes.c_int]
    waitid.restype = ctypes.c_int

    def observe(pid):
        info = SigInfo()
        if waitid(os.P_PID, pid, ctypes.byref(info),
                  os.WEXITED | os.WNOWAIT | os.WNOHANG) != 0:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error))
        if not info.pid:
            return None
        return _exit_code(info.code, info.status)
    return observe


def _exit_code(code, status):
    if code == os.CLD_EXITED:
        return status
    if code in (os.CLD_KILLED, os.CLD_DUMPED):
        return -status
    raise RuntimeError(f"unexpected child exit event: {code}, {status}")


def _darwin_only_zombie_leader(pid):
    """Darwin returns EPERM for a group containing only its zombie leader.

    The caller has independently observed that leader's exit WITHOUT reaping.
    libproc must confirm there are no other members before treating EPERM as
    an empty-group result. A live/unknown member or an API error fails closed.
    Two slots distinguish 'only our leader' from a larger, truncated group.
    proc_listpgrppids returns a PID count, unlike proc_listpids' byte count.
    """
    libproc = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    members = libproc.proc_listpgrppids
    members.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
    members.restype = ctypes.c_int
    pids = (ctypes.c_int * 2)()
    ctypes.set_errno(0)
    count = members(pid, pids, ctypes.sizeof(pids))
    if ctypes.get_errno():
        return False
    return count == 0 or (count == 1 and pids[0] == pid)


class OwnedProcess:
    def __init__(self, command, *, env=None, stdin=None, stdout=None, stderr=None, cwd=None):
        if signal.getsignal(signal.SIGCHLD) == signal.SIG_IGN:
            raise RuntimeError("owned process cannot use SIGCHLD=SIG_IGN")
        self._observe = _exit_observer()
        self.returncode = None
        self.retired = False
        self._process = subprocess.Popen(command, env=env, stdin=stdin, stdout=stdout,
                                         stderr=stderr, cwd=cwd,
                                         start_new_session=True)
        self.pid = self._process.pid
        self.args = self._process.args

    def poll(self):
        if not self.retired:
            self.returncode = self._observe(self.pid)
        return self.returncode

    def wait(self, timeout=None):
        deadline = None if timeout is None else time.monotonic() + timeout
        while True:
            code = self.poll()
            if code is not None:
                return code
            remaining = None if deadline is None else deadline - time.monotonic()
            if remaining is not None and remaining <= 0:
                raise subprocess.TimeoutExpired(self.args, timeout)
            time.sleep(.01 if remaining is None else min(.01, remaining))

    def send_signal(self, sig):
        if self.poll() is None:
            # Never call Popen.send_signal: its implicit poll would reap.
            try:
                os.kill(self.pid, sig)
            except ProcessLookupError:
                # An exit raced the signal. The unreaped PID is still ours.
                pass

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self, timeout=10):
        if self.retired:
            return
        # Fail closed if another reaper violated ownership: ECHILD must not
        # lead to a signal using a potentially recycled identifier.
        observed_exit = self.poll()
        try:
            os.killpg(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except PermissionError:
            if not (sys.platform == 'darwin' and observed_exit is not None
                    and _darwin_only_zombie_leader(self.pid)):
                raise
        # This is the only reaping operation, after the final group signal.
        self.returncode = self._process.wait(timeout=timeout)
        self.retired = True
