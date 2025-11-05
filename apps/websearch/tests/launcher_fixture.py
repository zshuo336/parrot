"""Real process/HTTP fixtures for production launcher control flow."""
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve()


def main():
    role = sys.argv[1]
    case = os.environ.get('WS_TEST_CASE', 'success')
    records = Path(os.environ['WS_TEST_RECORDS'])
    if role == 'driver':
        spec = importlib.util.spec_from_file_location('websearch_launcher_fixture', ROOT/'apps/websearch/deploy/launcher.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        class FixtureLauncher(module.Launcher):
            def prepare(self):
                if case == 'build-fails':
                    self.completed(self.spawn('build', [sys.executable, HERE, 'failed-build']))
                self.binary = Path(os.environ['WS_TEST_APP'])
            def gateways(self, target=None):
                return [self.spawn(name, [sys.executable, HERE, 'gateway', name], marker='READY=',
                                   wait_for_output=False, python_entrypoint=name == 'ray')
                        for name in (['jvm'] if self.serve_only else ['erl', 'jvm', 'ray'])]
        module.Launcher = FixtureLauncher
        return module.main(sys.argv[2:])
    if role == 'failed-build':
        return 17
    if role == 'descendant':
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        os.write(int(sys.argv[2]), b'R')
        os.close(int(sys.argv[2]))
        time.sleep(90)
        return 0
    if role == 'gateway':
        name = sys.argv[2]
        if name == 'ray':
            try:
                os.setpgrp()
            except PermissionError:
                if os.getpid() != os.getpgrp(): raise
        sock = socket.socket()
        sock.bind(('127.0.0.1', 0))
        sock.listen()
        read, write = os.pipe()
        descendant = subprocess.Popen([sys.executable, HERE, 'descendant', str(write)], pass_fds=(write, sock.fileno()))
        os.close(write)
        if os.read(read, 1) != b'R': raise RuntimeError('descendant readiness')
        os.close(read)
        (records/(name+'.json')).write_text(json.dumps(dict(pid=os.getpid(), descendant=descendant.pid, port=sock.getsockname()[1])))
        print('READY='+str(sock.getsockname()[1]), flush=True)
        if case == 'gateway-crash' and name == 'ray':
            time.sleep(0.5)
            return 8
        time.sleep(90)
        return 0
    if role == 'app':
        stopping = threading.Event()
        signal.signal(signal.SIGTERM, lambda *_: stopping.set())
        if '--validate-only' in sys.argv:
            return 0
        (records/'app-args.json').write_text(json.dumps(sys.argv[2:]))
        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'
            def log_message(self, *_): pass
            def do_GET(self):
                if case == 'trickle':
                    try:
                        for byte in b'HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n':
                            self.connection.sendall(bytes([byte]))
                            time.sleep(0.15)
                    except OSError: pass
                    return
                body = json.dumps(dict(docs=True if case == 'bad-health' else 0, terms=0, postings=0, queries=0, segments=0)).encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                try: self.wfile.write(body)
                except OSError: pass
        http = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        http.daemon_threads = True
        if '--bind' in sys.argv:
            registry = socket.socket()
            registry.bind(('127.0.0.1', 0))
            registry.listen()
            print('PARROT_WEBSEARCH_REGISTRY_PORT='+str(registry.getsockname()[1]), flush=True)
        print('PARROT_WEBSEARCH_PORT='+str(http.server_port), flush=True)
        print('网关已全部注册\n浏览器打开 http://localhost:'+str(http.server_port), flush=True)
        if case == 'dead-ready':
            http.server_close()
            return 0
        threading.Thread(target=http.serve_forever, daemon=True).start()
        stopping.wait(90 if case in ('signal', 'signal-drain', 'gateway-crash', 'bad-health', 'trickle') else 1)
        if case == 'signal-drain':
            time.sleep(3)
            dependencies = {}
            for name in ('erl', 'jvm', 'ray'):
                port = json.loads((records/(name+'.json')).read_text())['port']
                try:
                    with socket.create_connection(('127.0.0.1', port), timeout=1):
                        dependencies[name] = True
                except OSError:
                    dependencies[name] = False
            (records/'run/app-drained.json').write_text(json.dumps(dependencies))
        http.shutdown()
        http.server_close()
        return 23 if case == 'app-fails' else 0
    raise ValueError(role)


if __name__ == '__main__':
    raise SystemExit(main())
