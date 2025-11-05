//! Wire codec selection and checked readers for application payloads.
//! Registrations must provide the matching bincode or protobuf codec.

use crate::error::ErrCode;

mod payload;
pub use payload::{PayloadError, PayloadReader};

/// 编码栈。type_key 前缀路由：bin: → Bin；pb: → Pb。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecStack {
    /// bincode（serde 栈；Rust↔Rust 默认）
    Bin,
    /// protobuf（prost 编解码器或显式协议适配器）
    Pb,
}

impl CodecStack {
    /// type_key 前缀 → 栈。
    pub fn of_type_key(key: &str) -> Result<Self, ErrCode> {
        if key.len() > u16::MAX as usize || key.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(ErrCode::UnknownTypeKey);
        }
        if key
            .strip_prefix("bin:")
            .is_some_and(|name| !name.is_empty())
        {
            Ok(Self::Bin)
        } else if key.strip_prefix("pb:").is_some_and(|name| !name.is_empty()) {
            Ok(Self::Pb)
        } else {
            Err(ErrCode::UnknownTypeKey)
        }
    }

    /// 两种已实现的编码栈均可用；具体编解码函数由注册项提供。
    pub fn assert_available(&self) -> Result<(), ErrCode> {
        match self {
            Self::Bin | Self::Pb => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_routing() {
        for key in ["bin:", "pb:", "pb:x y", "bin:x\0y", "", "json:x"] {
            assert_eq!(CodecStack::of_type_key(key), Err(ErrCode::UnknownTypeKey));
        }
        assert_eq!(
            CodecStack::of_type_key(&format!("bin:{}", "x".repeat(65532))),
            Err(ErrCode::UnknownTypeKey)
        );
        assert_eq!(CodecStack::of_type_key("bin:x::M#v1"), Ok(CodecStack::Bin));
        assert_eq!(CodecStack::of_type_key("pb:pkg.Msg"), Ok(CodecStack::Pb));
        assert_eq!(
            CodecStack::of_type_key("json:x"),
            Err(ErrCode::UnknownTypeKey)
        );
        assert!(CodecStack::Bin.assert_available().is_ok());
        assert!(
            CodecStack::Pb.assert_available().is_ok(),
            "P2 起 pb 栈放行（K5）"
        );
    }
}
