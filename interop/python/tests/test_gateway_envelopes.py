"""Real socket driver rejects malformed execution envelopes before worker effects.

The worker/Ray boundary is an in-process spy; the Wire parser, handshake,
connection loop and response writer are the production implementation.
"""
import socket
import struct
import threading
import types
from contextlib import contextmanager

import pytest
from parrot_protocol import ray_gw as gateway, wire


@contextmanager
def connection(monkeypatch, *, initiator=False, ready=None, tcp=False, reply_payload=None):
    if tcp:
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen(1)
            peer = socket.create_connection(listener.getsockname(), timeout=2)
            local, _ = listener.accept()
    else:
        local, peer = socket.socketpair()
    peer.settimeout(2)
    calls = []

    def ask(path, key, payload):
        calls.append(('ask', key, payload))
        if key == 'bin:fail':
            raise ValueError('business rejected')
        return dict(ok=True, key=key, payload=payload if reply_payload is None else reply_payload)

    worker = types.SimpleNamespace(
        ask=types.SimpleNamespace(remote=lambda *args: lambda: ask(*args)),
        deliver=types.SimpleNamespace(remote=lambda path, key, payload: calls.append(('tell', key, payload))))
    ray = types.SimpleNamespace(get=lambda ref: ref())

    class AdminSpy:
        def __init__(self, out, **_):
            self.out = out

        def on_frame(self, cid, path, payload, **_):
            calls.append(('admin', path, payload))
            self.out.put(wire.build_frame(wire.FT_SYSTEM_EVENT, cid, path, '', b'\x04\x02\x01'))

    monkeypatch.setattr(gateway, 'RayAdminExecutor', AdminSpy)
    failures = []

    def serve():
        try:
            gateway._serve_connection_guarded(local, gateway.ParrotDispatcher(), worker, ray,
                                              initiator=initiator, ready=ready)
        except BaseException as error:
            failures.append(error)

    thread = threading.Thread(target=serve)
    thread.start()
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

    def handshake(flags=0):
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '', wire.handshake_body('envelope'), flags))
        return receive()

    try:
        yield peer, calls, receive, handshake
    finally:
        peer.close()
        thread.join(3)
        if thread.is_alive():
            local.shutdown(socket.SHUT_RDWR)
            thread.join(3)
        assert not thread.is_alive(), 'gateway driver escaped connection cleanup'
        assert not failures, failures


@pytest.mark.parametrize('ft', [wire.FT_ASK, wire.FT_TELL, wire.FT_SYSTEM_EVENT])
@pytest.mark.parametrize('flags', [1, 4, 16, 32, 64, 32768])
def test_unsupported_flags_cannot_reach_business_or_management(monkeypatch, ft, flags):
    with connection(monkeypatch) as (peer, calls, receive, handshake):
        assert handshake().ft == wire.FT_HANDSHAKE_ACK
        payload = struct.pack('<I', 0) + b'mutate' if ft == wire.FT_ASK else b'\x03mutate'
        peer.sendall(wire.build_frame(ft, 2, '/user/test', 'bin:mutate', payload, flags))
        assert receive() is None, 'unsupported envelope was not rejected'
        assert calls == []


def test_unsupported_handshake_flag_cannot_admit_connection(monkeypatch):
    with connection(monkeypatch) as (_, calls, _, handshake):
        assert handshake(1) is None
        assert calls == []


@pytest.mark.parametrize('payload', [b'', b'\x00', b'\xff\xff\xff\xff', b'\x05\x00\x00\x00abc', b'\x01\x00\x00\x00\xff'])
def test_bad_ask_prefix_has_error_no_side_effect_and_same_connection_recovery(monkeypatch, payload):
    with connection(monkeypatch) as (peer, calls, receive, handshake):
        assert handshake().ft == wire.FT_HANDSHAKE_ACK
        peer.sendall(wire.build_frame(wire.FT_ASK, 8, '/user/test', 'bin:mutate', payload))
        reply = receive()
        assert reply.ft == wire.FT_REPLY_ERR and reply.cid == 8 and reply.path == ''
        assert wire.decode_err_payload(reply.payload)[0] == wire.ERR_PROTOCOL_VIOLATION
        assert calls == []
        peer.sendall(wire.build_frame(wire.FT_ASK, 9, '/user/test', 'bin:good', b'\x03\x00\x00\x00/reok'))
        reply = receive()
        assert (reply.ft, reply.cid, reply.path, reply.payload) == (wire.FT_REPLY, 9, '/re', b'ok')
        assert calls == [('ask', 'bin:good', b'ok')]


@pytest.mark.parametrize('key', ['bin:echo', 'bin:fail'])
@pytest.mark.parametrize('reply_path', ['', '/reply', '/回复/😀'])
def test_legal_prefix_and_metadata_flags_preserve_reply_path(monkeypatch, key, reply_path):
    with connection(monkeypatch) as (peer, calls, receive, handshake):
        assert handshake().ft == wire.FT_HANDSHAKE_ACK
        encoded_path = reply_path.encode()
        payload = struct.pack('<I', len(encoded_path)) + encoded_path + b'body'
        peer.sendall(wire.build_frame(wire.FT_ASK, 5, '/user/test', key, payload, 2 | 8))
        reply = receive()
        expected_ft = wire.FT_REPLY_ERR if key == 'bin:fail' else wire.FT_REPLY
        assert (reply.ft, reply.cid, reply.path) == (expected_ft, 5, reply_path)
        assert calls == [('ask', key, b'body')]


def test_ordinary_tell_has_no_ask_prefix(monkeypatch):
    with connection(monkeypatch) as (peer, calls, receive, handshake):
        assert handshake().ft == wire.FT_HANDSHAKE_ACK
        peer.sendall(wire.build_frame(wire.FT_TELL, 0, '/user/test', 'bin:tell', b'raw', 2 | 8))
        peer.sendall(wire.build_frame(wire.FT_HEARTBEAT, 7, '', '', b''))
        assert receive().ft == wire.FT_HEARTBEAT_ACK
        assert calls == [('tell', 'bin:tell', b'raw')]


def test_durable_envelope_cannot_run_without_durable_support(monkeypatch):
    with connection(monkeypatch) as (peer, calls, receive, handshake):
        assert handshake().ft == wire.FT_HANDSHAKE_ACK
        peer.sendall(wire.build_frame(wire.FT_TELL, 3, '/user/test', 'parrot:durable#v1', b'raw'))
        assert receive() is None
        assert calls == []


def test_registration_rejects_unsupported_ack_flags_before_ready(monkeypatch):
    ready = threading.Event()
    with connection(monkeypatch, initiator=True, ready=ready.set) as (peer, calls, receive, _):
        assert receive().ft == wire.FT_HANDSHAKE
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', wire.handshake_ack_body('hub'), 4))
        assert receive() is None
        assert not ready.is_set() and calls == []


@pytest.mark.parametrize('node,extra', [(b'left\0right', b''), (b'peer', b'\x09\x03\0bin')])
def test_malformed_hello_cannot_admit_or_execute_pipelined_work(monkeypatch, node, extra):
    from test_wire_contracts import hello
    with connection(monkeypatch) as (peer, calls, receive, _):
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '', hello(node, extra=extra)) +
                     wire.build_frame(wire.FT_TELL, 0, '/user/test', 'bin:tell', b'mutate'))
        assert receive() is None
        assert calls == []


def test_nul_ack_never_marks_registration_ready(monkeypatch):
    from test_wire_contracts import hello, tlv
    ready = threading.Event()
    with connection(monkeypatch, initiator=True, ready=ready.set) as (peer, calls, receive, _):
        assert receive().ft == wire.FT_HANDSHAKE
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', hello(b'left\0right', extra=tlv(9, b'bin'))) +
                     wire.build_frame(wire.FT_HEARTBEAT, 2, '', '', b''))
        assert receive() is None
        assert not ready.is_set() and calls == []


@pytest.mark.parametrize('initiator', [False, True])
@pytest.mark.parametrize('invalid_tail', [b'\xff', b'\x01\x04\0peer'])
def test_tcp_extension_flood_and_rejected_handshake_pipeline(monkeypatch, initiator, invalid_tail):
    from test_wire_contracts import hello, tlv
    extension = tlv(255, b'future') * 32768
    for tail in (invalid_tail, b''):
        ready = threading.Event()
        with connection(monkeypatch, initiator=initiator, ready=ready.set, tcp=True) as (peer, calls, receive, _):
            if initiator:
                assert receive().ft == wire.FT_HANDSHAKE
            body = hello(extra=(tlv(9, b'bin') if initiator else b'') + extension + tail)
            kind = wire.FT_HANDSHAKE_ACK if initiator else wire.FT_HANDSHAKE
            peer.sendall(wire.build_frame(kind, 1, '', '', body) +
                         wire.build_frame(wire.FT_TELL, 0, '/user/test', 'bin:tell', b'work') +
                         wire.build_frame(wire.FT_HEARTBEAT, 2, '', '', b''))
            if tail:
                assert receive() is None
                assert not ready.is_set() and calls == []
            else:
                if not initiator:
                    assert receive().ft == wire.FT_HANDSHAKE_ACK
                assert receive().ft == wire.FT_HEARTBEAT_ACK
                assert ready.is_set() and calls == [('tell', 'bin:tell', b'work')]
