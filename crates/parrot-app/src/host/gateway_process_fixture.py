"""Real process fixture; a listening descendant deliberately ignores TERM."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time

mode, record = sys.argv[1:3]
if mode == 'dead-leader':
    import ctypes
    import threading
    def worker():
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0))
        listener.listen()
        value = dict(pid=os.getpid(), port=listener.getsockname()[1], group=os.getpgrp())
        Path(record + '.tmp').write_text(json.dumps(value))
        os.replace(record + '.tmp', record)
        connection, _ = listener.accept()
        connection.close()
        listener.close()
    threading.Thread(target=worker).start()
    while not Path(record).exists():
        time.sleep(.01)
    finish = ctypes.CDLL(None).pthread_exit
    finish.argtypes = [ctypes.c_void_p]
    finish.restype = None
    finish(None)
    raise RuntimeError('pthread_exit unexpectedly returned')
if mode == 'child':
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    listener = socket.socket()
    listener.bind(('127.0.0.1', 0))
    listener.listen()
    value = dict(pid=os.getpid(), parent=os.getppid(), group=os.getpgrp(),
                 port=listener.getsockname()[1])
    Path(record + '.tmp').write_text(json.dumps(value))
    os.replace(record + '.tmp', record)
    connection, _ = listener.accept()  # Rescue path if an assertion fails.
    connection.close()
    listener.close()
else:
    try:
        os.setpgrp() if mode.startswith('inline') else None
    except PermissionError:
        pass
    subprocess.Popen([sys.executable, __file__, 'child', record])
    while not Path(record).exists():
        time.sleep(.01)
    if mode == 'inline-crash':
        os._exit(17)
    if mode.startswith('exit-'):
        raise SystemExit(int(mode[5:]))
    while True:
        time.sleep(1)
