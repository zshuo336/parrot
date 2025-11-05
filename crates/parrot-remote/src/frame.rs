//! 职责：Wire 1.0 帧字节 ↔ 结构，不知道任何传输细节（07 §2.1，DEV_01 §3.1）。
//!
//! 布局（07 §2.1 定稿，X1 实证 28B body 开销）：
//! ```text
//! 偏移 长度 字段
//! 0    4    frame_len   u32 LE = 后续 body 总长
//! 4    1    version     0x01
//! 5    1    frame_type
//! 6    2    flags       u16 LE
//! 8    8    correlation_id u64 LE
//! 16   1    hop_count
//! 17   1    hop_limit   默认 8
//! 18   4    sequence    u32 LE (0 = no ordering)
//! 22   2    reserved    u16 = 0
//! 24   4    path_len    u32 LE
//! 28   var  path        UTF-8
//! ..   4    key_len     u32 LE
//! ..   var  type_key    UTF-8
//! ..   var  payload     = body_len − 28 − path_len − key_len
//! ```
//! 半包返回 Ok(None) 不消费缓冲（POC RC1 锁定行为）；整数一律 LE。

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::str::Utf8Error;

use crate::error::ErrCode;

pub const PROTOCOL_VERSION: u8 = 0x01;
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024; // 16 MiB
pub const DEFAULT_HOP_LIMIT: u8 = 8;
/// Hard work bound per batch; expanded frame metadata also fits MAX_FRAME_LEN.
pub const MAX_BATCH_ITEMS: usize = 1024;
/// Reserved success receipt for completion TELL; no business reply codec is used.
pub const TELL_COMPLETION_KEY: &str = "parrot:completion#v1";
/// Remote shadow receipt confirms raw mailbox admission, not handler completion.
pub const MIRROR_ADMISSION_KEY: &str = "parrot:mirror-admitted#v1";
/// v1 prefix: one version byte, 16 trace ID bytes, 8 parent span ID bytes.
pub const TRACE_CONTEXT_BYTES: usize = 25;
/// frame_len → reserved 定长段（不含 path_len）
pub const FIXED_HEADER_SIZE: usize = 24;
/// body 固定开销 = 定长段 + path_len(4)
pub const BODY_FIXED_OVERHEAD: usize = 28;

pub mod frame_type {
    pub const HANDSHAKE: u8 = 0x01;
    pub const HANDSHAKE_ACK: u8 = 0x02;
    pub const HEARTBEAT: u8 = 0x03;
    pub const HEARTBEAT_ACK: u8 = 0x04;
    pub const ASK: u8 = 0x10;
    pub const REPLY: u8 = 0x11;
    pub const REPLY_ERR: u8 = 0x12;
    pub const TELL: u8 = 0x13;
    pub const STOP: u8 = 0x14;
    /// Structurally recognized; execution support is checked separately in ingress.
    pub const FRAGMENT: u8 = 0x15;
    /// P1 收到即断连（协议违规；P2 启用管理/集群载荷）
    pub const SYSTEM_EVENT: u8 = 0x20;
    pub const RESOLVE_Q: u8 = 0x21; // federation resolve query
    pub const RESOLVE_R: u8 = 0x22;
    pub const INVALIDATE: u8 = 0x23;
    /// 方案 A 学习通道：hub 中转时向源节点推送目标直连地址（spoke 缓存后
    /// 直拨目标，流量不再经 hub）。payload = `[u16 node_len][node][u16 addr_len][addr]`。
    pub const ROUTE_HINT: u8 = 0x24;
    pub const ERROR: u8 = 0x7F;
}

pub mod flags {
    pub const COMPRESSED_ZSTD: u16 = 1 << 0; // P2
    pub const TRACING: u16 = 1 << 1; // P2
    pub const BATCH: u16 = 1 << 2; // P4
    pub const URGENT: u16 = 1 << 3; // P2
    pub const TELL_ACK: u16 = 1 << 4; // P3
    pub const APP_ENCRYPTED: u16 = 1 << 5; // 1.0 可选
    pub const TRACE_CONTEXT: u16 = 1 << 6;
    pub const MIRROR_SHADOW: u16 = 1 << 7;
}

/// 端到端序列号哨兵：0 = 本帧不参与重排（旧实现/异构网关发的帧——
/// 接收端直接投递不缓冲）。有效 seq 从 1 起。
pub const SEQ_NONE: u32 = 0;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// An underlying reader/writer error, not a malformed wire frame.
    /// Store comparable diagnostics so FrameError retains its Clone/Eq contract.
    #[error("frame I/O ({kind:?}, OS {raw_os_error:?}): {message}")]
    Io {
        kind: std::io::ErrorKind,
        raw_os_error: Option<i32>,
        message: String,
    },
    /// EOF with an incomplete frame. Expected includes the four-byte prefix,
    /// or is unknown when EOF arrived inside that prefix.
    #[error("truncated frame at EOF: received {received} bytes, expected {expected:?}")]
    Truncated {
        received: usize,
        expected: Option<usize>,
    },
    #[error("帧超限：声明长度 {len} > MAX_FRAME_LEN={max}（偏移 0）")]
    TooLarge { len: u32, max: u32 },
    #[error("版本不符：期望 {expect} 实得 {got}（偏移 4）")]
    VersionMismatch { expect: u8, got: u8 },
    #[error("未知帧类型 0x{got:02X}（偏移 5）——两端协议版本漂移")]
    UnknownFrameType { got: u8 },
    #[error(
        "长度不自洽：frame_len={flen} 但 path_len={plen}+key_len={klen}+固定28 超出（偏移 24）"
    )]
    MalformedLengths { flen: u32, plen: u32, klen: u32 },
    #[error("UTF-8 解码失败（字段 {field}）：{source}")]
    Utf8 {
        field: &'static str,
        source: Utf8Error,
    },
    #[error("reserved 非 0（偏移 18，实得 0x{got:012X}）——发送端实现有误")]
    ReservedNotZero { got: u64 },
    #[error("hop_count={count} ≥ hop_limit={limit}（偏移 16/17）——丢弃并回 RouteUnreachable")]
    HopExceeded { count: u8, limit: u8 },
    #[error("unsupported payload flags: 0x{flags:04x}")]
    UnsupportedFlags { flags: u16 },
    #[error("invalid causal trace context prefix")]
    InvalidTraceContext,
    #[error("batch exceeds {what} budget ({max})")]
    BatchLimit { what: &'static str, max: usize },
    #[error("BATCH 载荷损坏：offset {offset} 处 item 长度越界（载荷 {total}B）")]
    BatchMalformed { offset: usize, total: usize },
    #[error("BATCH 帧载荷需 ≥8B（至少容纳 item 计数）实得 {got}B")]
    BatchEmpty { got: usize },
}

impl FrameError {
    /// 机器可读映射（E5.5 三态）：帧级错误 → 结构化错误码。
    pub fn to_errcode(&self) -> ErrCode {
        match self {
            FrameError::Io { .. } => ErrCode::ConnectionLost,
            FrameError::Truncated { .. } => ErrCode::ProtocolViolation,
            FrameError::UnsupportedFlags { .. }
            | FrameError::BatchLimit { .. }
            | FrameError::InvalidTraceContext => ErrCode::ProtocolViolation,
            FrameError::TooLarge { .. } => ErrCode::ProtocolViolation,
            FrameError::VersionMismatch { .. } => ErrCode::ProtocolViolation,
            FrameError::UnknownFrameType { .. } => ErrCode::ProtocolViolation,
            FrameError::MalformedLengths { .. } => ErrCode::ProtocolViolation,
            FrameError::Utf8 { .. } => ErrCode::ProtocolViolation,
            FrameError::ReservedNotZero { .. } => ErrCode::ProtocolViolation,
            FrameError::HopExceeded { .. } => ErrCode::RouteUnreachable,
            FrameError::BatchMalformed { .. } | FrameError::BatchEmpty { .. } => {
                ErrCode::ProtocolViolation
            }
        }
    }
}

/// 定长头视图（frame_len → reserved 段；不持有 path/key/payload）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_len: u32,
    pub version: u8,
    pub frame_type: u8,
    pub flags: u16,
    pub correlation_id: u64,
    pub hop_count: u8,
    pub hop_limit: u8,
    /// 端到端序列号（per 源节点单调递增，TELL 重排用；0=不参与）。
    /// wire 位：reserved u48 的低 32bit。
    pub seq: u32,
}

/// Wire 帧：头 + path + type_key + payload。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub path: String,
    pub type_key: String,
    pub payload: Bytes,
}

impl Frame {
    fn new_header(frame_type: u8, cid: u64) -> FrameHeader {
        FrameHeader {
            frame_len: 0, // encode 时回填
            version: PROTOCOL_VERSION,
            frame_type,
            flags: 0,
            correlation_id: cid,
            hop_count: 0,
            hop_limit: DEFAULT_HOP_LIMIT,
            seq: SEQ_NONE,
        }
    }

    /// ASK：payload 头部带 reply_to 前缀（DEV_01 §3.1 类型键约定：
    /// `[u32 reply_to_len][reply_to_bytes][payload...]`，仅 ASK 帧）。
    pub fn ask(
        cid: u64,
        path: &str,
        type_key: &str,
        payload: Bytes,
        reply_to: Option<&str>,
    ) -> Frame {
        let mut body = BytesMut::with_capacity(4 + payload.len());
        match reply_to {
            Some(r) => {
                let rb = r.as_bytes();
                body.put_u32_le(rb.len() as u32);
                body.put_slice(rb);
            }
            None => body.put_u32_le(0),
        }
        body.put_slice(&payload);
        Frame {
            header: Self::new_header(frame_type::ASK, cid),
            path: path.to_string(),
            type_key: type_key.to_string(),
            payload: body.freeze(),
        }
    }

    pub fn tell(path: &str, type_key: &str, payload: Bytes) -> Frame {
        Frame {
            header: Self::new_header(frame_type::TELL, 0),
            path: path.to_string(),
            type_key: type_key.to_string(),
            payload,
        }
    }

    /// Request handler completion, with an independent nonzero callback ID.
    /// Payload is ordinary business bytes, without ASK's reply_to prefix. The
    /// ordinary TELL sequence is zero; this does not establish session FIFO.
    pub fn tell_with_ack(cid: u64, path: &str, type_key: &str, payload: Bytes) -> Frame {
        let mut frame = Self::tell(path, type_key, payload);
        frame.header.correlation_id = cid;
        frame.header.flags |= flags::TELL_ACK;
        frame
    }

    pub fn tell_completed(cid: u64) -> Frame {
        Self::reply(cid, "", TELL_COMPLETION_KEY, Bytes::new())
    }

    /// 带端到端 seq 的 TELL（reserved 低 32bit——重排用，见 ingress::ReorderTable）。
    pub fn tell_with_seq(path: &str, type_key: &str, payload: Bytes, seq: u32) -> Frame {
        let mut f = Self::tell(path, type_key, payload);
        f.header.seq = seq;
        f
    }

    pub fn reply(cid: u64, path: &str, type_key: &str, payload: Bytes) -> Frame {
        Frame {
            header: Self::new_header(frame_type::REPLY, cid),
            path: path.to_string(),
            type_key: type_key.to_string(),
            payload,
        }
    }

    pub fn reply_err(cid: u64, path: &str, code: ErrCode, detail: &str) -> Frame {
        Frame {
            header: Self::new_header(frame_type::REPLY_ERR, cid),
            path: path.to_string(),
            type_key: String::new(),
            payload: Bytes::from(crate::error::encode_err_payload(code, detail)),
        }
    }

    pub fn stop(path: &str) -> Frame {
        Frame {
            header: Self::new_header(frame_type::STOP, 0),
            path: path.to_string(),
            type_key: String::new(),
            payload: Bytes::new(),
        }
    }

    pub fn heartbeat() -> Frame {
        Frame {
            header: Self::new_header(frame_type::HEARTBEAT, 0),
            path: String::new(),
            type_key: String::new(),
            payload: Bytes::new(),
        }
    }

    pub fn heartbeat_ack() -> Frame {
        Frame {
            header: Self::new_header(frame_type::HEARTBEAT_ACK, 0),
            path: String::new(),
            type_key: String::new(),
            payload: Bytes::new(),
        }
    }

    /// ROUTE_HINT（方案 A）：hub → 源 spoke，告知目标节点直连地址。
    /// payload = `[u16 node_len][node][u16 addr_len][addr]`，addr = "host:port"。
    pub fn try_route_hint(node: &str, addr: &str) -> Result<Frame, FrameError> {
        if node.len() > u16::MAX as usize || addr.len() > u16::MAX as usize {
            return Err(FrameError::MalformedLengths {
                flen: 0,
                plen: node.len().min(u32::MAX as usize) as u32,
                klen: addr.len().min(u32::MAX as usize) as u32,
            });
        }
        let mut p = bytes::BytesMut::new();
        p.put_u16_le(node.len() as u16);
        p.put_slice(node.as_bytes());
        p.put_u16_le(addr.len() as u16);
        p.put_slice(addr.as_bytes());
        Ok(Frame {
            header: Self::new_header(frame_type::ROUTE_HINT, 0),
            path: String::new(),
            type_key: String::new(),
            payload: p.freeze(),
        })
    }

    /// Compatibility constructor for trusted callers. New code should use
    /// `try_route_hint` so attacker-controlled node/address strings cannot be
    /// silently truncated to the wire's u16 field widths.
    #[deprecated(note = "use try_route_hint and handle oversized fields")]
    pub fn route_hint(node: &str, addr: &str) -> Frame {
        Self::try_route_hint(node, addr).expect("ROUTE_HINT node/address exceeds u16 wire limit")
    }

    /// 解 ROUTE_HINT payload → (node, addr)。
    pub fn parse_route_hint(payload: &[u8]) -> Result<(String, String), FrameError> {
        let bad = |n: usize| FrameError::MalformedLengths {
            flen: n as u32,
            plen: 0,
            klen: 0,
        };
        if payload.len() < 4 {
            return Err(bad(payload.len()));
        }
        let nlen = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        if payload.len() < 2 + nlen + 2 {
            return Err(bad(payload.len()));
        }
        let node = std::str::from_utf8(&payload[2..2 + nlen]).map_err(|_| bad(nlen))?;
        let alen = u16::from_le_bytes([payload[2 + nlen], payload[3 + nlen]]) as usize;
        let aoff = 2 + nlen + 2;
        if payload.len() < aoff + alen {
            return Err(bad(payload.len()));
        }
        let addr = std::str::from_utf8(&payload[aoff..aoff + alen]).map_err(|_| bad(alen))?;
        Ok((node.to_string(), addr.to_string()))
    }

    pub fn error_frame(code: ErrCode, detail: &str) -> Frame {
        Frame {
            header: Self::new_header(frame_type::ERROR, 0),
            path: String::new(),
            type_key: String::new(),
            payload: Bytes::from(crate::error::encode_err_payload(code, detail)),
        }
    }

    pub fn handshake(payload: Bytes) -> Frame {
        Frame {
            header: Self::new_header(frame_type::HANDSHAKE, 0),
            path: String::new(),
            type_key: String::new(),
            payload,
        }
    }

    pub fn handshake_ack(payload: Bytes) -> Frame {
        Frame {
            header: Self::new_header(frame_type::HANDSHAKE_ACK, 0),
            path: String::new(),
            type_key: String::new(),
            payload,
        }
    }

    /// 剥 ASK payload 头部的 reply_to（DEV_01 §3.1 约定）。返回 (reply_to, 真实 payload)。
    pub fn split_reply_to(&self) -> Result<(Option<String>, Bytes), FrameError> {
        let payload = self.split_trace_parent()?.1;
        let (reply_to, offset) = Self::reply_to_parts(&payload, self.header.frame_len)?;
        Ok((
            reply_to.map(str::to_owned),
            payload[offset..].to_vec().into(),
        ))
    }

    /// Borrow the business slice for budgeted ingress copying. Do not allocate
    /// reply_to or clone the business payload before the mirror obtains permits.
    pub(crate) fn borrowed_ask_payload(&self) -> Result<&[u8], FrameError> {
        let (parent, _) = self.split_trace_parent()?;
        let start = if parent.is_some() {
            TRACE_CONTEXT_BYTES
        } else {
            0
        };
        let payload = &self.payload[start..];
        let (_, offset) = Self::reply_to_parts(payload, self.header.frame_len)?;
        Ok(&payload[offset..])
    }

    fn reply_to_parts(payload: &[u8], frame_len: u32) -> Result<(Option<&str>, usize), FrameError> {
        if payload.len() < 4 {
            return Err(FrameError::MalformedLengths {
                flen: frame_len,
                plen: 0,
                klen: 0,
            });
        }
        let rlen = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        if rlen > payload.len() - 4 {
            return Err(FrameError::MalformedLengths {
                flen: frame_len,
                plen: rlen as u32,
                klen: 0,
            });
        }
        let reply_to = if rlen == 0 {
            None
        } else {
            Some(
                std::str::from_utf8(&payload[4..4 + rlen]).map_err(|source| FrameError::Utf8 {
                    field: "reply_to",
                    source,
                })?,
            )
        };
        Ok((reply_to, 4 + rlen))
    }

    /// Validate the complete encoded body without allocating an output buffer.
    pub(crate) fn validate_size(&self, limit: u32) -> Result<u32, FrameError> {
        let limit = limit.min(MAX_FRAME_LEN);
        let body = BODY_FIXED_OVERHEAD as u64
            + self.path.len() as u64
            + self.type_key.len() as u64
            + self.payload.len() as u64;
        if body > limit as u64 {
            return Err(FrameError::TooLarge {
                len: body.min(u32::MAX as u64) as u32,
                max: limit,
            });
        }
        Ok(body as u32)
    }

    /// 编码进 buf（含 4B frame_len 前缀）。
    pub fn encode(&self, buf: &mut BytesMut) -> Result<(), FrameError> {
        let path_b = self.path.as_bytes();
        let key_b = self.type_key.as_bytes();
        let body = self.validate_size(MAX_FRAME_LEN)?;
        if self.header.version != PROTOCOL_VERSION {
            return Err(FrameError::VersionMismatch {
                expect: PROTOCOL_VERSION,
                got: self.header.version,
            });
        }
        buf.put_u32_le(body);
        buf.put_u8(self.header.version);
        buf.put_u8(self.header.frame_type);
        buf.put_u16_le(self.header.flags);
        buf.put_u64_le(self.header.correlation_id);
        buf.put_u8(self.header.hop_count);
        buf.put_u8(self.header.hop_limit);
        // reserved u48（6 字节，偏移 18..24）：低 32bit = seq（0=不参与
        // 重排——与旧实现字节兼容），高 16bit 仍保留 0
        buf.put_u32_le(self.header.seq);
        buf.put_slice(&[0u8; 2]);
        buf.put_u32_le(path_b.len() as u32);
        buf.put_slice(path_b);
        buf.put_u32_le(key_b.len() as u32);
        buf.put_slice(key_b);
        buf.put_slice(&self.payload);
        Ok(())
    }

    /// 从 buf 解码一帧；不足一帧返回 Ok(None) 且不消费。
    pub fn decode(buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        Self::decode_with_limit(buf, MAX_FRAME_LEN)
    }

    /// 从 buf 解码一帧，并应用连接握手协商出的 body 长度上限。
    ///
    /// `max_frame_len` 不能放宽协议全局上限；连接级协商只能收紧它。
    pub fn decode_with_limit(
        buf: &mut BytesMut,
        max_frame_len: u32,
    ) -> Result<Option<Frame>, FrameError> {
        let max_frame_len = max_frame_len.min(MAX_FRAME_LEN);
        if buf.len() < 4 {
            return Ok(None);
        }
        let body_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if body_len > max_frame_len {
            tracing::warn!(
                len = body_len,
                max = max_frame_len,
                "frame length exceeds cap (corrupted stream? malicious peer? mismatched MAX_FRAME_LEN)"
            );
            return Err(FrameError::TooLarge {
                len: body_len,
                max: max_frame_len,
            });
        }
        if body_len < BODY_FIXED_OVERHEAD as u32 {
            return Err(FrameError::MalformedLengths {
                flen: body_len,
                plen: 0,
                klen: 0,
            });
        }
        if buf.len() < 4 + body_len as usize {
            return Ok(None); // 半包
        }
        // Parse only this frame: malformed inner lengths must never consume a
        // following frame. Validate every variable field before slicing it.
        let mut body = buf.split_to(4 + body_len as usize);
        body.advance(4);
        let buf = &mut body;
        let version = buf.get_u8();
        if version != PROTOCOL_VERSION {
            tracing::warn!(
                expect = PROTOCOL_VERSION,
                got = version,
                body_len,
                "wire version mismatch (peer on different protocol major?)"
            );
            return Err(FrameError::VersionMismatch {
                expect: PROTOCOL_VERSION,
                got: version,
            });
        }
        let ft = buf.get_u8();
        // FRAGMENT remains reserved; SYSTEM_EVENT and federation control
        // frames are admitted here and receive authentication/payload checks
        // in ingress. The frame layer only rejects unknown code points.
        let known = matches!(
            ft,
            frame_type::HANDSHAKE
                | frame_type::HANDSHAKE_ACK
                | frame_type::HEARTBEAT
                | frame_type::HEARTBEAT_ACK
                | frame_type::ASK
                | frame_type::REPLY
                | frame_type::REPLY_ERR
                | frame_type::TELL
                | frame_type::STOP
                | frame_type::FRAGMENT
                | frame_type::SYSTEM_EVENT
                | frame_type::RESOLVE_Q
                | frame_type::RESOLVE_R
                | frame_type::INVALIDATE
                | frame_type::ROUTE_HINT
                | frame_type::ERROR
        );
        if !known {
            // 边界观测（RB3 类教训）：未知码点几乎必是两端协议版本漂移——
            // 静默断连最难排查，这里带全上下文打点（hex 便于对照 wire 文档）
            tracing::warn!(
                got = format!("0x{ft:02X}"),
                version,
                body_len,
                "unknown frame type rejected (peer protocol newer? check frame.rs frame_type)"
            );
            return Err(FrameError::UnknownFrameType { got: ft });
        }
        let fl = buf.get_u16_le();
        let cid = buf.get_u64_le();
        let hop_count = buf.get_u8();
        let hop_limit = buf.get_u8();
        if hop_limit == 0 || hop_count >= hop_limit {
            return Err(FrameError::HopExceeded {
                count: hop_count,
                limit: hop_limit,
            });
        }
        // reserved u48（帧偏移 18..24）——此前已顺序消费 ver/ft/flags/cid/hop 共 14B
        // 低 32bit = seq（端到端重排序号，0=不参与）；高 16bit 保留必须 0
        let seq = buf.get_u32_le();
        let reserved_hi: u64 = buf.get_u16_le() as u64;
        if reserved_hi != 0 {
            tracing::warn!(
                got = format!("0x{reserved_hi:04X}"),
                ft = format!("0x{ft:02X}"),
                cid,
                seq,
                "reserved high 16bit non-zero (peer using unallocated extension?)"
            );
            return Err(FrameError::ReservedNotZero {
                got: reserved_hi << 32,
            });
        }
        let path_len = buf.get_u32_le() as usize;
        if buf.remaining() < 4 || path_len > buf.remaining() - 4 {
            return Err(FrameError::MalformedLengths {
                flen: body_len,
                plen: path_len as u32,
                klen: 0,
            });
        }
        let path = String::from_utf8(buf.copy_to_bytes(path_len).to_vec()).map_err(|e| {
            tracing::warn!(
                len = path_len,
                valid_up_to = e.utf8_error().valid_up_to(),
                "path field not valid UTF-8 (codec mismatch or stream corruption)"
            );
            FrameError::Utf8 {
                field: "path",
                source: e.utf8_error(),
            }
        })?;
        let key_len = buf.get_u32_le() as usize;
        if key_len > buf.remaining() {
            return Err(FrameError::MalformedLengths {
                flen: body_len,
                plen: path_len as u32,
                klen: key_len as u32,
            });
        }
        let key = String::from_utf8(buf.copy_to_bytes(key_len).to_vec()).map_err(|e| {
            tracing::warn!(
                len = key_len,
                valid_up_to = e.utf8_error().valid_up_to(),
                "type_key field not valid UTF-8 (codec mismatch or stream corruption)"
            );
            FrameError::Utf8 {
                field: "type_key",
                source: e.utf8_error(),
            }
        })?;
        let payload = buf.copy_to_bytes(buf.remaining());
        let header = FrameHeader {
            frame_len: body_len,
            version,
            frame_type: ft,
            flags: fl,
            correlation_id: cid,
            hop_count,
            hop_limit,
            seq,
        };
        Ok(Some(Frame {
            header,
            path,
            type_key: key,
            payload,
        }))
    }

    /// PARROT_TRACE=frame 的单行摘要（E5.5：人类/机器/LLM 三态可读）。
    ///
    /// F1（DEV_09 §3.6）：`TRACING` 位帧输出 `trace_id=cid` 字段——
    /// span 聚合按 trace_id 关联（与 record-replay 数据源同构）。
    pub fn trace_line(&self) -> String {
        let trace = if self.header.flags & flags::TRACING != 0 {
            format!(" trace_id={}", self.header.correlation_id)
        } else {
            String::new()
        };
        format!(
            "frame ft=0x{:02X} cid={} hop={}/{} flags=0x{:04X}{trace} path={:?} key={:?} len={}",
            self.header.frame_type,
            self.header.correlation_id,
            self.header.hop_count,
            self.header.hop_limit,
            self.header.flags,
            self.path,
            self.type_key,
            self.payload.len()
        )
    }

    /// 本帧 trace_id（TRACING 位帧 = cid；未标记帧 None——不参与
    /// span 聚合）。
    pub fn trace_id(&self) -> Option<u64> {
        (self.header.flags & flags::TRACING != 0).then_some(self.header.correlation_id)
    }

    // ── D3 · BATCH 批量帧（DEV_04 §4 / 06 P4.3）────────────────────────
    //
    // 批量语义：同一目标 path 的 N 条消息共享一个外层帧的 28B 固定开销 +
    // path/type_key——小消息（如 64B heartbeat 式遥测）高频场景的字节级
    // 节省来源。载荷布局：[u64 count][count × ([u64 cid][u32 len][payload])]
    //（cid 为每条消息的关联号；长度前缀供接收端逐条切分）。

    /// 将同 path 的 N 帧打包为一个 BATCH 帧载体。
    ///
    /// * 仅接受 ASK/TELL/REPLY（控制帧无批量语义）；
    /// * path/type_key 取首帧（批量前提：同目标同类型）；
    /// * hop 取各帧最大值（保最严跳数语义）。
    pub fn batch(frames: &[Frame]) -> Result<Frame, FrameError> {
        let first = frames.first().ok_or(FrameError::BatchEmpty { got: 0 })?;
        if frames.len() > MAX_BATCH_ITEMS {
            return Err(FrameError::BatchLimit {
                what: "item count",
                max: MAX_BATCH_ITEMS,
            });
        }
        let malformed = || FrameError::BatchMalformed {
            offset: 0,
            total: frames.len(),
        };
        let mut encoded = 8usize;
        let mut expanded = 0usize;
        let mut hop_count = 0;
        let mut hop_limit = u8::MAX;
        for frame in frames {
            frame.validate_payload_flags()?;
            if !matches!(
                frame.header.frame_type,
                frame_type::ASK | frame_type::TELL | frame_type::REPLY
            ) || frame.header.frame_type != first.header.frame_type
                || frame.path != first.path
                || frame.type_key != first.type_key
                || frame.header.flags != first.header.flags
                || frame.header.flags & (flags::BATCH | flags::URGENT) != 0
                || frame.header.version != PROTOCOL_VERSION
                || frame.header.seq != SEQ_NONE
            {
                return Err(malformed());
            }
            hop_count = hop_count.max(frame.header.hop_count);
            hop_limit = hop_limit.min(frame.header.hop_limit);
            encoded = encoded
                .checked_add(12)
                .and_then(|n| n.checked_add(frame.payload.len()))
                .ok_or_else(malformed)?;
            expanded = batch_expanded_size(
                expanded,
                frame.path.len(),
                frame.type_key.len(),
                frame.payload.len(),
            )?;
        }
        if hop_count >= hop_limit {
            return Err(FrameError::HopExceeded {
                count: hop_count,
                limit: hop_limit,
            });
        }
        let body = encoded
            .checked_add(BODY_FIXED_OVERHEAD)
            .and_then(|n| n.checked_add(first.path.len()))
            .and_then(|n| n.checked_add(first.type_key.len()))
            .ok_or_else(malformed)?;
        if body > MAX_FRAME_LEN as usize {
            return Err(FrameError::BatchLimit {
                what: "encoded bytes",
                max: MAX_FRAME_LEN as usize,
            });
        }
        let mut payload = BytesMut::with_capacity(encoded);
        payload.put_u64_le(frames.len() as u64);
        for frame in frames {
            payload.put_u64_le(frame.header.correlation_id);
            payload.put_u32_le(frame.payload.len() as u32);
            payload.put_slice(&frame.payload);
        }
        let mut header = first.header.clone();
        header.frame_len = 0;
        header.flags |= flags::BATCH;
        header.correlation_id = 0;
        header.hop_count = hop_count;
        header.hop_limit = hop_limit;
        Ok(Frame {
            header,
            path: first.path.clone(),
            type_key: first.type_key.clone(),
            payload: payload.freeze(),
        })
    }

    /// Reject flags whose payload transformations this runtime cannot interpret.
    pub fn validate_payload_flags(&self) -> Result<(), FrameError> {
        let supported = flags::TRACING
            | flags::URGENT
            | flags::TELL_ACK
            | flags::BATCH
            | flags::TRACE_CONTEXT
            | flags::MIRROR_SHADOW;
        if (self.type_key == crate::durable::DURABLE_KEY
            && (self.header.frame_type != frame_type::TELL
                || self.header.flags & flags::TELL_ACK == 0))
            || self.header.flags & !supported != 0
            || (self.header.flags & flags::MIRROR_SHADOW != 0
                && (self.header.frame_type != frame_type::TELL
                    || self.header.flags & flags::TELL_ACK == 0
                    || self.header.flags & (flags::BATCH | flags::URGENT) != 0
                    || matches!(
                        self.type_key.as_str(),
                        crate::durable::DURABLE_KEY
                            | crate::roles::directory::client::READ_KEY
                            | crate::roles::directory::client::CLIENT_KEY
                            | crate::roles::directory::network::RAFT_KEY
                    )))
            || (self.header.flags & flags::TRACE_CONTEXT != 0
                && !matches!(self.header.frame_type, frame_type::ASK | frame_type::TELL))
            || (self.header.flags & flags::TELL_ACK != 0
                && (self.header.frame_type != frame_type::TELL
                    || self.header.seq != SEQ_NONE
                    || (self.header.flags & flags::BATCH == 0 && self.header.correlation_id == 0)))
            || (self.header.flags & (flags::BATCH | flags::URGENT)
                == (flags::BATCH | flags::URGENT))
            || (self.header.flags & flags::BATCH != 0
                && !matches!(
                    self.header.frame_type,
                    frame_type::ASK | frame_type::TELL | frame_type::REPLY
                ))
        {
            return Err(FrameError::UnsupportedFlags {
                flags: self.header.flags,
            });
        }
        if self.header.flags & flags::TRACE_CONTEXT != 0 && self.header.flags & flags::BATCH == 0 {
            self.split_trace_parent()?;
        }
        if self.header.flags & flags::MIRROR_SHADOW != 0 {
            let (_, payload) = self.split_trace_parent()?;
            let budget = payload
                .get(..4)
                .and_then(|p| p.try_into().ok())
                .map(u32::from_le_bytes);
            if !budget.is_some_and(|ms| (1..=60_000).contains(&ms)) {
                return Err(FrameError::UnsupportedFlags {
                    flags: self.header.flags,
                });
            }
        }
        Ok(())
    }

    /// Add a negotiated v1 causal parent before the existing business payload.
    /// ASK retains its reply_to prefix after this header. Batches contain one
    /// independently validated parent prefix per member, never one shared parent.
    pub fn with_trace_parent(
        mut self,
        parent: parrot_api::trace::TraceParent,
    ) -> Result<Self, FrameError> {
        if !matches!(self.header.frame_type, frame_type::ASK | frame_type::TELL)
            || self.header.flags & (flags::BATCH | flags::TRACE_CONTEXT) != 0
        {
            return Err(FrameError::InvalidTraceContext);
        }
        self.validate_payload_flags()?;
        self.validate_size(MAX_FRAME_LEN - TRACE_CONTEXT_BYTES as u32)?;
        let mut payload = BytesMut::with_capacity(TRACE_CONTEXT_BYTES + self.payload.len());
        payload.put_u8(1);
        payload.put_slice(&parent.trace_id());
        payload.put_slice(&parent.span_id());
        payload.put_slice(&self.payload);
        self.payload = payload.freeze();
        self.header.flags |= flags::TRACE_CONTEXT;
        self.header.frame_len = 0;
        Ok(self)
    }

    /// Read a single request's causal parent and return a shared payload slice.
    /// Expand a batch first. No trace identity is inferred from cid or path.
    pub fn split_trace_parent(
        &self,
    ) -> Result<(Option<parrot_api::trace::TraceParent>, Bytes), FrameError> {
        if self.header.flags & flags::TRACE_CONTEXT == 0 {
            return Ok((None, self.payload.clone()));
        }
        if self.header.flags & flags::BATCH != 0
            || !matches!(self.header.frame_type, frame_type::ASK | frame_type::TELL)
        {
            return Err(FrameError::InvalidTraceContext);
        }
        let parent = decode_trace_parent(&self.payload)?;
        Ok((Some(parent), self.payload.slice(TRACE_CONTEXT_BYTES..)))
    }

    /// Remove the transport extension once, retaining the validated parent for
    /// the receiver's actual operation scope. Relays explicitly reattach it.
    pub fn take_trace_parent(
        &mut self,
    ) -> Result<Option<parrot_api::trace::TraceParent>, FrameError> {
        let (parent, payload) = self.split_trace_parent()?;
        self.payload = payload;
        self.header.flags &= !flags::TRACE_CONTEXT;
        self.header.frame_len = 0;
        Ok(parent)
    }

    /// Fully validate a batch before yielding its first member. Iteration copies
    /// metadata for one member at a time and shares the bounded payload buffer.
    pub fn batch_frames(&self) -> Result<BatchFrames<'_>, FrameError> {
        self.validate_payload_flags()?;
        let malformed = |offset| FrameError::BatchMalformed {
            offset,
            total: self.payload.len(),
        };
        if self.header.flags & flags::BATCH == 0
            || self.header.version != PROTOCOL_VERSION
            || self.header.seq != SEQ_NONE
        {
            return Err(malformed(0));
        }
        if self.header.hop_count >= self.header.hop_limit {
            return Err(FrameError::HopExceeded {
                count: self.header.hop_count,
                limit: self.header.hop_limit,
            });
        }
        if self.payload.len() < 8 {
            return Err(FrameError::BatchEmpty {
                got: self.payload.len(),
            });
        }
        let count = u64::from_le_bytes(self.payload[..8].try_into().unwrap());
        if count == 0 {
            return Err(FrameError::BatchEmpty { got: 0 });
        }
        if count > MAX_BATCH_ITEMS as u64 {
            return Err(FrameError::BatchLimit {
                what: "item count",
                max: MAX_BATCH_ITEMS,
            });
        }
        let mut position = 8usize;
        let mut expanded = 0;
        for _ in 0..count {
            if self.payload.len() - position < 12 {
                return Err(malformed(position));
            }
            if self.header.flags & flags::TELL_ACK != 0
                && u64::from_le_bytes(self.payload[position..position + 8].try_into().unwrap()) == 0
            {
                return Err(malformed(position));
            }
            let size = u32::from_le_bytes(
                self.payload[position + 8..position + 12]
                    .try_into()
                    .unwrap(),
            ) as usize;
            position += 12;
            if size > self.payload.len() - position {
                return Err(malformed(position));
            }
            if self.header.flags & flags::TRACE_CONTEXT != 0 {
                decode_trace_parent(&self.payload[position..position + size])?;
            }
            expanded = batch_expanded_size(expanded, self.path.len(), self.type_key.len(), size)?;
            position += size;
        }
        if position != self.payload.len() {
            return Err(malformed(position));
        }
        Ok(BatchFrames {
            frame: self,
            position: 8,
            remaining: count as usize,
        })
    }

    /// Collect a validated batch. Use batch_frames for streaming dispatch.
    pub fn iter_batch(&self) -> Result<Vec<Frame>, FrameError> {
        Ok(self.batch_frames()?.collect())
    }
}

fn decode_trace_parent(bytes: &[u8]) -> Result<parrot_api::trace::TraceParent, FrameError> {
    if bytes.len() < TRACE_CONTEXT_BYTES || bytes[0] != 1 {
        return Err(FrameError::InvalidTraceContext);
    }
    parrot_api::trace::TraceParent::new(
        bytes[1..17].try_into().unwrap(),
        bytes[17..25].try_into().unwrap(),
    )
    .map_err(|_| FrameError::InvalidTraceContext)
}

fn batch_expanded_size(
    previous: usize,
    path: usize,
    key: usize,
    payload: usize,
) -> Result<usize, FrameError> {
    [path, key, payload, std::mem::size_of::<Frame>()]
        .into_iter()
        .try_fold(previous, usize::checked_add)
        .filter(|n| *n <= MAX_FRAME_LEN as usize)
        .ok_or(FrameError::BatchLimit {
            what: "expanded bytes",
            max: MAX_FRAME_LEN as usize,
        })
}
/// Validated batch members; payload slices retain the original bounded frame.
pub struct BatchFrames<'a> {
    frame: &'a Frame,
    position: usize,
    remaining: usize,
}
impl Iterator for BatchFrames<'_> {
    type Item = Frame;
    fn next(&mut self) -> Option<Frame> {
        if self.remaining == 0 {
            return None;
        }
        let bytes = &self.frame.payload;
        let cid = u64::from_le_bytes(bytes[self.position..self.position + 8].try_into().unwrap());
        let len = u32::from_le_bytes(
            bytes[self.position + 8..self.position + 12]
                .try_into()
                .unwrap(),
        ) as usize;
        self.position += 12;
        let payload = bytes.slice(self.position..self.position + len);
        self.position += len;
        self.remaining -= 1;
        let mut header = self.frame.header.clone();
        header.flags &= !flags::BATCH;
        header.frame_len = 0;
        header.correlation_id = cid;
        Some(Frame {
            header,
            path: self.frame.path.clone(),
            type_key: self.frame.type_key.clone(),
            payload,
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for BatchFrames<'_> {}
impl std::iter::FusedIterator for BatchFrames<'_> {}

/// PARROT_TRACE 环境桥（E5.5 冒烟）：首次调用解析 `PARROT_TRACE=frame`，
/// 之后进程内缓存（OnceLock——测试与运行时共用同一开关）。
pub fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("PARROT_TRACE")
            .map(|v| v.split(',').any(|t| t.trim() == "frame"))
            .unwrap_or(false)
    })
}

/// golden vector：固定输入 → 固定字节（四语言共享，07 §2.5 末尾）。
pub struct GoldenVector {
    pub name: &'static str,
    pub frame: Frame,
    pub bytes: &'static [u8],
}

/// 冻结向量集：`docs/vectors/wire1.json` 的唯一事实源（只增不改——
/// 修改 = 协议 break，必须走 version 协商）。dump-vectors 二进制从此导出。
/// OnceLock 惰性构造（Frame 含 String 非 const-constructible；向量内容仍是冻结字面量）。
pub fn golden_vectors() -> &'static [GoldenVector] {
    static V: std::sync::OnceLock<Vec<GoldenVector>> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        vec![
            GoldenVector {
                name: "ask-basic",
                frame: Frame::ask(1, "/x", "bin:t::M", Bytes::from_static(&[0xAB]), None),
                bytes: &[
                    0x2B, 0x00, 0x00, 0x00, // frame_len = 43
                    0x01, // version
                    0x10, // ASK
                    0x00, 0x00, // flags
                    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // cid=1
                    0x00, // hop_count
                    0x08, // hop_limit
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved u48
                    0x02, 0x00, 0x00, 0x00, // path_len=2
                    b'/', b'x', // "/x"
                    0x08, 0x00, 0x00, 0x00, // key_len=8
                    b'b', b'i', b'n', b':', b't', b':', b':', b'M', // "bin:t::M"
                    0x00, 0x00, 0x00, 0x00, // reply_to_len=0（ASK payload 头约定）
                    0xAB, // payload
                ],
            },
            GoldenVector {
                name: "tell-basic",
                frame: Frame::tell("/y", "bin:t::T", Bytes::from_static(&[0x01, 0x02])),
                bytes: &[
                    0x28, 0x00, 0x00, 0x00, // frame_len = 40 = 28+2+8+2
                    0x01, 0x13, 0x00, 0x00, // ver/TELL/flags
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // cid=0
                    0x00, 0x08, // hop
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
                    0x02, 0x00, 0x00, 0x00, // path_len
                    b'/', b'y', 0x08, 0x00, 0x00, 0x00, // key_len
                    b'b', b'i', b'n', b':', b't', b':', b':', b'T', 0x01, 0x02,
                ],
            },
            GoldenVector {
                name: "reply-err-stopped",
                frame: Frame::reply_err(7, "", ErrCode::Stopped, "actor stopped"),
                bytes: &[
                    0x2D, 0x00, 0x00, 0x00, // frame_len = 45 = 28 + 0 + 0 + (4+2+11)
                    0x01, 0x12, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, // cid=7
                    0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, // path_len=0
                    0x00, 0x00, 0x00, 0x00, // key_len=0
                    0x03, 0x00, // code=3 Stopped
                    0x00, 0x00, // reserved
                    b'a', b'c', b't', b'o', b'r', b' ', b's', b't', b'o', b'p', b'p', b'e', b'd',
                ],
            },
            GoldenVector {
                name: "heartbeat",
                frame: Frame::heartbeat(),
                bytes: &[
                    0x1C, 0x00, 0x00, 0x00, // frame_len = 28
                    0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, // path_len=0
                    0x00, 0x00, 0x00, 0x00, // key_len=0
                ],
            },
            GoldenVector {
                name: "reply-u64-full-header",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 32778,
                        correlation_id: 18446744073709551615,
                        hop_count: 1,
                        hop_limit: 9,
                        seq: 4294967295,
                    },
                    path: String::from_utf8(vec![
                        239, 187, 191, 233, 155, 170, 34, 92, 10, 123, 112, 97, 121, 108, 111, 97,
                        100, 125,
                    ])
                    .unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 116, 101, 115, 116]).unwrap(),
                    payload: Bytes::from_static(&[0x00, 0xff, 0x80]),
                },
                bytes: &[
                    0x38, 0x00, 0x00, 0x00, 0x01, 0x11, 0x0a, 0x80, 0xff, 0xff, 0xff, 0xff, 0xff,
                    0xff, 0xff, 0xff, 0x01, 0x09, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x12, 0x00,
                    0x00, 0x00, 0xef, 0xbb, 0xbf, 0xe9, 0x9b, 0xaa, 0x22, 0x5c, 0x0a, 0x7b, 0x70,
                    0x61, 0x79, 0x6c, 0x6f, 0x61, 0x64, 0x7d, 0x07, 0x00, 0x00, 0x00, 0x70, 0x62,
                    0x3a, 0x74, 0x65, 0x73, 0x74, 0x00, 0xff, 0x80,
                ],
            },
            GoldenVector {
                name: "codec-handshake",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 1,
                        flags: 0,
                        correlation_id: 0,
                        hop_count: 0,
                        hop_limit: 1,
                        seq: 0,
                    },
                    path: String::from_utf8(vec![]).unwrap(),
                    type_key: String::from_utf8(vec![]).unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x00, 0x80, 0xff]),
                },
                bytes: &[
                    0x20, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-handshake-ack",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 2,
                        flags: 0,
                        correlation_id: 1,
                        hop_count: 1,
                        hop_limit: 2,
                        seq: 1,
                    },
                    path: String::from_utf8(vec![
                        47, 231, 155, 174, 229, 189, 149, 47, 240, 159, 153, 130,
                    ])
                    .unwrap(),
                    type_key: String::from_utf8(vec![98, 105, 110, 58, 206, 187, 35, 118, 49])
                        .unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x01, 0x80, 0xff]),
                },
                bytes: &[
                    0x35, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x00,
                    0x00, 0x00, 0x2f, 0xe7, 0x9b, 0xae, 0xe5, 0xbd, 0x95, 0x2f, 0xf0, 0x9f, 0x99,
                    0x82, 0x09, 0x00, 0x00, 0x00, 0x62, 0x69, 0x6e, 0x3a, 0xce, 0xbb, 0x23, 0x76,
                    0x31, 0x00, 0x01, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-heartbeat-ack",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 4,
                        flags: 0,
                        correlation_id: 9007199254740991,
                        hop_count: 127,
                        hop_limit: 128,
                        seq: 2147483647,
                    },
                    path: String::from_utf8(vec![]).unwrap(),
                    type_key: String::from_utf8(vec![98, 105, 110, 58, 206, 187, 35, 118, 49])
                        .unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x02, 0x80, 0xff]),
                },
                bytes: &[
                    0x29, 0x00, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff,
                    0xff, 0x1f, 0x00, 0x7f, 0x80, 0xff, 0xff, 0xff, 0x7f, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x62, 0x69, 0x6e, 0x3a, 0xce, 0xbb, 0x23,
                    0x76, 0x31, 0x00, 0x02, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-stop",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 20,
                        flags: 0,
                        correlation_id: 9007199254740992,
                        hop_count: 254,
                        hop_limit: 255,
                        seq: 2147483648,
                    },
                    path: String::from_utf8(vec![
                        47, 231, 155, 174, 229, 189, 149, 47, 240, 159, 153, 130,
                    ])
                    .unwrap(),
                    type_key: String::from_utf8(vec![]).unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x03, 0x80, 0xff]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0xfe, 0xff, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x0c, 0x00,
                    0x00, 0x00, 0x2f, 0xe7, 0x9b, 0xae, 0xe5, 0xbd, 0x95, 0x2f, 0xf0, 0x9f, 0x99,
                    0x82, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-fragment",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 21,
                        flags: 0,
                        correlation_id: 9007199254740993,
                        hop_count: 0,
                        hop_limit: 1,
                        seq: 4294967295,
                    },
                    path: String::from_utf8(vec![]).unwrap(),
                    type_key: String::from_utf8(vec![98, 105, 110, 58, 206, 187, 35, 118, 49])
                        .unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x04, 0x80, 0xff]),
                },
                bytes: &[
                    0x29, 0x00, 0x00, 0x00, 0x01, 0x15, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0x01, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x62, 0x69, 0x6e, 0x3a, 0xce, 0xbb, 0x23,
                    0x76, 0x31, 0x00, 0x04, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-system-event",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 32,
                        flags: 0,
                        correlation_id: 9223372036854775807,
                        hop_count: 1,
                        hop_limit: 2,
                        seq: 0,
                    },
                    path: String::from_utf8(vec![
                        47, 231, 155, 174, 229, 189, 149, 47, 240, 159, 153, 130,
                    ])
                    .unwrap(),
                    type_key: String::from_utf8(vec![98, 105, 110, 58, 206, 187, 35, 118, 49])
                        .unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x05, 0x80, 0xff]),
                },
                bytes: &[
                    0x35, 0x00, 0x00, 0x00, 0x01, 0x20, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff,
                    0xff, 0xff, 0x7f, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x00,
                    0x00, 0x00, 0x2f, 0xe7, 0x9b, 0xae, 0xe5, 0xbd, 0x95, 0x2f, 0xf0, 0x9f, 0x99,
                    0x82, 0x09, 0x00, 0x00, 0x00, 0x62, 0x69, 0x6e, 0x3a, 0xce, 0xbb, 0x23, 0x76,
                    0x31, 0x00, 0x05, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-resolve-q",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 33,
                        flags: 0,
                        correlation_id: 9223372036854775808,
                        hop_count: 127,
                        hop_limit: 128,
                        seq: 1,
                    },
                    path: String::from_utf8(vec![]).unwrap(),
                    type_key: String::from_utf8(vec![]).unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x06, 0x80, 0xff]),
                },
                bytes: &[
                    0x20, 0x00, 0x00, 0x00, 0x01, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x80, 0x7f, 0x80, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-resolve-r",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 34,
                        flags: 0,
                        correlation_id: 18446744073709551615,
                        hop_count: 254,
                        hop_limit: 255,
                        seq: 2147483647,
                    },
                    path: String::from_utf8(vec![
                        47, 231, 155, 174, 229, 189, 149, 47, 240, 159, 153, 130,
                    ])
                    .unwrap(),
                    type_key: String::from_utf8(vec![98, 105, 110, 58, 206, 187, 35, 118, 49])
                        .unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x07, 0x80, 0xff]),
                },
                bytes: &[
                    0x35, 0x00, 0x00, 0x00, 0x01, 0x22, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff,
                    0xff, 0xff, 0xff, 0xfe, 0xff, 0xff, 0xff, 0xff, 0x7f, 0x00, 0x00, 0x0c, 0x00,
                    0x00, 0x00, 0x2f, 0xe7, 0x9b, 0xae, 0xe5, 0xbd, 0x95, 0x2f, 0xf0, 0x9f, 0x99,
                    0x82, 0x09, 0x00, 0x00, 0x00, 0x62, 0x69, 0x6e, 0x3a, 0xce, 0xbb, 0x23, 0x76,
                    0x31, 0x00, 0x07, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-invalidate",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 35,
                        flags: 0,
                        correlation_id: 81985529216486895,
                        hop_count: 0,
                        hop_limit: 1,
                        seq: 2147483648,
                    },
                    path: String::from_utf8(vec![]).unwrap(),
                    type_key: String::from_utf8(vec![98, 105, 110, 58, 206, 187, 35, 118, 49])
                        .unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x08, 0x80, 0xff]),
                },
                bytes: &[
                    0x29, 0x00, 0x00, 0x00, 0x01, 0x23, 0x00, 0x00, 0xef, 0xcd, 0xab, 0x89, 0x67,
                    0x45, 0x23, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x62, 0x69, 0x6e, 0x3a, 0xce, 0xbb, 0x23,
                    0x76, 0x31, 0x00, 0x08, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-route-hint",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 36,
                        flags: 0,
                        correlation_id: 17,
                        hop_count: 1,
                        hop_limit: 2,
                        seq: 4294967295,
                    },
                    path: String::from_utf8(vec![
                        47, 231, 155, 174, 229, 189, 149, 47, 240, 159, 153, 130,
                    ])
                    .unwrap(),
                    type_key: String::from_utf8(vec![]).unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x09, 0x80, 0xff]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x24, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x01, 0x02, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x0c, 0x00,
                    0x00, 0x00, 0x2f, 0xe7, 0x9b, 0xae, 0xe5, 0xbd, 0x95, 0x2f, 0xf0, 0x9f, 0x99,
                    0x82, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "codec-error",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 127,
                        flags: 0,
                        correlation_id: 42,
                        hop_count: 127,
                        hop_limit: 128,
                        seq: 0,
                    },
                    path: String::from_utf8(vec![]).unwrap(),
                    type_key: String::from_utf8(vec![98, 105, 110, 58, 206, 187, 35, 118, 49])
                        .unwrap(),
                    payload: Bytes::from_static(&[0x00, 0x0a, 0x80, 0xff]),
                },
                bytes: &[
                    0x29, 0x00, 0x00, 0x00, 0x01, 0x7f, 0x00, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x7f, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x62, 0x69, 0x6e, 0x3a, 0xce, 0xbb, 0x23,
                    0x76, 0x31, 0x00, 0x0a, 0x80, 0xff,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-00",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 1,
                        correlation_id: 9007199254740992,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 0,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x00, 0xff]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x00, 0xff,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-01",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 2,
                        correlation_id: 9007199254740993,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 1,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x01, 0xfe]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x01, 0xfe,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-02",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 4,
                        correlation_id: 9007199254740994,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 2,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x02, 0xfd]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x04, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x02, 0xfd,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-03",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 8,
                        correlation_id: 9007199254740995,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 3,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x03, 0xfc]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x08, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x03, 0xfc,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-04",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 16,
                        correlation_id: 9007199254740996,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 4,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x04, 0xfb]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x10, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x04, 0xfb,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-05",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 32,
                        correlation_id: 9007199254740997,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 5,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x05, 0xfa]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x20, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x05, 0xfa,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-06",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 64,
                        correlation_id: 9007199254740998,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 6,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x06, 0xf9]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x40, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x06, 0xf9,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-07",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 128,
                        correlation_id: 9007199254740999,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 7,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x07, 0xf8]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x80, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x07, 0xf8,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-08",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 256,
                        correlation_id: 9007199254741000,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 8,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x08, 0xf7]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x08, 0xf7,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-09",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 512,
                        correlation_id: 9007199254741001,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 9,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x09, 0xf6]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x00, 0x02, 0x09, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x09, 0xf6,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-10",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 1024,
                        correlation_id: 9007199254741002,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 10,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x0a, 0xf5]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x00, 0x04, 0x0a, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x0a, 0xf5,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-11",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 2048,
                        correlation_id: 9007199254741003,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 11,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x0b, 0xf4]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x00, 0x08, 0x0b, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x0b, 0xf4,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-12",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 4096,
                        correlation_id: 9007199254741004,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 12,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x0c, 0xf3]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x00, 0x10, 0x0c, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x0c, 0xf3,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-13",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 8192,
                        correlation_id: 9007199254741005,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 13,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x0d, 0xf2]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x00, 0x20, 0x0d, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x0d, 0xf2,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-14",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 16384,
                        correlation_id: 9007199254741006,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 14,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x0e, 0xf1]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x00, 0x40, 0x0e, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x0e, 0xf1,
                ],
            },
            GoldenVector {
                name: "reply-flag-bit-15",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 32768,
                        correlation_id: 9007199254741007,
                        hop_count: 0,
                        hop_limit: 255,
                        seq: 15,
                    },
                    path: String::from_utf8(vec![47, 102, 108, 97, 103, 115]).unwrap(),
                    type_key: String::from_utf8(vec![112, 98, 58, 98, 121, 116, 101, 115]).unwrap(),
                    payload: Bytes::from_static(&[0x0f, 0xf0]),
                },
                bytes: &[
                    0x2c, 0x00, 0x00, 0x00, 0x01, 0x11, 0x00, 0x80, 0x0f, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x20, 0x00, 0x00, 0xff, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00,
                    0x00, 0x00, 0x2f, 0x66, 0x6c, 0x61, 0x67, 0x73, 0x08, 0x00, 0x00, 0x00, 0x70,
                    0x62, 0x3a, 0x62, 0x79, 0x74, 0x65, 0x73, 0x0f, 0xf0,
                ],
            },
            GoldenVector {
                name: "reply-all-flags",
                frame: Frame {
                    header: FrameHeader {
                        frame_len: 0,
                        version: 1,
                        frame_type: 17,
                        flags: 65535,
                        correlation_id: 9223372036854775808,
                        hop_count: 254,
                        hop_limit: 255,
                        seq: 2147483648,
                    },
                    path: String::from_utf8(vec![]).unwrap(),
                    type_key: String::from_utf8(vec![]).unwrap(),
                    payload: Bytes::from_static(&[]),
                },
                bytes: &[
                    0x1c, 0x00, 0x00, 0x00, 0x01, 0x11, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x80, 0xfe, 0xff, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                ],
            },
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        // 全 frame_type 编码→解码 == 原
        let frames = vec![
            Frame::ask(
                9,
                "/a/b",
                "bin:x::Y",
                Bytes::from_static(b"hello"),
                Some("parrot://n/_remote/reply"),
            ),
            Frame::tell("/a", "bin:x::T", Bytes::from_static(b"tell")),
            Frame::reply(9, "/a", "bin:x::R", Bytes::from_static(b"ok")),
            Frame::reply_err(9, "/a", ErrCode::Timeout, "slow"),
            Frame::stop("/a"),
            Frame::heartbeat(),
            Frame::heartbeat_ack(),
            Frame::error_frame(ErrCode::ProtocolViolation, "bad"),
        ];
        for f in frames {
            let mut buf = BytesMut::new();
            f.encode(&mut buf).unwrap();
            let mut buf2 = buf.clone();
            let decoded = Frame::decode(&mut buf2)
                .unwrap()
                .expect("full frame decodes");
            let mut expect = f.clone();
            expect.header.frame_len = decoded.header.frame_len; // encode 前为 0，decode 回填权威值
            assert_eq!(
                decoded, expect,
                "roundtrip mismatch for ft=0x{:02X}",
                f.header.frame_type
            );
            assert!(buf2.is_empty(), "buffer fully consumed");
        }
    }

    #[test]
    fn frame_partial() {
        let f = Frame::ask(1, "/x", "bin:t::M", Bytes::from_static(&[0xAB]), None);
        let mut buf = BytesMut::new();
        f.encode(&mut buf).unwrap();
        let total = buf.len();
        for cut in 1..total {
            let mut partial = buf.clone();
            partial.truncate(cut);
            let mut probe = partial.clone();
            let r = Frame::decode(&mut probe).unwrap();
            assert!(r.is_none(), "cut={cut} should be partial");
            assert_eq!(probe.len(), cut, "partial must not consume");
        }
    }

    #[test]
    fn frame_golden() {
        for v in golden_vectors() {
            let mut buf = BytesMut::new();
            v.frame.encode(&mut buf).unwrap();
            assert_eq!(
                &buf[..],
                v.bytes,
                "golden vector {:?} byte mismatch",
                v.name
            );
            let mut back = BytesMut::from(v.bytes);
            let decoded = Frame::decode(&mut back).unwrap().expect("golden decodes");
            let mut expect = v.frame.clone();
            expect.header.frame_len = decoded.header.frame_len;
            assert_eq!(
                decoded, expect,
                "golden vector {:?} decode mismatch",
                v.name
            );
            assert!(back.is_empty(), "golden vector {:?} trailing bytes", v.name);
        }
    }

    #[test]
    fn frame_golden_every_split_preserves_bytes_and_headers() {
        for vector in golden_vectors() {
            for cut in 0..vector.bytes.len() {
                let mut input = BytesMut::from(&vector.bytes[..cut]);
                let original = input.clone();
                assert!(
                    Frame::decode(&mut input).unwrap().is_none(),
                    "{} cut={cut}",
                    vector.name
                );
                assert_eq!(input, original, "{} partial consumed", vector.name);
                input.extend_from_slice(&vector.bytes[cut..]);
                input.extend_from_slice(vector.bytes);
                let decoded = Frame::decode(&mut input).unwrap().unwrap();
                let mut expected = vector.frame.clone();
                expected.header.frame_len = (vector.bytes.len() - 4) as u32;
                assert_eq!(decoded, expected, "{} cut={cut}", vector.name);
                assert_eq!(&input[..], vector.bytes, "{} tail changed", vector.name);
            }
        }
    }

    #[test]
    fn frame_golden_pipeline_preserves_order_and_all_fields() {
        let mut input = BytesMut::new();
        for vector in golden_vectors() {
            input.extend_from_slice(vector.bytes);
        }
        for vector in golden_vectors() {
            let decoded = Frame::decode(&mut input).unwrap().unwrap();
            let mut expected = vector.frame.clone();
            expected.header.frame_len = (vector.bytes.len() - 4) as u32;
            assert_eq!(decoded, expected, "{} pipeline order/fields", vector.name);
        }
        assert!(input.is_empty());
        assert!(Frame::decode(&mut input).unwrap().is_none());
    }

    #[test]
    fn frame_malformed() {
        // 帧超限
        let mut b = BytesMut::new();
        b.extend_from_slice(&(0x0100_0001u32).to_le_bytes()); // MAX+1
        assert!(matches!(
            Frame::decode(&mut b),
            Err(FrameError::TooLarge { .. })
        ));
        // 版本错
        let f = Frame::tell("/a", "k", Bytes::new());
        let mut b = BytesMut::new();
        f.encode(&mut b).unwrap();
        b[4] = 0x09;
        assert!(matches!(
            Frame::decode(&mut b),
            Err(FrameError::VersionMismatch { got: 9, .. })
        ));
        // 未知帧类型
        let mut b = BytesMut::new();
        f.encode(&mut b).unwrap();
        b[5] = 0x33;
        assert!(matches!(
            Frame::decode(&mut b),
            Err(FrameError::UnknownFrameType { got: 0x33 })
        ));
        // reserved 高 16bit 脏（低 32bit 现在是合法 seq——只有高位仍拒）
        let mut b = BytesMut::new();
        f.encode(&mut b).unwrap();
        b[22] = 0xFF; // 偏移 22 = reserved 第 5 字节（高位段）
        assert!(matches!(
            Frame::decode(&mut b),
            Err(FrameError::ReservedNotZero { .. })
        ));
        // reserved 低 32bit = seq：合法帧（重排语义——不再拒）
        let mut b = BytesMut::new();
        let mut f2 = f.clone();
        f2.header.seq = 42;
        f2.encode(&mut b).unwrap();
        let got = Frame::decode(&mut b).unwrap().unwrap();
        assert_eq!(got.header.seq, 42);
        // 坏 UTF-8（path）
        let mut b = BytesMut::new();
        f.encode(&mut b).unwrap();
        b[28] = 0xFF; // path 首字节
        assert!(matches!(
            Frame::decode(&mut b),
            Err(FrameError::Utf8 { field: "path", .. })
        ));

        // A complete but too-short body must be rejected without a panic.
        let mut short = BytesMut::from(&[10u8, 0, 0, 0, 1, frame_type::TELL][..]);
        assert!(matches!(
            Frame::decode(&mut short),
            Err(FrameError::MalformedLengths { .. })
        ));
        let mut no_key = BytesMut::from(vec![0u8; 4 + BODY_FIXED_OVERHEAD].as_slice());
        no_key[..4].copy_from_slice(&(BODY_FIXED_OVERHEAD as u32).to_le_bytes());
        no_key[4] = PROTOCOL_VERSION;
        no_key[5] = frame_type::TELL;
        no_key[17] = DEFAULT_HOP_LIMIT;
        no_key[28..32].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            Frame::decode(&mut no_key),
            Err(FrameError::MalformedLengths { .. })
        ));

        // Internal path/key lengths cannot consume bytes from a following frame.
        let first = Frame::tell("/a", "k", Bytes::new());
        let second = Frame::tell("/b", "k", Bytes::new());
        let mut joined = BytesMut::new();
        first.encode(&mut joined).unwrap();
        joined[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut tail = BytesMut::new();
        second.encode(&mut tail).unwrap();
        joined.extend_from_slice(&tail);
        assert!(matches!(
            Frame::decode(&mut joined),
            Err(FrameError::MalformedLengths { .. })
        ));

        assert_eq!(
            joined, tail,
            "invalid lengths must not consume the next frame"
        );
        assert_eq!(Frame::decode(&mut joined).unwrap().unwrap().path, "/b");

        let mut exhausted = Frame::tell("/x", "k", Bytes::new());
        exhausted.header.hop_count = exhausted.header.hop_limit;
        let mut wire = BytesMut::new();
        exhausted.encode(&mut wire).unwrap();
        assert!(matches!(
            Frame::decode(&mut wire),
            Err(FrameError::HopExceeded { .. })
        ));
    }

    #[test]
    fn route_hint_rejects_wire_length_overflow() {
        let oversized = "n".repeat(u16::MAX as usize + 1);
        assert!(matches!(
            Frame::try_route_hint(&oversized, "127.0.0.1:1"),
            Err(FrameError::MalformedLengths { .. })
        ));
        assert!(matches!(
            Frame::try_route_hint("node", &oversized),
            Err(FrameError::MalformedLengths { .. })
        ));
        let frame = Frame::try_route_hint("node", "127.0.0.1:1").unwrap();
        assert_eq!(Frame::parse_route_hint(&frame.payload).unwrap().0, "node");
    }

    #[test]
    fn reply_to_prefix() {
        let f = Frame::ask(
            5,
            "/p",
            "k",
            Bytes::from_static(b"data"),
            Some("parrot://n/_remote/reply"),
        );
        let (rt, payload) = f.split_reply_to().unwrap();
        assert_eq!(rt.as_deref(), Some("parrot://n/_remote/reply"));
        assert_eq!(&payload[..], b"data");
        // 无 reply_to
        let f2 = Frame::ask(6, "/p", "k", Bytes::from_static(b"d2"), None);
        let (rt2, p2) = f2.split_reply_to().unwrap();
        assert!(rt2.is_none());
        assert_eq!(&p2[..], b"d2");
    }

    #[test]
    fn trace_line_readable() {
        let f = Frame::ask(1, "/x", "bin:t::M", Bytes::from_static(&[0xAB]), None);
        let line = f.trace_line();
        assert!(line.contains("ft=0x10"));
        assert!(line.contains("cid=1"));
        assert!(line.contains("hop=0/8"));
    }

    // ── D3 BATCH ──────────────────────────────────────────────

    #[test]
    fn batch_roundtrip() {
        let frames: Vec<Frame> = (0..8)
            .map(|i| {
                Frame::tell(
                    "/user/sink",
                    "bin:parrot::M",
                    Bytes::from(vec![0xA0 + i as u8; 32 + i * 7]),
                )
            })
            .collect();
        let bf = Frame::batch(&frames).unwrap();
        assert_eq!(bf.header.flags & flags::BATCH, flags::BATCH);
        assert_eq!(bf.header.frame_type, frame_type::TELL);
        // 线上编码 → 解码 → 解包
        let mut buf = BytesMut::new();
        bf.encode(&mut buf).unwrap();
        let wire = Frame::decode(&mut buf).unwrap().expect("batch decodes");
        let items = wire.iter_batch().unwrap();
        assert_eq!(items.len(), frames.len());
        for (orig, got) in frames.iter().zip(items.iter()) {
            assert_eq!(got.path, orig.path);
            assert_eq!(got.type_key, orig.type_key);
            assert_eq!(&got.payload[..], &orig.payload[..]);
            assert_eq!(got.header.flags & flags::BATCH, 0, "子帧不携带 BATCH");
            // 子帧独立 encode 也合法（自洽帧）
            let mut sb = BytesMut::new();
            got.encode(&mut sb).unwrap();
        }
    }

    #[test]
    fn batch_bandwidth_saving() {
        // 离线字节比较：10k条16B遥测，分成有界批次；不测量msg/s。
        // → 批量后带宽降 >60%（vs 单帧，门禁）
        let n = 10_000;
        let frames: Vec<Frame> = (0..n)
            .map(|i| {
                Frame::tell(
                    "/user/telemetry",
                    "bin:t::M",
                    Bytes::from(vec![i as u8; 16]),
                )
            })
            .collect();
        let per_frame: usize = frames
            .iter()
            .map(|f| {
                let mut b = BytesMut::new();
                f.encode(&mut b).unwrap();
                b.len()
            })
            .sum();
        let mut total_bytes = 0;
        for chunk in frames.chunks(MAX_BATCH_ITEMS) {
            let bf = Frame::batch(chunk).unwrap();
            let mut bb = BytesMut::new();
            bf.encode(&mut bb).unwrap();
            assert!(bb.len() <= MAX_FRAME_LEN as usize + 4, "单批不超帧限");
            total_bytes += bb.len();
        }
        let saved = 1.0 - (total_bytes as f64 / per_frame as f64);
        assert!(
            saved > 0.60,
            "batch saving {:.1}% <= 60% gate ({} vs {per_frame})",
            saved * 100.0,
            total_bytes
        );
    }

    #[test]
    fn batch_rejects_heterogeneous() {
        let a = Frame::tell("/a", "k", Bytes::new());
        let b = Frame::tell("/b", "k", Bytes::new()); // path 不同
        assert!(Frame::batch(&[a, b]).is_err());
        let c = Frame::heartbeat(); // 控制帧
        let d = Frame::tell("/a", "k", Bytes::new());
        assert!(Frame::batch(&[c, d]).is_err());
        assert!(matches!(
            Frame::batch(&[]),
            Err(FrameError::BatchEmpty { got: 0 })
        ));
    }

    #[test]
    fn batch_malformed_payload() {
        // 伪造 BATCH 帧：载荷声称 3 条但只有 1 条数据
        let f = Frame::tell("/a", "k", Bytes::new());
        let mut bf = f.clone();
        bf.header.flags |= flags::BATCH;
        let mut p = BytesMut::new();
        p.put_u64_le(3);
        p.put_u64_le(1);
        p.put_u32_le(1);
        p.put_u8(0xFF);
        bf.payload = p.freeze();
        assert!(matches!(
            bf.iter_batch(),
            Err(FrameError::BatchMalformed { .. })
        ));
        // 非 BATCH 帧调用 iter_batch
        assert!(f.iter_batch().is_err());
    }
}

#[cfg(test)]
mod mirror_payload_contracts {
    use super::*;
    #[test]
    fn mirror_ask_payload_borrows_wire_storage_with_or_without_trace() {
        for traced in [false, true] {
            let mut frame = Frame::ask(
                91,
                "/app",
                "key",
                Bytes::from_static(b"business"),
                Some("reply"),
            );
            if traced {
                frame = frame
                    .with_trace_parent(
                        parrot_api::trace::TraceParent::new([1; 16], [2; 8]).unwrap(),
                    )
                    .unwrap();
            }
            let slice = frame.borrowed_ask_payload().unwrap();
            assert_eq!(slice, b"business");
            assert_eq!(
                slice.as_ptr(),
                frame.payload[frame.payload.len() - 8..].as_ptr()
            );
            assert_eq!(
                frame.split_reply_to().unwrap(),
                (Some("reply".into()), Bytes::from_static(b"business"))
            );
        }
    }
    #[test]
    fn borrowed_and_owned_ask_parsers_agree_on_invalid_reply_addresses() {
        for payload in [
            vec![],
            vec![0, 0, 0],
            vec![255, 255, 255, 255],
            vec![1, 0, 0, 0, 255],
        ] {
            let mut frame = Frame::ask(91, "/app", "key", Bytes::new(), None);
            frame.payload = payload.into();
            assert_eq!(
                frame.borrowed_ask_payload().unwrap_err(),
                frame.split_reply_to().unwrap_err()
            );
        }
    }
}
