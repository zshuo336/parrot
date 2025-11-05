//! # Engine Neutral Face（M6 API 中立化）
//!
//! POC `api-neutral` 的 5 符号规范面：规范层只定义"擦除后的 actor
//! 运行时接口"，任何引擎（actix/thread/未来引擎/跨语言网关）实现并
//! 注册；规范层永远不知道引擎的名字与符号。
//!
//! ## 5 符号
//!
//! | 符号 | 职责 |
//! |------|------|
//! | [`BoxedMessage`] | 类型擦除消息（复用 `types::BoxedMessage`） |
//! | [`BoxedResult`] | 类型擦除结果 |
//! | [`ErasedActor`] | 引擎看到的最小 actor 接口（receive → 结果） |
//! | [`ActorRefErased`] | 用户侧拿到的最小 ref 接口（tell/ask/path） |
//! | [`EngineRuntime`] | 引擎运行时（spawn_erased），每引擎一个实现 |
//!
//! ## behaviour / runtime 二分原则
//!
//! - **behaviour**（`Actor` trait + `TypedReceive`）：用户写什么
//! - **runtime**（`EngineRuntime`）：引擎怎么跑
//!
//! 两者经 `ErasedActor` 边界对接；引擎侧扩展（如 actix 的
//! `receive_message_with_engine` 快路径）归属引擎 crate 的扩展 trait，
//! **不再进入规范 trait**（M6 语义变更 #5：旧位置直接移除）。
//!
//! Production asynchronous actors use [`AsyncEngineRuntime`] and [`AsyncEngineCatalog`].
//! Thread/Actix implement that boundary using their real managed registries,
//! references and lifecycle owners. The synchronous erased face below is opt-in.
//! Production actix/thread ActorSystems
//! do not currently implement it; their asynchronous Actor and lifecycle APIs
//! cannot be converted losslessly merely by registering a name here. The
//! independent core-lab contracts are separate types. [`EngineCatalog`] manages
//! discovery/registration ownership, not engine shutdown or actor supervision.

use std::sync::{Arc, OnceLock};

mod catalog;
pub use catalog::{EngineCatalog, EngineRegistration, RegistrationError};
mod local_runtime;
pub use local_runtime::{
    AsyncLocalActorSpawner, AsyncLocalRuntime, LocalActor, LocalRuntimeActor, LocalRuntimeFactory,
};
mod async_runtime;
pub use async_runtime::{
    AsyncActorSpawner, AsyncEngineRuntime, AsyncFactoryRuntime, FactoryOptions, InstanceConfig,
    RuntimeActor, RuntimeFactory, SpawnOptions,
};
mod async_catalog;
pub use async_catalog::{AsyncEngineCatalog, AsyncEngineRegistration};

use crate::types::BoxedMessage;

/// 类型擦除结果。
pub type BoxedResult = Result<BoxedMessage, String>;

/// 擦除 actor ref。
pub type BoxedActorRefErased = Arc<dyn ActorRefErased + Send + Sync>;

/// 擦除 actor：引擎看到的最小接口（receive 消息，返回结果）。
pub trait ErasedActor: Send + 'static {
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult;
}

/// 擦除 ref：用户侧拿到的最小接口。
pub trait ActorRefErased: Send + Sync {
    fn tell(&self, msg: BoxedMessage);
    fn ask(&self, msg: BoxedMessage) -> BoxedResult;
    fn path(&self) -> String;
}

/// 引擎运行时（每个引擎实现一个，注册到全局表）。
pub trait EngineRuntime: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn_erased(
        &self,
        actor: Box<dyn ErasedActor>,
        path: &str,
    ) -> Result<BoxedActorRefErased, String>;
}

// ---------------- Global convenience catalog ----------------

fn global_catalog() -> &'static EngineCatalog {
    static CATALOG: OnceLock<EngineCatalog> = OnceLock::new();
    CATALOG.get_or_init(EngineCatalog::new)
}

/// Persistently register a runtime, replacing the same name as before.
/// The old runtime is released outside the catalog lock. User name/Drop panics
/// may propagate, but cannot poison the catalog; a replacement is already visible
/// before the old runtime is released. Use scoped registration for owned cleanup.
pub fn register_engine(runtime: Arc<dyn EngineRuntime>) {
    drop(global_catalog().insert(runtime));
}

/// Register a runtime until its token is dropped; duplicate names are rejected.
pub fn register_engine_scoped(
    runtime: Arc<dyn EngineRuntime>,
) -> Result<EngineRegistration, RegistrationError> {
    global_catalog().register(runtime)
}

/// Remove a name, returning ownership of the runtime without shutting it down.
/// Previously captured runtime and actor references remain valid.
pub fn unregister_engine(name: &str) -> Option<Arc<dyn EngineRuntime>> {
    global_catalog().remove(name)
}

/// Capture the current runtime. No catalog lock is held by the returned handle.
pub fn lookup_engine(name: &str) -> Option<Arc<dyn EngineRuntime>> {
    global_catalog().lookup(name)
}

/// List registered engine names in deterministic lexical order.
pub fn list_engines() -> Vec<String> {
    global_catalog().list()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopRuntime;

    impl EngineRuntime for NoopRuntime {
        fn name(&self) -> &'static str {
            "noop"
        }
        fn spawn_erased(
            &self,
            mut actor: Box<dyn ErasedActor>,
            _path: &str,
        ) -> Result<BoxedActorRefErased, String> {
            let _ = actor.receive(Box::new(1u8));
            Err("noop".into())
        }
    }

    #[test]
    fn registry_register_and_lookup() {
        register_engine(Arc::new(NoopRuntime));
        assert!(lookup_engine("noop").is_some());
        assert!(lookup_engine("missing").is_none());
        assert!(list_engines().contains(&"noop".to_string()));
    }
}
