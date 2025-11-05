"""golden vectors 对齐（docs/vectors/wire1.json 逐字节）+ 半包语义。"""

import json
import pathlib
import struct

import pytest

from parrot_protocol import (
    FT_ASK,
    FT_REPLY,
    FT_REPLY_ERR,
    FrameDecoder,
    build_frame,
    decode_err_payload,
    encode_err_payload,
    handshake_ack_body,
    parse_tlv,
)

VECTORS = json.loads(
    (pathlib.Path(__file__).resolve().parents[3] / "docs" / "vectors" / "wire1.json").read_text()
)


def test_vectors_frozen_flag():
    assert VECTORS["format"] == "parrot-wire-1.0"
    assert VECTORS["frozen"] is True, "vectors must be frozen (Wire 1.0 contract)"
    names = [vec["name"] for vec in VECTORS["vectors"]]
    assert len(names) == len(set(names)), "duplicate shared vector name"
    assert {"ask-basic", "tell-basic", "reply-err-stopped", "heartbeat"} <= set(names)


@pytest.mark.parametrize(
    "vec",
    VECTORS["vectors"],
    ids=[v["name"] for v in VECTORS["vectors"]],
)
def test_golden_vector_roundtrip(vec):
    """逐字节重编码 == golden bytes。"""
    frame = build_frame(
        ft=int(vec["frame_type"], 16),
        cid=vec["correlation_id"],
        path=vec["path"],
        type_key=vec["type_key"],
        payload=bytes.fromhex(vec["payload_hex"]),
        flags=vec.get("flags", 0),
        hop_count=vec.get("hop_count", 0),
        hop_limit=vec.get("hop_limit", 8),
        sequence=vec.get("sequence", 0),
    )
    assert frame.hex() == vec["bytes_hex"], f"vector {vec['name']} byte mismatch"


@pytest.mark.parametrize(
    "vec",
    VECTORS["vectors"],
    ids=[v["name"] for v in VECTORS["vectors"]],
)
def test_golden_vector_decode(vec):
    """golden bytes → 解析字段一致。"""
    dec = FrameDecoder()
    dec.feed(bytes.fromhex(vec["bytes_hex"]))
    f = dec.next_frame()
    assert f is not None
    assert f.ft == int(vec["frame_type"], 16)
    assert f.flags == vec.get("flags", 0)
    assert f.cid == vec["correlation_id"]
    assert f.hop_count == vec.get("hop_count", 0)
    assert f.hop_limit == vec.get("hop_limit", 8)
    assert f.sequence == vec.get("sequence", 0)
    assert f.path == vec["path"]
    assert f.type_key == vec["type_key"]
    assert f.payload.hex() == vec["payload_hex"]
    assert f.encode().hex() == vec["bytes_hex"]
    assert dec.next_frame() is None  # 无残留


@pytest.mark.parametrize("vec", VECTORS["vectors"], ids=[v["name"] for v in VECTORS["vectors"]])
def test_golden_vector_all_split_points(vec):
    encoded = bytes.fromhex(vec["bytes_hex"])
    for split in range(len(encoded)):
        decoder = FrameDecoder()
        decoder.feed(encoded[:split])
        assert decoder.next_frame() is None, (vec["name"], split)
        decoder.feed(encoded[split:])
        frame = decoder.next_frame()
        assert frame is not None, (vec["name"], split)
        assert frame.encode() == encoded, (vec["name"], split)
        assert decoder.next_frame() is None, (vec["name"], split)


def test_shared_vectors_pipeline_preserves_order_and_bytes():
    encoded = [bytes.fromhex(vec["bytes_hex"]) for vec in VECTORS["vectors"]]
    decoder = FrameDecoder()
    decoder.feed(b"".join(encoded))
    for vec, expected in zip(VECTORS["vectors"], encoded):
        frame = decoder.next_frame()
        assert frame is not None, vec["name"]
        assert frame.encode() == expected, vec["name"]
    assert decoder.next_frame() is None


def test_partial_feed_semantics():
    """半包：分片喂入不丢字节、不提前出帧。"""
    frame = build_frame(FT_ASK, 7, "/user/x", "bin:t::M", struct.pack("<Q", 42))
    dec = FrameDecoder()
    for i in range(len(frame)):
        dec.feed(frame[i : i + 1])
        if i < len(frame) - 1:
            assert dec.next_frame() is None, "must not emit before complete"
    f = dec.next_frame()
    assert f is not None and f.cid == 7


def test_two_frames_pipeline():
    """连续两帧一次 feed——依序完整取出。"""
    f1 = build_frame(FT_REPLY, 1, "", "bin:t::R", b"\x01\x02")
    f2 = build_frame(FT_REPLY_ERR, 2, "", "", encode_err_payload(7, "route unreachable"))
    dec = FrameDecoder()
    dec.feed(f1 + f2)
    a = dec.next_frame()
    b = dec.next_frame()
    assert a.cid == 1 and b.cid == 2
    code, detail = decode_err_payload(b.payload)
    assert code == 7 and "route" in detail


def test_handshake_tlv_layout():
    """TLV 握手体：与 Rust/JVM 同源布局（tag u8 + len u16 LE）。"""
    body = handshake_ack_body("py-node-1")
    tags = {t for t, _ in parse_tlv(body)}
    assert {1, 4, 5, 6, 7, 9} <= tags, "mandatory TLV fields present"
    fields = dict(parse_tlv(body))
    assert fields[1] == b"py-node-1"
    (caps,) = struct.unpack("<I", fields[4])
    assert caps == 0x03, "python stack is bin|pb (crawler-lab dual-stack)"
    assert 8 not in fields, "tag 8 is optional DIRECT_ADDR, not the chosen codec"
    assert fields[9] == b"bin", "tag 9 matches Rust HandshakeAckBody"


def test_err_payload_roundtrip_all_codes():
    for code in range(1, 14):
        c, d = decode_err_payload(encode_err_payload(code, f"err-{code}"))
        assert c == code and d == f"err-{code}"


ERROR_CASES = [line.split('\t') for line in
               (pathlib.Path(__file__).resolve().parents[3] / 'docs/vectors/error-payloads.tsv')
               .read_text().splitlines() if not line.startswith('#')]
assert len(ERROR_CASES) == 30


@pytest.mark.parametrize('name,result,payload,code,detail', ERROR_CASES, ids=[r[0] for r in ERROR_CASES])
def test_shared_error_payload_corpus(name, result, payload, code, detail):
    data = bytes.fromhex(payload)
    if result == 'reject':
        with pytest.raises(ValueError):
            decode_err_payload(data)
    else:
        assert result == 'accept'
        actual_code, actual_detail = decode_err_payload(data)
        assert actual_code == int(code)
        assert actual_detail.encode('utf-8') == bytes.fromhex(detail)
        assert encode_err_payload(actual_code, actual_detail) == data


@pytest.mark.parametrize('code', [0, 14, -1, 65536, 1.5, True])
def test_error_encoder_rejects_invalid_codes(code):
    with pytest.raises(ValueError):
        encode_err_payload(code, 'error')
