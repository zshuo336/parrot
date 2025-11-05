#!/usr/bin/env python3
"""Run the real obs binary against owned Erlang, JVM and Ray gateway processes.

Prerequisites: cargo build --locked -p parrot-obs; make check-erl check-jvm;
install interop/python[ray]. Missing artifacts/tools fail, never silently skip.
"""
from contextlib import ExitStack
from collections import deque
import importlib.util
import tempfile
from pathlib import Path
import os
import queue
import signal
import socket
import json
import urllib.request
import urllib.error
import subprocess
import sys
import threading

ROOT = Path(__file__).resolve().parents[3]


_spec = importlib.util.spec_from_file_location("gateway_manager", ROOT / "tools/parrot-obs/gateway_manager.py")
manager = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(manager)


class OwnedGateways:
    """Use the production guardians for the actual gateway process trees."""
    def __init__(self, config):
        self.config = config
        self.snapshot = None

    def __enter__(self):
        self.snapshot = manager.start(self.config)
        return self

    def nodes(self):
        return [f'{name}@{self.snapshot["nodes"][name]}=127.0.0.1:{port}'
                for name, port in sorted(self.snapshot['ports'].items())]

    def close(self):
        manager.stop(Path(self.config['directory']))

    def __exit__(self, *_):
        self.close()


class ObservedCli:
    """Capture the known Rust CLI, which does not launch gateway subprocesses."""
    def __init__(self, command, marker, *, cwd=ROOT, env=None):
        self.lines = deque(maxlen=200)
        self.ready = queue.Queue()
        self.process = subprocess.Popen(command, cwd=cwd, env=env,
                                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                        text=True, start_new_session=True)
        self.reader = threading.Thread(target=self._read, args=(marker,), daemon=True)
        self.reader.start()

    def _read(self, marker):
        for line in self.process.stdout:
            self.lines.append(line.rstrip())
            if line.startswith(marker):
                self.ready.put(line[len(marker):].strip())
        self.ready.put(None)

    def port(self):
        try:
            value = self.ready.get(timeout=120)
            port = int(value.split()[0]) if value else 0
            if not 1 <= port <= 65535:
                raise ValueError("gateway exited or gave invalid port")
            return port
        except (queue.Empty, ValueError) as error:
            raise RuntimeError("gateway not ready:\n" + "\n".join(self.lines)) from error

    def close(self):
        # The CLI has no child processes. Signal the unreaped direct child only.
        if self.process.returncode is None:
            try:
                self.process.terminate()
            except ProcessLookupError:
                pass
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
        self.reader.join(timeout=5)
        self.process.stdout.close()


def run():
    binary = ROOT / "target/debug/parrot-obs"
    if not binary.is_file():
        raise FileNotFoundError(f"required artifact missing: {binary}")
    with tempfile.TemporaryDirectory(prefix='pg-obs-', dir='/tmp') as temporary, ExitStack() as cleanup:
        environment = dict(os.environ, GW_ERL_PORT='0', GW_JVM_PORT='0', GW_RAY_PORT='0',
                           GW_JVM_NODE='jvm-search-1')
        config = manager.config_from_environment(Path(temporary), environment)
        gateways = cleanup.enter_context(OwnedGateways(config))
        nodes = gateways.nodes()
        for arguments in [["status"], ["metrics"], ["ping", "--rounds=2"],
                          ["ask", "--rounds=5"], ["trace", "--seconds=1"],
                          ["load", "--seconds=1", "--conc=2"]]:
            command = [str(binary), *arguments, *nodes, "--insecure-development"]
            result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, timeout=60)
            print(f"OBS_GATEWAY_COMMAND={arguments!r} exit={result.returncode}", flush=True)
            print(result.stdout, end="", flush=True)
            if result.returncode:
                raise RuntimeError(result.stderr)
        # Watch must produce a second sampling round and honor graceful interruption.
        watcher = ObservedCli([str(binary), "watch", *nodes, "--interval=1", "--insecure-development"], "[")
        cleanup.callback(watcher.close)
        for _ in range(4):
            sample = watcher.ready.get(timeout=15)
            if sample is None or "asks/s=" not in sample:
                raise AssertionError(f"watch failed: {sample!r}")
        if "asks/s=n/a" in sample:
            raise AssertionError("second sampling round failed to establish a rate")
        watcher.process.send_signal(signal.SIGINT)
        if watcher.process.wait(timeout=10) != 0:
            raise AssertionError("watch did not shut down cleanly on Ctrl-C")
        print("OBS_WATCH_OK: second round observed and graceful interrupt joined", flush=True)

        # A collision fails startup instead of accidentally checking another server.
        # The readiness marker comes only from our child after successful bind.
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            web_port = reservation.getsockname()[1]
        web = ObservedCli([str(binary), "web", *nodes, f"--web-port={web_port}", "--insecure-development"],
                      "Web dashboard: http://127.0.0.1:")
        cleanup.callback(web.close)
        if web.port() != web_port:
            raise AssertionError("web bound an unexpected port")
        url = f"http://127.0.0.1:{web_port}"
        with urllib.request.urlopen(url + "/api/metrics", timeout=20) as response:
            if response.headers.get("Cache-Control") != "no-store":
                raise AssertionError("metrics must not be cached")
            snapshots = json.load(response)
        if set(snapshots) != {"erl-gw-1", "ray-gw-1", "jvm-search-1"}:
            raise AssertionError(f"wrong gateway set: {snapshots.keys()}")
        for name, snapshot in snapshots.items():
            if not snapshot.get("runtime") or not isinstance(snapshot.get("ts_ms"), str) or int(snapshot["ts_ms"]) <= 0:
                raise AssertionError(f"invalid live snapshot: {snapshot}")
            expected_mask = (1 << 17) - 1
            if sys.platform != 'linux':
                expected_mask &= ~(1 << 15)
            if snapshot.get('available') != str(expected_mask) or not snapshot.get('epoch'):
                raise AssertionError(f"missing live measured capability: {snapshot}")
            unit = {'erl-gw-1':'erlang_processes', 'ray-gw-1':'python_threads', 'jvm-search-1':'jvm_threads'}[name]
            if snapshot.get('process_kind') != unit or not snapshot.get('sample_clock'):
                raise AssertionError(f"missing units or sampling clock: {snapshot}")
            if not isinstance(snapshot.get('sampled_at_ns'), str) or int(snapshot['sampled_at_ns']) < 0:
                raise AssertionError(f"missing receipt timestamp: {snapshot}")
        with urllib.request.urlopen(url, timeout=5) as response:
            page = response.read().decode()
            if "script-src 'self'" not in response.headers.get("Content-Security-Policy", ""):
                raise AssertionError("page missing script policy")
        if '<script src="/dashboard.js" defer>' not in page or '<script>' in page:
            raise AssertionError("dashboard script must use its same-origin resource")
        with urllib.request.urlopen(url + "/dashboard.js", timeout=5) as response:
            if "function render(" not in response.read().decode():
                raise AssertionError("dashboard script was not served")
        request = urllib.request.Request(url + "/api/metrics", headers={"Host": "untrusted.example"})
        try:
            urllib.request.urlopen(request, timeout=5)
        except urllib.error.HTTPError as error:
            if error.code != 403:
                raise
        else:
            raise AssertionError("foreign Host was accepted")
        web.process.send_signal(signal.SIGINT)
        if web.process.wait(timeout=10) != 0:
            raise AssertionError("web did not shut down cleanly on Ctrl-C")
        print("OBS_WEB_OK: real snapshots, precision, script, policy and Host boundary", flush=True)

        # A real disconnected target must make the process fail, including clean shutdown.
        gateways.close()
        result = subprocess.run([str(binary), "ask", nodes[0], "--rounds=1", "--insecure-development"],
                                cwd=ROOT, text=True, capture_output=True, timeout=30)
        if result.returncode == 0 or "panicked" in result.stderr:
            raise AssertionError(f"disconnected gateway produced wrong verdict: {result.stdout} {result.stderr}")
        print("OBS_GATEWAYS_OK: eight commands, all three dialects, disconnected target fails", flush=True)


if __name__ == "__main__":
    run()
