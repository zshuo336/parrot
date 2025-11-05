"""Actual release packaging, receiver and relocated multi-language business.

SSH transport alone is replaced by a local shell. Builds, bundle verification,
installed start-remote.sh, Erlang/Ray/JVM and Rust indexing/replay are real.
"""
from contextlib import ExitStack
from pathlib import Path
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile

import run_integration as web

ROOT=Path(__file__).resolve().parents[3]
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--report',type=Path)
args=parser.parse_args()
root=Path(tempfile.mkdtemp(prefix='ws-dist-e2e-',dir='/tmp'))
report=dict(runtime_root=str(root),scope='Local SSH transport substitute; real packaging, receiver and relocated gateways',cases=[])
print('Artifacts:',root,flush=True)
try:
    shim=root/'transport';shim.mkdir()
    ssh=shim/'ssh'
    ssh.write_text('#!'+sys.executable+'\nimport subprocess,sys\nraise SystemExit(subprocess.call(["sh","-c",sys.argv[-1]]))\n')
    ssh.chmod(0o700)
    release=root/'release space 中文 #?%'
    with (root/'distribution.log').open('w') as output:
        subprocess.run(['bash',str(ROOT/'apps/websearch/deploy/distribute.sh'),'isolated-local-fixture','--root',str(release),'--profile','dev'],
                       env=dict(os.environ,PATH=str(shim)+':'+os.environ['PATH']),stdout=output,stderr=subprocess.STDOUT,check=True,timeout=600)
    manifest=json.loads((release/'MANIFEST.json').read_text())
    web.require(all(hashlib.sha256((release/name).read_bytes()).hexdigest()==sha for name,sha in manifest.items()),'installed files differ from manifest')
    cp=(release/'gw/cp.txt').read_text().strip().split(':')
    web.require(all(not Path(p).is_absolute() and (release/'gw'/p).is_file() for p in cp),'classpath is not self-contained')
    report['release_files']=len(manifest);report['jvm_dependencies']=len(cp)
    with ExitStack() as stack:
        suite=web.Suite(root,ROOT/'target/debug/websearch',stack)
        data=root/'data'
        app,port=suite.app(data,{},suite.corpus.base+'/root','--bind','127.0.0.1:0','--wait','90','--node-root',str(release),'--pages','10','--sites','1','--maxdepth','1',port=0)
        target=web.until(lambda:re.search(r'应用监听 (127\.0\.0\.1:\d+)',app.text()),[app]).group(1)
        gateways=[]
        for role in ['erl','ray','jvm']:
            env=dict(suite.env)
            if role=='ray':
                for key in ['HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy']:env[key]=suite.runtime_proxy.base
            gateways.append(suite.process(role,['bash',str(release/'start-remote.sh'),role,target,'--root',str(release),'--data',str(data)],env,cwd=root))
        port=int(web.until(lambda:re.search(r'PARROT_WEBSEARCH_PORT=(\d+)',app.text()),[app,*gateways]).group(1))
        stats=suite.completed(app,port,gateways,2)
        before=suite.assert_search(port,['/root','/child'])
        suite.corpus.assert_requests({'/robots.txt':1,'/root':1,'/child':1})
        report['cases'].append(dict(name='relocated-registry',stats=stats,scores=before))
        app.stop()
        for gateway in gateways:gateway.stop()
        app,port=suite.app(data,{},'--serve-only','--bind','127.0.0.1:0','--wait','30','--node-root',str(release),port=0)
        target=web.until(lambda:re.search(r'应用监听 (127\.0\.0\.1:\d+)',app.text()),[app]).group(1)
        jvm=suite.process('replay-jvm',['bash',str(release/'start-remote.sh'),'jvm',target,'--root',str(release),'--data',str(data)],cwd=root)
        port=int(web.until(lambda:re.search(r'PARROT_WEBSEARCH_PORT=(\d+)',app.text()),[app,jvm]).group(1))
        stats=suite.ready(app,port,[jvm]);after=suite.assert_search(port,['/root','/child'])
        web.require(before==after,'restarted JVM changed exact search URLs/scores')
        suite.corpus.assert_requests({'/robots.txt':1,'/root':1,'/child':1})
        report['cases'].append(dict(name='relocated-replay',stats=stats,scores=after))
        app.stop();jvm.stop()
        report['http_requests']=dict(suite.corpus.requests)
        report['runtime_probes']=suite.runtime_proxy.blocked
    report['status']='passed'
except BaseException as error:
    report['status']='failed';report['error']=repr(error)
    raise
finally:
    text=json.dumps(report,indent=2,ensure_ascii=False)+'\n'
    (root/'result.json').write_text(text)
    if args.report is not None:args.report.write_text(text)
