//! L0 帧格式（05 §1 的 POC 实现）。
//!
//! 布局与 05 文档逐字节一致：24B 定长头 + path + type_key + payload。

use bytes::{Buf, BufMut, Bytes, BytesMut};

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
    pub fn ask(cid: u64, path: impl Into<String>, type_key: impl Into<String>, payload: Vec<u8>) -> Self {
        Self { frame_type: frame_type::ASK, correlation_id: cid, path: path.into(), type_key: type_key.into(), payload: Bytes::from(payload) }
    }
    pub fn tell(path: impl Into<String>, type_key: impl Into<String>, payload: Vec<u8>) -> Self {
        Self { frame_type: frame_type::TELL, correlation_id: 0, path: path.into(), type_key: type_key.into(), payload: Bytes::from(payload) }
    }
    pub fn reply(cid: u64, type_key: impl Into<String>, payload: Vec<u8>) -> Self {
        Self { frame_type: frame_type::REPLY, correlation_id: cid, path: String::new(), type_key: type_key.into(), payload: Bytes::from(payload) }
    }
    pub fn reply_err(cid: u64, msg: impl Into<String>) -> Self {
        Self { frame_type: frame_type::REPLY_ERR, correlation_id: cid, path: String::new(), type_key: String::new(), payload: Bytes::from(msg.into().into_bytes()) }
    }

    /// 整帧编码（含 4B 长度前缀，用于流式传输）。
    /// 24B 定长头：ver(1) ft(1) flags(2) cid(8) reserved(8) path_len(4)。
    pub fn encode(&self, buf: &mut BytesMut) {
        let path_b = self.path.as_bytes();
        let key_b = self.type_key.as_bytes();
        // 头 28B = frame_len 之外：ver(1)+ft(1)+flags(2)+cid(8)+reserved(8)+path_len(4)+key_len(4)
        let body = 28 + path_b.len() + key_b.len() + self.payload.len();
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
    }

    /// 从缓冲解码一帧；不足一帧返回 Ok(None) 不消费。
    pub fn decode(buf: &mut BytesMut) -> Result<Option<Frame>, String> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let body_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if buf.len() < 4 + body_len {
            return Ok(None); // 半包，等更多数据
        }
        buf.advance(4);
        let _ver = buf.get_u8();
        let ft = buf.get_u8();
        let _flags = buf.get_u16_le();
        let cid = buf.get_u64_le();
        let _reserved = buf.get_u64_le();
        let path_len = buf.get_u32_le() as usize;
        let path = String::from_utf8(buf.copy_to_bytes(path_len).to_vec()).map_err(|e| e.to_string())?;
        let key_len = buf.get_u32_le() as usize;
        let key = String::from_utf8(buf.copy_to_bytes(key_len).to_vec()).map_err(|e| e.to_string())?;
        let payload = buf.copy_to_bytes(body_len - 28 - path_len - key_len);
        Ok(Some(Frame { frame_type: ft, correlation_id: cid, path, type_key: key, payload }))
    }
}

/// golden vector：固定输入 → 固定字节。POC 断言 + 未来 TS/C++/JVM 引用。
pub fn golden_vectors() -> Vec<(&'static str, Frame, Vec<u8>)> {
    let mut v = Vec::new();
    // ASK: cid=1, path="/x", key="bin:t::M", payload=[0xAB]
    let mut buf = BytesMut::new();
    let f = Frame::ask(1, "/x", "bin:t::M", vec![0xAB]);
    f.encode(&mut buf);
    v.push(("ask-basic", f, buf.freeze().to_vec()));
    v
}
