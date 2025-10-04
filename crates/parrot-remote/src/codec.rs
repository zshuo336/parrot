//! 职责：CodecStack trait + bin 栈实现（pb P1 占位返回 Unimplemented——05 §3.3）。

use crate::error::ErrCode;

/// 编码栈。type_key 前缀路由：bin: → Bin；pb: → Pb。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecStack {
    /// bincode（serde 栈；Rust↔Rust 默认）
    Bin,
    /// protobuf（P2 随 akka 网关交付；P1 全路径返回 Unimplemented）
    Pb,
}

impl CodecStack {
    /// type_key 前缀 → 栈。
    pub fn of_type_key(key: &str) -> Result<Self, ErrCode> {
        if key.starts_with("bin:") {
            Ok(Self::Bin)
        } else if key.starts_with("pb:") {
            Ok(Self::Pb)
        } else {
            Err(ErrCode::UnknownTypeKey)
        }
    }

    /// P1 时 Pb 返回 Unimplemented；P2（K5）起 pb 栈随 JVM 桥消息交付放行。
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
        assert_eq!(CodecStack::of_type_key("bin:x::M#v1"), Ok(CodecStack::Bin));
        assert_eq!(CodecStack::of_type_key("pb:pkg.Msg"), Ok(CodecStack::Pb));
        assert_eq!(
            CodecStack::of_type_key("json:x"),
            Err(ErrCode::UnknownTypeKey)
        );
        assert!(CodecStack::Bin.assert_available().is_ok());
        assert!(CodecStack::Pb.assert_available().is_ok(), "P2 起 pb 栈放行（K5）");
    }
}
