"""Private Linux node workers die with their raylet, including idle workers.

Ray workers create independent process groups. Bind each worker to its actual
parent before importing Ray or executing runtime environment commands; Linux
preserves this binding through exec. This is lifetime ownership, not a sandbox.
"""
import ctypes
import os
import runpy
import signal
import sys


def bind_parent():
    if sys.platform != 'linux':
        raise RuntimeError('owned Ray worker bootstrap requires Linux')
    parent = os.getppid()
    if parent == 1:
        raise RuntimeError('owned Ray worker lost its parent before bootstrap')
    library = ctypes.CDLL(None, use_errno=True)
    library.prctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong,
                             ctypes.c_ulong, ctypes.c_ulong]
    library.prctl.restype = ctypes.c_int
    # PR_SET_PDEATHSIG is cleared by fork but survives the runtime's exec chain.
    if library.prctl(1, int(signal.SIGKILL), 0, 0, 0) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    if os.getppid() != parent:
        raise RuntimeError('owned Ray worker lost its parent while binding lifetime')


if __name__ == '__main__':
    bind_parent()
    runpy.run_module('ray._private.workers.setup_worker', run_name='__main__')
