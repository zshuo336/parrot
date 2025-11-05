"""Causal identity, scoped execution and real TCP admission contracts."""
import asyncio
from contextlib import contextmanager
from concurrent.futures import ThreadPoolExecutor
import socket
import struct
import threading
import types

import pytest
from parrot_protocol import wire
from parrot_protocol.trace import (
    TraceParent, TraceActor, AsyncTraceActor, current_parent, capture_parent,
    parent_scope, transferred_scope,
)
from parrot_protocol import ray_gw

PARENT = TraceParent(bytes(range(1, 17)), bytes(range(17, 25)))
OTHER = TraceParent(b'\x31' * 16, b'\x42' * 8)


@pytest.mark.parametrize('trace,span', [
    (b'', b'a' * 8), (b'a' * 15, b'b' * 8), (b'a' * 17, b'b' * 8),
    (b'\0' * 16, b'a' * 8), (b'a' * 16, b'\0' * 8),
    (bytearray(b'a' * 16), b'b' * 8), (b'a' * 16, '12345678'),
])
def test_invalid_identity_is_never_admitted(trace, span):
    with pytest.raises(ValueError):
        TraceParent(trace, span)


@pytest.mark.parametrize('encoded', [b'', b'\1' * 24, b'\1' * 26, b'\2' + b'a' * 24,
                                      b'\1' + b'\0' * 16 + b'b' * 8,
                                      b'\1' + b'a' * 16 + b'\0' * 8, bytearray(b'\1' * 25)])
def test_transfer_envelope_is_exact_and_validated(encoded):
    with pytest.raises(ValueError):
        with transferred_scope(encoded):
            pytest.fail('invalid scope entered')
    assert current_parent() is None


def test_literal_rust_wire_prefix_keeps_ask_reply_to_and_business_payload():
    payload = struct.pack('<I', 6) + b'/reply' + b'body'
    original = wire.Frame(wire.FT_ASK, 711, '/user/a', 'bin:echo', payload, 2 | 8)
    framed = original.with_trace_parent(PARENT)
    assert framed.payload == b'\x01' + bytes(range(1, 25)) + payload
    assert original.payload == payload and original.flags == 10
    decoder = wire.FrameDecoder()
    decoder.feed(framed.encode())
    decoded = decoder.next_frame()
    assert decoded.flags == 74 and decoded.cid == 711
    assert decoded.take_trace_parent() == PARENT
    assert decoded.flags == 10 and wire.split_reply_to(decoded.payload) == ('/reply', b'body')
    assert decoded.take_trace_parent() is None
    assert decoded.payload == payload


@pytest.mark.parametrize('ft,flags', [(wire.FT_REPLY, 0), (wire.FT_HEARTBEAT, 0),
                                      (wire.FT_SYSTEM_EVENT, 0), (wire.FT_ASK, 4),
                                      (wire.FT_TELL, 64)])
def test_parent_cannot_mark_wrong_frame_or_nest_prefixes(ft, flags):
    f = wire.Frame(ft, 1, '', '', b'payload', flags)
    with pytest.raises(ValueError):
        f.with_trace_parent(PARENT)
    assert f.payload == b'payload' and f.flags == flags


def test_scope_restores_outer_context_after_base_exception_and_empty_scope():
    class Fatal(BaseException):
        pass
    with parent_scope(PARENT):
        with transferred_scope(None):
            assert current_parent() is None
        assert current_parent() == PARENT
        with pytest.raises(Fatal):
            with parent_scope(OTHER):
                assert capture_parent() == OTHER.encode()
                raise Fatal()
        assert current_parent() == PARENT
    assert current_parent() is None


def test_threads_receive_explicit_captures_and_do_not_leak_on_reuse():
    with ThreadPoolExecutor(max_workers=1) as executor:
        with parent_scope(PARENT):
            encoded = capture_parent()
            assert executor.submit(capture_parent).result() is None
            def inspect():
                with transferred_scope(encoded):
                    return capture_parent()
            assert executor.submit(inspect).result() == PARENT.encode()
        assert executor.submit(capture_parent).result() is None


def test_async_native_actor_scopes_are_isolated_across_awaits():
    class Actor(AsyncTraceActor):
        async def ask(self, key, payload):
            before = capture_parent()
            await asyncio.sleep(0)
            assert capture_parent() == before
            if payload == b'fail':
                raise ValueError('business error')
            return key, before
    async def run():
        actor = Actor()
        results = await asyncio.gather(actor.ask_traced('a', b'', PARENT.encode()),
                                       actor.ask_traced('b', b'', OTHER.encode()),
                                       actor.ask_traced('c', b'', None))
        assert results == [('a', PARENT.encode()), ('b', OTHER.encode()), ('c', None)]
        with pytest.raises(ValueError):
            await actor.ask_traced('a', b'fail', PARENT.encode())
        assert current_parent() is None
    asyncio.run(run())


def test_native_sync_mixin_preserves_business_signature_and_retires_scope():
    class Actor(TraceActor):
        def ask(self, key, payload):
            assert payload == b'body'
            return key, capture_parent()
        def deliver(self, key, payload):
            assert current_parent() == OTHER
            raise ValueError('tell failed')
    actor = Actor()
    assert actor.ask_traced('key', b'body', PARENT.encode()) == ('key', PARENT.encode())
    with pytest.raises(ValueError):
        actor.deliver_traced('key', b'body', OTHER.encode())
    assert current_parent() is None


def test_sync_mixin_rejects_unawaited_async_business_without_running_it():
    entered = []
    class Actor(TraceActor):
        async def ask(self, key, payload):
            entered.append(capture_parent())
    with pytest.raises(TypeError, match='AsyncTraceActor'):
        Actor().ask_traced('k', b'', PARENT.encode())
    assert not entered and current_parent() is None


def test_outgoing_context_requires_negotiation_and_never_infers_from_cid():
    f = wire.Frame(wire.FT_TELL, 42, '/user/a', 'key', b'data')
    assert wire.trace_request(f, wire.CAPS_TRACE_CONTEXT) is f
    with parent_scope(PARENT):
        assert wire.trace_request(f, 3) is f
        marked = wire.trace_request(f, 3 | wire.CAPS_TRACE_CONTEXT)
    assert marked.payload == PARENT.encode() + b'data'
    assert wire.trace_request(marked, wire.CAPS_TRACE_CONTEXT) is marked
    with pytest.raises(ValueError):
        wire.trace_request(marked, 3)


def test_trace_header_is_charged_to_frame_size_limit(monkeypatch):
    monkeypatch.setattr(wire, 'MAX_FRAME_LEN', 64)
    f = wire.Frame(wire.FT_TELL, 0, '', '', b'x' * 12)
    with pytest.raises(ValueError):
        f.with_trace_parent(PARENT)
    good = wire.Frame(wire.FT_TELL, 0, '', '', b'x' * 11).with_trace_parent(PARENT)
    assert len(good.encode()) == 68


@contextmanager
def gateway_connection(*, capabilities=3 | wire.CAPS_TRACE_CONTEXT, worker=None, ray=None):
    dispatcher = ray_gw.ParrotDispatcher()
    calls = []
    @dispatcher.handler('bin:context')
    def context(key, payload):
        parent = capture_parent()
        calls.append((payload, parent))
        if payload == b'fail':
            raise ValueError('requested business failure')
        return key, (b'-' if parent is None else parent) + payload
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0)); listener.listen(1)
        peer = socket.create_connection(listener.getsockname(), timeout=3)
        local, _ = listener.accept()
    state = ray_gw.RayAdminState()
    errors = []
    def drive():
        try:
            ray_gw._serve_connection_guarded(local, dispatcher, worker, ray, state)
        except BaseException as error:
            errors.append(error)
    thread = threading.Thread(target=drive); thread.start()
    decoder = wire.FrameDecoder()
    def receive():
        while True:
            frame = decoder.next_frame()
            if frame is not None:
                return frame
            data = peer.recv(65536)
            if not data:
                return None
            decoder.feed(data)
    peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '', wire.handshake_body('trace-peer', capabilities)))
    ack = receive()
    assert ack.ft == wire.FT_HANDSHAKE_ACK
    assert wire.decode_handshake(ack.payload, acknowledgement=True)['capabilities'] & wire.CAPS_TRACE_CONTEXT
    try:
        yield peer, receive, calls, state
    finally:
        peer.close(); thread.join(5)
        if thread.is_alive():
            local.shutdown(socket.SHUT_RDWR); thread.join(5)
        assert not thread.is_alive() and not errors, errors
        state.management.close(__import__('time').monotonic() + 5)
        state.ask_admission.wait(__import__('time').monotonic() + 5)


def ask(cid, payload, parent):
    frame = wire.Frame(wire.FT_ASK, cid, '/user/probe', 'bin:context', wire.with_reply_to_prefix('/reply', payload))
    return frame if parent is None else frame.with_trace_parent(parent)


def test_real_tcp_ask_scopes_survive_failure_and_do_not_leak_to_next_request():
    with gateway_connection() as (peer, receive, calls, state):
        for cid, payload, parent in [(2, b'first', PARENT), (3, b'fail', OTHER), (4, b'plain', None), (5, b'last', OTHER)]:
            peer.sendall(ask(cid, payload, parent).encode()); reply = receive()
            assert reply.cid == cid and reply.path == '/reply' and not reply.flags
            if payload == b'fail':
                assert reply.ft == wire.FT_REPLY_ERR
            else:
                assert reply.ft == wire.FT_REPLY
                assert reply.payload == (b'-' if parent is None else parent.encode()) + payload
        assert calls == [(b'first', PARENT.encode()), (b'fail', OTHER.encode()), (b'plain', None), (b'last', OTHER.encode())]


def test_real_tcp_tell_strips_only_trace_prefix_and_restores_driver_scope():
    with gateway_connection() as (peer, receive, calls, state):
        peer.sendall(wire.Frame(wire.FT_TELL, 0, '/user/probe', 'bin:context', b'tell').with_trace_parent(PARENT).encode())
        peer.sendall(wire.Frame(wire.FT_TELL, 0, '/user/probe', 'bin:context', b'plain').encode())
        peer.sendall(wire.Frame(wire.FT_HEARTBEAT, 99, '', '', b'').encode())
        assert receive().ft == wire.FT_HEARTBEAT_ACK
        assert calls == [(b'tell', PARENT.encode()), (b'plain', None)]


@pytest.mark.parametrize('capabilities', [3, 3 | wire.CAPS_TRACE_CONTEXT])
@pytest.mark.parametrize('malformation', ['short', 'version', 'zero', 'management', 'batch'])
def test_real_tcp_invalid_or_unnegotiated_parent_never_reaches_business(capabilities, malformation):
    with gateway_connection(capabilities=capabilities) as (peer, receive, calls, state):
        frame = ask(9, b'bad', PARENT)
        if malformation == 'short': frame.payload = b'\1'
        if malformation == 'version': frame.payload = b'\2' + frame.payload[1:]
        if malformation == 'zero': frame.payload = b'\1' + bytes(24) + b'body'
        if malformation == 'management': frame.ft = wire.FT_SYSTEM_EVENT
        if malformation == 'batch': frame.flags |= 4
        peer.sendall(frame.encode())
        assert receive() is None and not calls


def test_valid_but_unnegotiated_trace_is_not_silently_treated_as_business():
    with gateway_connection(capabilities=3) as (peer, receive, calls, state):
        peer.sendall(ask(8, b'body', PARENT).encode())
        assert receive() is None and not calls


def test_ask_admission_charges_trace_bytes_before_stripping_and_recovers():
    with gateway_connection() as (peer, receive, calls, state):
        frame = ask(81, b'charged', PARENT)
        reserved = state.ask_admission.acquire(16 * 1024 * 1024 - len(frame.encode()) + 1)
        assert reserved is not None
        try:
            peer.sendall(frame.encode())
            reply = receive()
            assert reply.ft == wire.FT_REPLY_ERR and reply.cid == 81
            assert wire.decode_err_payload(reply.payload)[0] == wire.ERR_OVERLOADED
            assert calls == []
        finally:
            reserved.release()
        peer.sendall(frame.encode())
        reply = receive()
        assert reply.ft == wire.FT_REPLY and reply.payload == PARENT.encode() + b'charged'
        assert calls == [(b'charged', PARENT.encode())]
