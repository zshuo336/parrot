"""Observe owned guardian sessions before reaping their identity anchor.

Only an unreaped Popen child authorizes a signal to its original process group.
Enumerated process IDs are observation data and are never signal targets.
Session escape via setsid is outside this local trusted-process contract.
"""
import ctypes
import errno
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

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


_OBSERVE = _exit_observer()


def still_owned(process):
    """WNOWAIT rejects external reaping before any use of the pinned group."""
    while True:
        try:
            return _OBSERVE(process.pid)
        except InterruptedError:
            continue
        except ChildProcessError as error:
            raise RuntimeError('guardian identity no longer owned') from error


def final_signal(process):
    observed = still_owned(process)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        if not (sys.platform == 'darwin' and observed is not None
                and _darwin_only_zombie_leader(process.pid)):
            raise


def _deadline(deadline):
    if time.monotonic() >= deadline:
        raise TimeoutError('guardian session retirement observation deadline')


def _gone(error):
    return error.errno in (errno.ENOENT, errno.ESRCH)


def _stat(path):
    try:
        with path.open('rb') as stream:
            raw = stream.read(4097)
    except OSError as error:
        if _gone(error):
            return None
        raise
    if len(raw) > 4096 or b')' not in raw:
        raise RuntimeError('invalid process stat during retirement')
    # comm is arbitrary bytes and may contain spaces and parentheses.
    fields = raw.rsplit(b')', 1)[1].split()
    if len(fields) < 4 or len(fields[0]) != 1:
        raise RuntimeError('invalid process stat fields during retirement')
    return fields[0], int(fields[3])


def _linux_member_live(path, session, deadline):
    _deadline(deadline)
    value = _stat(path / 'stat')
    if value is None or value[1] != session:
        return False
    if value[0] not in (b'Z', b'X', b'x'):
        return True
    # A zombie main thread may still own live threads and listening sockets.
    try:
        for task in (path / 'task').iterdir():
            _deadline(deadline)
            value = _stat(task / 'stat')
            if value is not None and value[1] == session and value[0] not in (b'Z', b'X', b'x'):
                return True
    except OSError as error:
        if not _gone(error):
            raise
    return False


def _darwin_session_live(session, deadline):
    library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    listing = library.proc_listpids
    listing.argtypes = [ctypes.c_uint, ctypes.c_uint, ctypes.c_void_p, ctypes.c_int]
    listing.restype = ctypes.c_int
    size = 256
    while True:
        _deadline(deadline)
        pids = (ctypes.c_int * size)()
        ctypes.set_errno(0)
        count = listing(1, 0, pids, ctypes.sizeof(pids))  # PROC_ALL_PIDS; returns bytes
        error = ctypes.get_errno()
        if count < 0 or (count == 0 and error not in (0, errno.ESRCH)):
            raise OSError(error, 'cannot enumerate guardian session')
        if count < ctypes.sizeof(pids):
            break
        if size >= 65536:
            raise RuntimeError('session process enumeration exceeds limit')
        size *= 2
    if count % ctypes.sizeof(ctypes.c_int):
        raise RuntimeError('invalid process enumeration size')
    class ShortInfo(ctypes.Structure):
        _fields_ = [(name, ctypes.c_uint32) for name in ('pid','ppid','pgid','status')] + [
            ('comm', ctypes.c_char * 16)] + [
            (name, ctypes.c_uint32) for name in ('flags','uid','gid','ruid','rgid','svuid','svgid','reserved')]
    query = library.proc_pidinfo
    query.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    query.restype = ctypes.c_int
    for pid in pids[:count // ctypes.sizeof(ctypes.c_int)]:
        _deadline(deadline)
        if pid <= 0:
            continue
        try:
            if os.getsid(pid) != session:
                continue
        except ProcessLookupError:
            continue
        info = ShortInfo();ctypes.set_errno(0)
        got = query(pid, 13, 0, ctypes.byref(info), ctypes.sizeof(info))
        if not got and ctypes.get_errno() == errno.ESRCH:
            continue
        if got != ctypes.sizeof(info) or info.pid != pid:
            raise RuntimeError('cannot read guardian session process')
        try:
            if os.getsid(pid) == session and info.status != 5:  # SZOMB
                return True
        except ProcessLookupError:
            continue
    return False


def session_live(session, deadline):
    if sys.platform == 'linux':
        for process in Path('/proc').iterdir():
            _deadline(deadline)
            if process.name.isdecimal() and _linux_member_live(process, session, deadline):
                return True
        return False
    if sys.platform == 'darwin':
        return _darwin_session_live(session, deadline)
    raise NotImplementedError('session retirement requires Linux or macOS')


def retirement_deadline(seconds=10):
    if not math.isfinite(seconds) or seconds <= 0:
        raise ValueError('retirement budget must be finite and positive')
    return time.monotonic() + seconds


def wait_retired(process, deadline):
    """Keep the guardian unreaped until two complete empty session scans."""
    empty = False
    while True:
        still_owned(process)
        live = session_live(process.pid, deadline)
        if not live and empty:
            return process.wait(timeout=max(0, deadline - time.monotonic()))
        empty = not live
        time.sleep(.01)
