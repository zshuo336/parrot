"""Saturation may omit a span, but cannot erase a live handler's owner."""
import threading

import asyncio
import pytest

from parrot_protocol.trace import parent_scope
from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder
from test_causal_context import PARENT
from test_trace_export import collector, wait_for


def test_finish_cannot_claim_completion_while_an_overflow_handler_is_running():
    with collector() as sink:
        sink.block = threading.Event()
        recorder = TraceRecorder(TraceConfig(capacity=1, export=TraceExportConfig(
            sink.endpoint, interval=0.01, request_timeout=2)))
        entered = threading.Event()
        release = threading.Event()
        errors = []
        def business():
            try:
                with recorder.handler('/user/overflow', 'slow', 'ask') as child:
                    assert child is None
                    entered.set()
                    assert release.wait(10), 'test did not release the active handler'
            except BaseException as error:
                errors.append(error)
        thread = threading.Thread(target=business)
        try:
            with recorder.handler('/user/recorded', 'fast', 'ask'):
                pass
            assert sink.received.wait(5)
            thread.start()
            assert entered.wait(5)
            sink.block.set()
            wait_for(lambda: recorder.export_status()['exported'] == 1)
            status = recorder.finish_export(0.1)
            assert thread.is_alive() and not release.is_set()
            assert status['ok'] is False, status
            assert status['active'] == 1 and status['running'], status
            assert status['dropped'] == 1 and status['pending'] == 0
        finally:
            sink.block.set()
            release.set()
            if thread.ident is not None:
                thread.join(5)
            result = recorder.finish_export(5)
            assert not thread.is_alive() and not errors, errors
            assert result['ok'] and not result['running'] and result['active'] == 0
        assert len(sink.spans()) == 1


@pytest.mark.parametrize('error', [ValueError, asyncio.CancelledError])
def test_overflow_scope_is_counted_and_unwinds_after_error_or_cancellation(error):
    recorder = TraceRecorder(TraceConfig(capacity=1))
    with recorder.handler('/user/recorded', 'first', 'ask'):
        pass
    with pytest.raises(error), parent_scope(PARENT):
        with recorder.handler('/user/overflow', 'error', 'ask') as parent:
            assert parent == PARENT
            assert recorder.snapshot()['active'] == 1
            raise error('requested failure')
    snapshot = recorder.snapshot()
    assert snapshot['active'] == 0 and snapshot['dropped'] == 1
    assert len(snapshot['records']) == 1


def test_released_record_capacity_is_reusable_while_overflow_business_is_active():
    recorder = TraceRecorder(TraceConfig(capacity=1))
    with recorder.handler('/user/recorded', 'first', 'ask'):
        pass
    with parent_scope(PARENT), recorder.handler('/user/overflow', 'hold', 'ask') as parent:
        assert parent == PARENT
        assert recorder.snapshot()['active'] == 1
        first = recorder.snapshot()['records'][0]['span_id']
        assert recorder.acknowledge([first]) == 1
        with recorder.handler('/user/recorded', 'second', 'ask') as child:
            assert child.trace_id == PARENT.trace_id and child.span_id != PARENT.span_id
            assert recorder.snapshot()['active'] == 2
    snapshot = recorder.snapshot()
    assert snapshot['active'] == 0 and snapshot['dropped'] == 1
    assert snapshot['acknowledged'] == 1 and len(snapshot['records']) == 1


def test_flush_cannot_report_an_overflow_handler_as_quiescent():
    with collector() as sink:
        sink.block = threading.Event()
        recorder = TraceRecorder(TraceConfig(capacity=1, export=TraceExportConfig(
            sink.endpoint, interval=0.01)))
        try:
            with recorder.handler('/user/recorded', 'first', 'ask'):
                pass
            assert sink.received.wait(5)
            with recorder.handler('/user/overflow', 'hold', 'ask'):
                sink.block.set()
                wait_for(lambda: recorder.export_status()['exported'] == 1)
                receipt = recorder.flush_export(0.05)
                assert not receipt['ok'] and receipt['active'] == 1 and not receipt['closed']
            assert recorder.flush_export(5)['ok']
        finally:
            sink.block.set()
            assert recorder.finish_export(5)['ok']
