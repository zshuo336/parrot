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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

// ---------------- 全局引擎注册表（规范层，无引擎符号） ----------------

static ENGINE_REGISTRY: Mutex<Option<HashMap<String, Arc<dyn EngineRuntime>>>> = Mutex::new(None);

/// 注册一个引擎运行时（幂等：同名覆盖）。
pub fn register_engine(runtime: Arc<dyn EngineRuntime>) {
    let mut g = ENGINE_REGISTRY.lock().unwrap();
    g.get_or_insert_with(HashMap::new)
        .insert(runtime.name().to_string(), runtime);
}

/// 按名查引擎。
pub fn lookup_engine(name: &str) -> Option<Arc<dyn EngineRuntime>> {
    let g = ENGINE_REGISTRY.lock().unwrap();
    g.as_ref().and_then(|m| m.get(name).cloned())
}

/// 已注册引擎列表（诊断用）。
pub fn list_engines() -> Vec<String> {
    let g = ENGINE_REGISTRY.lock().unwrap();
    g.as_ref()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
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
