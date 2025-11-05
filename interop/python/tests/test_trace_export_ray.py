"""Collector receipts across real Ray deployment and retirement boundaries."""
from contextlib import contextmanager
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
from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig
from ray_test_cluster import owned_ray_cluster
from test_causal_context import PARENT, gateway_connection
from test_causal_ray_context import SOURCE, request
from test_trace_export import collector, wait_for


@pytest.fixture(scope='module')
def exporting_cluster(tmp_path_factory):
    configured = os.environ.get('PARROT_EXPORT_RAY_EVIDENCE')
    root = Path(configured) if configured else tmp_path_factory.mktemp('export-ray')
    root.mkdir(parents=True, exist_ok=True)
    source = root/'source'
    source.mkdir()
    # Async retirement must yield so this admitted task can finish naturally.
    text = SOURCE.replace("        if key == 'bin:parallel':", """        if key == 'bin:slow':
            await asyncio.sleep(0.4)
        if key == 'bin:parallel':""")
    text = text.replace('class Sync(TraceActor, Business):', '''class SetupFailure(TraceActor, Business):
    def parrot_trace_configure(self, config, path):
        raise OSError('recorder startup failed before initialization')

class Sync(TraceActor, Business):''')
    text = text.replace("{'sync': Sync, 'async': Async, 'plain': Business}",
                        "{'sync': Sync, 'async': Async, 'plain': Business, 'setup-failure': SetupFailure}")
    text = text.replace("    kind = ctx['path'].rsplit('/', 1)[1]", """    kind = ctx['path'].rsplit('/', 1)[1]
    if ctx['config'].get('reject_host_finish_once'):
        from parrot_protocol.trace_recording import TraceRecorder
        original = TraceRecorder.finish_export
        rejected = []
        def finish(self, timeout=5.0):
            if not rejected:
                rejected.append(True)
                return dict(ok=False, enabled=True, running=True, pending=0, active=0, error=None)
            return original(self, timeout)
        TraceRecorder.finish_export = finish""")
    (source/'export_fixture.py').write_text(text)
    sys.path.insert(0, str(source))
    try:
        importlib.invalidate_caches()
        importlib.import_module('export_fixture')
        with owned_ray_cluster(ray, evidence=root/'cluster'):
            yield root, source
    finally:
        sys.path.remove(str(source))
        sys.modules.pop('export_fixture', None)


@contextmanager
def deployment(cluster, sink, kind, config=None):
    root, source = cluster
    worker = None
    try:
        worker = ray.remote(IsolatedRuntime).options(num_cpus=0).remote(ParrotDispatcher(),
            trace_config=TraceConfig(capacity=8, export=TraceExportConfig(sink.endpoint, batch_size=2, max_attempts=2)))
        assert ray.get(worker.ready.remote(), timeout=30)
        receipt = ray.get(worker.mount_module.remote(kind, 'export_fixture', str(source),
            {'config': {'setup_timeout_seconds': 60}}, ['/user/'+kind], '1', config or {}), timeout=120)
        assert receipt == dict(ok=True, paths=['/user/'+kind]), receipt
        yield worker, root
    finally:
        sink.set_response()
        if sink.block is not None:
            sink.block.set()
        if worker is not None:
            receipt = ray.get(worker.close.remote(), timeout=30)
            assert receipt == dict(ok=True, pending=[]), receipt
            _kill_and_confirm(ray, worker, worker.ready, time.monotonic()+10)


@pytest.mark.parametrize('kind', ['dispatcher', 'sync', 'async'])
def test_real_wire_handlers_export_and_retire_only_after_receipt(exporting_cluster, kind):
    with collector() as sink, deployment(exporting_cluster, sink, kind) as (worker, root):
        identities = []
        with gateway_connection(worker=worker, ray=ray) as (peer, receive, _, _):
            peer.settimeout(20)
            for i, payload in enumerate([b'one', b'fail', b'two', b'three']):
                reply = request(peer, receive, i+2, kind, payload, PARENT)
                if payload == b'fail':
                    assert reply.ft == wire.FT_REPLY_ERR
                else:
                    assert reply.ft == wire.FT_REPLY
                    value = json.loads(reply.payload)
                    identities.append(bytes.fromhex(value['parent']))
        result = ray.get(worker.retire_paths.remote(kind, ['/user/'+kind]), timeout=20)
        assert result == {'/user/'+kind: None}, result
        spans = sink.spans()
        assert len(spans) == 4 and len({s['spanId'] for s in spans}) == 4
        assert all(s['parentSpanId'] == PARENT.span_id.hex() for s in spans)
        by_span = {s['spanId']:s for s in spans}
        for identity in identities:
            assert by_span[identity[17:].hex()]['traceId'] == identity[1:17].hex()
        assert sorted(s['status']['code'] for s in spans) == [1, 1, 1, 2]
        case = root/('export-'+kind)
        case.mkdir()
        (case/'documents.json').write_text(json.dumps(sink.documents(), indent=2))
        (case/'retired.json').write_text(json.dumps(result))


def test_collector_failure_retains_native_owner_until_successful_retry(exporting_cluster):
    with collector() as sink, deployment(exporting_cluster, sink, 'sync') as (worker, root):
        sink.set_response(503)
        result = ray.get(worker.ask.remote('/user/sync', 'bin:context', b'committed'), timeout=20)
        assert result['ok']
        info = json.loads(result['payload'])
        actor = ray.get_actor('causal-sync', namespace=info['namespace'])
        wait_for(lambda: not ray.get(actor.parrot_trace_export_status.remote(), timeout=5)['running'])
        first = ray.get(worker.retire_paths.remote('sync', ['/user/sync']), timeout=20)
        assert first == {'/user/sync': 'stop_failed'}
        ray.get(actor.__ray_ready__.remote(), timeout=5)
        status = ray.get(worker.trace_export_status.remote('/user/sync'), timeout=10)
        assert status['pending'] == 1 and status['exported'] == 0 and status['closed']
        snapshot = ray.get(worker.trace_snapshot.remote('/user/sync'), timeout=10)
        assert len(snapshot['records']) == 1
        assert not ray.get(worker.ask.remote('/user/sync', 'bin:context', b'must-not-run'), timeout=5)['ok']
        sink.set_response()
        second = ray.get(worker.retire_paths.remote('sync', ['/user/sync']), timeout=20)
        assert second == {'/user/sync': None}
        with pytest.raises(ray.exceptions.ActorDiedError):
            ray.get(actor.__ray_ready__.remote(), timeout=5)
        # Earlier HTTP failures are observations, not accepted-span receipts.
        assert len({s['spanId'] for s in sink.spans()}) == 1
        case = root/'failure-retry'
        case.mkdir()
        (case/'documents.json').write_text(json.dumps(sink.documents(), indent=2))
        (case/'retirement.json').write_text(json.dumps(dict(first=first, retained=status, second=second)))


def test_real_async_actor_finish_waits_without_blocking_its_handler(exporting_cluster):
    with collector() as sink, deployment(exporting_cluster, sink, 'async') as (worker, root):
        result = ray.get(worker.ask.remote('/user/async', 'bin:snapshot', b''), timeout=20)
        actor = ray.get_actor('causal-async', namespace=json.loads(result['payload'])['namespace'])
        task = actor.ask_traced.remote('bin:slow', b'body', PARENT.encode())
        wait_for(lambda: ray.get(actor.parrot_trace_export_status.remote(), timeout=5)['active'] == 1)
        finish = actor.parrot_trace_finish.remote(5)
        _, body = ray.get(task, timeout=10)
        child = bytes.fromhex(json.loads(body)['parent'])
        status = ray.get(finish, timeout=10)
        assert status['ok'] and status['exported'] == 2 and not status['running']
        assert any(s['spanId'] == child[17:].hex() for s in sink.spans())
        assert ray.get(worker.retire_paths.remote('async', ['/user/async']), timeout=20) == {'/user/async': None}
        case = root/'async-finish'
        case.mkdir()
        (case/'documents.json').write_text(json.dumps(sink.documents(), indent=2))
        (case/'finished.json').write_text(json.dumps(status))


def test_trace_startup_failure_before_recording_does_not_leave_unretirable_actor(exporting_cluster):
    _, source = exporting_cluster
    with collector() as sink:
        worker = ray.remote(IsolatedRuntime).options(num_cpus=0).remote(ParrotDispatcher(),
            trace_config=TraceConfig(export=TraceExportConfig(sink.endpoint)))
        try:
            receipt = ray.get(worker.mount_module.remote('failure', 'export_fixture', str(source),
                {'config': {'setup_timeout_seconds': 60}}, ['/user/setup-failure'], '1', {}), timeout=120)
            assert not receipt['ok'] and receipt['paths'] == [], receipt
            assert 'recorder startup failed' in receipt['detail']
            assert ray.get(worker.close.remote(), timeout=20) == dict(ok=True, pending=[])
            assert sink.requests == []
        finally:
            # On the preserved counterexample, close returns a retained owner;
            # the test's exclusive cluster still owns its process teardown.
            receipt = ray.get(worker.close.remote(), timeout=20)
            _kill_and_confirm(ray, worker, worker.ready, time.monotonic()+10)


def test_live_runtime_flush_recovers_exporter_and_preserves_business_admission(exporting_cluster):
    with collector() as sink, deployment(exporting_cluster, sink, 'sync') as (worker, root):
        sink.set_response(503)
        first = ray.get(worker.ask.remote('/user/sync', 'bin:context', b'first'), timeout=20)
        assert first['ok']
        wait_for(lambda: not ray.get(worker.trace_export_status.remote('/user/sync'), timeout=5)['running'])
        sink.set_response()
        status = ray.get(worker.trace_flush.remote('/user/sync', 5), timeout=10)
        assert status['ok'] and not status['closed'] and status['exported'] == 1
        assert ray.get(worker.ask.remote('/user/sync', 'bin:context', b'second'), timeout=10)['ok']
        assert ray.get(worker.retire_paths.remote('sync', ['/user/sync']), timeout=20) == {'/user/sync': None}
        assert len({s['spanId'] for s in sink.spans()}) == 2
        case = root/'live-flush'
        case.mkdir()
        (case/'documents.json').write_text(json.dumps(sink.documents(), indent=2))
        (case/'flush.json').write_text(json.dumps(status))


def test_deployment_host_is_retained_until_its_export_thread_retirement_is_confirmed(exporting_cluster):
    with collector() as sink, deployment(exporting_cluster, sink, 'dispatcher',
                                        dict(reject_host_finish_once=True)) as (worker, root):
        reply = ray.get(worker.ask.remote('/user/dispatcher', 'bin:context', b'body'), timeout=20)
        info = json.loads(reply['payload'])
        host = ray.get_actor('host', namespace=info['namespace'])
        first = ray.get(worker.retire_paths.remote('dispatcher', ['/user/dispatcher']), timeout=20)
        assert first == {'/user/dispatcher': 'stop_failed'}
        assert ray.get(host.ready.remote(), timeout=5)
        assert len(sink.spans()) == 1
        second = ray.get(worker.retire_paths.remote('dispatcher', ['/user/dispatcher']), timeout=20)
        assert second == {'/user/dispatcher': None}
        with pytest.raises(ray.exceptions.ActorDiedError):
            ray.get(host.ready.remote(), timeout=5)
        case = root/'host-retirement'
        case.mkdir()
        (case/'documents.json').write_text(json.dumps(sink.documents(), indent=2))
        (case/'retirement.json').write_text(json.dumps(dict(first=first, second=second)))
