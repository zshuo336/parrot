//! Receptionist 类型定义（DEV_02 §3.1 / K2）。
//!
//! 分层：parrot-api 只定义接口类型（key/event/stream）；实现（本地表 +
//! gossip 同车 + 订阅流）在 parrot-remote `receptionist.rs`——引擎 context
//! 经依赖注入转发（双引擎各自集成）。
//!
//! wire 序列化（serde 派生）在 `remote` feature 下；核心类型无 feature 门
//! （context trait 三方法引用——默认实现可用）。

#[cfg(feature = "remote")]
use serde::{Deserialize, Serialize};

/// 命名空间规范 "{scope}/{name}"，scope∈{edge,cloud,jvm,ray,media}。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "remote", derive(Serialize, Deserialize))]
pub struct ReceptionistKey(String);

impl ReceptionistKey {
    /// 非法字符（空格/`*`/`#`）启动即拒——ACL 前置（DEV_02 §9.6）。
    pub fn new(s: impl Into<String>) -> Result<Self, String> {
        let s: String = s.into();
        if s.contains(' ') || s.contains('*') || s.contains('#') {
            return Err(format!("illegal chars in receptionist key: {s:?}"));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "remote", derive(Serialize, Deserialize))]
pub enum ReceptionistEvent {
    Registered { key: ReceptionistKey, remote_path: String },
    Unregistered { key: ReceptionistKey, remote_path: String },
}

/// 订阅流（先回放快照再续流——DEV_02 §3.2）。
pub type ReceptionistStream = tokio::sync::mpsc::Receiver<ReceptionistEvent>;

/// Receptionist 网关 trait（K2 注入面——倒置依赖，同 RemoteGateway 模式）。
///
/// 实现方（parrot-remote 的 `ReceptionistFacade`）在应用层经
/// `ParrotActorSystem::register_receptionist_gateway` 注入；未注入时双引擎
/// context 三方法返回 Unsupported。
#[async_trait::async_trait]
pub trait ReceptionistGateway: Send + Sync + 'static {
    /// 注册：key + 调用者路径。
    async fn register(
        &self,
        key: ReceptionistKey,
        remote_path: String,
    ) -> crate::types::ActorResult<()>;
    /// 去注册。
    async fn deregister(
        &self,
        key: ReceptionistKey,
        remote_path: String,
    ) -> crate::types::ActorResult<()>;
    /// 订阅（快照回放 + 续流）。
    async fn subscribe(
        &self,
        key: ReceptionistKey,
    ) -> crate::types::ActorResult<ReceptionistStream>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation() {
        assert!(ReceptionistKey::new("edge/ok").is_ok());
        assert!(ReceptionistKey::new("bad key").is_err());
        assert!(ReceptionistKey::new("star*").is_err());
        assert!(ReceptionistKey::new("hash#").is_err());
    }
}
