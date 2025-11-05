"""Kill the real crawler after destructive Next, then recover the pending URL."""
from contextlib import ExitStack
from pathlib import Path
import argparse,hashlib,json,signal,tempfile,shutil
from run_integration import Suite,until,require,CorruptFirstReply,results
ROOT=Path(__file__).resolve().parents[3]

def stop_app(app):
    app.child.send_signal(signal.SIGTERM);app.child.wait(timeout=65)
    require(app.child.returncode==0,'application retirement failed: '+app.text())

class HeldReply(CorruptFirstReply):
    """Hold a complete real reply so SIGKILL occurs at an observed wire boundary."""
    def __init__(self,upstream,key):
        import threading
        self.observed=threading.Event();self.release=threading.Event();self.receipt=None
        super().__init__(upstream,key=key,payload=b'')
    def exact(self,sock,size):
        import struct
        body=super().exact(sock,size)
        if len(body)>=28 and body[1]==0x11:
            path_size,=struct.unpack_from('<I',body,20)
            key_size,=struct.unpack_from('<I',body,24+path_size)
            start=28+path_size
            if body[start:start+key_size]==self.key and not self.observed.is_set():
                self.receipt=body[start+key_size:];self.observed.set()
                while not self.stopping.is_set() and not self.release.wait(.1):pass
        return body
    def close(self):
        self.release.set();super().close()


def fault_recovery(directory,binary,report):
    import struct
    for name,tag,key in [('push','erl',b'bin:ws/PushAck'),('document','jvm',b'bin:ws/DocumentsAck'),
                         ('checkpoint','jvm',b'bin:ws/CheckpointV2Ack'),('clear','jvm',b'bin:ws/ClearAck')]:
        with ExitStack() as stack:
            root=directory/name;root.mkdir();suite=Suite(root,binary,stack);data=root/'data'
            gateways,ports=suite.gateways(data)
            proxy=HeldReply(ports[tag],key);stack.callback(proxy.close)
            addresses=dict(ports);addresses[tag]=proxy.port
            app,_=suite.app(data,addresses,suite.corpus.base+'/extra','--pages','1','--sites','1','--maxdepth','0')
            until(proxy.observed.is_set,[app,*gateways],seconds=90)
            app.child.send_signal(signal.SIGKILL);app.child.wait(timeout=10);proxy.release.set()
            require(app.child.returncode==-signal.SIGKILL and proxy.receipt,'actual accepted receipt and SIGKILL required: '+name)
            require('爬取收尾完成' not in app.text(),'false completion after '+name)
            before=suite.corpus.requests['/extra']
            for gateway in gateways:gateway.stop()
            gateways,ports=suite.gateways(data)
            app,port=suite.app(data,ports,suite.corpus.base+'/extra','--keep','--pages','1','--sites','1','--maxdepth','0')
            stats=suite.completed(app,port,gateways,1);suite.assert_search(port,['/extra'])
            require(suite.corpus.requests['/extra']==before+1,'unconfirmed task not replayed after '+name)
            stop_app(app)
            report['cases'].append(dict(name='SIGKILL before receiving '+name+' acknowledgement',receipt_hex=proxy.receipt.hex(),stats=stats,requests=dict(suite.corpus.requests)))
            print('PASS recovery after lost '+name+' acknowledgement',flush=True)


def semantic_recovery(directory,binary,report):
    from collections import Counter
    from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
    import threading
    with ExitStack() as stack:
        root=directory/'semantics';root.mkdir();suite=Suite(root,binary,stack)
        requests=Counter();lock=threading.Lock();report['semantic_requests']=requests
        class Handler(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_GET(self):
                with lock:requests[self.path]+=1;count=requests[self.path]
                if self.path=='/alias':
                    self.send_response(302);self.send_header('Location','/final');self.send_header('Content-Length','0');self.end_headers();return
                if self.path=='/retry' and count==1:self.send_error(503);return
                if self.path=='/robots.txt':body='User-agent: *\nAllow: /\n'
                elif self.path in ('/retry','/root','/child','/final'):
                    link='<a href="/child">child</a>' if self.path=='/root' else ''
                    body=f'<html><title>{self.path}</title><body>{suite.token} {link}</body></html>'
                else:self.send_error(502);return
                content=body.encode();self.send_response(200);self.send_header('Content-Type','text/html');self.send_header('Content-Length',str(len(content)));self.end_headers();self.wfile.write(content)
        server=ThreadingHTTPServer(('127.0.0.1',0),Handler);server.daemon_threads=True
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        stack.callback(thread.join,5);stack.callback(server.server_close);stack.callback(server.shutdown)
        origin=f'http://127.0.0.1:{server.server_port}'
        def start(data,path,keep=False,depth=0,pages=1):
            gateways,ports=suite.gateways(data)
            app,port=suite.app(data,ports,origin+path,*(['--keep'] if keep else []),'--pages',str(pages),'--sites','1000','--maxdepth',str(depth),crawl_origins=[origin])
            return gateways,ports,app,port
        def retire(gateways,app):
            stop_app(app)
            for gateway in gateways:gateway.stop()
        data=root/'retry';gateways,ports,app,port=start(data,'/retry')
        until(lambda:'爬取收尾完成' in app.text(),[app,*gateways]);require(suite.ready(app,port,gateways)['docs']==0,'failed page indexed')
        retire(gateways,app)
        gateways,ports,app,port=start(data,'/retry',True);suite.completed(app,port,gateways,1)
        require(requests['/retry']==2,'failed task was treated as complete');retire(gateways,app)
        report['cases'].append(dict(name='failed attempt retries on next invocation',requests=dict(requests)))

        data=root/'redirect';gateways,ports,app,port=start(data,'/alias');suite.completed(app,port,gateways,1)
        require(results(port,suite.token)[0]==[origin+'/final'],'redirect final document missing');retire(gateways,app)
        gateways,ports,app,port=start(data,'/alias',True);suite.completed(app,port,gateways,1)
        require(requests['/alias']==1 and requests['/final']==1,'completion used final URL instead of original task URL');retire(gateways,app)
        report['cases'].append(dict(name='original redirect task completed exactly once',requests=dict(requests)))

        data=root/'depth';gateways,ports,app,port=start(data,'/root',depth=1);suite.completed(app,port,gateways,1);retire(gateways,app)
        earlier=root/'index-before-child';shutil.copytree(data/'index',earlier)
        gateways,ports,app,port=start(data,'/root',True,depth=0)
        until(lambda:app.child.poll() is not None,gateways);require(app.child.returncode!=0 and 'pending crawl task is outside' in app.text(),'lost stored depth')
        require(requests['/child']==0,'lower depth scope fetched pending child')
        for gateway in gateways:gateway.stop()
        gateways,ports,app,port=start(data,'/root',True,depth=1);suite.completed(app,port,gateways,2)
        require(set(results(port,suite.token)[0])=={origin+'/root',origin+'/child'},'discovered child not recovered')
        require(requests['/root']==1 and requests['/child']==1,'parent repeated or child skipped');retire(gateways,app)
        report['cases'].append(dict(name='discovered child retains depth; reduced scope rejected',requests=dict(requests)))

        # Restore an actual older complete index; GC need not retain obsolete
        # bodies in the active directory. Preserve both sides of the rollback.
        index=data/'index';saved=root/'latest-index.saved';index.rename(saved)
        shutil.copytree(earlier,index)
        gateways,ports,app,port=start(data,'/root',True,depth=1)
        until(lambda:app.child.poll() is not None,gateways)
        require(app.child.returncode!=0 and 'generation mismatch' in app.text(),'old index accepted completed journal tasks')
        require(requests['/child']==1 and requests['/root']==1,'mismatch performed HTTP effects')
        for gateway in gateways:gateway.stop()
        index.rename(root/'rolled-back-index.saved');saved.rename(index)
        report['cases'].append(dict(name='older index generation rejected before HTTP',requests=dict(requests)))
        index.rename(saved)
        shutil.copytree(root/'redirect/index',index)
        gateways,ports,app,port=start(data,'/root',True,depth=1)
        until(lambda:app.child.poll() is not None,gateways)
        require(app.child.returncode!=0 and 'generation mismatch' in app.text(),'foreign index epoch accepted')
        for gateway in gateways:gateway.stop()
        index.rename(root/'foreign-epoch.saved');saved.rename(index)
        report['cases'].append(dict(name='foreign index epoch rejected before HTTP'))

        data=root/'legacy';data.mkdir();(data/'dedupe.tsv').write_text(origin+'/retry\n'+origin+'/final\n')
        counts=dict(requests)
        gateways,ports,app,port=start(data,'/retry',True,pages=2);suite.completed(app,port,gateways,2)
        require(requests['/retry']==counts['/retry']+1 and requests['/final']==counts['/final']+1,'legacy URLs incorrectly inferred complete')
        require('legacy dedupe migration' in app.text(),'legacy replay migration was not reported');retire(gateways,app)
        report['cases'].append(dict(name='legacy dedupe conservatively replays unknown tasks as roots',requests=dict(requests)))
        print('PASS retry / redirect identity / discovery depth / index rollback / foreign epoch / legacy migration contracts',flush=True)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report',type=Path)
    parser.add_argument('--only-semantics',action='store_true',help='Run the retry, identity, scope and migration cases only')
    options=parser.parse_args()
    directory=Path(tempfile.mkdtemp(prefix='ws-rec-',dir='/tmp'))
    binary=ROOT/'target/debug/websearch'
    report=dict(status='failed',runtime_root=str(directory),binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),cases=[])
    print('Artifacts:',directory,flush=True)
    try:
        if not options.only_semantics:
            with ExitStack() as stack:
                suite=Suite(directory,binary,stack);data=directory/'data'
                gateways,ports=suite.gateways(data)
                app,port=suite.app(data,ports,suite.corpus.base+'/slow','--pages','1','--sites','1','--maxdepth','0')
                until(suite.corpus.slow_started.is_set,[app,*gateways])
                require(suite.corpus.requests['/slow']==1,'interruption must follow actual HTTP dispatch')
                contender,_=suite.app(data,ports,'--serve-only')
                until(lambda:contender.child.poll() is not None,[app,*gateways])
                require(contender.child.returncode!=0 and 'directory already has an owner' in contender.text(),'serve-only stole an active crawl directory')
                report['cases'].append(dict(name='concurrent serve-only rejected before remote deployment'))
                app.child.send_signal(signal.SIGKILL);app.child.wait(timeout=10)
                require(app.child.returncode==-signal.SIGKILL,'real abrupt stop')
                suite.corpus.slow_release.set()
                for gateway in gateways:gateway.stop()
                gateways,ports=suite.gateways(data)
                app,port=suite.app(data,ports,suite.corpus.base+'/slow','--keep','--pages','1','--sites','1','--maxdepth','0')
                stats=suite.completed(app,port,gateways,1)
                scores=suite.assert_search(port,['/slow'])
                require(suite.corpus.requests['/slow']==2,'pending original URL was not replayed exactly once')
                report['cases'].append(dict(name='SIGKILL after Next and HTTP dispatch',stats=stats,scores=scores,requests=dict(suite.corpus.requests)))
                app.child.send_signal(signal.SIGTERM);app.child.wait(timeout=65)
                require(app.child.returncode==0,'recovered application exit')
                for gateway in gateways:gateway.stop()
                gateways,ports=suite.gateways(data)
                app,port=suite.app(data,ports,suite.corpus.base+'/slow','--keep','--pages','1','--sites','1','--maxdepth','0')
                suite.completed(app,port,gateways,1)
                require(suite.corpus.requests['/slow']==2,'durably completed URL was fetched again')
                require(suite.assert_search(port,['/slow'])==scores,'second restart changed index')
                report['cases'].append(dict(name='completed task skipped after fresh gateways',requests=dict(suite.corpus.requests)))
            fault_recovery(directory,binary,report)
        semantic_recovery(directory,binary,report)
        report['status']='passed'
        print(f'PASS frontier recovery: {len(report["cases"])} verified cases',flush=True)
    except BaseException as error:
        report['error']=str(error);raise
    finally:
        text=json.dumps(report,indent=2)+'\n';(directory/'result.json').write_text(text)
        if options.report:options.report.write_text(text)

if __name__=='__main__':main()
