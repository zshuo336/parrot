"""Real crawler replaces an existing URL's obsolete terms and persists the result.

Requires current debug application and JVM/Erlang artifacts. The initial document
is written through the legacy wire API to verify upgrade and replay compatibility.
"""
from contextlib import ExitStack
from pathlib import Path
import argparse
import hashlib
import json
import signal
import struct
import sys
import tempfile
ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT/'apps/websearch/tests'))
from run_component_lifecycle import Client
import run_integration as web
from parrot_protocol import wire


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    options = parser.parse_args()
    directory = Path(tempfile.mkdtemp(prefix='ws-document-replay-', dir='/tmp'))
    binary = ROOT/'target/debug/websearch'
    report = dict(status='failed', runtime_root=str(directory), cases=[],
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    print('Artifacts:', directory, flush=True)
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(signum)
    signal.signal(signal.SIGTERM, interrupted)
    def string(text):
        data = text.encode()
        return struct.pack('<I',len(data)) + data
    try:
        with ExitStack() as stack:
            suite = web.Suite(directory, binary, stack)
            data = directory/'data'
            gateways, ports = suite.gateways(data)
            url = suite.corpus.base+'/extra'
            doc = int.from_bytes(hashlib.sha256(url.encode()).digest()[:8], 'little')
            with ExitStack() as observer:
                client = Client(observer,ports['jvm'])
                reply = client.admin('deploy',component=dict(name='search',version='legacy',
                    artifact=dict(kind='jvm',main_class='websearch.search.SearchComponent',coords=None,
                                  uri=(ROOT/'apps/websearch/jvm/target/websearch-jvm-1.0.0.jar').as_uri()),
                    instances=dict(kind='singleton'),config=None))
                web.require(reply['kind']=='deployed', f'initial search: {reply}')
                def ask(key, body=b''):
                    client.sequence += 1
                    client.socket.sendall(wire.build_frame(wire.FT_ASK,client.sequence,'/user/search','bin:ws/'+key,
                                                          wire.with_reply_to_prefix('/observer/reply',body)))
                    while True:
                        frame=client.decoder.next_frame()
                        if frame is None:
                            chunk=client.socket.recv(65536)
                            web.require(chunk,'observer disconnected before business reply')
                            client.decoder.feed(chunk)
                            continue
                        if frame.ft==wire.FT_HEARTBEAT:
                            client.socket.sendall(wire.build_frame(wire.FT_HEARTBEAT_ACK,frame.cid,'','',b''));continue
                        web.require(frame.cid==client.sequence and frame.ft==wire.FT_REPLY, f'business receipt: {frame}')
                        return frame.payload
                obsolete = 'obsoleteword'
                web.require(ask('DocMeta',struct.pack('<Q',doc)+string(url)+string('legacy title')+string(obsolete))==struct.pack('<I',1),'metadata receipt')
                web.require(ask('IndexTerms',struct.pack('<I',1)+string(obsolete)+struct.pack('<QI',doc,7))==struct.pack('<I',1),'terms receipt')
                web.require(ask('Flush')==struct.pack('<I',1),'initial flush receipt')
                old=json.loads(ask('Search',struct.pack('<I',10)+string(obsolete)))
                web.require(len(old)==1 and old[0]['url']==url,'legacy baseline must actually match')
                report['cases'].append(dict(name='legacy-baseline',hits=old))
                web.require(client.admin('stop', path_prefix='/user/search')['kind']=='stopped', 'legacy fixture must retire its own component before migration')

            app,port=suite.app(data,ports,url,'--keep','--pages','1','--sites','1','--maxdepth','0')
            stats=suite.completed(app,port,gateways,1)
            scores=suite.assert_search(port,['/extra'])
            web.require(web.results(port,obsolete)[0]==[],'old term survived real crawler replacement')
            suite.corpus.assert_requests({'/robots.txt':1,'/extra':1})
            report['cases'].append(dict(name='crawler-replacement',stats=stats,scores=scores))
            app.child.send_signal(signal.SIGTERM);app.child.wait(timeout=65)
            web.require(app.child.returncode==0 and 'shutdown complete' in app.text(),'application retirement')
            for gateway in gateways:gateway.stop()

            gateways,ports=suite.gateways(data,jvm_only=True)
            app,port=suite.app(data,ports,'--serve-only')
            stats=suite.ready(app,port,gateways)
            web.require(stats['docs']==1,'restart changed document count')
            web.require(suite.assert_search(port,['/extra'])==scores,'restart changed replacement scores')
            web.require(web.results(port,obsolete)[0]==[],'restart resurrected old term')
            suite.corpus.assert_requests({'/robots.txt':1,'/extra':1})
            app.child.send_signal(signal.SIGTERM);app.child.wait(timeout=65)
            web.require(app.child.returncode==0 and 'shutdown complete' in app.text(),'replay application retirement')
            report['cases'].append(dict(name='fresh-jvm-replay',stats=stats,scores=scores))
            report['requests']=dict(suite.corpus.requests)
        report['status']='passed'
        print('PASS legacy baseline / actual crawler replacement / fresh JVM replay',flush=True)
    except (Exception,KeyboardInterrupt) as error:
        report['error']=str(error)
        raise
    finally:
        body=json.dumps(report,indent=2,ensure_ascii=False)+'\n'
        (directory/'result.json').write_text(body)
        if options.report:options.report.write_text(body)
    return 0


if __name__=='__main__':
    raise SystemExit(main())
