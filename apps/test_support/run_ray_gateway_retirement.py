#!/usr/bin/env python3
"""Retire gateway actors/artifacts while preserving a caller's live Ray job."""
import json
import os
from pathlib import Path
import re
import secrets
import signal
import subprocess
import sys
import tempfile

from owned_process import OwnedProcess
from run_ray_lifecycle import Client, ROOT, wire
from run_ray_shutdown import require, wait_until

MODULE = '''import json, os, time
from pathlib import Path
import ray
@ray.remote
class Native:
    def __init__(self, root, gate): self.root, self.gate = root, Path(gate)
    def ask(self, key, payload):
        return key, json.dumps(dict(root=self.root, pid=os.getpid(), namespace=ray.get_runtime_context().namespace)).encode()
    def ping(self): return True
    def deliver(self, key, payload):
        self.gate.with_name('entered').touch()
        while not self.gate.exists(): time.sleep(.01)
def parrot_entry(ctx):
    return Native.options(name='owned-'+ctx['path'].rsplit('/',1)[1]).remote(str(Path(__file__).parent), ctx['config'].decode())
'''

CHILD = r'''import json, os, sys, threading, time
from pathlib import Path
import ray
from parrot_protocol.ray_gw import serve, GatewayCleanupError
from parrot_protocol.ray_startup import graceful_sigterm, initialize
directory = Path(sys.argv[1])
mode = sys.argv[2]
def publish(name, value):
    path=directory/(name+'.tmp')
    path.write_text(json.dumps(value));path.replace(directory/name)
with graceful_sigterm():
    initialize(ray, {'address':'local','num_cpus':2,'include_dashboard':False,
                    'log_to_driver':False,'_temp_dir':str(directory/'ray')})
    @ray.remote(num_cpus=0)
    class Unrelated:
        def ping(self): return 42
    unrelated=Unrelated.remote()
    if ray.get(unrelated.ping.remote()) != 42: raise RuntimeError('unrelated actor startup')
    original_remote=ray.remote
    coordinators=[]
    class Builder:
        def __init__(self, inner): self.inner=inner
        def options(self, **kw): self.inner=self.inner.options(**kw);return self
        def remote(self, *args):
            handle=self.inner.remote(*args);coordinators.append(handle);return handle
    def remote(cls, *args, **kw):
        value=original_remote(cls,*args,**kw)
        return Builder(value) if getattr(cls,'__name__','')=='IsolatedRuntime' else value
    ray.remote=remote
    native=[];drivers=[]
    stop_capture=threading.Event()
    def capture():
        try:
            while not (directory/'capture').exists():
                if stop_capture.wait(.01): return
            namespaces=json.loads((directory/'namespaces.json').read_text())
            native.extend(ray.get_actor('owned-pool-'+str(i),namespace=namespaces[i]) for i in range(2))
            for coordinator in coordinators:
                drivers.extend(ray.get(coordinator.__ray_call__.remote(
                    lambda owner:[d['job'].process.pid for d in owner._deployments.values()])))
            publish('captured.json', dict(count=len(native)))
        except BaseException as error:
            publish('captured.json', dict(error=repr(error)))
    thread=threading.Thread(target=capture);thread.start()
    try:
        try:
            serve(0, shutdown_timeout=.2 if mode=='blocked' else 20)
        except (KeyboardInterrupt, GatewayCleanupError) as error:
            retained=sys.exc_info()
            if isinstance(error, GatewayCleanupError):
                if mode != 'blocked': raise
                publish('pending.json', dict(initialized=ray.is_initialized(),
                    unrelated=ray.get(unrelated.ping.remote(),timeout=3),
                    retained_owner=error.owner.worker is not None))
                deadline=time.monotonic()+15
                while not (directory/'release').exists():
                    if time.monotonic()>deadline: raise TimeoutError('parent did not unblock actor')
                    time.sleep(.01)
                error.owner.close(timeout=20)
            elif mode == 'blocked':
                raise RuntimeError('blocked retirement was falsely reported complete')
            dead=[]
            for actor, method in [(a,a.ready) for a in coordinators]+[(a,a.ping) for a in native]:
                try: ray.get(method.remote(),timeout=3)
                except ray.exceptions.ActorDiedError: dead.append(True)
                else: raise RuntimeError('gateway actor survived serve exit')
            for pid in drivers:
                try:os.kill(pid,0)
                except ProcessLookupError:pass
                else:raise RuntimeError('deployment driver survived serve exit')
            publish('retired.json', dict(initialized=ray.is_initialized(),
                    unrelated=ray.get(unrelated.ping.remote(),timeout=3), dead=len(dead), drivers_exited=len(drivers)))
            deadline=time.monotonic()+15
            while not (directory/'continue').exists():
                if time.monotonic()>deadline: raise TimeoutError('parent verification deadline')
                time.sleep(.01)
    finally:
        ray.remote=original_remote
        stop_capture.set();thread.join(5)
        ray.shutdown()
publish('finished.json', dict(initialized=ray.is_initialized()))
'''


def run(mode='normal'):
    directory=Path(tempfile.mkdtemp(prefix='prret-',dir='/tmp'))
    print('RAY_RETIREMENT_RESULTS='+str(directory),flush=True)
    artifact=directory/'artifact';artifact.mkdir()
    (artifact/'component.py').write_text(MODULE)
    script=directory/'child.py';script.write_text(CHILD)
    logpath=directory/'gateway.log'
    env=dict(os.environ,PYTHONPATH=str(ROOT/'interop/python'),RAY_AUTH_TOKEN=secrets.token_hex(32),
             RAY_USAGE_STATS_ENABLED='0',RAY_DISABLE_FAILURE_SIGNAL_HANDLER='0')
    env.pop('RAY_AUTH_TOKEN_PATH',None);env.pop('RAY_ADDRESS',None)
    result=dict(passed=False,natural_exit=None);client=None
    with logpath.open('w') as log:
        with OwnedProcess([sys.executable,str(script),str(directory),mode],env=env,
                stdout=log,stderr=subprocess.STDOUT) as child:
            try:
                match=wait_until(lambda:re.search(r'RAY_GW_PORT=(\d+)\n',logpath.read_text()),child)
                client=Client(int(match.group(1)))
                reply=client.admin('deploy',component=dict(name='pool',version='1',
                    artifact=dict(kind='pymodule',module='component',uri=artifact.as_uri()),
                    instances=dict(kind='pool',count=2),config=str(directory/'release').encode()))
                require(reply['kind']=='deployed',repr(reply))
                metadata=[client.read('/user/pool-'+str(i)) for i in range(2)]
                roots={item['root'] for item in metadata}
                require(roots and all(Path(root).is_dir() for root in roots),'owned snapshots missing before stop')
                (directory/'namespaces.json').write_text(json.dumps([item['namespace'] for item in metadata]))
                (directory/'capture').touch()
                wait_until(lambda:(directory/'captured.json').exists(),child)
                require(json.loads((directory/'captured.json').read_text())=={'count':2},'native capture failed')
                if mode=='blocked':
                    client.send(wire.FT_TELL, '/user/pool-0', 'block', b'')
                    wait_until(lambda:(directory/'entered').exists(),child)
                child.send_signal(signal.SIGTERM)
                if mode=='blocked':
                    wait_until(lambda:(directory/'pending.json').exists(),child,15)
                    pending=json.loads((directory/'pending.json').read_text())
                    require(pending=={'initialized':True,'unrelated':42,'retained_owner':True},repr(pending))
                    require(all(Path(root).is_dir() for root in roots),'uncertain cleanup lost snapshots')
                    result['pending']=pending
                    (directory/'release').touch()
                wait_until(lambda:(directory/'retired.json').exists(),child,30)
                receipt=json.loads((directory/'retired.json').read_text())
                require(receipt=={'initialized':True,'unrelated':42,'dead':3,'drivers_exited':1},repr(receipt))
                require(child.poll() is None,'embedding exited before assertions')
                require(all(not Path(root).exists() for root in roots),'owned snapshot survived gateway retirement')
                require(artifact.is_dir(),'caller source artifact was removed')
                client.sock.settimeout(3);require(client.sock.recv(1)==b'','gateway socket survived retirement')
                result.update(receipt=receipt,retired_snapshots=sorted(roots))
                (directory/'continue').touch()
                result['natural_exit']=child.wait(timeout=20)
                require(result['natural_exit']==0,'embedding shutdown failed')
                require(json.loads((directory/'finished.json').read_text())=={'initialized':False},'caller shutdown missing')
                result['passed']=True
            finally:
                (directory/'result.json').write_text(json.dumps(result,indent=2)+'\n')
                if client is not None:client.sock.close()
                (directory/'release').touch()
                (directory/'continue').touch()
                try:
                    if child.poll() is None:
                        child.send_signal(signal.SIGTERM)
                        try:child.wait(timeout=30)
                        except subprocess.TimeoutExpired:pass
                finally:
                    child.close()
    print('RAY_RETIREMENT_OK '+mode+': coordinator/native actors dead, snapshots removed, caller runtime and unrelated actor alive',flush=True)


if __name__=='__main__':
    for mode in ['normal', 'blocked']:
        run(mode)
