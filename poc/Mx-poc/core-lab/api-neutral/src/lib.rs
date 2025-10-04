//! POC 3：parrot-api 中立化改造（"框架的框架"定位回归）。
//!
//! 评测批评：抽象"照着 actix 设计"，thread 引擎被迫长成 actix 形状。
//! 本 POC 定义一套**引擎中立**的最小 API 面（Face），并用两个形态迥异的
//! 假想引擎（线程邮箱型 / 事件循环型）验证同一抽象可自然粘合两种范式。
//!
//! 对照 Erlang 的 behaviour：callback module 定义"做什么"，
//! runtime 决定"怎么跑"。Parrot-API 应该是 behaviour 层。

pub mod engine;

// ============ 中立 API 面 ============

/// 消息处理回调（behaviour）：引擎负责调度，behaviour 只管业务。
/// 注意：没有 Context 生命周期钩子的强假设——那是 actix 形状。
pub trait ActorBehaviour {
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult;
}

/// 生命周期（可选实现，覆盖默认空实现）。
pub trait Lifecycle {
    fn pre_start(&mut self) -> Result<(), String> { Ok(()) }
    fn post_stop(&mut self) {}
}

/// 生成器（统一 spawn 语义）。
pub trait Spawner {
    fn spawn<B: ActorBehaviour + Lifecycle + 'static>(
        &self,
        behaviour: B,
        path: &str,
    ) -> Result<BoxedActorRef, String>;
}

// re-export
pub use engine::{ActorRefErased, BoxedActorRef, BoxedMessage, BoxedResult, ErasedActor, EngineRuntime};
