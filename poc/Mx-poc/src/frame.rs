//! L0 帧格式（05 §1 的 POC 实现）。
//!
//! 布局与 05 文档逐字节一致：28B 定长字段 + path + type_key + payload。

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// Maximum body size, excluding the four-byte length prefix.
pub const MAX_FRAME_BODY: usize = 1024 * 1024;

pub mod frame_type {
    pub const ASK: u8 = 0x10;
    pub const REPLY: u8 = 0x11;
    pub const REPLY_ERR: u8 = 0x12;
    pub const TELL: u8 = 0x13;
    pub const STOP: u8 = 0x14;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub frame_type: u8,
    pub correlation_id: u64,
    pub path: String,
    pub type_key: String,
    pub payload: Bytes,
}

impl Frame {
    pub fn ask(
        cid: u64,
        path: impl Into<String>,
        type_key: impl Into<String>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            frame_type: frame_type::ASK,
            correlation_id: cid,
            path: path.into(),
            type_key: type_key.into(),
            payload: Bytes::from(payload),
        }
    }
    pub fn tell(path: impl Into<String>, type_key: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            frame_type: frame_type::TELL,
            correlation_id: 0,
            path: path.into(),
            type_key: type_key.into(),
            payload: Bytes::from(payload),
        }
    }
    pub fn reply(cid: u64, type_key: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            frame_type: frame_type::REPLY,
            correlation_id: cid,
            path: String::new(),
            type_key: type_key.into(),
            payload: Bytes::from(payload),
        }
    }
    pub fn reply_err(cid: u64, msg: impl Into<String>) -> Self {
        Self {
            frame_type: frame_type::REPLY_ERR,
            correlation_id: cid,
            path: String::new(),
            type_key: String::new(),
            payload: Bytes::from(msg.into().into_bytes()),
        }
    }

    /// Shared validation for queue admission and encoding, without an encoded buffer.
    pub(crate) fn validated_body_len(&self) -> Result<usize, String> {
        let body = 28usize
            .checked_add(self.path.len())
            .and_then(|n| n.checked_add(self.type_key.len()))
            .and_then(|n| n.checked_add(self.payload.len()))
            .filter(|n| *n <= MAX_FRAME_BODY)
            .ok_or("frame exceeds size limit")?;
        if !(frame_type::ASK..=frame_type::STOP).contains(&self.frame_type) {
            return Err("unknown frame type".into());
        }
        Ok(body)
    }

    /// 整帧编码（含 4B 长度前缀，用于流式传输）。
    /// 28B 固定字段：ver(1) ft(1) flags(2) cid(8) reserved(8) path_len(4) key_len(4)。
    pub fn encode(&self, buf: &mut BytesMut) -> Result<(), String> {
        let body = self.validated_body_len()?;
        let path_b = self.path.as_bytes();
        let key_b = self.type_key.as_bytes();
        buf.put_u32_le(body as u32);
        buf.put_u8(0x01); // version
        buf.put_u8(self.frame_type);
        buf.put_u16_le(0); // flags
        buf.put_u64_le(self.correlation_id);
        buf.put_u64_le(0); // reserved（对齐 24B 头，未来 flags 扩展）
        buf.put_u32_le(path_b.len() as u32);
        buf.put_slice(path_b);
        buf.put_u32_le(key_b.len() as u32);
        buf.put_slice(key_b);
        buf.put_slice(&self.payload);
        Ok(())
    }

    /// 从缓冲解码一帧；不足一帧返回 Ok(None) 不消费。
    pub fn decode(buf: &mut BytesMut) -> Result<Option<Frame>, String> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let body_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if !(28..=MAX_FRAME_BODY).contains(&body_len) {
            return Err("invalid frame body length".into());
        }
        if buf.len() < 4 + body_len {
            return Ok(None);
        }
        // Parse only this frame. No malformed field may borrow from the next
        // frame, and neither partial nor invalid input is consumed.
        let body = &buf[4..4 + body_len];
        let ft = body[1];
        if body[0] != 1
            || body[2..4] != [0; 2]
            || body[12..20] != [0; 8]
            || !(frame_type::ASK..=frame_type::STOP).contains(&ft)
        {
            return Err("unsupported frame header".into());
        }
        let cid = u64::from_le_bytes(body[4..12].try_into().unwrap());
        let path_len = u32::from_le_bytes(body[20..24].try_into().unwrap()) as usize;
        if path_len > body_len - 28 {
            return Err("path length exceeds frame".into());
        }
        let key_offset = 24 + path_len;
        let key_len =
            u32::from_le_bytes(body[key_offset..key_offset + 4].try_into().unwrap()) as usize;
        let key_start = key_offset + 4;
        if key_len > body_len - key_start {
            return Err("type key length exceeds frame".into());
        }
        let path = std::str::from_utf8(&body[24..key_offset])
            .map_err(|e| e.to_string())?
            .to_owned();
        let type_key = std::str::from_utf8(&body[key_start..key_start + key_len])
            .map_err(|e| e.to_string())?
            .to_owned();
        let payload = Bytes::copy_from_slice(&body[key_start + key_len..]);
        buf.advance(4 + body_len);
        Ok(Some(Frame {
            frame_type: ft,
            correlation_id: cid,
            path,
            type_key,
            payload,
        }))
    }
}

/// golden vector：固定输入 → 固定字节。POC 断言 + 未来 TS/C++/JVM 引用。
pub fn golden_vectors() -> Vec<(&'static str, Frame, Vec<u8>)> {
    vec![(
        "ask-basic",
        Frame::ask(1, "/x", "bin:t::M", vec![0xAB]),
        vec![
            39, 0, 0, 0, 1, 16, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0,
            47, 120, 8, 0, 0, 0, 98, 105, 110, 58, 116, 58, 58, 77, 171,
        ],
    )]
}
