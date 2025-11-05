#!/usr/bin/env python3
"""Verify transport retirement while its embedding process and Ray stay alive."""
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import struct
import subprocess
import sys
import tempfile

from owned_process import OwnedProcess
from run_ray_lifecycle import Client, ROOT, wire
from run_ray_shutdown import require, wait_until


CHILD = r'''import json, sys, threading, time
from pathlib import Path
import ray
from parrot_protocol.ray_gw import ParrotDispatcher, serve
from parrot_protocol.ray_startup import graceful_sigterm
directory = Path(sys.argv[1])
dispatch = ParrotDispatcher()
dispatch.handler('probe')(lambda key, payload: (key, payload))
retained = None
with graceful_sigterm():
    try:
        serve(0, dispatch, ray_kwargs={'address':'local','num_cpus':2,
            'include_dashboard':False,'log_to_driver':False,'_temp_dir':str(directory/'ray')})
    except KeyboardInterrupt:
        # Keep all unwound frames alive. Refcount/GC cannot close leaked sockets
        # for the implementation, and process exit cannot close them either.
        retained = sys.exc_info()
        active = [thread.name for thread in threading.enumerate()
                  if thread.name.startswith(('parrot-gateway-', 'parrot-direct')) or
                  getattr(getattr(thread, '_target', None), '__name__', '') == '_serve_connection_guarded']
        (directory/'caught.tmp').write_text(json.dumps(dict(
            initialized=ray.is_initialized(), network_threads=active)))
        (directory/'caught.tmp').replace(directory/'caught.json')
        deadline = time.monotonic() + 15
        while not (directory/'continue').exists():
            if time.monotonic() >= deadline:
                raise TimeoutError('parent did not finish live-process assertions')
            time.sleep(.01)
    finally:
        ray.shutdown()
(directory/'finished.json').write_text(json.dumps(dict(initialized=ray.is_initialized())))
'''


def run():
    directory = Path(tempfile.mkdtemp(prefix='prnet-', dir='/tmp'))
    print('RAY_NETWORK_RESULTS=' + str(directory), flush=True)
    script = directory/'child.py'
    script.write_text(CHILD)
    logpath = directory/'gateway.log'
    env = dict(os.environ, PYTHONPATH=str(ROOT/'interop/python'),
               RAY_AUTH_TOKEN=secrets.token_hex(32), RAY_USAGE_STATS_ENABLED='0',
               RAY_DISABLE_FAILURE_SIGNAL_HANDLER='0')
    env.pop('RAY_AUTH_TOKEN_PATH', None)
    env.pop('RAY_ADDRESS', None)
    result = dict(passed=False, natural_exit=None)
    client = None
    with logpath.open('w') as log:
        with OwnedProcess([sys.executable, str(script), str(directory)], env=env,
                                 stdout=log, stderr=subprocess.STDOUT) as child:
            try:
                match = wait_until(lambda: re.search(r'RAY_GW_PORT=(\d+)\n', logpath.read_text()), child)
                port = int(match.group(1))
                client = Client(port)
                frame = client.exchange(wire.FT_ASK, '/user/probe', 'probe',
                                        wire.with_reply_to_prefix('/reply', struct.pack('<Q', 7)))
                require(frame.ft == wire.FT_REPLY and frame.type_key == 'probe'
                        and frame.payload == struct.pack('<Q', 7), 'wrong actual Ray reply')
                child.send_signal(signal.SIGTERM)
                wait_until(lambda: (directory/'caught.json').exists(), child, 15)
                caught = json.loads((directory/'caught.json').read_text())
                result['caught'] = caught
                require(child.poll() is None and caught['initialized'], 'embedding/Ray exited before assertions')
                require(caught['network_threads'] == [], 'owned network threads survived: ' + repr(caught))
                client.sock.settimeout(3)
                require(client.sock.recv(1) == b'', 'peer did not observe EOF while driver stayed alive')
                with socket.socket() as replacement:
                    replacement.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    replacement.bind(('127.0.0.1', port))
                    replacement.listen()
                    require(child.poll() is None, 'process exited before listener replacement')
                result['live_process_eof_and_rebind'] = True
                (directory/'continue').touch()
                result['natural_exit'] = child.wait(timeout=20)
                require(result['natural_exit'] == 0, 'child cleanup failed')
                require(json.loads((directory/'finished.json').read_text()) == {'initialized': False},
                        'embedding caller failed to shut down its runtime')
                result['passed'] = True
            finally:
                # Preserve the unassisted result before best-effort fixture cleanup.
                (directory/'result.json').write_text(json.dumps(result, indent=2)+'\n')
                if client is not None:
                    client.sock.close()
                (directory/'continue').touch()
                try:
                    if child.poll() is None:
                        try:
                            child.wait(timeout=20)
                        except subprocess.TimeoutExpired:
                            pass
                finally:
                    child.close()
    print('RAY_NETWORK_OK live embedding process: peer EOF, listener reuse, joined network drivers', flush=True)


if __name__ == '__main__':
    run()
