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
    (pathlib.Path(__file__).resolve().parents[2] / "docs" / "vectors" / "wire1.json").read_text()
)


def test_vectors_frozen_flag():
    assert VECTORS["frozen"], "vectors must be frozen (Wire 1.0 contract)"


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
        hop_count=vec.get("hop_count", 0),
        hop_limit=vec.get("hop_limit", 8),
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
    assert f.cid == vec["correlation_id"]
    assert f.path == vec["path"]
    assert f.type_key == vec["type_key"]
    assert f.payload.hex() == vec["payload_hex"]
    assert dec.next_frame() is None  # 无残留


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
    assert {1, 4, 5, 6, 7, 8} <= tags, "mandatory TLV fields present"
    fields = dict(parse_tlv(body))
    assert fields[1] == b"py-node-1"
    (caps,) = struct.unpack("<I", fields[4])
    assert caps == 0x02, "python stack is pb-only"
    assert fields[8] == b"pb", "chosen codec"


def test_err_payload_roundtrip_all_codes():
    for code in range(1, 14):
        c, d = decode_err_payload(encode_err_payload(code, f"err-{code}"))
        assert c == code and d == f"err-{code}"
