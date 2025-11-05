#!/usr/bin/env python3
"""Real deployment drivers retain bounded useful diagnostics and retire readers."""
import json
import os
from pathlib import Path
import secrets
import sys
import tempfile
import time

from run_ray_lifecycle import ROOT
from run_ray_shutdown import require

sys.path.insert(0, str(ROOT / "tools/parrot-obs"))
from process_owner import OwnedChild

SITE = '''import sys
if sys.argv[-1:] == ['--driver']:
    import os,ray
    original_init,original_shutdown=ray.init,ray.shutdown
    def write(fd,data):
        while data:data=data[os.write(fd,data):]
    def initialize(*args,**kwargs):
        value=original_init(*args,**kwargs)
        for _ in range(32):write(1,b'x'*65536)
        write(2,b'PARROT_DRIVER_DIAGNOSTIC_END\\n')
        if os.environ['PARROT_TEST_JOB_OUTPUT']=='startup-error':
            raise RuntimeError('PARROT_DRIVER_STARTUP_ERROR')
        return value
    def shutdown(*args,**kwargs):
        active=ray.is_initialized()
        result=original_shutdown(*args,**kwargs)
        if active:write(2,b'PARROT_DRIVER_RETIRED\\n')
        return result
    ray.init,ray.shutdown=initialize,shutdown
'''

CHILD = r'''import json,os,sys,time
from pathlib import Path
import ray
from parrot_protocol.ray_startup import initialize
from parrot_protocol.ray_artifacts import Snapshot,collect_orphaned_snapshots
from parrot_protocol.ray_environments import host_environment
from parrot_protocol.ray_job import DeploymentJob
from parrot_protocol.job_output import MAX_OUTPUT_BYTES
root=Path(sys.argv[1]);mode=sys.argv[2];result=dict(passed=False,mode=mode)
def require(condition,detail):
    if not condition:raise RuntimeError(detail)
snapshot=Snapshot(root/'source');job=None;output=None
try:
    initialize(ray,dict(address='local',num_cpus=2,include_dashboard=False,log_to_driver=False,_temp_dir=str(root/'ray')))
    @ray.remote(num_cpus=0)
    class Unrelated:
        def read(self):return 43
    unrelated=Unrelated.remote()
    require(ray.get(unrelated.read.remote())==43,'unrelated actor not ready')
    context=ray.get_runtime_context();caller_job=context.get_job_id()
    import parrot_protocol
    environment=host_environment({},snapshot.root,str(Path(parrot_protocol.__file__).resolve().parent.parent),
                                 lease=str(snapshot._lease.path),inherited_env=context.runtime_env)
    job=DeploymentJob(ray,context,snapshot.root,environment)
    if mode=='startup-error':
        try:job.start()
        except RuntimeError as error:
            require('exited before startup receipt' in str(error),repr(error))
        else:raise RuntimeError('driver startup error was accepted')
    else:
        host=job.start()
        require(ray.get(host.ready.remote(),timeout=20) is True,'deployment host not ready')
        require(job.job_id!=caller_job,'deployment used caller job')
    output=job.log
    deadline=time.monotonic()+5
    while 'PARROT_DRIVER_DIAGNOSTIC_END' not in output.tail(MAX_OUTPUT_BYTES):
        if time.monotonic()>deadline:raise TimeoutError('final diagnostic was not captured')
        time.sleep(.01)
    require(output.bytes_received>2*1024*1024,'driver did not emit the full workload')
    require(output.retained_bytes==MAX_OUTPUT_BYTES,'retention is not exactly the bounded tail')
    job.close()
    require(job.log is None and output.closed,'diagnostic reader survived retirement')
    require('PARROT_DRIVER_RETIRED' in output.tail(MAX_OUTPUT_BYTES),'shutdown diagnostic lost')
    require(job.process.poll()==(0 if mode=='success' else 1),'unexpected guard terminal status')
    snapshot_path=Path(snapshot.root);snapshot.close()
    require(not snapshot_path.exists(),'retired job still leases its snapshot')
    require(ray.get(unrelated.read.remote(),timeout=5)==43 and ray.is_initialized(),'caller job affected')
    result.update(passed=True,bytes_received=output.bytes_received,retained_bytes=output.retained_bytes,
                  reader_exited=output.closed,guard_exit=job.process.returncode,snapshot_removed=True,
                  unrelated=43,caller_job=caller_job)
except BaseException as error:
    result['error']=str(error)
    raise
finally:
    (root/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    if job is not None:job.close()
    ray.shutdown();snapshot.close();collect_orphaned_snapshots()
'''


def run(mode):
    root=Path(tempfile.mkdtemp(prefix='prjoblog-',dir='/tmp'))
    print('RAY_JOB_OUTPUT_RESULTS='+str(root),flush=True)
    (root/'source').mkdir();(root/'source/component.py').write_text('value=1\n')
    (root/'injection').mkdir();(root/'injection/sitecustomize.py').write_text(SITE)
    (root/'child.py').write_text(CHILD)
    env=dict(os.environ,PYTHONPATH=os.pathsep.join([str(root/'injection'),str(ROOT/'interop/python')]),
             RAY_AUTH_TOKEN=secrets.token_hex(32),RAY_USAGE_STATS_ENABLED='0',PARROT_TEST_JOB_OUTPUT=mode)
    env.pop('RAY_AUTH_TOKEN_PATH',None);env.pop('RAY_ADDRESS',None)
    # Keep an unreaped session anchor until the final group signal. Ray runs
    # inside that anchor so macOS setpgrp cannot escape the owned group.
    owner=OwnedChild('job-output-'+mode,[sys.executable,str(root/'child.py'),str(root),mode],
                     root/'gateway.log',environment=env,python_entrypoint=True)
    try:
        deadline=time.monotonic()+90
        while owner.code is None:
            owner.poll()
            if owner.failure:raise RuntimeError(owner.failure)
            if time.monotonic()>deadline:raise TimeoutError('job output fixture deadline')
            time.sleep(.01)
        status=owner.code
        result=json.loads((root/'result.json').read_text())
    finally:
        owner.begin_stop();owner.finish_stop()
    result['process_owner']=owner.state()
    (root/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    require(status==0 and result['passed'],'job output acceptance failed: '+repr(result))
    print('RAY_JOB_OUTPUT_OK '+mode+': bounded diagnostics, reader/job retired, caller preserved',flush=True)


if __name__=='__main__':
    for mode in ['success','startup-error']:run(mode)
