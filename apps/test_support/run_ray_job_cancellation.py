#!/usr/bin/env python3
"""Real Ray jobs: owner death during native init, and native shutdown stalls."""
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

# Fault injection is confined to this fixture's Python startup path. The module
# and production subprocess command are unchanged. PyDLL deliberately holds the
# GIL: a watchdog thread in the Ray driver's interpreter cannot resolve the stall.
SITE = '''import sys
if sys.argv[-1:] == ['--driver']:
    import ctypes, json, os
    from pathlib import Path
    import ray
    from parrot_protocol.ray_snapshot_lease import _WORKER_LEASES
    mode=os.environ['PARROT_TEST_JOB_STALL'].removeprefix('guard-')
    original_init,original_shutdown=ray.init,ray.shutdown
    def stall():
        record=dict(driver=os.getpid(),guard=os.getppid(),job=ray.get_runtime_context().get_job_id(),
                    controls=list(_WORKER_LEASES),phase=mode)
        target=Path(os.environ['PARROT_TEST_STALL_RECORD'])
        target.with_suffix('.tmp').write_text(json.dumps(record));target.with_suffix('.tmp').replace(target)
        ctypes.PyDLL(None).sleep(60)
        raise RuntimeError('native stall was not cancelled')
    def initialize(*args,**kwargs):
        value=original_init(*args,**kwargs)
        if mode=='init':stall()
        return value
    def shutdown(*args,**kwargs):
        if mode=='shutdown' and ray.is_initialized():stall()
        return original_shutdown(*args,**kwargs)
    ray.init,ray.shutdown=initialize,shutdown
'''

MODULE = '''from parrot_protocol.ray_gw import ParrotDispatcher
def parrot_entry(ctx):return ParrotDispatcher()
'''

CHILD = r'''import json,os,signal,sys,time
from pathlib import Path
sys.path.insert(0,sys.argv[3])
from owned_process import process_exited as exited
import ray
from parrot_protocol.ray_startup import initialize
from parrot_protocol.ray_gw import ParrotDispatcher
from parrot_protocol.ray_hosts import IsolatedRuntime
from parrot_protocol.ray_artifacts import Snapshot,collect_orphaned_snapshots
root=Path(sys.argv[1]);mode=sys.argv[2];result=dict(passed=False,mode=mode)
phase=mode.removeprefix('guard-')
def require(condition,detail):
    if not condition:raise RuntimeError(detail)
sentinel=Snapshot(root/'source')
try:
    initialize(ray,dict(address='local',num_cpus=2,include_dashboard=False,log_to_driver=False,_temp_dir=str(root/'ray')))
    @ray.remote(num_cpus=0)
    class Unrelated:
        def read(self):return 43
    unrelated=Unrelated.remote()
    require(ray.get(unrelated.read.remote())==43,'unrelated actor not ready')
    caller_job=ray.get_runtime_context().get_job_id()
    worker=ray.remote(IsolatedRuntime).options(num_cpus=0).remote(ParrotDispatcher())
    mount=worker.mount_module.remote('app','component',str(root/'source'),{},['/user/app'],'1',None)
    if phase=='shutdown':
        reply=ray.get(mount,timeout=30);require(reply['ok'],repr(reply))
        retirement=worker.retire_paths.remote('app',['/user/app'])
    deadline=time.monotonic()+20
    while not (root/'stalled.json').exists():
        if time.monotonic()>deadline:raise TimeoutError('native stall was not reached')
        time.sleep(.01)
    record=json.loads((root/'stalled.json').read_text());result['stalled']=record
    require(len(record['controls'])==1,'driver has no unique source lease')
    control=Path(record['controls'][0]);snapshot=control/'payload'
    require(snapshot.is_dir(),'source missing at native stall')
    require(record['job']!=caller_job,'deployment used caller job')
    start=time.monotonic()
    if mode.startswith('guard-'):
        os.kill(record['guard'],signal.SIGKILL)
        deadline=time.monotonic()+5
        while not exited(record['driver']):
            if time.monotonic()>deadline:raise TimeoutError('native driver survived guard death')
            time.sleep(.01)
        if phase=='init':
            reply=ray.get(mount,timeout=5)
            require(not reply['ok'],'failed driver startup was reported as success')
            retirement=worker.retire_paths.remote('app',['/user/app'])
        reply=ray.get(retirement,timeout=10)
        require(reply=={'/user/app':None},repr(reply));result['retired']=reply
    elif mode=='init':
        collect_orphaned_snapshots()
        require(snapshot.is_dir(),'live startup lease was ignored')
        ray.kill(worker,no_restart=True)
        try:ray.get(mount,timeout=5)
        except ray.exceptions.ActorDiedError:pass
        else:raise RuntimeError('coordinator survived injected death')
    else:
        reply=ray.get(retirement,timeout=10)
        require(reply=={'/user/app':None},repr(reply));result['retired']=reply
    deadline=time.monotonic()+10
    while snapshot.exists() or not exited(record['driver']) or not exited(record['guard']):
        if time.monotonic()>deadline:raise TimeoutError('owned driver/guard/source survived cancellation')
        collect_orphaned_snapshots()
        require(Path(sentinel.root).is_dir(),'unrelated live snapshot removed')
        time.sleep(.01)
    result.update(seconds=time.monotonic()-start,driver_exited=True,guard_exited=True,snapshot_removed=True,
                  unrelated=ray.get(unrelated.read.remote(),timeout=5),caller_job=ray.get_runtime_context().get_job_id())
    require(result['unrelated']==43 and result['caller_job']==caller_job and ray.is_initialized(),'caller job affected')
    require((root/'source/component.py').is_file() and Path(sentinel.root).is_dir(),'caller source/sentinel removed')
    result['passed']=True
except BaseException as error:
    result['error']=str(error)
    raise
finally:
    (root/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    ray.shutdown();sentinel.close();collect_orphaned_snapshots()
'''


def run(mode):
    root=Path(tempfile.mkdtemp(prefix='prcancel-',dir='/tmp'))
    print('RAY_JOB_CANCEL_RESULTS='+str(root),flush=True)
    (root/'source').mkdir();(root/'source/component.py').write_text(MODULE)
    (root/'injection').mkdir();(root/'injection/sitecustomize.py').write_text(SITE)
    (root/'child.py').write_text(CHILD)
    env=dict(os.environ,PYTHONPATH=os.pathsep.join([str(root/'injection'),str(ROOT/'interop/python')]),
             RAY_AUTH_TOKEN=secrets.token_hex(32),RAY_USAGE_STATS_ENABLED='0',
             PARROT_TEST_JOB_STALL=mode,PARROT_TEST_STALL_RECORD=str(root/'stalled.json'))
    env.pop('RAY_AUTH_TOKEN_PATH',None);env.pop('RAY_ADDRESS',None)
    with (root/'gateway.log').open('w') as log:
        with OwnedProcess([sys.executable,str(root/'child.py'),str(root),mode,str(Path(__file__).resolve().parent)],env=env,
                                 stdout=log,stderr=subprocess.STDOUT) as process:
            status=process.wait(timeout=90)
            result=json.loads((root/'result.json').read_text())
    require(status==0 and result['passed'],'job cancellation failed: '+repr(result))
    print('RAY_JOB_CANCEL_OK '+mode+': native stall retired, caller and live snapshot preserved',flush=True)


if __name__=='__main__':
    for mode in ['init','shutdown','guard-init','guard-shutdown']:run(mode)
