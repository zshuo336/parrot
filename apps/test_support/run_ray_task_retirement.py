#!/usr/bin/env python3
"""Real task-worker lifetime, return delivery and deployment retirement."""
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

MODULE = '''import json, os, time
from pathlib import Path
import ray
from parrot_protocol.ray_gw import ParrotDispatcher
from parrot_protocol.ray_snapshot_lease import _WORKER_LEASES
RETURNED=False
def task_setup():os.environ['TASK_HOOK']='child'
class LateValue:
    def __init__(self,value):self.value=value
    def __reduce__(self):
        root=Path(__file__).parent
        if not RETURNED:raise RuntimeError('serialization did not follow function return')
        result=dict(value=self.value+1,root=str(root),pid=os.getpid(),leased=str(root.parent) in _WORKER_LEASES,
            source=(root/'data.txt').read_text(),hook=os.environ.get('TASK_HOOK'),
            worker=ray._private.worker.global_worker.core_worker.get_worker_id().hex(),
            job=ray.get_runtime_context().get_job_id())
        if not result['leased']:raise RuntimeError('source lease lost before serialization')
        return dict,(list(result.items()),)
def finish(value):
    global RETURNED
    RETURNED=False
    try:return LateValue(value)
    finally:RETURNED=True
@ray.remote
def work(value):return finish(value)
@ray.remote(max_calls=5)
def finite(value):return finish(value)
@ray.remote
def failing():
    raise ValueError('expected-task-error:'+(Path(__file__).parent/'data.txt').read_text())
@ray.remote
def stream():
    for value in range(3):yield finish(value)
@ray.remote
def nested(value):return work.remote(value)
@ray.remote(max_retries=1,retry_exceptions=True)
def retrying():
    signals=Path(os.environ['TASK_SIGNALS'])
    with (signals/'retry-attempts').open('a') as output:output.write(str(os.getpid())+'\\n')
    try:descriptor=os.open(signals/'retry-first',os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)
    except FileExistsError:return finish(80)
    else:
        os.close(descriptor)
        raise ValueError('retry this attempt')
@ray.remote
def cancellable():
    signals=Path(os.environ['TASK_SIGNALS']);(signals/'cancel-entered').touch()
    while True:time.sleep(.01)
@ray.remote
def blocked():
    signals=Path(os.environ['TASK_SIGNALS']);(signals/'entered').touch()
    deadline=time.monotonic()+20
    while not (signals/'release').exists():
        if time.monotonic()>deadline:raise TimeoutError('parent did not release task')
        time.sleep(.01)
    return finish(40)
@ray.remote
class Native:
    def __init__(self,creator):self.creator=creator
    def ask(self,key,payload):
        root=Path(__file__).parent
        value=dict(value=int(payload)+1,root=str(root),leased=str(root.parent) in _WORKER_LEASES,
            source=(root/'data.txt').read_text(),creator=self.creator,
            worker=ray._private.worker.global_worker.core_worker.get_worker_id().hex(),
            job=ray.get_runtime_context().get_job_id())
        return key,json.dumps(value).encode()
@ray.remote
def create_native():
    return Native.remote(ray._private.worker.global_worker.core_worker.get_worker_id().hex())
def parrot_entry(ctx):
    if os.environ.get('TASK_FACTORY')=='native':return ray.get(create_native.remote())
    dispatcher=ParrotDispatcher()
    @dispatcher.handler('run')
    def run(key,payload):return key,json.dumps(ray.get(work.remote(int(payload)))).encode()
    @dispatcher.handler('finite')
    def bounded(key,payload):return key,json.dumps(ray.get(finite.remote(int(payload)))).encode()
    @dispatcher.handler('override')
    def override(key,payload):return key,json.dumps(ray.get(work.options(runtime_env={'worker_process_setup_hook':'component.task_setup'}).remote(30))).encode()
    @dispatcher.handler('error')
    def error(key,payload):
        try:ray.get(failing.remote())
        except ValueError as failure:return key,json.dumps(dict(detail=str(failure))).encode()
        raise RuntimeError('task failure swallowed')
    @dispatcher.handler('stream')
    def streaming(key,payload):return key,json.dumps([ray.get(ref) for ref in stream.remote()]).encode()
    @dispatcher.handler('nested')
    def nesting(key,payload):
        ref=ray.get(nested.remote(50))
        return key,json.dumps(ray.get(ref)).encode()
    @dispatcher.handler('retry')
    def retry(key,payload):return key,json.dumps(ray.get(retrying.remote())).encode()
    @dispatcher.handler('cancel')
    def cancel(key,payload):
        ref=cancellable.remote();deadline=time.monotonic()+10
        signals=Path(os.environ['TASK_SIGNALS'])
        while not (signals/'cancel-entered').exists():
            if time.monotonic()>deadline:raise TimeoutError('cancelled task did not start')
            time.sleep(.01)
        ray.cancel(ref,force=False)
        try:ray.get(ref,timeout=10)
        except ray.exceptions.TaskCancelledError:return key,json.dumps(dict(cancelled=True)).encode()
        raise RuntimeError('cancelled task returned success')
    @dispatcher.handler('blocked')
    def blocking(key,payload):return key,json.dumps(ray.get(blocked.remote())).encode()
    return dispatcher
'''
CHILD = r'''import json, os, sys, time
from pathlib import Path
import ray
from parrot_protocol.ray_gw import ParrotDispatcher
from parrot_protocol.ray_hosts import IsolatedRuntime
from parrot_protocol.ray_startup import initialize
root=Path(sys.argv[1]);result=dict(passed=False)
def require(condition,detail):
    if not condition:raise RuntimeError(detail)
def unrelated():
    import ray
    manager=ray._private.worker.global_worker.function_actor_manager
    return dict(value=43,bounded=bool(getattr(manager.get_execution_info,'_parrot_owned_tasks',False)))
try:
    initialize(ray,dict(address='local',num_cpus=1,include_dashboard=False,log_to_driver=False,_temp_dir=str(root/'ray')))
    probe=ray.remote(unrelated)
    worker=ray.remote(IsolatedRuntime).options(num_cpus=0).remote(ParrotDispatcher())
    reply=ray.get(worker.mount_module.remote('app','component',str(root/'source'),{'env_vars':{'TASK_SIGNALS':str(root)}},['/user/app'],'1',None),timeout=30)
    require(reply['ok'],str(reply))
    values=[];observations=[]
    for operation,payload in [('run',b'10'),('run',b'20'),('finite',b'1'),('finite',b'2'),('finite',b'3'),('finite',b'4'),('finite',b'5'),('finite',b'6'),('override',b''),('error',b''),('stream',b''),('nested',b''),('retry',b''),('cancel',b'')]:
        start=time.monotonic()
        reply=ray.get(worker.ask.remote('/user/app',operation,payload),timeout=25)
        require(reply['ok'],str(reply))
        actual=json.loads(reply['payload'])
        observations.append(dict(operation=operation,seconds=time.monotonic()-start,answer=actual))
        if operation=='error':require('expected-task-error:owned' in actual['detail'],'task error was lost')
        elif operation=='cancel':require(actual=={'cancelled':True},'cancellation result lost')
        else:
            items=actual if operation=='stream' else [actual]
            expected=[1,2,3] if operation=='stream' else [31 if operation=='override' else 51 if operation=='nested' else 81 if operation=='retry' else int(payload)+1]
            require([entry['value'] for entry in items]==expected,'task returned wrong values')
            require(all(entry['leased'] and entry['source']=='owned' for entry in items),'serialization lost source lease')
            values.extend(items)
            if operation=='override':require(actual['hook']=='child','task setup override lost')
    result['observations']=observations
    require(len((root/'retry-attempts').read_text().splitlines())==2,'retry did not run exactly twice')
    require(values[0]['worker']==values[1]['worker'],'ordinary task worker was not reused')
    finite=[entry['answer']['worker'] for entry in observations if entry['operation']=='finite']
    require(finite[0]==finite[1] and finite[4]!=finite[5],'explicit max_calls=5 was not preserved')
    require(len({entry['job'] for entry in values})==1 and values[0]['job']!=ray.get_runtime_context().get_job_id(),'task pool is not in its own job')
    result['job']=values[0]['job']
    result['job_driver']=ray.get(worker.__ray_call__.remote(lambda owner:owner._deployments['app']['job'].process.pid),timeout=5)
    pending=worker.ask.remote('/user/app','blocked',b'')
    deadline=time.monotonic()+15
    while not (root/'entered').exists():
        require(time.monotonic()<deadline,'blocked task did not start');time.sleep(.01)
    retirement=worker.retire_paths.remote('app',['/user/app'])
    try:
        ray.get(retirement,timeout=.2)
        raise RuntimeError('retirement completed while task was blocked')
    except ray.exceptions.GetTimeoutError:pass
    snapshot=Path(values[0]['root'])
    require(snapshot.is_dir(),'active task source was deleted')
    result['blocked_snapshot_preserved']=True
    (root/'release').touch()
    reply=ray.get(pending,timeout=20);require(reply['ok'],str(reply))
    released=json.loads(reply['payload'])
    require(released['value']==41 and released['leased'],'released task result was lost')
    start=time.monotonic()
    result['retired']=ray.get(retirement,timeout=20)
    result['retirement_seconds']=time.monotonic()-start
    result['snapshot_remains']=snapshot.exists()
    result['unrelated']=ray.get(probe.remote(),timeout=10)
    result['caller_alive']=ray.is_initialized()
    try:os.kill(result['job_driver'],0)
    except ProcessLookupError:result['job_driver_exited']=True
    else:result['job_driver_exited']=False
    require(result['job_driver_exited'],'deployment driver survived retirement')
    require(result['retired']=={'/user/app':None} and not result['snapshot_remains'],'task snapshot did not retire')
    require(result['unrelated']=={'value':43,'bounded':False} and result['caller_alive'],'caller job/policy changed')
    reply=ray.get(worker.mount_module.remote('native','component',str(root/'source'),{'env_vars':{'TASK_FACTORY':'native'}},['/user/native'],'1',None),timeout=30)
    require(reply['ok'],str(reply))
    native=[]
    for value in [60,70]:
        reply=ray.get(worker.ask.remote('/user/native','run',str(value).encode()),timeout=20)
        require(reply['ok'],str(reply))
        answer=json.loads(reply['payload']);native.append(answer)
        require(answer['value']==value+1 and answer['leased'] and answer['source']=='owned','task-created actor lost its source/result')
    require(native[0]['worker']==native[1]['worker'],'actor worker was incorrectly bounded like a task')
    result['actor_from_task']=native
    require(native[0]['job']!=result['job'] and native[0]['job']!=ray.get_runtime_context().get_job_id(),'separate deployments shared a job')
    result['native_retired']=ray.get(worker.retire_paths.remote('native',['/user/native']),timeout=20)
    require(result['native_retired']=={'/user/native':None} and not Path(native[0]['root']).exists(),'task-created actor/creator did not retire')
    result['passed']=True
except BaseException as error:
    result['error']=str(error)
    raise
finally:
    (root/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    ray.shutdown()
'''


def run():
    root=Path(tempfile.mkdtemp(prefix='prtask-',dir='/tmp'))
    print('RAY_TASK_RESULTS='+str(root),flush=True)
    (root/'source').mkdir();(root/'source/component.py').write_text(MODULE)
    (root/'source/data.txt').write_text('owned');(root/'child.py').write_text(CHILD)
    env=dict(os.environ,PYTHONPATH=str(ROOT/'interop/python'),RAY_AUTH_TOKEN=secrets.token_hex(32),RAY_USAGE_STATS_ENABLED='0')
    env.pop('RAY_AUTH_TOKEN_PATH',None);env.pop('RAY_ADDRESS',None)
    with (root/'gateway.log').open('w') as log:
        with OwnedProcess([sys.executable,str(root/'child.py'),str(root)],env=env,stdout=log,stderr=subprocess.STDOUT) as process:
            status=process.wait(timeout=120)
            result=json.loads((root/'result.json').read_text())
    require(status==0 and result['passed'],'task worker retirement failed: '+json.dumps(result))
    print('RAY_TASK_OK results delivered, snapshot retired, caller job preserved',flush=True)


if __name__=='__main__':run()
