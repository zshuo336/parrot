#!/usr/bin/env python3
"""Real job/actor setup-hook inheritance with snapshot lease ordering."""
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import tempfile

from owned_process import OwnedProcess
from run_ray_lifecycle import ROOT
from run_ray_shutdown import require

HOOKS = '''import json, os
from pathlib import Path
from parrot_protocol.ray_snapshot_lease import _WORKER_LEASES
def record(name):
    control=os.environ.get('PARROT_SNAPSHOT_CONTROL')
    with open(os.environ['HOOK_RECEIPTS'],'a') as output:
        output.write(json.dumps(dict(name=name,pid=os.getpid(),leased=control in _WORKER_LEASES))+'\\n')
    os.environ['ACTUAL_SETUP_HOOK']=name
    if os.environ.get('HOOK_CASE')=='failing':raise RuntimeError('injected inherited setup failure')
def job_setup():record('job')
'''
MODULE = '''import json, os
from pathlib import Path
import ray
from hooks import record
from parrot_protocol.ray_snapshot_lease import _WORKER_LEASES
def deployment_setup():record('deployment')
def native_setup():record('native')
def state():
    return dict(pid=os.getpid(),hook=os.environ.get('ACTUAL_SETUP_HOOK'),
        leased=str(Path(__file__).parent.parent) in _WORKER_LEASES,extra=os.environ.get('NATIVE_EXTRA'))
class NativeBase:
    def __init__(self,host):self.host=host
    def ask(self,key,payload):return key,json.dumps(dict(host=self.host,native=state())).encode()
Native=ray.remote(NativeBase)
@ray.remote(runtime_env={'worker_process_setup_hook':'component.native_setup'})
class Decorated(NativeBase):pass
def parrot_entry(ctx):
    with open(os.environ['FACTORY_EFFECTS'],'a') as output:output.write(os.environ['HOOK_CASE']+'\\n')
    mode=os.environ['HOOK_CASE']
    if mode=='decorator':return Decorated.remote(state())
    options={}
    if mode=='env-only':options=dict(runtime_env=dict(env_vars={'NATIVE_EXTRA':'yes'}))
    if mode=='override':options=dict(runtime_env=dict(worker_process_setup_hook='component.native_setup',env_vars={'NATIVE_EXTRA':'yes'}))
    return Native.options(**options).remote(state())
'''
CHILD = r'''import json, os, sys
from pathlib import Path
import ray
from hooks import job_setup
from parrot_protocol.ray_gw import ParrotDispatcher
from parrot_protocol.ray_hosts import IsolatedRuntime
from parrot_protocol.ray_startup import initialize
root=Path(sys.argv[1]);kind=sys.argv[2]
if kind=='callable':
    # Deliberately use a cloudpickled closure, not an importable module hook.
    label='job'
    def configure():
        from hooks import record
        record(label)
    job_hook=configure
else:job_hook='hooks.job_setup'
results=[]
try:
    initialize(ray,dict(address='local',num_cpus=2,include_dashboard=False,log_to_driver=False,
        _temp_dir=str(root/'ray'),runtime_env=dict(worker_process_setup_hook=job_hook,
        env_vars={'HOOK_RECEIPTS':str(root/'hooks.jsonl'),'FACTORY_EFFECTS':str(root/'factory.jsonl')})))
    worker=ray.remote(IsolatedRuntime).options(num_cpus=0).remote(ParrotDispatcher())
    for case in ['inherited','env-only','override','decorator','disabled','failing']:
        env=dict(env_vars={'HOOK_CASE':case})
        if case=='override':env['worker_process_setup_hook']='component.deployment_setup'
        if case=='disabled':env['worker_process_setup_hook']=''
        reply=ray.get(worker.mount_module.remote(case,'component',str(root/'source'),env,['/user/'+case],'1',None),timeout=30)
        if case=='failing':
            retired=ray.get(worker.retire_paths.remote(case,['/user/'+case]),timeout=20)
            effects=(root/'factory.jsonl').read_text().splitlines()
            errors=[]
            if reply['ok']:errors.append('failed hook was accepted')
            if case in effects:errors.append('factory ran after failed setup hook')
            if retired!={'/user/'+case:None}:errors.append('dead host retirement failed: '+repr(retired))
            results.append(dict(case=case,reply=reply,retired=retired,errors=errors,passed=not errors))
            continue
        if not reply['ok']:raise RuntimeError(str(reply))
        answer=ray.get(worker.ask.remote('/user/'+case,'read',b''),timeout=20)
        if not answer['ok']:raise RuntimeError(str(answer))
        value=json.loads(answer['payload'])
        expected={'host':'deployment','native':'native'} if case=='override' else {'host':'job','native':'job'}
        if case=='decorator':expected['native']='native'
        if case=='disabled':expected={'host':None,'native':None}
        events=[json.loads(line) for line in (root/'hooks.jsonl').read_text().splitlines()]
        errors=[]
        for role in ['host','native']:
            actual=value[role]
            if actual['hook']!=expected[role]:errors.append(role+' hook: '+repr(actual['hook'])+' != '+expected[role])
            if not actual['leased']:errors.append(role+' has no snapshot lease')
            calls=[event for event in events if event['pid']==actual['pid']]
            required=[] if expected[role] is None else [dict(name=expected[role],pid=actual['pid'],leased=True)]
            if calls!=required:errors.append(role+' hook count/order: '+repr(calls))
        if case in ['env-only','override'] and value['native']['extra']!='yes':errors.append('native environment override lost')
        retired=ray.get(worker.retire_paths.remote(case,['/user/'+case]),timeout=20)
        if retired!={'/user/'+case:None}:errors.append('retirement failed: '+repr(retired))
        results.append(dict(case=case,observed=value,errors=errors,passed=not errors))
    close=ray.get(worker.close.remote(),timeout=20)
    if close!={'ok':True,'pending':[]}:raise RuntimeError(str(close))
finally:
    (root/'result.json').write_text(json.dumps(dict(kind=kind,cases=results),indent=2)+'\n')
    ray.shutdown()
if len(results)!=6 or any(not result['passed'] for result in results):raise SystemExit(1)
'''


def run(kind):
    root=Path(tempfile.mkdtemp(prefix='prhook-',dir='/tmp'))
    print('RAY_HOOK_RESULTS='+str(root),flush=True)
    (root/'source').mkdir();(root/'source/component.py').write_text(MODULE)
    (root/'hooks.py').write_text(HOOKS);(root/'child.py').write_text(CHILD)
    env=dict(os.environ,PYTHONPATH=os.pathsep.join([str(root),str(ROOT/'interop/python')]),
        RAY_AUTH_TOKEN=secrets.token_hex(32),RAY_USAGE_STATS_ENABLED='0')
    env.pop('RAY_AUTH_TOKEN_PATH',None);env.pop('RAY_ADDRESS',None)
    with (root/'gateway.log').open('w') as log:
        with OwnedProcess([sys.executable,str(root/'child.py'),str(root),kind],env=env,
            stdout=log,stderr=subprocess.STDOUT) as process:
            status=process.wait(timeout=180)
            result=json.loads((root/'result.json').read_text())
            passed=status==0 and len(result['cases'])==6 and all(case['passed'] for case in result['cases'])
            (root/'parent-result.json').write_text(json.dumps(dict(exit=status,passed=passed))+'\n')
    if passed:print('RAY_HOOK_OK '+kind+': inherited job hook, native overrides, setup failure, leases and retirement',flush=True)
    else:print('RAY_HOOK_FAILED '+kind+': '+json.dumps(result),flush=True)
    return passed


if __name__=='__main__':
    outcomes=[run(kind) for kind in ['import','callable']]
    require(all(outcomes),'setup hook inheritance contract failed; inspect recorded results')
