"""Parrot Wire 1.0 帧编解码（Python 栈）。

布局（07 §2.1——与 Rust frame.rs / JVM WireFrame.scala / TS lite 逐字节一致）：
  外层: [u32 body_len LE][body]
  body: [ver u8][ft u8][flags u16 LE][cid u64 LE][hop_count u8][hop_limit u8][rsv u16]
        [path_len u32 LE][path][key_len u32 LE][key][payload]

注意 body 前 12B 为 ver/ft/flags/cid；hop 双 u8 + rsv u16 共 4B（POC 期
reserved 整体 u64 的旧布局已废弃——golden vectors 为准）。
"""

from __future__ import annotations

import struct
from dataclasses import dataclass
from typing import Optional

WIRE_VERSION = 1

# 帧类型（07 §2.1 全集；Python 网关数据面只用到后四种）
FT_HANDSHAKE = 0x01
FT_HANDSHAKE_ACK = 0x02
FT_HEARTBEAT = 0x03
FT_HEARTBEAT_ACK = 0x04
FT_SYSTEM_EVENT = 0x05
FT_ASK = 0x10
FT_REPLY = 0x11
FT_REPLY_ERR = 0x12
FT_TELL = 0x13

# flags 位
FLAG_TELL_ACK = 0x0001  # E2 durable tell ACK 位

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

    def encode(self) -> bytes:
        path = self.path.encode()
        key = self.type_key.encode()
        body_len = 12 + 2 + 6 + 4 + 4 + len(path) + len(key) + len(self.payload)
        # 定长头 12B + hop(2B) + rsv48(6B) + 长度域 8B（07 §2.1 权威布局）
        head = struct.pack(
            "<BBHQ",
            WIRE_VERSION,
            self.ft,
            self.flags,
            self.cid,
        )
        # hop_count u8 + hop_limit u8 + reserved u48（6 字节 0）
        hop = struct.pack("<BB", self.hop_count, self.hop_limit) + b"\x00" * 6
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
) -> bytes:
    return Frame(ft, cid, path, type_key, payload, flags, hop_count, hop_limit).encode()


class FrameDecoder:
    """非消费式半包解码器（与 JVM WireDecoder / Rust read_frame 同语义）。"""

    def __init__(self) -> None:
        self.buf = bytearray()

    def feed(self, data: bytes) -> None:
        self.buf.extend(data)

    def next_frame(self) -> Optional[Frame]:
        if len(self.buf) < 4:
            return None
        (body_len,) = struct.unpack_from("<I", self.buf, 0)
        if len(self.buf) < 4 + body_len:
            return None
        body = bytes(self.buf[4 : 4 + body_len])
        del self.buf[: 4 + body_len]
        return _parse_body(body)


def _parse_body(body: bytes) -> Frame:
    ver, ft, flags, cid = struct.unpack_from("<BBHQ", body, 0)
    if ver != WIRE_VERSION:
        raise ValueError(f"unsupported wire version {ver}")
    hop_count, hop_limit = struct.unpack_from("<BB", body, 12)
    (path_len,) = struct.unpack_from("<I", body, 20)
    path = body[24 : 24 + path_len].decode()
    off = 24 + path_len
    (key_len,) = struct.unpack_from("<I", body, off)
    key = body[off + 4 : off + 4 + key_len].decode()
    payload = body[off + 4 + key_len :]
    return Frame(ft, cid, path, key, payload, flags, hop_count, hop_limit)


# ---------------- 错误体（与 error.rs encode_err_payload 一致） ----------------


def encode_err_payload(code: int, detail: str) -> bytes:
    d = detail.encode()
    return struct.pack("<HH", code, 0) + d


def decode_err_payload(b: bytes) -> tuple[int, str]:
    if len(b) < 4:
        return (ERR_PROTOCOL_VIOLATION, "<undecodable>")
    (code,) = struct.unpack_from("<H", b, 0)
    return (code, b[4:].decode(errors="replace"))


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
TLV_CHOSEN_CODEC = 8

CAPS_PB_ONLY = 0x02  # 07 §8.1 接入矩阵原始值（历史——pb-only 时期）
CAPS_BIN_PB = 0x03   # bin|pb 双栈（crawler-lab 起 bin: 裸键载荷走本网关——
                     # 注册模式下 Rust accept 侧协商需要公共栈；erl/jvm 同步）


def _tlv(tag: int, value: bytes) -> bytes:
    return struct.pack("<BH", tag, len(value)) + value


def handshake_body(node_id: str) -> bytes:
    out = _tlv(TLV_NODE_ID, node_id.encode())
    out += _tlv(TLV_CAPABILITIES, struct.pack("<I", CAPS_BIN_PB))
    out += _tlv(TLV_MAX_FRAME_LEN, struct.pack("<I", 1 << 20))
    out += _tlv(TLV_TOPOLOGY_ROLE, bytes([0]))
    out += _tlv(TLV_HOP_LIMIT, bytes([8]))
    return out


def handshake_ack_body(node_id: str) -> bytes:
    return handshake_body(node_id) + _tlv(TLV_CHOSEN_CODEC, b"pb")


def parse_tlv(body: bytes) -> list[tuple[int, bytes]]:
    out = []
    p = 0
    while p + 3 <= len(body):
        tag = body[p]
        (ln,) = struct.unpack_from("<H", body, p + 1)
        if p + 3 + ln > len(body):
            break
        out.append((tag, body[p + 3 : p + 3 + ln]))
        p += 3 + ln
    return out
