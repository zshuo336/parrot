"""Owned HTTP corpus for the owned four-image acceptance network."""
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import re
import sys
import threading


def serve(token):
    if not re.fullmatch(r'parrot[0-9a-f]{32}', token):
        raise ValueError('expected a unique corpus token')
    requests = Counter()
    blocked = []
    lock = threading.Lock()
    release = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            if self.path == '/__release':
                release.set()
                body = '{}'
                content_type = 'application/json'
            elif self.path == '/__state':
                with lock:
                    body = json.dumps(dict(requests=dict(requests), blocked=list(blocked)))
                content_type = 'application/json'
            else:
                with lock:
                    requests[self.path] += 1
                content_type = 'text/html; charset=utf-8'
                if self.path == '/robots.txt':
                    body = 'User-agent: *\nDisallow: /denied\n'
                elif self.path in ('/root', '/child', '/extra', '/slow'):
                    if self.path == '/slow' and not release.wait(15):
                        self.send_error(504, 'release deadline')
                        return
                    word = {'/root': 'alpha', '/child': 'beta', '/extra': 'gamma', '/slow': 'delta'}[self.path]
                    links = '<a href="child">child</a><a href="/denied">denied</a>' if self.path == '/root' else ''
                    body = f'<html><title>{word} {token}</title><body>{word} {token} 搜索测试 {links}</body></html>'
                elif self.path == '/missing':
                    self.send_error(404)
                    return
                else:
                    with lock:
                        blocked.append(self.path)
                    self.send_error(502, 'outside corpus')
                    return
            data = body.encode()
            self.send_response(200)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(('0.0.0.0', 9000), Handler)
    server.daemon_threads = True
    print('CORPUS_READY', flush=True)
    server.serve_forever()


if __name__ == '__main__':
    serve(sys.argv[1])
