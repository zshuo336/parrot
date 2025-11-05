"""Real TCP -> Ray coordinator -> deployment job -> host -> native actor.

This suite starts its own local Ray cluster; it never attaches to an existing
cluster. Every call is bounded and fixture teardown requires retirement receipts.
"""
import json
import importlib
import os
from pathlib import Path
import sys
import time

import pytest
import ray

from parrot_protocol import wire
from parrot_protocol.ray_gw import ParrotDispatcher, _kill_and_confirm
from parrot_protocol.ray_hosts import IsolatedRuntime
from ray_test_cluster import owned_ray_cluster
from test_causal_context import PARENT, OTHER, ask, gateway_connection


SOURCE = '''
import asyncio
import json
import os
import ray
from parrot_protocol.ray_gw import ParrotDispatcher
from parrot_protocol.trace import TraceActor, AsyncTraceActor, capture_parent

class Business:
    def __init__(self):
        self.events = []
    def ask(self, key, payload):
        parent = capture_parent()
        if key != 'bin:snapshot':
            self.events.append([payload.hex(), None if parent is None else parent.hex()])
        if payload == b'fail':
            raise ValueError('requested business failure')
        context = ray.get_runtime_context()
        return key, json.dumps(dict(parent=None if parent is None else parent.hex(),
            payload=payload.hex(), pid=os.getpid(), job=context.get_job_id(),
            namespace=context.namespace, events=self.events)).encode()
    def deliver(self, key, payload):
        self.ask(key, payload)

class Sync(TraceActor, Business):
    pass

class Async(AsyncTraceActor, Business):
    async def ask(self, key, payload):
        before = capture_parent()
        if key == 'bin:parallel':
            if not hasattr(self, 'gate'):
                self.gate = asyncio.Event()
                self.arrived = 0
            self.arrived += 1
            if self.arrived == 24:
                self.gate.set()
            await asyncio.wait_for(self.gate.wait(), timeout=5)
        await asyncio.sleep(0.02)
        if capture_parent() != before:
            raise AssertionError('parent changed across await')
        return Business.ask(self, key, payload)
    async def deliver(self, key, payload):
        return await self.ask(key, payload)

def parrot_entry(ctx):
    kind = ctx['path'].rsplit('/', 1)[1]
    if kind == 'dispatcher':
        business = Business()
        dispatcher = ParrotDispatcher()
        dispatcher.handler('bin:context')(business.ask)
        dispatcher.handler('bin:snapshot')(business.ask)
        return dispatcher
    cls = {'sync': Sync, 'async': Async, 'plain': Business}[kind]
    return ray.remote(cls).options(num_cpus=0, name='causal-' + kind).remote()
'''


@pytest.fixture(scope='module')
def deployed(tmp_path_factory):
    if ray.is_initialized():
        raise RuntimeError('Ray is already initialized; suite must own its cluster')
    configured = os.environ.get('PARROT_CAUSAL_RAY_EVIDENCE')
    root = Path(configured) if configured is not None else tmp_path_factory.mktemp('causal-ray')
    root.mkdir(parents=True, exist_ok=True)
    source = root / 'source'
    source.mkdir()
    (source / 'causal_fixture.py').write_text(SOURCE)
    worker = None
    native = []
    # Ray resolves named-actor method signatures in the calling process. This
    # direct actor client must have the fixture's class module available too;
    # the gateway itself only needs the deployment-owned snapshot.
    sys.path.insert(0, str(source))
    try:
        importlib.invalidate_caches()
        importlib.import_module('causal_fixture')
        with owned_ray_cluster(ray, evidence=root / 'cluster'):
            try:
                worker = ray.remote(IsolatedRuntime).options(num_cpus=0).remote(ParrotDispatcher())
                assert ray.get(worker.ready.remote(), timeout=30)
                paths = ['/user/' + kind for kind in ('dispatcher', 'sync', 'async', 'plain')]
                receipt = ray.get(worker.mount_module.remote('causal', 'causal_fixture', str(source),
                    {'config': {'setup_timeout_seconds': 60}}, paths, '1', {}), timeout=120)
                assert receipt == dict(ok=True, paths=paths), receipt
                yield ray, worker, native
            finally:
                if worker is not None:
                    receipt = ray.get(worker.close.remote(), timeout=60)
                    (root / 'retirement.json').write_text(json.dumps(receipt))
                    assert receipt == dict(ok=True, pending=[]), receipt
                    for handle in native:
                        with pytest.raises(ray.exceptions.ActorDiedError):
                            ray.get(handle.__ray_ready__.remote(), timeout=10)
                    _kill_and_confirm(ray, worker, worker.ready, time.monotonic() + 10)
    finally:
        sys.path.remove(str(source))
        sys.modules.pop('causal_fixture', None)


def request(peer, receive, cid, kind, payload, parent, key='bin:context'):
    frame = ask(cid, payload, parent)
    frame.path = '/user/' + kind
    frame.type_key = key
    peer.sendall(frame.encode())
    reply = receive()
    assert reply.cid == cid and reply.path == '/reply' and not reply.flags
    return reply


@pytest.mark.parametrize('kind', ['dispatcher', 'sync', 'async'])
def test_tcp_context_reaches_real_deployment_and_clears_after_failure(deployed, kind):
    ray, worker, native = deployed
    with gateway_connection(worker=worker, ray=ray) as (peer, receive, _, _):
        peer.settimeout(20)
        for cid, payload, parent in [(2, b'first', PARENT), (3, b'fail', OTHER),
                                     (4, b'plain', None), (5, b'last', OTHER)]:
            reply = request(peer, receive, cid, kind, payload, parent)
            if payload == b'fail':
                assert reply.ft == wire.FT_REPLY_ERR
            else:
                assert reply.ft == wire.FT_REPLY, reply.payload
                value = json.loads(reply.payload)
                assert value['parent'] == (None if parent is None else parent.encode().hex())
                assert value['payload'] == payload.hex()
                assert value['pid'] != os.getpid()
                assert value['job'] != ray.get_runtime_context().get_job_id()
                assert value['namespace'].startswith('parrot-deployment-')
        assert value['events'][-4:] == [[payload.hex(), None if parent is None else parent.encode().hex()]
            for payload, parent in [(b'first', PARENT), (b'fail', OTHER), (b'plain', None), (b'last', OTHER)]]
        if kind != 'dispatcher':
            native.append(ray.get_actor('causal-' + kind, namespace=value['namespace']))


@pytest.mark.parametrize('kind', ['dispatcher', 'sync', 'async'])
def test_tcp_tell_transfers_parent_without_leaking_to_next_delivery(deployed, kind):
    ray, worker, _ = deployed
    with gateway_connection(worker=worker, ray=ray) as (peer, receive, _, _):
        peer.settimeout(20)
        for payload, parent in [(b'tell-traced', PARENT), (b'tell-plain', None)]:
            frame = wire.Frame(wire.FT_TELL, 0, '/user/' + kind, 'bin:context', payload)
            peer.sendall((frame if parent is None else frame.with_trace_parent(parent)).encode())
        reply = request(peer, receive, 12, kind, b'', None, 'bin:snapshot')
        assert reply.ft == wire.FT_REPLY
        value = json.loads(reply.payload)
        assert value['parent'] is None
        assert value['events'][-2:] == [[b'tell-traced'.hex(), PARENT.encode().hex()], [b'tell-plain'.hex(), None]]


def test_native_actor_without_trace_contract_rejects_before_business_effect(deployed):
    ray, worker, native = deployed
    with gateway_connection(worker=worker, ray=ray) as (peer, receive, _, _):
        peer.settimeout(20)
        reply = request(peer, receive, 20, 'plain', b'must-not-run', PARENT)
        assert reply.ft == wire.FT_REPLY_ERR and wire.decode_err_payload(reply.payload)[0] == 4
        reply = request(peer, receive, 21, 'plain', b'', None, 'bin:snapshot')
        assert reply.ft == wire.FT_REPLY
        value = json.loads(reply.payload)
        assert value['events'] == [] and value['parent'] is None
        native.append(ray.get_actor('causal-plain', namespace=value['namespace']))


def test_concurrent_tcp_requests_keep_their_own_parent_and_correlation(deployed):
    ray, worker, _ = deployed
    with gateway_connection(worker=worker, ray=ray) as (peer, receive, _, _):
        peer.settimeout(20)
        expected = {}
        for cid in range(100, 124):
            parent = (PARENT, OTHER, None)[cid % 3]
            payload = str(cid).encode()
            frame = ask(cid, payload, parent); frame.path = '/user/async'
            peer.sendall(frame.encode())
            expected[cid] = (payload.hex(), None if parent is None else parent.encode().hex())
        for _ in range(24):
            reply = receive()
            assert reply.ft == wire.FT_REPLY and reply.cid in expected
            value = json.loads(reply.payload)
            assert (value['payload'], value['parent']) == expected.pop(reply.cid)
        assert not expected


def test_real_async_actor_simultaneous_tasks_and_exception_restore_context(deployed):
    ray, worker, _ = deployed
    result = ray.get(worker.ask.remote('/user/async', 'bin:snapshot', b''), timeout=20)
    info = json.loads(result['payload'])
    actor = ray.get_actor('causal-async', namespace=info['namespace'])
    refs = [actor.ask_traced.remote('bin:parallel', str(i).encode(), parent)
            for i, parent in enumerate([PARENT.encode(), OTHER.encode(), None] * 8)]
    for i, (_, body) in enumerate(ray.get(refs, timeout=20)):
        value = json.loads(body)
        assert value['payload'] == str(i).encode().hex()
        assert value['parent'] == [PARENT.encode().hex(), OTHER.encode().hex(), None][i % 3]
    with pytest.raises(ray.exceptions.RayTaskError):
        ray.get(actor.ask_traced.remote('bin:context', b'fail', PARENT.encode()), timeout=20)
    _, body = ray.get(actor.ask.remote('bin:snapshot', b''), timeout=20)
    assert json.loads(body)['parent'] is None
