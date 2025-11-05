"""Count actual complete Wire IO, not queued replies or payload-only estimates."""
import socket
import threading
import types

import pytest
from parrot_protocol import ray_gw as gateway, wire


@pytest.fixture
def metrics(monkeypatch):
    owner = gateway._GatewayMetrics()
    monkeypatch.setattr(gateway, 'METRICS', owner)
    return owner


def recv_exact(peer, size):
    result = b''
    while len(result) < size:
        chunk = peer.recv(size - len(result))
        assert chunk
        result += chunk
    return result


def test_complete_writer_counts_headers_unicode_management_and_error_replies(metrics):
    local, peer = socket.socketpair()
    peer.settimeout(2)
    out = gateway._ConnectionOutbox(local)
    frames = [wire.build_frame(wire.FT_REPLY, 1, '/路', 'bin:回', b'payload'),
              wire.build_frame(wire.FT_REPLY_ERR, 2, '', '', wire.encode_err_payload(6, 'unknown')),
              wire.build_frame(wire.FT_SYSTEM_EVENT, 3, 'admin', '', b'\x04\x02\x03')]
    try:
        for frame in frames:
            out.put(frame)
        expected = b''.join(frames)
        assert recv_exact(peer, len(expected)) == expected
        out.close()  # Join the writer before observing its completed event.
        sample = metrics.snapshot()
        assert sample['bytes_tx'] == len(expected)
        assert sample['replies_tx'] == 2 and sample['reply_errs'] == 1
        assert sample['bytes_rx'] == 0
    finally:
        out.close()
        local.close()
        peer.close()


def test_partial_sendall_failure_does_not_report_a_complete_frame(metrics):
    local, peer = socket.socketpair()
    peer.settimeout(2)

    class PartialSocket:
        def sendall(self, frame):
            local.sendall(frame[:7])
            raise BrokenPipeError('injected failure after actual partial transfer')

        def shutdown(self, how):
            local.shutdown(how)

    out = gateway._ConnectionOutbox(PartialSocket())
    frame = wire.build_frame(wire.FT_REPLY_ERR, 1, '', '', b'failed')
    try:
        out.put(frame)
        assert recv_exact(peer, 7) == frame[:7]
        assert peer.recv(1) == b''
        out.close()
        sample = metrics.snapshot()
        assert sample['bytes_tx'] == sample['replies_tx'] == sample['reply_errs'] == 0
    finally:
        out.close()
        local.close()
        peer.close()


def test_actual_connection_counts_all_received_and_successfully_written_frames(metrics):
    local, peer = socket.socketpair()
    peer.settimeout(3)
    errors = []

    def reject(*_):
        raise KeyError('missing business key')

    worker = types.SimpleNamespace(
        ask=types.SimpleNamespace(remote=lambda *args: lambda: reject(*args)),
        deliver=types.SimpleNamespace(remote=lambda *args: None))
    ray = types.SimpleNamespace(get=lambda ref: ref())

    def serve():
        try:
            gateway._serve_connection_guarded(local, gateway.ParrotDispatcher(), worker, ray)
        except BaseException as error:
            errors.append(error)

    thread = threading.Thread(target=serve)
    thread.start()
    frames = [wire.build_frame(wire.FT_HANDSHAKE, 1, '', '__handshake__', wire.handshake_body('metrics')),
              wire.build_frame(wire.FT_TELL, 2, '/路', 'bin:回', b'tell'),
              wire.build_frame(wire.FT_ASK, 3, '/路', 'bin:不存在', wire.with_reply_to_prefix('/reply', b'ask')),
              wire.build_frame(wire.FT_SYSTEM_EVENT, 4, 'admin', '', b'\x00'),
              wire.build_frame(wire.FT_HEARTBEAT, 5, '', '', b'')]
    received = bytearray()
    decoder = wire.FrameDecoder()
    kinds = []
    try:
        # Coalesced input still consists of five separate decoded Wire frames.
        peer.sendall(b''.join(frames))
        while len(kinds) < 3:
            chunk = peer.recv(65536)
            assert chunk
            received.extend(chunk)
            decoder.feed(chunk)
            while (frame := decoder.next_frame()) is not None:
                kinds.append(frame.ft)
                if frame.ft == wire.FT_REPLY_ERR:
                    assert frame.cid == 3 and frame.path == '/reply'
                    assert wire.decode_err_payload(frame.payload)[0] == wire.ERR_PROTOCOL_VIOLATION
        assert sorted(kinds) == sorted([wire.FT_HANDSHAKE_ACK, wire.FT_REPLY_ERR, wire.FT_HEARTBEAT_ACK])
    finally:
        peer.close()
        thread.join(5)
        if thread.is_alive():
            local.shutdown(socket.SHUT_RDWR)
            thread.join(5)
        assert not thread.is_alive()
        assert not errors, errors
    sample = metrics.snapshot()
    assert sample['bytes_rx'] == sum(len(frame) for frame in frames)
    assert sample['bytes_tx'] == len(received)
    assert sample['asks_rx'] == sample['tells_rx'] == sample['heartbeats_rx'] == 1
    assert sample['replies_tx'] == sample['reply_errs'] == 1
    assert sample['connections'] == 0


def test_cumulative_counters_saturate_instead_of_overflowing_wire_u64(metrics):
    metrics.bump('bytes_tx', (1 << 64)-2)
    metrics.bump('bytes_tx', 5)
    assert metrics.snapshot()['bytes_tx'] == (1 << 64)-1
