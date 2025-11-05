#!/usr/bin/env python3
"""Actual gateways and HTTP graph: fetch layers must not interleave under prefetch."""
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from pathlib import Path
import json
import re
import signal
import sys
import tempfile
import threading

ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'apps/websearch/tests'))
from run_integration import Suite, require


def run():
    root=Path(tempfile.mkdtemp(prefix='parrot-bfs-',dir='/tmp'))
    print(f'FRONTIER_BFS_ARTIFACTS={root}',flush=True)
    report=dict(passed=[],failed=[])
    events=[];lock=threading.Lock()
    try:
        with ExitStack() as stack:
            class Handler(BaseHTTPRequestHandler):
                def log_message(self,*args): pass
                def do_GET(self):
                    if self.path=='/robots.txt':
                        body='User-agent: *\nAllow: /\n'
                    else:
                        match=re.fullmatch(r'/(z-seed|a-child|0-grandchild)-(\d{2})',self.path)
                        if not match or not 0<=int(match[2])<20:
                            self.send_error(502,'outside BFS corpus');return
                        depth={'z-seed':0,'a-child':1,'0-grandchild':2}[match[1]]
                        with lock: events.append((depth,self.path))
                        link=''
                        if depth<2:
                            origin=origins[1] if depth==0 else origins[0]
                            prefix='a-child' if depth==0 else '0-grandchild'
                            link=f'<a href="{origin}/{prefix}-{match[2]}">next</a>'
                        body=f'<html><title>BFS {self.path}</title><body>frontier layer contract {link}</body></html>'
                    data=body.encode()
                    self.send_response(200);self.send_header('Content-Type','text/html; charset=utf-8')
                    self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
            # Two literal IP identities preserve the cross-host counterexample
            # without granting localhost (or any other name) private DNS access.
            origins=[]
            for host in ('127.0.0.1', '127.0.0.2'):
                server=ThreadingHTTPServer((host,0),Handler);server.daemon_threads=True
                thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
                stack.callback(thread.join,5);stack.callback(server.server_close);stack.callback(server.shutdown)
                origins.append(f'http://{host}:{server.server_port}')
            report['crawl_origins']=origins
            suite=Suite(root,ROOT/'target/debug/websearch',stack)
            data=root/'data';data.mkdir()
            gateways,direct=suite.gateways(data)
            # Reverse names expose URL sorting, 20 roots exceed a 16-entry prefetch,
            # two hosts expose promotion of an unthrottled child ahead of roots.
            seeds=[f'{origins[0]}/z-seed-{i:02}' for i in reversed(range(20))]
            app,port=suite.app(data,direct,*seeds,'--insecure-development','--sites','1000',
                               '--maxdepth','2','--pages','60',crawl_origins=origins)
            suite.completed(app,port,gateways,60)
            with lock: actual=list(events)
            report['events']=actual
            require(len(actual)==60 and len({path for _,path in actual})==60,actual)
            require([d for d,_ in actual]==[0]*20+[1]*20+[2]*20,actual)
            require([path for d,path in actual if d==0]==[f'/z-seed-{i:02}' for i in reversed(range(20))],actual)
            require(not suite.corpus.blocked,'unexpected public crawl')
            report['passed'].append('60 actual HTTP requests preserve layers and root FIFO with prefetch and per-host throttling')
            print('FRONTIER_BFS_HTTP_OK: 60 requests, 3 layers, 20 FIFO roots, two host identities',flush=True)
        report['status']='passed'
    except BaseException as error:
        report['status']='failed';report['failed'].append(str(error));raise
    finally:
        with lock: report['events']=list(events)
        (root/'result.json').write_text(json.dumps(report,indent=2)+'\n')


if __name__=='__main__':
    signal.signal(signal.SIGTERM,lambda signum,frame:(_ for _ in ()).throw(KeyboardInterrupt(signum)))
    run()
