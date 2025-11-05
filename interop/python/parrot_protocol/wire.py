"""Parrot Wire 1.0 帧编解码（Python 栈）。

布局（与 Rust / JVM / TS 的 Wire 1.0 字段一致）：
  外层: [u32 body_len LE][body]
  body: [ver u8][ft u8][flags u16 LE][cid u64 LE][hop_count u8][hop_limit u8]
        [sequence u32 LE][reserved u16=0][path_len u32 LE][path]
        [key_len u32 LE][key][payload]

sequence=0 表示不参与重排。编解码保留该字段，但不实现执行端重排或持久确认。
"""

from __future__ import annotations

import struct
from dataclasses import dataclass, replace
from typing import Optional
from .trace import TraceParent, current_parent

WIRE_VERSION = 1
MAX_FRAME_LEN = 16 * 1024 * 1024
DEFAULT_HANDSHAKE_FRAME_LEN = 1 << 20
BODY_FIXED_OVERHEAD = 28
_KNOWN_FRAME_TYPES = frozenset((1, 2, 3, 4, 0x10, 0x11, 0x12, 0x13,
                              0x14, 0x15, 0x20, 0x21, 0x22, 0x23, 0x24, 0x7F))

# 帧类型（07 §2.1 全集；Python 网关数据面只用到后四种）
FT_HANDSHAKE = 0x01
FT_HANDSHAKE_ACK = 0x02
FT_HEARTBEAT = 0x03
FT_HEARTBEAT_ACK = 0x04
FT_SYSTEM_EVENT = 0x20  # 管理通道（Rust frame.rs frame_type::SYSTEM_EVENT——B3 对齐修正：0x05 是 HEARTBEAT_ACK 段，错值会让 Rust 发来的 admin 帧被静默丢弃）
FT_ASK = 0x10
FT_REPLY = 0x11
FT_REPLY_ERR = 0x12
FT_TELL = 0x13
FT_ROUTE_HINT = 0x24  # 方案 A：hub 注入目标直连地址（学习后直拨）
FT_ERROR = 0x7F  # Connection-terminal protocol error, distinct from REPLY_ERR.

# flags 位
FLAG_TELL_ACK = 0x0010  # Rust frame::flags::TELL_ACK; bit 0 is compression.
FLAG_TRACE_CONTEXT = 1 << 6
TRACE_CONTEXT_BYTES = 25

# 错误码（与 error.rs ErrCode 一致）
ERR_ACTOR_NOT_FOUND = 1
ERR_TIMEOUT = 2
ERR_STOPPED = 3
ERR_NOT_REMOTABLE = 4
ERR_CODEC_ERROR = 5
ERR_UNKNOWN_TYPE_KEY = 6
ERR_ROUTE_UNREACHABLE = 7
ERR_CONNECTION_LOST = 8
ERR_DIRECTORY_STALE = 9
ERR_OVERLOADED = 10
ERR_NO_COMMON_CODEC = 11
ERR_PROTOCOL_VIOLATION = 12
ERR_FORBIDDEN = 13


def _uint(value: int, maximum: int, name: str) -> None:
    if not isinstance(value, int) or not 0 <= value <= maximum:
        raise ValueError(f"invalid {name}")


@dataclass
class Frame:
    ft: int
    cid: int
    path: str
    type_key: str
    payload: bytes
    flags: int = 0
    hop_count: int = 0
    hop_limit: int = 8
    sequence: int = 0

    def with_trace_parent(self, parent: TraceParent):
        if not isinstance(parent, TraceParent):
            raise TypeError("expected TraceParent")
        if self.ft not in (FT_ASK, FT_TELL) or self.flags & (4 | FLAG_TRACE_CONTEXT):
            raise ValueError("causal context requires an unmarked single request")
        if (BODY_FIXED_OVERHEAD + len(self.path.encode()) + len(self.type_key.encode()) +
                len(self.payload) + TRACE_CONTEXT_BYTES > MAX_FRAME_LEN):
            raise ValueError("causal context exceeds frame limit")
        return replace(self, flags=self.flags | FLAG_TRACE_CONTEXT,
                       payload=parent.encode() + self.payload)

    def take_trace_parent(self):
        if not self.flags & FLAG_TRACE_CONTEXT:
            return None
        if self.ft not in (FT_ASK, FT_TELL) or self.flags & 4:
            raise ValueError("invalid causal context frame")
        parent = TraceParent.decode(self.payload[:TRACE_CONTEXT_BYTES])
        self.payload = self.payload[TRACE_CONTEXT_BYTES:]
        self.flags &= ~FLAG_TRACE_CONTEXT
        return parent

    def encode(self) -> bytes:
        if self.ft not in _KNOWN_FRAME_TYPES:
            raise ValueError("unknown frame type")
        for name, value, maximum in (("frame type", self.ft, 255),
                                     ("flags", self.flags, 65535),
                                     ("correlation id", self.cid, (1 << 64) - 1),
                                     ("hop count", self.hop_count, 255),
                                     ("hop limit", self.hop_limit, 255),
                                     ("sequence", self.sequence, (1 << 32) - 1)):
            _uint(value, maximum, name)
        if not 0 < self.hop_limit <= 255 or not 0 <= self.hop_count < self.hop_limit:
            raise ValueError("hop limit exhausted")
        path = self.path.encode()
        key = self.type_key.encode()
        body_len = 12 + 2 + 6 + 4 + 4 + len(path) + len(key) + len(self.payload)
        if body_len < BODY_FIXED_OVERHEAD or body_len > MAX_FRAME_LEN:
            raise ValueError(f"frame too large: {body_len}")
        # Fixed header: 12B + hop(2B) + sequence(4B) + reserved(2B).
        head = struct.pack(
            "<BBHQ",
            WIRE_VERSION,
            self.ft,
            self.flags,
            self.cid,
        )
        hop = struct.pack("<BBIH", self.hop_count, self.hop_limit, self.sequence, 0)
        return (
            struct.pack("<I", body_len)
            + head
            + hop
            + struct.pack("<I", len(path))
            + path
            + struct.pack("<I", len(key))
            + key
            + self.payload
        )


def build_frame(
    ft: int,
    cid: int,
    path: str,
    type_key: str,
    payload: bytes,
    flags: int = 0,
    hop_count: int = 0,
    hop_limit: int = 8,
    sequence: int = 0,
) -> bytes:
    return Frame(ft, cid, path, type_key, payload, flags, hop_count, hop_limit, sequence).encode()


class FrameDecoder:
    """非消费式半包解码器（与 JVM WireDecoder / Rust read_frame 同语义）。"""

    def __init__(self, max_frame_len: int = MAX_FRAME_LEN) -> None:
        if not BODY_FIXED_OVERHEAD <= max_frame_len <= MAX_FRAME_LEN:
            raise ValueError("invalid frame decoder limit")
        self.max_frame_len = max_frame_len
        self.buf = bytearray()

    def feed(self, data: bytes) -> None:
        self.buf.extend(data)

    def next_frame(self) -> Optional[Frame]:
        if len(self.buf) < 4:
            return None
        (body_len,) = struct.unpack_from("<I", self.buf, 0)
        if not BODY_FIXED_OVERHEAD <= body_len <= self.max_frame_len:
            raise ValueError(f"invalid frame length {body_len}")
        if len(self.buf) < 4 + body_len:
            return None
        body = bytes(self.buf[4 : 4 + body_len])
        del self.buf[: 4 + body_len]
        return _parse_body(body)


def _parse_body(body: bytes) -> Frame:
    if not BODY_FIXED_OVERHEAD <= len(body) <= MAX_FRAME_LEN:
        raise ValueError(f"invalid frame length {len(body)}")
    ver, ft, flags, cid = struct.unpack_from("<BBHQ", body, 0)
    if ver != WIRE_VERSION:
        raise ValueError(f"unsupported wire version {ver}")
    if ft not in _KNOWN_FRAME_TYPES:
        raise ValueError(f"unknown frame type {ft}")
    hop_count, hop_limit = struct.unpack_from("<BB", body, 12)
    if hop_limit == 0 or hop_count >= hop_limit:
        raise ValueError("hop limit exhausted")
    # Low 32 bits carry the Rust sequence extension; only the high 16 are reserved.
    if body[18:20] != b"\x00\x00":
        raise ValueError("reserved high bits must be zero")
    (path_len,) = struct.unpack_from("<I", body, 20)
    if path_len > len(body) - BODY_FIXED_OVERHEAD:
        raise ValueError("path exceeds frame body")
    path = body[24 : 24 + path_len].decode()
    off = 24 + path_len
    (key_len,) = struct.unpack_from("<I", body, off)
    if key_len > len(body) - off - 4:
        raise ValueError("type key exceeds frame body")
    key = body[off + 4 : off + 4 + key_len].decode()
    payload = body[off + 4 + key_len :]
    return Frame(ft, cid, path, key, payload, flags, hop_count, hop_limit,
                 struct.unpack_from("<I", body, 14)[0])


# ---------------- 错误体（与 error.rs encode_err_payload 一致） ----------------


def encode_err_payload(code: int, detail: str) -> bytes:
    if type(code) is not int or not 1 <= code <= 13:
        raise ValueError("unknown error code")
    d = detail.encode()
    return struct.pack("<HH", code, 0) + d


def decode_err_payload(b: bytes) -> tuple[int, str]:
    if len(b) < 4 or b[2] or b[3]:
        raise ValueError("invalid error envelope")
    (code,) = struct.unpack_from("<H", b, 0)
    if not 1 <= code <= 13:
        raise ValueError("unknown error code")
    return (code, b[4:].decode("utf-8"))


# ---------------- ASK reply_to 前缀（与 frame.rs split_reply_to 一致） ----------------


def with_reply_to_prefix(reply_to: str, payload: bytes) -> bytes:
    rb = reply_to.encode()
    return struct.pack("<I", len(rb)) + rb + payload


def split_reply_to(b: bytes) -> Optional[tuple[str, bytes]]:
    if len(b) < 4:
        return None
    (rlen,) = struct.unpack_from("<I", b, 0)
    if len(b) < 4 + rlen:
        return None
    return (b[4 : 4 + rlen].decode(), b[4 + rlen :])


# ---------------- 握手 TLV（tag u8 + len u16 LE + value） ----------------

TLV_NODE_ID = 1
TLV_REALM = 2
TLV_CLUSTER = 3
TLV_CAPABILITIES = 4
TLV_MAX_FRAME_LEN = 5
TLV_TOPOLOGY_ROLE = 6
TLV_HOP_LIMIT = 7
TLV_DIRECT_ADDR = 8
TLV_CHOSEN_CODEC = 9

CAPS_MEASURED_METRICS = 1 << 10
CAPS_TRACE_CONTEXT = 1 << 13
CAPS_PB_ONLY = 0x02  # 07 §8.1 接入矩阵原始值（历史——pb-only 时期）
CAPS_BIN_PB = 0x03   # bin|pb 双栈（crawler-lab 起 bin: 裸键载荷走本网关——
                     # 注册模式下 Rust accept 侧协商需要公共栈；erl/jvm 同步）


def trace_request(frame: Frame, negotiated_capabilities: int) -> Frame:
    """Attach the current explicit parent only on a mutually negotiated link."""
    if frame.flags & FLAG_TRACE_CONTEXT:
        if not negotiated_capabilities & CAPS_TRACE_CONTEXT:
            raise ValueError("unnegotiated causal context")
        # Validate without removing or adding another prefix.
        replace(frame).take_trace_parent()
        return frame
    parent = current_parent()
    if parent is None or not negotiated_capabilities & CAPS_TRACE_CONTEXT:
        return frame
    return frame.with_trace_parent(parent)


def _tlv(tag: int, value: bytes) -> bytes:
    if len(value) > 65535:
        raise ValueError("handshake TLV exceeds u16 length")
    return struct.pack("<BH", tag, len(value)) + value


def handshake_body(node_id: str, capabilities: int = CAPS_BIN_PB) -> bytes:
    if not node_id or "\0" in node_id:
        raise ValueError("invalid node identity")
    _uint(capabilities, (1 << 32) - 1, "capabilities")
    out = _tlv(TLV_NODE_ID, node_id.encode())
    out += _tlv(TLV_CAPABILITIES, struct.pack("<I", capabilities))
    out += _tlv(TLV_MAX_FRAME_LEN, struct.pack("<I", DEFAULT_HANDSHAKE_FRAME_LEN))
    out += _tlv(TLV_TOPOLOGY_ROLE, bytes([0]))
    out += _tlv(TLV_HOP_LIMIT, bytes([8]))
    return out


def handshake_ack_body(node_id: str, capabilities: int = CAPS_BIN_PB,
                       chosen_codec: str = "bin") -> bytes:
    _uint(capabilities, (1 << 32) - 1, "capabilities")
    if chosen_codec not in ("bin", "pb") or not capabilities & {"bin": 1, "pb": 2}[chosen_codec]:
        raise ValueError("invalid chosen codec")
    return handshake_body(node_id, capabilities) + _tlv(TLV_CHOSEN_CODEC, chosen_codec.encode())


def parse_tlv(body: bytes) -> list[tuple[int, bytes]]:
    out = []
    p = 0
    while p < len(body):
        if len(body) - p < 3:
            raise ValueError("truncated TLV header")
        tag = body[p]
        (ln,) = struct.unpack_from("<H", body, p + 1)
        if p + 3 + ln > len(body):
            raise ValueError("truncated TLV value")
        out.append((tag, body[p + 3 : p + 3 + ln]))
        p += 3 + ln
    return out


def _handshake_fields(body: bytes) -> dict[int, bytes]:
    # Retain only the first occurrence of the nine known tags. The generic
    # public parse_tlv API deliberately returns every field; network admission
    # must not materialize that untrusted list or copy unknown payloads.
    fields = {}
    duplicate = False
    p = 0
    while p < len(body):
        if len(body) - p < 3:
            raise ValueError("truncated TLV header")
        tag = body[p]
        (length,) = struct.unpack_from("<H", body, p + 1)
        end = p + 3 + length
        if end > len(body):
            raise ValueError("truncated TLV value")
        if 1 <= tag <= TLV_CHOSEN_CODEC:
            if tag in fields:
                duplicate = True
            else:
                fields[tag] = body[p + 3:end]
        p = end
    # Preserve full structural validation before semantic duplicate errors.
    if duplicate:
        raise ValueError("duplicate handshake field")
    return fields


def decode_handshake(body: bytes, *, acknowledgement: bool = False) -> dict:
    fields = _handshake_fields(body)
    for tag, size in [(TLV_CAPABILITIES, 4), (TLV_MAX_FRAME_LEN, 4),
                      (TLV_TOPOLOGY_ROLE, 1)]:
        if tag not in fields or len(fields[tag]) != size:
            raise ValueError("missing or malformed handshake field")
    node = fields.get(TLV_NODE_ID, b"").decode()
    if not node or "\0" in node:
        raise ValueError("invalid node identity")
    if not acknowledgement and TLV_CHOSEN_CODEC in fields:
        raise ValueError("chosen codec is ACK-only")
    for tag in (TLV_REALM, TLV_CLUSTER, TLV_DIRECT_ADDR):
        if tag in fields:
            fields[tag].decode()
    capabilities = int.from_bytes(fields[TLV_CAPABILITIES], "little")
    common = capabilities & CAPS_BIN_PB
    if not common:
        raise ValueError("no common codec")
    limit = int.from_bytes(fields[TLV_MAX_FRAME_LEN], "little")
    if limit < BODY_FIXED_OVERHEAD or fields[TLV_TOPOLOGY_ROLE][0] > 3:
        raise ValueError("invalid handshake bounds or role")
    if TLV_HOP_LIMIT in fields and (len(fields[TLV_HOP_LIMIT]) != 1 or fields[TLV_HOP_LIMIT][0] == 0):
        raise ValueError("invalid handshake hop limit")
    chosen = fields.get(TLV_CHOSEN_CODEC, b"bin").decode() if acknowledgement else (
        "bin" if common & 1 else "pb")
    if chosen not in ("bin", "pb") or not common & {"bin": 1, "pb": 2}[chosen]:
        raise ValueError("invalid chosen codec")
    return {"node_id": node, "capabilities": capabilities, "chosen_codec": chosen,
            "max_frame_len": limit}
