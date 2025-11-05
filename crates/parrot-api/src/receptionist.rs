//! Receptionist 类型定义（DEV_02 §3.1 / K2）。
//!
//! 分层：parrot-api 定义 key/event/stream；本地表、订阅流和独立快照合并
//! 在 parrot-remote `receptionist.rs`；托管 Thread/Actix context 均经依赖注入转发。
//! 独立本地网关不联网；RemoteActorSystem 的网关支持获授权直连的完整快照同步。
//! 内置网关自动撤销托管 Thread/Actix actor 旧代注册；任意稀疏拓扑传播
//! 和应用自动装配仍待完成。
//!
//! wire 编解码在 `remote` feature 下，解码复用 key 构造校验；核心类型无 feature 门
//! （context trait 三方法引用——默认实现可用）。

pub use crate::receptionist_scope::RegistrationScope;

#[cfg(feature = "remote")]
use serde::{Deserialize, Serialize};

/// 命名空间规范 "{scope}/{name}"，scope∈{edge,cloud,jvm,ray,media}。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "remote", derive(Serialize))]
pub struct ReceptionistKey(String);

impl ReceptionistKey {
    /// 非法字符（空格/`*`/`#`）在构造和 wire 解码时拒绝（DEV_02 §9.6）。
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

#[cfg(feature = "remote")]
impl<'de> Deserialize<'de> for ReceptionistKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "remote", derive(Serialize, Deserialize))]
pub enum ReceptionistEvent {
    Registered {
        key: ReceptionistKey,
        remote_path: String,
    },
    Unregistered {
        key: ReceptionistKey,
        remote_path: String,
    },
}

/// 订阅流：先回放完整快照，再按顺序传递实时事件；路径去重由消费者负责。
/// 流关闭时已排队的事件仍可消费。需要持续追踪的消费者应重新订阅并用新
/// 快照重建状态，因为关闭也可能表示消费者过慢、已错过后续事件。
pub type ReceptionistStream = tokio::sync::mpsc::Receiver<ReceptionistEvent>;

/// Receptionist 网关 trait（K2 注入面——倒置依赖，同 RemoteGateway 模式）。
///
/// 实现方（parrot-remote 的 `ReceptionistFacade`）在应用层经
/// `ParrotActorSystem::register_receptionist_gateway` 注入 Thread/Actix 引擎。
/// 已注册和后注册的引擎均接收当前网关。托管 context 在构造调用 future 时捕获
/// 网关；原生引擎通过通用 `register_runtime` 登记也继承当前网关。外部自定义
/// runtime 负责自己的 context 接线。后续替换不改变已构造调用的归属，
/// 既有订阅仍属于原网关。
/// 托管引擎调用 scoped 写入方法；内置网关在正常停止、错误、panic 和监督替换
/// 时按 actor 代际撤销注册。Resume 保留同代注册。直接调用 register/deregister
/// 仍是手动生命周期；旧自定义网关默认 scoped 方法也保持该兼容行为。
/// 未注入或未绑定系统的 context 的三个方法均返回
/// `ActorError::InternalError("receptionist not enabled")`。
#[async_trait::async_trait]
pub trait ReceptionistGateway: Send + Sync + 'static {
    /// Generation-aware publication. The built-in facade fences publication and
    /// retires registrations automatically. Legacy custom gateways retain manual
    /// lifecycle semantics until they override both scoped methods.
    async fn register_scoped(
        &self,
        key: ReceptionistKey,
        remote_path: String,
        scope: std::sync::Arc<RegistrationScope>,
    ) -> crate::types::ActorResult<()> {
        scope.with_live(|| ())?;
        self.register(key, remote_path).await
    }

    /// Generation-aware explicit removal; built-in facades only remove matching
    /// ownership, so delayed old calls cannot erase a replacement generation.
    async fn deregister_scoped(
        &self,
        key: ReceptionistKey,
        remote_path: String,
        scope: std::sync::Arc<RegistrationScope>,
    ) -> crate::types::ActorResult<()> {
        scope.with_live(|| ())?;
        self.deregister(key, remote_path).await
    }
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
