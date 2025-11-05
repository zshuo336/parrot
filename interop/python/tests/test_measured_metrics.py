"""Measured metrics: independent wire bytes and actual connection lifetimes."""
from contextlib import contextmanager
import json
from pathlib import Path
import socket
import struct
import sys
import threading
import types

import pytest
from parrot_protocol import admin_v2 as admin, ray_gw as gateway, wire
from test_gateway_registry import connection

VECTORS = json.loads((Path(__file__).resolve().parents[3] / 'docs/vectors/measured_metrics.json').read_text())['vectors']


@pytest.mark.parametrize('vector', VECTORS, ids=lambda v: v['name'])
def test_independent_rust_measured_vectors_and_all_truncations(vector):
    raw = bytes.fromhex(vector['payload_hex'])
    decode = admin.decode_admin_cmd_v2 if raw[0] == 3 else admin.decode_admin_reply_v2
    encode = admin.encode_admin_cmd_v2 if raw[0] == 3 else admin.encode_admin_reply_v2
    result = decode(raw)
    assert encode(result) == raw
    if raw[0] == 4:
        assert result['metrics']['available'] == 1 << 5
        assert result['metrics']['snapshot']['asks_rx'] == 0
        assert result['metrics']['epoch'] == 'owner-a'
    for cut in range(len(raw)):
        with pytest.raises(ValueError):
            decode(raw[:cut])
    with pytest.raises(ValueError):
        decode(raw + b'\0')


@pytest.mark.parametrize('field,value', [('available',1 << 17), ('available',-1),
    ('epoch','界' * 43), ('process_kind','x' * 129), ('process_kind','')])
def test_invalid_metrics_metadata_is_rejected(field, value):
    metrics = {'snapshot':{}, 'available':1 << 14, 'epoch':'owner', 'process_kind':'python_threads'}
    metrics[field] = value
    with pytest.raises(ValueError):
        admin.encode_admin_reply_v2({'kind':'measured_metrics', 'req_id':1, 'metrics':metrics})


@pytest.fixture
def metrics(monkeypatch):
    owner = gateway._GatewayMetrics()
    monkeypatch.setattr(gateway, 'METRICS', owner)
    return owner


def test_known_zero_missing_rss_epoch_and_process_unit(metrics, monkeypatch):
    def forbidden():
        raise AssertionError('metrics must never query the Ray cluster')
    monkeypatch.setitem(sys.modules, 'ray', types.SimpleNamespace(state=types.SimpleNamespace(actors=forbidden)))
    monkeypatch.setattr(gateway, '_resident_bytes', lambda: None)
    first = metrics.measured()
    assert first['available'] & (1 << 5) and first['snapshot']['asks_rx'] == 0
    assert not first['available'] & (1 << 15)
    assert not first['available'] & ((1 << 12) | (1 << 13))
    assert first['process_kind'] == 'python_threads'
    assert first['snapshot']['processes'] == threading.active_count()
    assert metrics.measured()['epoch'] == first['epoch']
    assert gateway._GatewayMetrics().measured()['epoch'] != first['epoch']
    monkeypatch.setattr(gateway, '_resident_bytes', lambda: 0)
    zero = metrics.measured()
    assert zero['available'] & (1 << 15) and zero['snapshot']['memory_rss'] == 0


def test_real_admin_connections_share_epoch_and_retire_gauge(metrics):
    registry, dispatcher = gateway.RayAdminState(), gateway.ParrotDispatcher()
    samples = []
    for attempt in (1, 2):
        with connection(registry, dispatcher) as client:
            result = client('measured_metrics', attempt)['metrics']
            samples.append(result)
            snapshot = result['snapshot']
            assert snapshot['connections'] == 1 and snapshot['handshakes_ok'] == attempt
            assert snapshot['handshakes_failed'] == 0
            assert result['available'] & ((1 << 12) | (1 << 13)) == (1 << 12) | (1 << 13)
            assert snapshot['components'] == 0 and snapshot['component_states'] == []
        assert metrics.snapshot()['connections'] == 0
    assert samples[0]['epoch'] == samples[1]['epoch']
    assert samples[1]['snapshot']['bytes_rx'] > samples[0]['snapshot']['bytes_rx']


@contextmanager
def driver(**options):
    local, peer = socket.socketpair()
    peer.settimeout(2)
    errors = []
    def serve():
        try:
            gateway._serve_connection_guarded(local, gateway.ParrotDispatcher(), None, None, **options)
        except BaseException as error:
            errors.append(error)
    thread = threading.Thread(target=serve)
    thread.start()
    try:
        yield peer
    finally:
        peer.close()
        thread.join(3)
        if thread.is_alive():
            local.shutdown(socket.SHUT_RDWR)
            thread.join(3)
        assert not thread.is_alive()
        assert not errors, errors


def read_frame(peer):
    decoder = wire.FrameDecoder()
    while True:
        chunk = peer.recv(65536)
        assert chunk
        decoder.feed(chunk)
        frame = decoder.next_frame()
        if frame is not None:
            return frame


def test_initiator_not_ready_until_valid_ack_and_counts_failure_on_wrong_peer(metrics):
    ready = threading.Event()
    with driver(initiator=True, ready=ready.set, expected_node='upstream') as peer:
        handshake = read_frame(peer)
        assert handshake.ft == wire.FT_HANDSHAKE
        assert wire.decode_handshake(handshake.payload)['capabilities'] & wire.CAPS_MEASURED_METRICS
        assert not ready.is_set() and metrics.snapshot()['connections'] == 0
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '__handshake__', wire.handshake_ack_body('wrong')))
        assert peer.recv(1) == b''
        assert not ready.is_set()
    assert metrics.snapshot()['handshakes_failed'] == 1
    with driver(initiator=True, ready=ready.set, expected_node='upstream') as peer:
        read_frame(peer)
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '__handshake__', wire.handshake_ack_body('upstream')))
        assert ready.wait(2)
        assert metrics.snapshot()['connections'] == 1
    assert metrics.snapshot()['connections'] == 0 and metrics.snapshot()['handshakes_ok'] == 1


@pytest.mark.parametrize('frame_type,payload', [
    (wire.FT_TELL, wire.handshake_body('wrong-frame-type')),
    (wire.FT_HANDSHAKE, b''),
    (wire.FT_HANDSHAKE, wire.handshake_body('truncated') + b'\x09'),
    (wire.FT_HANDSHAKE, wire.handshake_body('duplicate') + b'\x04\x04\0\x03\0\0\0'),
])
def test_bad_handshake_never_enters_active_gauge(metrics, frame_type, payload):
    with driver() as peer:
        peer.sendall(wire.build_frame(frame_type, 1, '', '__handshake__', payload))
        assert peer.recv(1) == b''
    sample = metrics.snapshot()
    assert sample['connections'] == sample['handshakes_ok'] == 0
    assert sample['handshakes_failed'] == 1


def test_partial_handshake_times_out_and_repeated_handshake_is_not_a_second_success(metrics):
    with driver(handshake_timeout=0.05) as peer:
        peer.sendall(b'\x64')
        assert peer.recv(1) == b''
    assert metrics.snapshot()['handshakes_failed'] == 1
    with driver() as peer:
        handshake = wire.build_frame(wire.FT_HANDSHAKE, 1, '', '__handshake__', wire.handshake_body('client'))
        peer.sendall(handshake)
        ack = read_frame(peer)
        fields = dict(wire.parse_tlv(ack.payload))
        assert 8 not in fields and fields[9] == b'bin'
        peer.sendall(handshake)
        assert peer.recv(1) == b''
    sample = metrics.snapshot()
    assert sample['connections'] == 0 and sample['handshakes_ok'] == sample['handshakes_failed'] == 1


def test_direct_children_are_bounded_deduplicated_and_joined(metrics):
    listener = socket.socket()
    listener.bind(('127.0.0.1',0))
    listener.listen(4)
    listener.settimeout(2)
    links = gateway._DirectLinks(gateway.ParrotDispatcher(), None, None, gateway.RayAdminState(), max_links=1)
    address = f'127.0.0.1:{listener.getsockname()[1]}'.encode()
    def hint(node):
        return struct.pack('<H',len(node)) + node + struct.pack('<H',len(address)) + address
    peer = None
    try:
        links.hint(hint(b'child'))
        peer, _ = listener.accept()
        peer.settimeout(2)
        assert read_frame(peer).ft == wire.FT_HANDSHAKE
        links.hint(hint(b'child'))
        links.hint(hint(b'other'))
        listener.settimeout(0.1)
        with pytest.raises(socket.timeout):
            listener.accept()
        links.close()
        assert peer.recv(1) == b''
        assert metrics.snapshot()['handshakes_failed'] == 1
        assert metrics.snapshot()['connections'] == 0
        links.close()
    finally:
        links.close()
        if peer is not None:
            peer.close()
        listener.close()


def test_negotiated_frame_limit_is_enforced_before_enqueue(metrics):
    local, peer = socket.socketpair()
    peer.settimeout(2)
    out = gateway._ConnectionOutbox(local)
    out.max_frame_len = wire.BODY_FIXED_OVERHEAD
    try:
        out.put(wire.build_frame(wire.FT_REPLY, 1, '', '', b'too-large'))
        assert peer.recv(1) == b''
        out.close()
        assert metrics.snapshot()['bytes_tx'] == 0
    finally:
        out.close()
        local.close()
        peer.close()


@pytest.mark.parametrize('data,expected', [(b'VmRSS: 0 kB\n', 0), (b'VmRSS: 12 kB\n', 12288),
    (b'VmRSS: nope kB\n', None), (b'VmRSS: 12 MB\n', None),
    (b'VmRSS: -1 kB\n', None), (b'x' * 65537, None)])
def test_rss_reader_is_bounded_and_closes_source(monkeypatch, data, expected):
    import io
    source = io.BytesIO(data)
    monkeypatch.setattr('builtins.open', lambda *args, **kwargs: source)
    assert gateway._resident_bytes() == expected
    assert source.closed


def test_owned_direct_connection_serves_measured_requests_and_retires(metrics):
    listener = socket.socket()
    listener.bind(('127.0.0.1',0))
    listener.listen(1)
    listener.settimeout(2)
    links = gateway._DirectLinks(gateway.ParrotDispatcher(), None, None, gateway.RayAdminState())
    address = f'127.0.0.1:{listener.getsockname()[1]}'.encode()
    peer = None
    try:
        links.hint(b'\x05\0child' + struct.pack('<H',len(address)) + address)
        peer, _ = listener.accept()
        peer.settimeout(2)
        read_frame(peer)
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE_ACK,1,'','__handshake__',wire.handshake_ack_body('child')))
        peer.sendall(wire.build_frame(wire.FT_SYSTEM_EVENT,2,'admin','',b'\x03\x05\x02'))
        reply = admin.decode_admin_reply_v2(read_frame(peer).payload)
        assert reply['kind'] == 'measured_metrics' and reply['req_id'] == 2
        assert reply['metrics']['snapshot']['connections'] == 1
        links.close()
        assert peer.recv(1) == b''
        assert metrics.snapshot()['connections'] == 0
        assert metrics.snapshot()['handshakes_ok'] == 1
    finally:
        links.close()
        if peer is not None:
            peer.close()
        listener.close()
