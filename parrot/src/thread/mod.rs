#![doc = " Thread-based actor system implementation for Parrot."]

pub mod actor;
pub mod address;
pub mod common;
pub mod config;
pub mod context;
pub mod envelope;
pub mod error;
pub mod mailbox;
pub mod message;
pub mod message_pool;
pub mod processor;
pub mod reply;
pub mod scheduler;
pub mod single_alloc;
pub mod supervisor_exec;
pub mod system;
pub mod typed;

// Re-export key types for easier usage
pub use actor::ThreadActor;
pub use address::ThreadActorRef;
pub use config::{
    BackpressureStrategy, SchedulingMode, SupervisorStrategy, ThreadActorConfig,
    ThreadActorSystemConfig,
};
pub use context::{ThreadContext, WeakSystemRef};
pub use error::{AskError, MailboxError, SpawnError, SupervisorError, SystemError};
pub use mailbox::WeakMailboxRef;
pub use message::{CloneableMessage, make_cloneable};
pub use processor::{ActorProcessor, ActorProcessorManager, ProcessorInterface};
pub use scheduler::queue::SchedulingQueue;
pub use scheduler::shared::SharedThreadPool;
pub use system::ThreadActorSystem;
pub use typed::{SingleDispatch, TypedActorRef};

/// M1 derive-decouple: thread 引擎的中立 Context 面。
///
/// derive 宏生成代码经 `__parrot_engine::EngineContext<Self>` 引用 context；
/// 本引擎的具体类型是 `ThreadContext<Self>`。
/// 用户侧绑定：`use parrot::thread::__parrot_engine_binding::*;`
/// 或 `use parrot::thread as __parrot_engine;`。
pub use context::EngineContext;

/// M1 derive-decouple: 宏消费的"引擎面"模块（全部引擎无关符号）。
pub mod __parrot_engine_binding {
    pub use super::EngineContext;
}

/// Shared test fixtures for the thread engine's unit tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use crate::thread::context::ThreadContext;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

    /// Minimal actor used by context unit tests.
    #[derive(Debug)]
    pub struct DummyActor;

    impl Actor for DummyActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
}
