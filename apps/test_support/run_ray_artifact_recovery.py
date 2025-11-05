#!/usr/bin/env python3
"""Recover registered snapshots after coordinator or driver process death."""
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from owned_process import OwnedProcess, process_exited
from run_ray_lifecycle import ROOT
from run_ray_shutdown import require, wait_until
from parrot_protocol import ray_artifacts

MODULE = '''import json, os
from pathlib import Path
import ray
from parrot_protocol.ray_snapshot_lease import _WORKER_LEASES
def setup_probe():
    with open(os.environ['PARROT_HOOK_RECEIPTS'],'a') as out:
        out.write(json.dumps(dict(pid=os.getpid(),leased=bool(_WORKER_LEASES)))+'\\n')
@ray.remote
class Native:
    def __init__(self, root, host_leased): self.root,self.host_leased=root,host_leased
    def ask(self,key,payload):
        return key,json.dumps(dict(root=self.root,pid=os.getpid(),host_leased=self.host_leased,
            native_leased=str(Path(self.root).parent) in _WORKER_LEASES)).encode()
def parrot_entry(ctx):
    root=Path(__file__).parent
    return Native.remote(str(root),str(root.parent) in _WORKER_LEASES)
'''

CHILD = r'''import json, os, sys, time
from pathlib import Path
import ray
from parrot_protocol.ray_gw import ParrotDispatcher
from parrot_protocol.ray_hosts import IsolatedRuntime
from parrot_protocol.ray_startup import initialize
directory=Path(sys.argv[1]);mode=sys.argv[2]
initialize(ray,dict(address='local',num_cpus=2,include_dashboard=False,
    log_to_driver=False,_temp_dir=str(directory/'ray')))
worker=ray.remote(IsolatedRuntime).options(num_cpus=0).remote(ParrotDispatcher())
env=dict(worker_process_setup_hook='component.setup_probe',env_vars={'PARROT_HOOK_RECEIPTS':str(directory/'hooks.jsonl')})
reply=ray.get(worker.mount_module.remote('app','component',str(directory/'source'),env,['/user/app'],'1',None))
if not reply['ok']:raise RuntimeError(repr(reply))
reply=ray.get(worker.ask.remote('/user/app','read',b''))
metadata=json.loads(reply['payload'])
metadata['job_driver']=ray.get(worker.__ray_call__.remote(lambda owner:owner._deployments['app']['job'].process.pid))
metadata['caller_job']=ray.get_runtime_context().get_job_id()
@ray.remote
class Unrelated:
    def read(self):return 43
unrelated=Unrelated.remote()
if ray.get(unrelated.read.remote())!=43:raise RuntimeError('unrelated actor did not start')
(directory/'ready.tmp').write_text(json.dumps(metadata));(directory/'ready.tmp').replace(directory/'ready.json')
deadline=time.monotonic()+20
while not (directory/'crash').exists():
    if time.monotonic()>deadline:raise TimeoutError('parent did not trigger failure')
    time.sleep(.01)
if mode=='coordinator':
    ray.kill(worker,no_restart=True)
    deadline=time.monotonic()+5
    while True:
        remaining=deadline-time.monotonic()
        if remaining<=0:raise RuntimeError('coordinator death was not confirmed')
        try:ray.get(worker.ready.remote(),timeout=remaining)
        except ray.exceptions.ActorDiedError:break
        time.sleep(.01)
    (directory/'coordinator-dead').touch()
    deadline=time.monotonic()+30
    while not (directory/'recovered').exists():
        if time.monotonic()>deadline:raise TimeoutError('parent did not confirm recovery')
        time.sleep(.01)
    value=ray.get(unrelated.read.remote(),timeout=5)
    alive=dict(value=value,initialized=ray.is_initialized(),job=ray.get_runtime_context().get_job_id())
    if value!=43 or alive['job']!=metadata['caller_job']:raise RuntimeError('caller job was not preserved')
    (directory/'caller-alive.json').write_text(json.dumps(alive))
    ray.shutdown()
# No Python finally/atexit can run here; the driver case also bypasses shutdown.
os._exit(23)
'''


def run(mode):
    directory=Path(tempfile.mkdtemp(prefix='prrec-',dir='/tmp'))
    print('RAY_RECOVERY_RESULTS='+str(directory),flush=True)
    source=directory/'source';source.mkdir();(source/'component.py').write_text(MODULE)
    script=directory/'child.py';script.write_text(CHILD)
    result=dict(mode=mode,passed=False);snapshot=None
    sentinel=ray_artifacts.Snapshot(source)
    env=dict(os.environ,PYTHONPATH=str(ROOT/'interop/python'),RAY_AUTH_TOKEN=secrets.token_hex(32),
             RAY_USAGE_STATS_ENABLED='0')
    env.pop('RAY_AUTH_TOKEN_PATH',None);env.pop('RAY_ADDRESS',None)
    collect=getattr(ray_artifacts,'collect_orphaned_snapshots',lambda:0)
    with (directory/'gateway.log').open('w') as log:
        with OwnedProcess([sys.executable,str(script),str(directory),mode],env=env,
            stdout=log,stderr=subprocess.STDOUT) as child:
            try:
                wait_until(lambda:(directory/'ready.json').exists(),child)
                metadata=json.loads((directory/'ready.json').read_text())
                result['job_driver']=metadata['job_driver']
                result['caller_job']=metadata['caller_job']
                snapshot=Path(metadata['root'])
                result['snapshot']=str(snapshot)
                require(metadata['host_leased'] and metadata['native_leased'],'host/native setup hooks did not acquire leases')
                hooks=[json.loads(line) for line in (directory/'hooks.jsonl').read_text().splitlines()]
                require(len({entry['pid'] for entry in hooks})>=2 and all(entry['leased'] for entry in hooks),
                        'user setup hook ran before lease acquisition or was lost')
                result['setup_hooks']=hooks
                require(snapshot.is_dir(),'snapshot missing while deployment is live')
                collect()
                require(snapshot.is_dir(),'recovery removed a live deployment')
                (directory/'crash').touch()
                if mode=='coordinator':
                    wait_until(lambda:(directory/'coordinator-dead').exists(),child)
                else:
                    result['driver_exit']=child.wait(timeout=20)
                    require(result['driver_exit']==23,'fault injection did not execute')
                def driver_gone():
                    return process_exited(metadata['job_driver'])
                deadline=time.monotonic()+15
                while (snapshot.exists() or not driver_gone()) and time.monotonic()<deadline:
                    collect()
                    require(Path(sentinel.root).is_dir(),'recovery removed unrelated live snapshot')
                    if mode=='coordinator':require(child.poll() is None,'caller exited before recovery')
                    time.sleep(.05)
                result['snapshot_remains']=snapshot.exists()
                result['job_driver_exited']=driver_gone()
                require(result['job_driver_exited'],'independent deployment driver survived owner death')
                require(not snapshot.exists(),'orphan snapshot remains after process death and recovery')
                require(source.is_dir() and Path(sentinel.root).is_dir(),'recovery removed caller-owned/live files')
                if mode=='coordinator':
                    (directory/'recovered').touch()
                    result['driver_exit']=child.wait(timeout=10)
                    require(result['driver_exit']==23,'coordinator scenario did not finish')
                    result['caller_alive']=json.loads((directory/'caller-alive.json').read_text())
                    require(result['caller_alive']['initialized'],'caller job died during recovery')
                result['passed']=True
            finally:
                (directory/'result.json').write_text(json.dumps(result,indent=2)+'\n')
                child.close()
                sentinel.close()
                collect()
                # Baseline snapshots predate registration. Only this fixture's
                # exact captured directory is removed, after the owned group dies.
                if not hasattr(ray_artifacts,'collect_orphaned_snapshots') and snapshot is not None and snapshot.exists():
                    shutil.rmtree(snapshot)
    print('RAY_RECOVERY_OK '+mode+': dead-owner snapshot removed, live snapshot and source preserved',flush=True)


if __name__=='__main__':
    for mode in ['coordinator','driver']:run(mode)
