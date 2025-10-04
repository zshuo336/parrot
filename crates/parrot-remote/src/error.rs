//! 职责：RemoteError + 07 §2.5 十三项错误码表 + ActorError 映射（DEV_01 §3.8）。
//!
//! wire 契约：REPLY_ERR payload = `[u16 code][u16 rsv=0][utf-8 detail]`。
//! 码值冻结，禁止重排（errcode_table_frozen 测试守卫）。

use parrot_api::errors::ActorError;

/// 07 §2.5 结构化错误码（X4 裁定：13 项，冻结）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ErrCode {
    ActorNotFound = 1,
    Timeout = 2,
    Stopped = 3,
    NotRemotable = 4,
    CodecError = 5,
    UnknownTypeKey = 6,
    RouteUnreachable = 7,
    ConnectionLost = 8,
    DirectoryStale = 9,
    Overloaded = 10,
    NoCommonCodec = 11,
    ProtocolViolation = 12,
    Forbidden = 13,
}

impl ErrCode {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::ActorNotFound,
            2 => Self::Timeout,
            3 => Self::Stopped,
            4 => Self::NotRemotable,
            5 => Self::CodecError,
            6 => Self::UnknownTypeKey,
            7 => Self::RouteUnreachable,
            8 => Self::ConnectionLost,
            9 => Self::DirectoryStale,
            10 => Self::Overloaded,
            11 => Self::NoCommonCodec,
            12 => Self::ProtocolViolation,
            13 => Self::Forbidden,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ActorNotFound => "ActorNotFound",
            Self::Timeout => "Timeout",
            Self::Stopped => "Stopped",
            Self::NotRemotable => "NotRemotable",
            Self::CodecError => "CodecError",
            Self::UnknownTypeKey => "UnknownTypeKey",
            Self::RouteUnreachable => "RouteUnreachable",
            Self::ConnectionLost => "ConnectionLost",
            Self::DirectoryStale => "DirectoryStale",
            Self::Overloaded => "Overloaded",
            Self::NoCommonCodec => "NoCommonCodec",
            Self::ProtocolViolation => "ProtocolViolation",
            Self::Forbidden => "Forbidden",
        }
    }

    /// 接收侧映射（07 §2.5 第三列；05 §9 策略：不新增 ActorError 变体）。
    pub fn to_actor_error(self, detail: String) -> ActorError {
        match self {
            Self::ActorNotFound => ActorError::ActorNotFound(detail),
            Self::Timeout => ActorError::TimeoutDetail(detail),
            Self::Stopped => ActorError::Stopped,
            Self::NotRemotable => {
                ActorError::MessageHandlingError(format!("not remotable: {detail}"))
            }
            Self::CodecError => ActorError::MessageHandlingError(format!("codec: {detail}")),
            Self::UnknownTypeKey => {
                ActorError::MessageHandlingError(format!("unknown type key: {detail}"))
            }
            Self::RouteUnreachable => ActorError::ActorNotFound(detail),
            Self::ConnectionLost => ActorError::InternalError("remote connection lost".into()),
            Self::DirectoryStale => ActorError::InternalError(format!("directory stale: {detail}")),
            Self::Overloaded => ActorError::InternalError("remote outbound queue full".into()),
            Self::NoCommonCodec => ActorError::InternalError("no common codec".into()),
            Self::ProtocolViolation => {
                ActorError::InternalError(format!("protocol violation: {detail}"))
            }
            Self::Forbidden => ActorError::MessageHandlingError(format!("forbidden: {detail}")),
        }
    }

    /// 发送侧映射：本地 ActorError → (ErrCode, detail)。11 变体全覆盖。
    pub fn from_actor_error(e: &ActorError) -> (Self, String) {
        match e {
            ActorError::InitializationError(s) => (Self::ProtocolViolation, s.clone()),
            ActorError::MessageHandlingError(s) => (Self::CodecError, s.clone()),
            ActorError::Stopped => (Self::Stopped, "actor stopped".into()),
            ActorError::Timeout => (Self::Timeout, "timeout".into()),
            ActorError::TimeoutDetail(s) => (Self::Timeout, s.clone()),
            ActorError::ActorNotFound(s) => (Self::ActorNotFound, s.clone()),
            // enqueue 失败且 detail 标 Closed = 目标 mailbox 已关（stopped）——
            // 远程死信等价映射 Stopped（RC5；引擎在 closed 通道上的权威信号）
            ActorError::InternalError(s) if s.contains("Closed") => (Self::Stopped, s.clone()),
            ActorError::InternalError(s) => (Self::ConnectionLost, s.clone()),
            ActorError::ProcessMessageError(s) => (Self::CodecError, s.clone()),
            // 引擎 ReplyChannelError（mailbox closed/ask 通道关）在远程语境 =
            // 目标 actor 已停止接收 → Stopped（RC5 跨网等价语义）
            ActorError::ReplyChannelError(s) => {
                (Self::Stopped, format!("reply channel closed: {s}"))
            }
            ActorError::Panic(s) => (Self::ProtocolViolation, s.clone()),
            ActorError::Other(e) => (Self::ProtocolViolation, e.to_string()),
        }
    }
}

/// REPLY_ERR payload 编码：`[u16 code][u16 rsv=0][utf-8 detail]`
pub fn encode_err_payload(code: ErrCode, detail: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + detail.len());
    v.extend_from_slice(&(code as u16).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(detail.as_bytes());
    v
}

/// REPLY_ERR payload 解码。
pub fn decode_err_payload(payload: &[u8]) -> Result<(ErrCode, String), String> {
    if payload.len() < 4 {
        return Err(format!("err payload too short: {}", payload.len()));
    }
    let code = u16::from_le_bytes([payload[0], payload[1]]);
    let code = ErrCode::from_u16(code).ok_or_else(|| format!("unknown err code {code}"))?;
    let detail = String::from_utf8_lossy(&payload[4..]).to_string();
    Ok((code, detail))
}

/// 远程层错误（本地侧 API 面；wire 上走 ErrCode）。
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("node not in table: {0}")]
    UnknownNode(String),
    #[error("handshake: {0}")]
    Handshake(String),
    #[error("frame: {0}")]
    Frame(#[from] crate::frame::FrameError),
    #[error("codec: {0}")]
    Codec(String),
    #[error("callback table full (capacity {0})")]
    CallbacksFull(usize),
    #[error("system shutting down")]
    ShuttingDown,
}

impl RemoteError {
    pub fn to_actor_error(&self) -> ActorError {
        match self {
            RemoteError::Transport(s) => {
                ActorError::InternalError(format!("remote transport: {s}"))
            }
            RemoteError::UnknownNode(n) => {
                ActorError::ActorNotFound(format!("remote node unknown: {n}"))
            }
            RemoteError::Handshake(s) => ActorError::InternalError(format!("handshake: {s}")),
            RemoteError::Frame(fe) => fe.to_errcode().to_actor_error(fe.to_string()),
            RemoteError::Codec(s) => ErrCode::CodecError.to_actor_error(s.clone()),
            RemoteError::CallbacksFull(n) => {
                ErrCode::Overloaded.to_actor_error(format!("callbacks capacity {n}"))
            }
            RemoteError::ShuttingDown => ActorError::InternalError("remote shutting down".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errcode_table_frozen() {
        // 13 项逐一断言数值与字符串名（防手滑重排——wire 契约）
        let table: &[(ErrCode, u16, &str)] = &[
            (ErrCode::ActorNotFound, 1, "ActorNotFound"),
            (ErrCode::Timeout, 2, "Timeout"),
            (ErrCode::Stopped, 3, "Stopped"),
            (ErrCode::NotRemotable, 4, "NotRemotable"),
            (ErrCode::CodecError, 5, "CodecError"),
            (ErrCode::UnknownTypeKey, 6, "UnknownTypeKey"),
            (ErrCode::RouteUnreachable, 7, "RouteUnreachable"),
            (ErrCode::ConnectionLost, 8, "ConnectionLost"),
            (ErrCode::DirectoryStale, 9, "DirectoryStale"),
            (ErrCode::Overloaded, 10, "Overloaded"),
            (ErrCode::NoCommonCodec, 11, "NoCommonCodec"),
            (ErrCode::ProtocolViolation, 12, "ProtocolViolation"),
            (ErrCode::Forbidden, 13, "Forbidden"),
        ];
        for (code, num, name) in table {
            assert_eq!(*code as u16, *num);
            assert_eq!(code.name(), *name);
            assert_eq!(ErrCode::from_u16(*num), Some(*code));
        }
        assert_eq!(ErrCode::from_u16(14), None);
        assert_eq!(ErrCode::from_u16(0), None);
    }

    #[test]
    fn errcode_roundtrip_actor_error() {
        // 11 变体 → (code, detail) → to_actor_error 回程一致类别
        let variants = vec![
            ActorError::ActorNotFound("/a".into()),
            ActorError::TimeoutDetail("1s".into()),
            ActorError::Stopped,
            ActorError::MessageHandlingError("boom".into()),
            ActorError::InitializationError("init".into()),
            ActorError::InternalError("int".into()),
            ActorError::ProcessMessageError("pm".into()),
            ActorError::ReplyChannelError("rc".into()),
            ActorError::Panic("p".into()),
            ActorError::Timeout,
        ];
        for e in &variants {
            let (code, detail) = ErrCode::from_actor_error(e);
            // wire 编解码往返
            let payload = encode_err_payload(code, &detail);
            let (code2, detail2) = decode_err_payload(&payload).unwrap();
            assert_eq!(code2, code);
            assert_eq!(detail2, detail);
            // 关键类别守卫
            if matches!(e, ActorError::Stopped) {
                assert_eq!(code, ErrCode::Stopped);
                assert!(matches!(code.to_actor_error(detail2), ActorError::Stopped));
            }
            if matches!(e, ActorError::ActorNotFound(_)) {
                assert_eq!(code, ErrCode::ActorNotFound);
            }
        }
    }

    #[test]
    fn err_payload_malformed() {
        assert!(decode_err_payload(&[0, 3]).is_err());
        assert!(decode_err_payload(&[14, 0, 0, 0, b'x']).is_err()); // 未知码 14
    }
}
