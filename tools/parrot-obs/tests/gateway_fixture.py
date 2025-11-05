#!/usr/bin/env python3
import argparse
import os
import signal
import socket
import subprocess
import sys
import time

parser=argparse.ArgumentParser()
parser.add_argument('--marker',default='READY=')
parser.add_argument('--port',type=int,default=0)
parser.add_argument('--mode',default='ready')
parser.add_argument('--starts')
parser.add_argument('--tag',default='')
args=parser.parse_args()
if args.mode in ('inline-child', 'inline-crash'):
    try:
        os.setpgrp()
    except PermissionError:
        if os.getpgrp() != os.getpid():
            raise
    # Model a native runtime that exits immediately on TERM and cannot run the
    # guardian's Python finally. Its watchdog must retain the manager lease.
    signal.signal(signal.SIGTERM, lambda *_: os._exit(17))
if args.starts:
    with open(args.starts,'a') as f: f.write(str(os.getpid())+'\n')
if args.mode=='exit': raise SystemExit(7)
listener=socket.socket();listener.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
listener.bind(('127.0.0.1',args.port));listener.listen(8)
if args.mode in ('child', 'inline-child', 'inline-crash'):
    # A child that ignores graceful termination and holds the listening FD.
    received,ready=os.pipe()
    child=subprocess.Popen([sys.executable,'-c','import os,signal,time,sys; signal.signal(signal.SIGTERM,signal.SIG_IGN); os.write(int(sys.argv[1]),b"ready"); time.sleep(120)',str(ready)],pass_fds=(listener.fileno(),ready))
    os.close(ready)
    assert os.read(received,5)==b'ready'
    os.close(received)
if args.mode=='bad': print(args.marker+'not-a-port',flush=True)
elif args.mode!='never': print(args.marker+str(listener.getsockname()[1]),flush=True)
if args.mode=='crash':
    time.sleep(0.4)
    raise SystemExit(8)
if args.mode == 'inline-crash':
    time.sleep(0.4)
    os._exit(18)
while True: time.sleep(0.1)
