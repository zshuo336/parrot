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
//! 18   6    reserved    u48 = 0
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
    /// P1 不实现收发；解码遇之报 UnknownFrameType（常量仅为对端版本 > 本端时统一报错）
    pub const FRAGMENT: u8 = 0x15;
    /// P1 收到即断连（协议违规；P2 启用管理/集群载荷）
    pub const SYSTEM_EVENT: u8 = 0x20;
    pub const RESOLVE_Q: u8 = 0x21; // P1 收到回 ERROR 断连（P5）
    pub const RESOLVE_R: u8 = 0x22;
    pub const INVALIDATE: u8 = 0x23;
    pub const ERROR: u8 = 0x7F;
}

pub mod flags {
    pub const COMPRESSED_ZSTD: u16 = 1 << 0; // P2
    pub const TRACING: u16 = 1 << 1; // P2
    pub const BATCH: u16 = 1 << 2; // P4
    pub const URGENT: u16 = 1 << 3; // P2
    pub const TELL_ACK: u16 = 1 << 4; // P3
    pub const APP_ENCRYPTED: u16 = 1 << 5; // 1.0 可选
}

/// reserved u48 掩码（6 字节低位）
const RESERVED_MASK: u64 = 0xFFFF_FFFF_FFFF;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
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
    #[error("BATCH 载荷损坏：offset {offset} 处 item 长度越界（载荷 {total}B）")]
    BatchMalformed { offset: usize, total: usize },
    #[error("BATCH 帧载荷需 ≥8B（至少容纳 item 计数）实得 {got}B")]
    BatchEmpty { got: usize },
}

impl FrameError {
    /// 机器可读映射（E5.5 三态）：帧级错误 → 结构化错误码。
    pub fn to_errcode(&self) -> ErrCode {
        match self {
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
        if self.payload.len() < 4 {
            return Err(FrameError::MalformedLengths {
                flen: self.header.frame_len,
                plen: 0,
                klen: 0,
            });
        }
        // 零整段 clone 读前缀（DEV_08：slice 直读——payload 为 Vec<u8>，
        // 旧实现 clone 整段 BytesMut 再消费是大载荷入站隐性分配热点）
        let rlen = u32::from_le_bytes([
            self.payload[0],
            self.payload[1],
            self.payload[2],
            self.payload[3],
        ]) as usize;
        if self.payload.len() < 4 + rlen {
            return Err(FrameError::MalformedLengths {
                flen: self.header.frame_len,
                plen: rlen as u32,
                klen: 0,
            });
        }
        let reply_to = if rlen == 0 {
            None
        } else {
            Some(
                String::from_utf8(self.payload[4..4 + rlen].to_vec()).map_err(|e| {
                    FrameError::Utf8 {
                        field: "reply_to",
                        source: e.utf8_error(),
                    }
                })?,
            )
        };
        Ok((reply_to, self.payload[4 + rlen..].to_vec().into()))
    }

    /// 编码进 buf（含 4B frame_len 前缀）。
    pub fn encode(&self, buf: &mut BytesMut) -> Result<(), FrameError> {
        let path_b = self.path.as_bytes();
        let key_b = self.type_key.as_bytes();
        let body = BODY_FIXED_OVERHEAD as u64
            + path_b.len() as u64
            + key_b.len() as u64
            + self.payload.len() as u64;
        if body > MAX_FRAME_LEN as u64 {
            return Err(FrameError::TooLarge {
                len: body as u32,
                max: MAX_FRAME_LEN,
            });
        }
        if self.header.version != PROTOCOL_VERSION {
            return Err(FrameError::VersionMismatch {
                expect: PROTOCOL_VERSION,
                got: self.header.version,
            });
        }
        buf.put_u32_le(body as u32);
        buf.put_u8(self.header.version);
        buf.put_u8(self.header.frame_type);
        buf.put_u16_le(self.header.flags);
        buf.put_u64_le(self.header.correlation_id);
        buf.put_u8(self.header.hop_count);
        buf.put_u8(self.header.hop_limit);
        // reserved u48 = 0（6 字节，偏移 18..24）
        buf.put_slice(&[0u8; 6]);
        buf.put_u32_le(path_b.len() as u32);
        buf.put_slice(path_b);
        buf.put_u32_le(key_b.len() as u32);
        buf.put_slice(key_b);
        buf.put_slice(&self.payload);
        Ok(())
    }

    /// 从 buf 解码一帧；不足一帧返回 Ok(None) 且不消费。
    pub fn decode(buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let body_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if body_len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge {
                len: body_len,
                max: MAX_FRAME_LEN,
            });
        }
        if buf.len() < 4 + body_len as usize {
            return Ok(None); // 半包
        }
        buf.advance(4);
        let version = buf.get_u8();
        if version != PROTOCOL_VERSION {
            return Err(FrameError::VersionMismatch {
                expect: PROTOCOL_VERSION,
                got: version,
            });
        }
        let ft = buf.get_u8();
        // P1 白名单外的帧类型（FRAGMENT 不收发；SYSTEM_EVENT/RESOLVE*/INVALIDATE 由
        // 连接层断连处理——帧层仍需能解析以便回 ERROR，故仅拒未知码点）。
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
                | frame_type::ERROR
        );
        if !known {
            return Err(FrameError::UnknownFrameType { got: ft });
        }
        let fl = buf.get_u16_le();
        let cid = buf.get_u64_le();
        let hop_count = buf.get_u8();
        let hop_limit = buf.get_u8();
        // reserved u48（帧偏移 18..24）——此前已顺序消费 ver/ft/flags/cid/hop 共 14B
        let reserved_bytes = buf.copy_to_bytes(6);
        let mut reserved: u64 = 0;
        for i in 0..6 {
            reserved |= (reserved_bytes[i] as u64) << (8 * i);
        }
        let _ = reserved;
        let reserved_check: u64 = reserved & RESERVED_MASK;
        if reserved_check != 0 {
            return Err(FrameError::ReservedNotZero {
                got: reserved_check,
            });
        }
        let path_len = buf.get_u32_le() as usize;
        let path = String::from_utf8(buf.copy_to_bytes(path_len).to_vec()).map_err(|e| {
            FrameError::Utf8 {
                field: "path",
                source: e.utf8_error(),
            }
        })?;
        let key_len = buf.get_u32_le() as usize;
        let key = String::from_utf8(buf.copy_to_bytes(key_len).to_vec()).map_err(|e| {
            FrameError::Utf8 {
                field: "type_key",
                source: e.utf8_error(),
            }
        })?;
        let payload_len = body_len as usize - BODY_FIXED_OVERHEAD - path_len - key_len;
        let payload = buf.copy_to_bytes(payload_len);
        let header = FrameHeader {
            frame_len: body_len,
            version,
            frame_type: ft,
            flags: fl,
            correlation_id: cid,
            hop_count,
            hop_limit,
        };
        Ok(Some(Frame {
            header,
            path,
            type_key: key,
            payload,
        }))
    }

    /// PARROT_TRACE=frame 的单行摘要（E5.5：人类/机器/LLM 三态可读）。
    pub fn trace_line(&self) -> String {
        format!(
            "frame ft=0x{:02X} cid={} hop={}/{} flags=0x{:04X} path={:?} key={:?} len={}",
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
        if frames.is_empty() {
            return Err(FrameError::BatchEmpty { got: 0 });
        }
        let first = &frames[0];
        for f in frames {
            let ok = matches!(
                f.header.frame_type,
                frame_type::ASK | frame_type::REPLY | frame_type::TELL
            );
            if !ok {
                return Err(FrameError::BatchMalformed {
                    offset: 0,
                    total: frames.len(),
                });
            }
            if f.path != first.path || f.type_key != first.type_key {
                return Err(FrameError::BatchMalformed {
                    offset: 0,
                    total: frames.len(),
                });
            }
        }
        let mut payload = BytesMut::new();
        payload.put_u64_le(frames.len() as u64);
        for f in frames {
            payload.put_u64_le(f.header.correlation_id);
            payload.put_u32_le(f.payload.len() as u32);
            payload.put_slice(&f.payload);
        }
        let hop_count = frames.iter().map(|f| f.header.hop_count).max().unwrap_or(0);
        Ok(Frame {
            header: FrameHeader {
                frame_len: 0, // encode 回填
                version: PROTOCOL_VERSION,
                frame_type: first.header.frame_type,
                flags: first.header.flags | flags::BATCH,
                correlation_id: 0, // 批载体无整体 cid
                hop_count,
                hop_limit: DEFAULT_HOP_LIMIT,
            },
            path: first.path.clone(),
            type_key: first.type_key.clone(),
            payload: payload.freeze(),
        })
    }

    /// 解包 BATCH 帧 → 逐条子帧（恢复独立 cid 与载体 flags 的净拷贝）。
    ///
    /// 空 payload / item 越界 → BatchMalformed/BatchEmpty（协议违规回
    /// ERROR 帧断连——与帧层错误处理一致）。
    pub fn iter_batch(&self) -> Result<Vec<Frame>, FrameError> {
        if self.header.flags & flags::BATCH == 0 {
            return Err(FrameError::BatchMalformed {
                offset: 0,
                total: self.payload.len(),
            });
        }
        let mut p = &self.payload[..];
        if p.len() < 8 {
            return Err(FrameError::BatchEmpty { got: p.len() });
        }
        let count = u64::from_le_bytes(p[..8].try_into().unwrap()) as usize;
        p = &p[8..];
        let mut out = Vec::with_capacity(count.min(1024));
        for i in 0..count {
            let _ = i;
            if p.len() < 12 {
                return Err(FrameError::BatchMalformed {
                    offset: self.payload.len() - p.len(),
                    total: self.payload.len(),
                });
            }
            let cid = u64::from_le_bytes(p[..8].try_into().unwrap());
            let len = u32::from_le_bytes(p[8..12].try_into().unwrap()) as usize;
            p = &p[12..];
            if p.len() < len {
                return Err(FrameError::BatchMalformed {
                    offset: self.payload.len() - p.len(),
                    total: self.payload.len(),
                });
            }
            out.push(Frame {
                header: FrameHeader {
                    frame_len: 0,
                    version: self.header.version,
                    frame_type: self.header.frame_type,
                    flags: self.header.flags & !flags::BATCH, // 子帧还原净 flags
                    correlation_id: cid,
                    hop_count: self.header.hop_count,
                    hop_limit: self.header.hop_limit,
                },
                path: self.path.clone(),
                type_key: self.type_key.clone(),
                payload: Bytes::copy_from_slice(&p[..len]),
            });
            p = &p[len..];
        }
        Ok(out)
    }
}

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
        }
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
        // reserved 脏
        let mut b = BytesMut::new();
        f.encode(&mut b).unwrap();
        b[18] = 0xFF;
        assert!(matches!(
            Frame::decode(&mut b),
            Err(FrameError::ReservedNotZero { .. })
        ));
        // 坏 UTF-8（path）
        let mut b = BytesMut::new();
        f.encode(&mut b).unwrap();
        b[28] = 0xFF; // path 首字节
        assert!(matches!(
            Frame::decode(&mut b),
            Err(FrameError::Utf8 { field: "path", .. })
        ));
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
        // 传感流场景（06 P4.3 验收口径）：10k msg/s、16B 遥测小消息
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
        let bf = Frame::batch(&frames).unwrap();
        let mut bb = BytesMut::new();
        bf.encode(&mut bb).unwrap();
        assert!(bb.len() <= MAX_FRAME_LEN as usize + 4, "单批不超帧限");
        let saved = 1.0 - (bb.len() as f64 / per_frame as f64);
        assert!(
            saved > 0.60,
            "batch saving {:.1}% <= 60% gate ({} vs {per_frame})",
            saved * 100.0,
            bb.len()
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
