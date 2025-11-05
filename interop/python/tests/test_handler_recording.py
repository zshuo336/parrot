"""Execution receipts, bounded accounting and causal trees, not just API presence."""
import asyncio
from concurrent.futures import ThreadPoolExecutor
import json
import pickle
import threading

import pytest

from parrot_protocol.trace import TraceActor, AsyncTraceActor, TraceParent, current_parent, parent_scope
from parrot_protocol.trace_recording import TraceConfig, TraceRecorder, snapshot_otlp
from parrot_protocol.ray_gw import ParrotDispatcher, _ComponentRuntime, UnknownTypeKeyError
from parrot_protocol.ray_hosts import IsolatedRuntime

PARENT = TraceParent(b'a' * 16, b'b' * 8)
OTHER = TraceParent(b'c' * 16, b'd' * 8)


@pytest.mark.parametrize('capacity', [0, -1, 4097, True, 1.5, '2'])
def test_invalid_capacity_rejected(capacity):
    with pytest.raises(ValueError):
        TraceConfig(capacity=capacity)


@pytest.mark.parametrize('name', ['', 'x' * 257, '\u4e2d' * 86, None])
def test_invalid_service_rejected(name):
    with pytest.raises(ValueError):
        TraceConfig(service_name=name)


def test_nested_real_handlers_form_tree_and_snapshots_are_independent():
    recorder = TraceRecorder(TraceConfig())
    with parent_scope(PARENT):
        with recorder.handler('/user/a', 'key', 'ask') as child:
            assert current_parent() == child and child.trace_id == PARENT.trace_id and child != PARENT
            with recorder.handler('/user/b', 'nested', 'tell') as grandchild:
                assert current_parent() == grandchild
            assert current_parent() == child
        assert current_parent() == PARENT
    records = recorder.snapshot()['records']
    assert [(r['span_id'], r['parent_span_id']) for r in records] == [
        (grandchild.span_id.hex(), child.span_id.hex()), (child.span_id.hex(), PARENT.span_id.hex())]
    records[0]['outcome'] = 'tampered'
    assert recorder.snapshot()['records'][0]['outcome'] == 'ok'
    assert current_parent() is None


def test_roots_have_real_identity_and_do_not_reuse_previous_request():
    recorder = TraceRecorder(TraceConfig())
    for _ in range(3):
        with recorder.handler('/user/a', 'key', 'ask'):
            assert current_parent() is not None
    records = recorder.snapshot()['records']
    assert len({r['trace_id'] for r in records}) == 3
    assert all(r['parent_span_id'] is None for r in records)


def test_concurrent_admissions_count_active_and_completed_without_eviction():
    recorder = TraceRecorder(TraceConfig(capacity=2))
    gate = threading.Barrier(3)
    release = threading.Event()
    def work():
        with recorder.handler('/user/a', 'key', 'ask'):
            gate.wait(timeout=5)
            assert release.wait(5)
    with ThreadPoolExecutor(max_workers=2) as executor:
        tasks = [executor.submit(work) for _ in range(2)]
        try:
            gate.wait(timeout=5)
            assert recorder.snapshot()['active'] == 2
            with parent_scope(PARENT), recorder.handler('/user/a', 'overflow', 'ask'):
                assert current_parent() == PARENT
            assert recorder.snapshot()['dropped'] == 1
        finally:
            release.set()
        for task in tasks:
            task.result(timeout=5)
    with recorder.handler('/user/a', 'full', 'ask'):
        assert current_parent() is None
    result = recorder.snapshot()
    assert len(result['records']) == 2 and result['active'] == 0 and result['dropped'] == 2


def test_close_preserves_inflight_completion_and_reports_rejected_admission():
    recorder = TraceRecorder(TraceConfig())
    with recorder.handler('/user/a', 'key', 'ask'):
        recorder.close()
        with recorder.handler('/user/b', 'key', 'ask'):
            pass
    result = recorder.snapshot()
    assert result['closed'] and result['active'] == 0 and result['dropped'] == 1
    assert len(result['records']) == 1


def test_failure_preserves_exact_exception_and_does_not_capture_sensitive_message():
    recorder = TraceRecorder(TraceConfig())
    class Fatal(BaseException):
        pass
    error = Fatal('secret payload')
    with pytest.raises(Fatal) as caught:
        with parent_scope(PARENT), recorder.handler('/user/a', 'key', 'ask'):
            raise error
    assert caught.value is error and current_parent() is None
    record = recorder.snapshot()['records'][0]
    assert record['outcome'] == 'error' and record['error_type'] == 'Fatal'
    assert 'secret' not in json.dumps(recorder.otlp())


def test_monotonic_duration_survives_wall_clock_rollback(monkeypatch):
    recorder = TraceRecorder(TraceConfig())
    monkeypatch.setattr('parrot_protocol.trace_recording.time.time_ns', lambda: 100)
    ticks = iter([1000, 1100])
    monkeypatch.setattr('parrot_protocol.trace_recording.time.monotonic_ns', lambda: next(ticks))
    with recorder.handler('/user/a', 'key', 'ask'):
        monkeypatch.setattr('parrot_protocol.trace_recording.time.time_ns', lambda: 1)
    record = recorder.snapshot()['records'][0]
    assert (record['start'], record['end']) == (100, 200)


def test_identity_failure_releases_reservation(monkeypatch):
    recorder = TraceRecorder(TraceConfig(capacity=1))
    def fail(_):
        raise OSError('entropy unavailable')
    with monkeypatch.context() as patch:
        patch.setattr('parrot_protocol.trace_recording.secrets.token_bytes', fail)
        with pytest.raises(OSError):
            with recorder.handler('/user/a', 'key', 'ask'):
                pytest.fail('handler must not enter after identity failure')
    with recorder.handler('/user/a', 'key', 'ask'):
        pass
    snapshot = recorder.snapshot()
    assert snapshot['active'] == 0 and snapshot['dropped'] == 1 and len(snapshot['records']) == 1


def test_config_transfers_but_live_locks_do_not():
    config = TraceConfig()
    assert pickle.loads(pickle.dumps(config)) == config
    with pytest.raises(TypeError, match='owning process'):
        pickle.dumps(TraceRecorder(config))


def test_acknowledgement_releases_only_saved_records_and_rejects_foreign_ids_atomically():
    recorder = TraceRecorder(TraceConfig(capacity=2))
    for path in ('/user/a', '/user/b'):
        with recorder.handler(path, 'key', 'ask'):
            pass
    a, b = recorder.snapshot()['records']
    with pytest.raises(ValueError):
        recorder.acknowledge([a['span_id'], b['span_id']], '/user/a')
    with pytest.raises(ValueError):
        recorder.acknowledge([a['span_id'], 'not-a-record'])
    with pytest.raises(ValueError):
        recorder.acknowledge([a['span_id'], a['span_id']])
    assert len(recorder.snapshot()['records']) == 2
    assert recorder.acknowledge([a['span_id']], '/user/a') == 1
    with recorder.handler('/user/a', 'new', 'tell'):
        pass
    snapshot = recorder.snapshot()
    assert snapshot['acknowledged'] == 1 and len(snapshot['records']) == 2 and snapshot['dropped'] == 0
    assert recorder.snapshot('/user/b')['records'] == [b]


def test_long_unicode_logical_path_can_be_collected_and_acknowledged():
    path = '/user/' + '\u4e2d' * 260
    recorder = TraceRecorder(TraceConfig())
    with recorder.handler(path, 'key', 'ask'):
        pass
    snapshot = recorder.snapshot(path)
    assert len(snapshot['records']) == 1
    assert recorder.acknowledge([snapshot['records'][0]['span_id']], path) == 1


@pytest.mark.parametrize('runtime_type', [_ComponentRuntime, IsolatedRuntime])
def test_actual_dispatcher_records_only_executed_handlers_and_tell_operation(runtime_type):
    dispatcher = ParrotDispatcher()
    calls = []
    @dispatcher.handler('bin:echo')
    def echo(key, body):
        calls.append((body, current_parent()))
        if body == b'fail':
            raise ValueError('business failure')
        return key, body
    runtime = runtime_type(dispatcher, trace_config=TraceConfig())
    with parent_scope(PARENT):
        assert runtime.ask('/user/a', 'bin:echo', b'one')['payload'] == b'one'
        with pytest.raises(ValueError):
            runtime.ask('/user/a', 'bin:echo', b'fail')
    runtime.deliver('/user/a', 'bin:echo', b'tell')
    assert not runtime.ask('/user/missing', 'unknown', b'')['ok']
    result = runtime.trace_snapshot()
    assert [r['outcome'] for r in result['records']] == ['ok', 'error', 'ok']
    assert [r['operation'] for r in result['records']] == ['ask', 'ask', 'tell']
    assert [r['span_id'] for r in result['records']] == [p.span_id.hex() for _, p in calls]
    assert result['records'][-1]['parent_span_id'] is None
    with pytest.raises(RuntimeError):
        runtime.configure_tracing(TraceConfig())


def test_mounted_dispatcher_unknown_key_never_creates_record():
    dispatcher, sub = ParrotDispatcher(), ParrotDispatcher()
    recorder = TraceRecorder(TraceConfig())
    sub.handler('known')(lambda k, p: (k, p))
    dispatcher.mount(sub)
    assert dispatcher.dispatch('known', b'body', trace_recorder=recorder) == ('known', b'body')
    with pytest.raises(UnknownTypeKeyError):
        dispatcher.dispatch('missing', b'', trace_recorder=recorder)
    assert len(recorder.snapshot()['records']) == 1


def test_sync_native_real_result_error_and_disabled_compatibility():
    class Actor(TraceActor):
        def ask(self, key, body):
            if body == b'fail':
                raise ValueError('business')
            return key, (body, current_parent())
    actor = Actor()
    assert actor.ask_traced('key', b'body', PARENT.encode())[1][1] == PARENT
    assert actor.parrot_trace_snapshot() == dict(enabled=False)
    actor.parrot_trace_configure(TraceConfig(), '/user/native')
    _, (body, child) = actor.ask_traced('key', b'body', PARENT.encode())
    assert body == b'body' and child != PARENT and child.trace_id == PARENT.trace_id
    with pytest.raises(ValueError):
        actor.deliver_traced('key', b'fail', None)
    records = actor.parrot_trace_snapshot()['records']
    assert [(r['operation'], r['outcome']) for r in records] == [('ask', 'ok'), ('tell', 'error')]
    with pytest.raises(RuntimeError):
        actor.parrot_trace_configure(TraceConfig(), '/user/other')


def test_async_native_parallel_context_and_actual_cancellation():
    async def run():
        entered = asyncio.Event()
        class Actor(AsyncTraceActor):
            async def ask(self, key, body):
                before = current_parent()
                if body == b'cancel':
                    entered.set()
                    await asyncio.Event().wait()
                await asyncio.sleep(0)
                assert current_parent() == before
                return key, before
        actor = Actor()
        actor.parrot_trace_configure(TraceConfig(), '/user/async')
        replies = await asyncio.gather(*(actor.ask_traced('key', b'body', p.encode())
                                        for p in (PARENT, OTHER) * 12))
        assert len({p.span_id for _, p in replies}) == 24
        assert [p.trace_id for _, p in replies] == [p.trace_id for p in (PARENT, OTHER) * 12]
        task = asyncio.create_task(actor.ask_traced('key', b'cancel', PARENT.encode()))
        await asyncio.wait_for(entered.wait(), 5)
        assert actor.parrot_trace_snapshot()['active'] == 1
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        snapshot = actor.parrot_trace_snapshot()
        assert len(snapshot['records']) == 25 and snapshot['active'] == 0
        assert snapshot['records'][-1]['outcome'] == 'cancelled'
        assert snapshot['records'][-1]['parent_span_id'] == PARENT.span_id.hex()
        assert current_parent() is None
    asyncio.run(run())


def test_otlp_json_has_parent_hex_status_timestamps_and_no_payload():
    recorder = TraceRecorder(TraceConfig(service_name='handler-test'))
    with parent_scope(PARENT), recorder.handler('/user/a', 'key', 'ask'):
        pass
    with pytest.raises(ValueError), recorder.handler('/user/a', 'key', 'tell'):
        raise ValueError('private body')
    snapshot = recorder.snapshot()
    document = snapshot_otlp(snapshot)
    resource = document['resourceSpans'][0]
    assert resource['resource']['attributes'][0]['value']['stringValue'] == 'handler-test'
    spans = resource['scopeSpans'][0]['spans']
    assert spans[0]['traceId'] == PARENT.trace_id.hex() and spans[0]['parentSpanId'] == PARENT.span_id.hex()
    assert 'parentSpanId' not in spans[1]
    assert [s['status']['code'] for s in spans] == [1, 2]
    assert all(s['kind'] == 2 and int(s['endTimeUnixNano']) >= int(s['startTimeUnixNano']) > 0 for s in spans)
    assert 'private body' not in json.dumps(document)
