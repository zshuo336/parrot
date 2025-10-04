//! 职责：RemoteEnvelope——线上语义单元（05 §4 语义模型）。
//!
//! 注意：envelope 不直接上线。线上承载 = Frame + ASK payload 头部
//! reply_to 约定（DEV_01 §3.1；ADR-17 信封瘦身：线上只传投递必需字段）。

use bytes::Bytes;

/// 语义模型：一次远程投递的完整语义快照（测试/追踪/未来 durable tell 复用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEnvelope {
    /// 目标逻辑地址 parrot://node/system/user/uuid
    pub path: String,
    /// 消息类型键
    pub type_key: String,
    /// 编码后载荷（不含 reply_to 前缀——那是 Frame 层约定）
    pub payload: Bytes,
    /// ask 发起方系统回程路径（parrot://{self_node}/_remote/reply）
    pub reply_to: Option<String>,
    /// 发起方整体超时（远端不二次超时，07 §7——字段仅语义记录，不上线）
    pub timeout: Option<std::time::Duration>,
}

impl RemoteEnvelope {
    /// ask 语义 → ASK 帧（reply_to 进 payload 头）。
    pub fn to_ask_frame(&self, cid: u64) -> crate::frame::Frame {
        crate::frame::Frame::ask(cid, &self.path, &self.type_key, self.payload.clone(), self.reply_to.as_deref())
    }

    /// tell 语义 → TELL 帧。
    pub fn to_tell_frame(&self) -> crate::frame::Frame {
        crate::frame::Frame::tell(&self.path, &self.type_key, self.payload.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_to_frames() {
        let e = RemoteEnvelope {
            path: "/user/a".into(),
            type_key: "bin:x::M".into(),
            payload: Bytes::from_static(b"p"),
            reply_to: Some("parrot://n/_remote/reply".into()),
            timeout: Some(std::time::Duration::from_millis(50)),
        };
        let ask = e.to_ask_frame(7);
        assert_eq!(ask.header.frame_type, crate::frame::frame_type::ASK);
        let (rt, payload) = ask.split_reply_to().unwrap();
        assert_eq!(rt.as_deref(), Some("parrot://n/_remote/reply"));
        assert_eq!(&payload[..], b"p");
        let tell = e.to_tell_frame();
        assert_eq!(tell.header.frame_type, crate::frame::frame_type::TELL);
    }
}
