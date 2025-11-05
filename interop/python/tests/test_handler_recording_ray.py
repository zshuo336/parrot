"""Actual TCP -> coordinator -> deployment host -> native Ray execution receipts."""
import importlib
import json
import os
from pathlib import Path
import sys
import time

import pytest
import ray

from parrot_protocol import wire
from parrot_protocol.ray_gw import ParrotDispatcher, _kill_and_confirm
from parrot_protocol.ray_hosts import IsolatedRuntime
from parrot_protocol.trace_recording import TraceConfig, snapshot_otlp
from ray_test_cluster import owned_ray_cluster
from test_causal_context import PARENT, OTHER, gateway_connection
from test_causal_ray_context import SOURCE, request


@pytest.fixture(scope='module')
def recorded(tmp_path_factory):
    configured = os.environ.get('PARROT_RECORDING_RAY_EVIDENCE')
    root = Path(configured) if configured else tmp_path_factory.mktemp('handler-ray')
    root.mkdir(parents=True, exist_ok=True)
    source = root / 'source'
    source.mkdir()
    actor_source = SOURCE.replace("        if key == 'bin:parallel':", """        if key == 'bin:cancel':
            await asyncio.Event().wait()
        if key == 'bin:parallel':""")
    (source / 'recorded_fixture.py').write_text(actor_source)
    sys.path.insert(0, str(source))
    worker = None
    try:
        importlib.invalidate_caches()
        importlib.import_module('recorded_fixture')
        with owned_ray_cluster(ray, evidence=root / 'cluster'):
            try:
                worker = ray.remote(IsolatedRuntime).options(num_cpus=0).remote(
                    ParrotDispatcher(), trace_config=TraceConfig(service_name='real-ray-handlers', capacity=128))
                assert ray.get(worker.ready.remote(), timeout=30)
                paths = ['/user/' + kind for kind in ('dispatcher', 'sync', 'async')]
                receipt = ray.get(worker.mount_module.remote('recorded', 'recorded_fixture', str(source),
                    {'config': {'setup_timeout_seconds': 60}}, paths, '1', {}), timeout=120)
                assert receipt == dict(ok=True, paths=paths), receipt
                yield worker, root, source
            finally:
                if worker is not None:
                    receipt = ray.get(worker.close.remote(), timeout=60)
                    (root / 'retirement.json').write_text(json.dumps(receipt))
                    assert receipt == dict(ok=True, pending=[]), receipt
                    _kill_and_confirm(ray, worker, worker.ready, time.monotonic() + 10)
    finally:
        sys.path.remove(str(source))
        sys.modules.pop('recorded_fixture', None)


@pytest.mark.parametrize('kind', ['dispatcher', 'sync', 'async'])
def test_real_tcp_handler_child_receipts_errors_roots_and_tells(recorded, kind):
    worker, root, _ = recorded
    before = ray.get(worker.trace_snapshot.remote('/user/' + kind), timeout=20)
    assert before['enabled'] and before['records'] == []
    with gateway_connection(worker=worker, ray=ray) as (peer, receive, _, _):
        peer.settimeout(20)
        observed = []
        for cid, payload, parent in [(2, b'first', PARENT), (3, b'fail', OTHER),
                                     (4, b'plain', None), (5, b'last', OTHER)]:
            reply = request(peer, receive, cid, kind, payload, parent)
            if payload == b'fail':
                assert reply.ft == wire.FT_REPLY_ERR
            else:
                assert reply.ft == wire.FT_REPLY, reply.payload
                info = json.loads(reply.payload)
                child = bytes.fromhex(info['parent'])
                assert info['payload'] == payload.hex() and info['pid'] != os.getpid()
                assert info['job'] != ray.get_runtime_context().get_job_id()
                assert info['namespace'].startswith('parrot-deployment-')
                assert len(child) == 25 and child[0] == 1
                if parent is not None:
                    assert child[1:17] == parent.trace_id and child[17:] != parent.span_id
                observed.append(child)
        for payload, parent in [(b'tell', PARENT), (b'plain-tell', None)]:
            frame = wire.Frame(wire.FT_TELL, 0, '/user/' + kind, 'bin:context', payload)
            peer.sendall((frame if parent is None else frame.with_trace_parent(parent)).encode())
        # Same connection barrier: TELL completes before its next ASK dispatch.
        reply = request(peer, receive, 12, kind, b'', None, 'bin:snapshot')
        assert reply.ft == wire.FT_REPLY
        info = json.loads(reply.payload)
        assert len(info['events']) == 6
    snapshot = ray.get(worker.trace_snapshot.remote('/user/' + kind), timeout=20)
    records = snapshot['records']
    assert len(records) == 7 and snapshot['active'] == 0 and snapshot['dropped'] == 0
    assert [r['outcome'] for r in records] == ['ok', 'error', 'ok', 'ok', 'ok', 'ok', 'ok']
    assert [r['operation'] for r in records] == ['ask'] * 4 + ['tell'] * 2 + ['ask']
    assert [r['parent_span_id'] for r in records] == [PARENT.span_id.hex(), OTHER.span_id.hex(),
        None, OTHER.span_id.hex(), PARENT.span_id.hex(), None, None]
    assert [records[i]['span_id'] for i in (0, 2, 3)] == [p[17:].hex() for p in observed]
    assert all(r['path'] == '/user/' + kind for r in records)
    for event, record in zip(info['events'], records):
        identity = bytes.fromhex(event[1])
        assert (identity[1:17].hex(), identity[17:].hex()) == (record['trace_id'], record['span_id'])
    case = root / kind
    case.mkdir()
    (case / 'snapshot.json').write_text(json.dumps(snapshot, indent=2))
    (case / 'documents.json').write_text(json.dumps([snapshot_otlp(snapshot)], indent=2))
    (case / 'handler-receipts.json').write_text(json.dumps(info, indent=2))


def test_native_without_recording_contract_is_rejected_before_publication(recorded):
    worker, root, source = recorded
    receipt = ray.get(worker.mount_module.remote('unsupported', 'recorded_fixture', str(source),
        {'config': {'setup_timeout_seconds': 60}}, ['/user/plain'], '1', {}), timeout=120)
    assert not receipt['ok'] and receipt['paths'] == [], receipt
    assert 'TraceActor' in receipt['detail']
    reply = ray.get(worker.ask.remote('/user/plain', 'bin:context', b'must-not-run'), timeout=20)
    assert not reply['ok']
    with pytest.raises(ray.exceptions.RayTaskError):
        ray.get(worker.trace_snapshot.remote('/user/plain'), timeout=20)
    (root / 'unsupported.json').write_text(json.dumps(receipt))


def test_native_async_concurrent_recorded_tasks_keep_distinct_children(recorded):
    worker, root, _ = recorded
    reply = ray.get(worker.ask.remote('/user/async', 'bin:snapshot', b''), timeout=20)
    info = json.loads(reply['payload'])
    actor = ray.get_actor('causal-async', namespace=info['namespace'])
    parents = [PARENT.encode(), OTHER.encode(), None] * 8
    refs = [actor.ask_traced.remote('bin:parallel', str(i).encode(), parent) for i, parent in enumerate(parents)]
    replies = ray.get(refs, timeout=20)
    children = []
    for i, (_, body) in enumerate(replies):
        value = json.loads(body)
        child = bytes.fromhex(value['parent'])
        assert value['payload'] == str(i).encode().hex()
        if parents[i] is not None:
            assert child[1:17] == parents[i][1:17] and child[17:] != parents[i][17:]
        children.append(child)
    snapshot = ray.get(actor.parrot_trace_snapshot.remote(), timeout=20)
    by_span = {r['span_id']: r for r in snapshot['records']}
    assert len({c[17:] for c in children}) == 24 and snapshot['active'] == 0
    for child, parent in zip(children, parents):
        record = by_span[child[17:].hex()]
        assert record['parent_span_id'] == (None if parent is None else parent[17:].hex())
        assert record['trace_id'] == child[1:17].hex() and record['outcome'] == 'ok'
    case = root / 'parallel'
    case.mkdir()
    (case / 'snapshot.json').write_text(json.dumps(snapshot, indent=2))
    (case / 'documents.json').write_text(json.dumps([snapshot_otlp(snapshot)], indent=2))
    (case / 'handler-receipts.json').write_text(json.dumps([c.hex() for c in children]))


def test_real_ray_cancellation_finishes_record_and_restores_actor_scope(recorded):
    worker, root, _ = recorded
    reply = ray.get(worker.ask.remote('/user/async', 'bin:snapshot', b''), timeout=20)
    actor = ray.get_actor('causal-async', namespace=json.loads(reply['payload'])['namespace'])
    task = actor.ask_traced.remote('bin:cancel', b'', PARENT.encode())
    try:
        end = time.monotonic() + 10
        while True:
            snapshot = ray.get(actor.parrot_trace_snapshot.remote(), timeout=5)
            if snapshot['active'] == 1:
                break
            if time.monotonic() >= end:
                pytest.fail('cancel target did not enter its real handler')
            time.sleep(0.01)
        ray.cancel(task, force=False)
        with pytest.raises(ray.exceptions.TaskCancelledError):
            ray.get(task, timeout=10)
        end = time.monotonic() + 10
        while True:
            snapshot = ray.get(actor.parrot_trace_snapshot.remote(), timeout=5)
            if snapshot['active'] == 0:
                break
            if time.monotonic() >= end:
                pytest.fail('cancelled handler did not release recording admission')
            time.sleep(0.01)
        cancelled = [r for r in snapshot['records'] if r['key'] == 'bin:cancel']
        assert len(cancelled) == 1 and cancelled[0]['outcome'] == 'cancelled'
        assert cancelled[0]['parent_span_id'] == PARENT.span_id.hex()
        # Deliberately bypass the mixin to observe the ambient actor scope.
        _, body = ray.get(actor.ask.remote('bin:snapshot', b''), timeout=10)
        assert json.loads(body)['parent'] is None
        case = root / 'cancelled'
        case.mkdir()
        (case / 'snapshot.json').write_text(json.dumps(snapshot, indent=2))
        (case / 'documents.json').write_text(json.dumps([snapshot_otlp(snapshot)], indent=2))
    finally:
        ray.cancel(task, force=False)
