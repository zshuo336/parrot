#!/usr/bin/env python3
"""Actual TERM acceptance: live Wire connection, retry loop and explicit cleanup.

Records natural exit and shutdown receipts before any emergency group cleanup.
No process-name killing, ambient Ray cluster or public network is used.
"""
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
import time

from owned_process import OwnedProcess
from run_ray_lifecycle import Client, ROOT, wire


CHILD = r'''import sys, json, runpy, time
from pathlib import Path
import ray
from ray._private import ray_constants
receipt, mode, repo, directory, address = sys.argv[1:]
receipt, repo, directory = Path(receipt), Path(repo), Path(directory)
original_init = ray.init
def worker_signal_option():
    from ray._private import ray_constants
    return getattr(ray_constants, 'RAY_DISABLE_FAILURE_SIGNAL_HANDLER', False)
def initialize(*args, **kwargs):
    result = original_init(*args, **kwargs)
    # The compatibility seam must affect only the driver being initialized.
    # A real remote worker must retain Ray's normal native diagnostics.
    if ray.get(ray.remote(worker_signal_option).remote()) is not False:
        raise RuntimeError('native signal diagnostics disabled in remote worker')
    return result
ray.init = initialize
original = ray.shutdown
def record(event, **fields):
    with receipt.open('a') as out:
        out.write(json.dumps(dict(event=event, **fields))+'\n')
def shutdown(*args, **kwargs):
    record('shutdown_enter')
    result = original(*args, **kwargs)
    if mode == 'cli':
        # Hold a real caller cleanup step after native shutdown has returned.
        # Without this rendezvous, a second TERM might arrive before native
        # teardown resets the OS disposition and accidentally pass the test.
        (directory/'native-returned').touch()
        deadline = time.monotonic() + 10
        while not (directory/'repeat-sent').exists():
            if time.monotonic() >= deadline:
                raise TimeoutError('parent did not send repeated TERM')
            time.sleep(0.01)
    record('shutdown_return', initialized=ray.is_initialized())
    return result
ray.shutdown = shutdown
try:
    if mode == 'wrapper':
        wrapper = repo/'apps/websearch/tests/ray_gateway.py'
        sys.argv = [str(wrapper), '0', str(directory/'ray')]
        runpy.run_path(str(wrapper), run_name='__main__')
    else:
        from parrot_protocol.ray_gw import main
        args = ['ray_gw', '0'] + (['parrot='+address] if mode == 'registration' else [])
        main(args, ray_kwargs={'address':'local','num_cpus':2,'include_dashboard':False,
                              'log_to_driver':False,'_temp_dir':str(directory/'ray')})
finally:
    record('caller_finally')
'''


def require(condition, detail):
    if not condition:
        raise RuntimeError(detail)


def wait_until(probe, child, seconds=90):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        require(child.poll() is None, f"gateway exited before expected event: {child.returncode}")
        time.sleep(0.02)
    raise TimeoutError("gateway event deadline")


def run():
    root = Path(tempfile.mkdtemp(prefix="prstop-", dir="/tmp"))
    print("RAY_SHUTDOWN_RESULTS=" + str(root), flush=True)
    results = []
    for index, mode in enumerate(["wrapper", "cli", "registration"]):
        directory = root / str(index)
        directory.mkdir()
        script = directory / "child.py"
        script.write_text(CHILD)
        receipt, logpath = directory / "receipt.jsonl", directory / "gateway.log"
        env = dict(os.environ, PYTHONPATH=str(ROOT / "interop/python"),
                   RAY_AUTH_TOKEN=secrets.token_hex(32), RAY_USAGE_STATS_ENABLED="0",
                   RAY_DISABLE_FAILURE_SIGNAL_HANDLER="0")
        env.pop("RAY_AUTH_TOKEN_PATH", None)
        env.pop("RAY_ADDRESS", None)
        # Hold a local non-listening endpoint so registration reliably retries.
        with socket.socket() as refusal, logpath.open("w") as log:
            refusal.bind(("127.0.0.1", 0))
            address = "127.0.0.1:" + str(refusal.getsockname()[1])
            with OwnedProcess([sys.executable, str(script), str(receipt), mode,
                                      str(ROOT), str(directory), address], env=env,
                                     stdout=log, stderr=subprocess.STDOUT) as child:
                client = None
                natural_exit = None
                try:
                    if mode == "registration":
                        wait_until(lambda: "connect failed" in logpath.read_text(), child)
                    else:
                        match = wait_until(lambda: re.search(r"RAY_GW_PORT=(\d+)", logpath.read_text()), child)
                        client = Client(int(match.group(1)))
                        if mode == "cli":
                            frame = client.exchange(wire.FT_ASK, "/user/probe", "bin:u:Ping",
                                                    wire.with_reply_to_prefix("/test/reply", struct.pack("<Q", 7)))
                            require(frame.ft == wire.FT_REPLY and frame.type_key == "bin:u:Pong"
                                    and frame.payload == struct.pack("<Q", 9), "wrong CLI Wire probe response")
                    start = time.monotonic()
                    child.send_signal(signal.SIGTERM)
                    if mode == "cli":
                        wait_until(lambda: (directory/'native-returned').exists(), child, 15)
                        # Native teardown must not revoke the caller's TERM policy
                        # before the rest of the caller's cleanup completes.
                        child.send_signal(signal.SIGTERM)
                        (directory/'repeat-sent').touch()
                    natural_exit = child.wait(timeout=20)
                    events = [json.loads(line) for line in receipt.read_text().splitlines()]
                    require(natural_exit == 0, f"TERM exit {natural_exit}; see {logpath}")
                    require(events == [{"event":"shutdown_enter"},
                                       {"event":"shutdown_return","initialized":False},
                                       {"event":"caller_finally"}], f"cleanup receipt mismatch: {events}")
                    if client is not None:
                        client.sock.settimeout(5)
                        require(client.sock.recv(1) == b"", "established connection remained open")
                    results.append(dict(mode=mode, exit=natural_exit, seconds=time.monotonic()-start, events=events))
                finally:
                    if client is not None:
                        client.sock.close()
                    # Record prior to fallback: forced teardown never counts as a pass.
                    (root / "result.json").write_text(json.dumps(dict(passed=results, last_mode=mode,
                                                                     natural_exit=natural_exit), indent=2)+'\n')
                    child.close()
    print("RAY_SHUTDOWN_OK wrapper, CLI/repeated TERM and registration retry exit 0 with explicit cleanup", flush=True)


if __name__ == "__main__":
    run()
