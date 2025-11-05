"""Real TCP and production connection/dispatcher; Ray RPC is controlled here."""
from contextlib import contextmanager
import socket
import threading
import time
import types

import pytest
from parrot_protocol import ray_gw as gateway, wire


@contextmanager
def session(handler, remote):
    dispatch = gateway.ParrotDispatcher()
    dispatch.handler('probe')(handler)
    dispatch.handler('healthy')(lambda key, body: (key, body))
    state = gateway.RayAdminState()
    listener = socket.socket()
    listener.bind(('127.0.0.1', 0))
    listener.listen(1)
    listener.settimeout(3)
    errors = []

    def ask(path, key, body):
        reply_key, payload = dispatch.dispatch(key, body)
        return dict(ok=True, key=reply_key, payload=payload)

    worker = types.SimpleNamespace(ask=types.SimpleNamespace(
        remote=lambda *args: lambda: ask(*args))) if remote else None
    ray = types.SimpleNamespace(get=lambda ref: ref())

    def serve():
        try:
            accepted, _ = listener.accept()
            gateway._serve_connection_guarded(accepted, dispatch, worker, ray, state)
        except BaseException as error:
            errors.append(error)

    thread = threading.Thread(target=serve)
    thread.start()
    peer = socket.create_connection(listener.getsockname(), timeout=3)
    peer.settimeout(1)
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

    def send(cid, key):
        peer.sendall(wire.build_frame(wire.FT_ASK, cid, '/user/test', key,
                                     wire.with_reply_to_prefix('/reply', b'body')))

    try:
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '', wire.handshake_body('terminal-test')))
        assert receive().ft == wire.FT_HANDSHAKE_ACK
        yield send, receive, state
    finally:
        peer.close()
        listener.close()
        thread.join(4)
        assert not thread.is_alive(), 'connection driver must retire'
        assert not errors, errors
        deadline = time.monotonic() + 2
        while True:
            with state.ask_admission.lock:
                remaining = (state.ask_admission.pending, state.ask_admission.bytes)
            if remaining == (0, 0):
                break
            assert time.monotonic() < deadline, ('request admission leaked', remaining)
            time.sleep(.01)


@pytest.mark.parametrize('remote', [False, True], ids=['local-runtime', 'controlled-ray-rpc'])
@pytest.mark.parametrize('kind', ['system-exit', 'keyboard-interrupt', 'generator-exit',
                                  'business-key-error', 'invalid-key', 'invalid-payload', 'missing-key'])
def test_request_failure_has_accurate_receipt_and_same_connection_recovers(remote, kind):
    entered = threading.Event()
    def handler(key, payload):
        entered.set()
        if kind == 'system-exit':
            raise SystemExit('user exit')
        if kind == 'keyboard-interrupt':
            raise KeyboardInterrupt('user interrupt')
        if kind == 'generator-exit':
            raise GeneratorExit('user generator exit')
        if kind == 'business-key-error':
            raise KeyError('business dictionary lookup')
        if kind == 'invalid-key':
            return '\ud800', payload
        if kind == 'invalid-payload':
            return key, 'not bytes'
        return gateway.ParrotDispatcher().dispatch('missing', payload)

    with session(handler, remote) as (send, receive, state):
        send(2, 'probe')
        assert entered.wait(2), 'business must execute before observing failure'
        reply = receive()
        assert reply is not None and reply.ft == wire.FT_REPLY_ERR and reply.cid == 2
        expected = wire.ERR_UNKNOWN_TYPE_KEY if kind == 'missing-key' else wire.ERR_PROTOCOL_VIOLATION
        code, detail = wire.decode_err_payload(reply.payload)
        assert code == expected, (kind, code, detail)
        assert detail
        send(3, 'healthy')
        reply = receive()
        assert reply.ft == wire.FT_REPLY and reply.cid == 3 and reply.payload == b'body'


@pytest.mark.parametrize('failure', [RuntimeError, SystemExit])
def test_failure_while_building_error_receipt_closes_connection(monkeypatch, failure):
    entered = threading.Event()
    def handler(*_):
        entered.set()
        raise ValueError('user failure')
    def broken_detail(_):
        raise failure('diagnostic failed')
    monkeypatch.setattr(gateway, '_detail', broken_detail)
    with session(handler, False) as (send, receive, _):
        send(2, 'probe')
        assert entered.wait(2)
        assert receive() is None, 'failed diagnostic must not strand caller on live socket'
