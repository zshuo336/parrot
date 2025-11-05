"""Malformed admin data must fail before it can become a valid command/reply."""
import json
from pathlib import Path

import pytest
from parrot_protocol import admin_v2 as codec

VECTORS = json.loads((Path(__file__).resolve().parents[3] / 'docs/vectors/admin_v2.json').read_text())['vectors']


@pytest.mark.parametrize('vector', VECTORS, ids=lambda vector: vector['name'])
def test_all_frozen_vector_prefixes_and_trailing_bytes_are_rejected(vector):
    raw = bytes.fromhex(vector['payload_hex'])
    decode = codec.decode_admin_cmd_v2 if raw[0] == 3 else codec.decode_admin_reply_v2
    encode = codec.encode_admin_cmd_v2 if raw[0] == 3 else codec.encode_admin_reply_v2
    assert encode(decode(raw)) == raw
    for length in range(len(raw)):
        with pytest.raises(ValueError):
            decode(raw[:length])
    for suffix in [b'\0', raw, b'\xff']:
        with pytest.raises(ValueError):
            decode(raw + suffix)


def test_truncated_last_string_is_not_a_shorter_successful_status_command():
    raw = codec.encode_admin_cmd_v2({'kind':'status', 'req_id':4, 'path_prefix':'/user/target'})
    with pytest.raises(ValueError):
        codec.decode_admin_cmd_v2(raw[:-1])


def test_truncated_optional_config_is_not_a_shorter_successful_deployment():
    command = {'kind':'deploy', 'req_id':1, 'component':{
        'name':'x', 'version':'1', 'artifact':{'kind':'props', 'factory':'x'},
        'instances':{'kind':'singleton'}, 'config':b'configured'}}
    raw = codec.encode_admin_cmd_v2(command)
    with pytest.raises(ValueError):
        codec.decode_admin_cmd_v2(raw[:-1])


@pytest.mark.parametrize('tag', [2, 250, 251, 255])
def test_invalid_option_discriminant_is_rejected(tag):
    with pytest.raises(ValueError):
        codec.read_artifact(memoryview(bytes([1, 1, ord('x'), tag])), 0)
    with pytest.raises(ValueError):
        codec.read_opt_bytes(memoryview(bytes([tag, 0])), 0)


@pytest.mark.parametrize('value', [-1, 1 << 64, 3.5, True])
def test_encoder_requires_unsigned_64_bit_integer(value):
    with pytest.raises(ValueError):
        codec.put_varint(value)


def test_metrics_component_count_is_independent_of_number_of_instance_states():
    message = {'kind':'metrics', 'req_id':8, 'snapshot':{
        'components':1, 'component_states':[
            {'path':'/user/pool-0', 'state':'running', 'version':'1'},
            {'path':'/user/pool-1', 'state':'running', 'version':'1'}]}}
    sample = codec.decode_admin_reply_v2(codec.encode_admin_reply_v2(message))['snapshot']
    assert sample['components'] == 1
    assert len(sample['component_states']) == 2


def test_impossible_vector_count_and_utf8_are_rejected():
    # STATUS reply with a u64-max number of states and no data.
    with pytest.raises(ValueError):
        codec.decode_admin_reply_v2(b'\x04\x03\x01' + codec.put_varint((1 << 64)-1))
    with pytest.raises(ValueError):
        codec.decode_admin_cmd_v2(b'\x03\x03\x01\x01\xff')


def test_truncated_stop_is_rejected_before_admin_dispatch(factory):
    import queue
    from parrot_protocol.ray_gw import RayAdminExecutor
    from test_gateway_registry import LocalRay, component

    out = queue.Queue()
    executor = RayAdminExecutor(out, LocalRay())
    assert executor.deploy(component())['kind'] == 'deployed'
    raw = codec.encode_admin_cmd_v2({'kind':'stop', 'req_id':9, 'path_prefix':'/user/shared!'})
    try:
        assert executor.on_frame(9, 'admin', raw[:-1])
        from parrot_protocol import wire
        decoder = wire.FrameDecoder()
        decoder.feed(out.get(timeout=1))
        frame = decoder.next_frame()
        reply = codec.decode_admin_reply_v2(frame.payload)
        assert frame.cid == reply['req_id'] == 9
        assert reply['kind'] == 'failed' and reply['code'] == codec.ERR_DIALECT_MISMATCH
        assert executor.status('/user/shared')['states'][0]['state'] == 'running'
    finally:
        executor.stop('/user/shared')


# Import only the owned component factory fixture; no Ray runtime is needed.
from test_gateway_registry import factory


@pytest.mark.parametrize("code", [65536, (1 << 64)-1])
def test_failed_reply_code_respects_rust_u16(code):
    with pytest.raises(ValueError):
        codec.encode_admin_reply_v2({"kind":"failed", "req_id":1, "code":code, "detail":"x"})
    with pytest.raises(ValueError):
        codec.decode_admin_reply_v2(b"\x04\x04\x01" + codec.put_varint(code) + b"\x01x")


def test_dylib_abi_respects_rust_u32():
    artifact = {"kind":"dylib", "digest":"d", "uri":"u", "abi":1 << 32}
    with pytest.raises(ValueError):
        codec.put_artifact(artifact)
    with pytest.raises(ValueError):
        codec.read_artifact(memoryview(b"\x05\x01d\x01u" + codec.put_varint(1 << 32)), 0)


def test_metrics_counters_are_not_silently_truncated_from_floats():
    with pytest.raises(ValueError):
        codec.encode_admin_reply_v2({"kind":"metrics", "req_id":1, "snapshot":{"asks_rx":2.5}})
