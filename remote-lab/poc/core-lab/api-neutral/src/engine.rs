//! 规范层"引擎中立"注册点（POC 1 与 POC 3 共用的 parrot-api 侧形态）。
//!
//! 关键设计：规范层只定义"擦除后的 actor 运行时接口"，任何引擎
//! （actix/thread/未来引擎/跨语言网关）都实现这一接口并注册。
//! 规范层永远不知道引擎的名字与符号。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub type BoxedMessage = Box<dyn std::any::Any + Send>;
pub type BoxedResult = Result<BoxedMessage, String>;
pub type BoxedActorRef = Arc<dyn ActorRefErased + Send + Sync>;

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
    fn spawn_erased(&self, actor: Box<dyn ErasedActor>, path: &str) -> Result<BoxedActorRef, String>;
}

// ---------------- 全局引擎注册表（规范层，无引擎符号） ----------------

type BindingFactory = fn() -> Box<dyn ErasedActor>;

static ENGINE_BINDINGS: Mutex<Option<HashMap<String, Vec<BindingFactory>>>> = Mutex::new(None);

/// 宏生成的注册调用入口（derive 时按 #[engine] 属性生成对它的调用）。
pub fn register_engine_binding(name: &str, factory: BindingFactory) {
    let mut g = ENGINE_BINDINGS.lock().unwrap();
    g.get_or_insert_with(HashMap::new)
        .entry(name.to_string())
        .or_default()
        .push(factory);
}

/// 引擎枚举（测试用）。
pub fn list_bindings() -> Vec<String> {
    let g = ENGINE_BINDINGS.lock().unwrap();
    g.as_ref().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}
