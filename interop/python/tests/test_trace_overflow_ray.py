"""A full recorder cannot make native async business safe to kill early."""
import importlib
import json
import os
from pathlib import Path
import sys
import threading
import time

import pytest
import ray

from parrot_protocol.ray_gw import ParrotDispatcher, _kill_and_confirm
from parrot_protocol.ray_hosts import IsolatedRuntime
from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig
from ray_test_cluster import owned_ray_cluster
from test_trace_export import collector, wait_for


SOURCE = '''
import asyncio
import json
import os
import ray
from parrot_protocol.trace import AsyncTraceActor, capture_parent

class Overflow(AsyncTraceActor):
    def __init__(self):
        self.entered = False
        self.gate = asyncio.Event()
    async def ask(self, key, payload):
        parent = capture_parent()
        if key == 'hold':
            self.entered = True
            await asyncio.wait_for(self.gate.wait(), timeout=30)
        return key, json.dumps(dict(pid=os.getpid(),
            namespace=ray.get_runtime_context().namespace,
            parent=None if parent is None else parent.hex(),
            completed=True)).encode()
    async def probe(self):
        return self.entered
    async def release(self):
        self.gate.set()
        return True

def parrot_entry(ctx):
    return ray.remote(Overflow).options(num_cpus=0, name='overflow-owner').remote()
'''


def test_real_native_retirement_retains_unrecorded_business_until_scope_exit(tmp_path):
    configured=os.environ.get('PARROT_OVERFLOW_RAY_EVIDENCE')
    root=Path(configured) if configured else tmp_path
    root.mkdir(parents=True,exist_ok=True)
    source=root/'source';source.mkdir()
    (source/'overflow_fixture.py').write_text(SOURCE)
    sys.path.insert(0,str(source))
    try:
        importlib.invalidate_caches();importlib.import_module('overflow_fixture')
        with owned_ray_cluster(ray,evidence=root/'cluster'),collector() as sink:
            sink.block=threading.Event()
            worker=None;actor=None;held=None
            try:
                config=TraceConfig(capacity=1,export=TraceExportConfig(sink.endpoint,
                    batch_size=1,interval=0.01,request_timeout=5))
                worker=ray.remote(IsolatedRuntime).options(num_cpus=0).remote(
                    ParrotDispatcher(),trace_config=config)
                assert ray.get(worker.ready.remote(),timeout=30)
                receipt=ray.get(worker.mount_module.remote('overflow','overflow_fixture',str(source),
                    {'config':{'setup_timeout_seconds':60}},['/user/overflow'],'1',{}),timeout=120)
                assert receipt==dict(ok=True,paths=['/user/overflow'])
                result=ray.get(worker.ask.remote('/user/overflow','first',b''),timeout=20)
                assert result['ok']
                info=json.loads(result['payload'])
                actor=ray.get_actor('overflow-owner',namespace=info['namespace'])
                assert info['pid']!=os.getpid() and info['parent'] is not None
                assert sink.received.wait(5)
                held=actor.ask_traced.remote('hold',b'',None)
                wait_for(lambda:ray.get(actor.probe.remote(),timeout=5),timeout=10)
                sink.block.set()
                wait_for(lambda:ray.get(actor.parrot_trace_export_status.remote(),timeout=5)['exported']==1)
                first=ray.get(worker.retire_paths.remote('overflow',['/user/overflow'],100),timeout=10)
                (root/'first-retirement.json').write_text(json.dumps(first))
                assert first=={'/user/overflow':'drain_failed'},first
                status=ray.get(actor.parrot_trace_export_status.remote(),timeout=5)
                assert status['active']==1 and status['running'] and status['pending']==0
                assert status['dropped']==1 and status['closed']
                assert ray.get(actor.release.remote(),timeout=5) is True
                key,body=ray.get(held,timeout=10)
                value=json.loads(body)
                assert key=='hold' and value['completed'] and value['parent'] is None
                assert value['pid']==info['pid']
                second=ray.get(worker.retire_paths.remote('overflow',['/user/overflow'],5000),timeout=15)
                assert second=={'/user/overflow':None}
                with pytest.raises(ray.exceptions.ActorDiedError):
                    ray.get(actor.__ray_ready__.remote(),timeout=5)
                assert len(sink.spans())==1
                (root/'documents.json').write_text(json.dumps(sink.documents(),indent=2))
                (root/'receipt.json').write_text(json.dumps(dict(first=first,retained=status,
                    business=value,second=second,actor_dead=True),indent=2))
            finally:
                sink.block.set()
                if actor is not None:
                    try:
                        ray.get(actor.release.remote(),timeout=5)
                        if held is not None:ray.get(held,timeout=10)
                    except ray.exceptions.ActorDiedError:
                        pass
                if worker is not None:
                    receipt=ray.get(worker.close.remote(),timeout=30)
                    (root/'retirement.json').write_text(json.dumps(receipt))
                    assert receipt==dict(ok=True,pending=[])
                    _kill_and_confirm(ray,worker,worker.ready,time.monotonic()+10)
    finally:
        sys.path.remove(str(source));sys.modules.pop('overflow_fixture',None)
