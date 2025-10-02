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
pub mod processor;
pub mod reply;
pub mod scheduler;
pub mod system;

// Re-export key types for easier usage
pub use config::{ThreadActorSystemConfig, ThreadActorConfig, SchedulingMode, BackpressureStrategy, SupervisorStrategy};
pub use error::{MailboxError, AskError, SpawnError, SystemError, SupervisorError};
pub use address::ThreadActorRef;
pub use context::{ThreadContext, WeakSystemRef};
pub use mailbox::WeakMailboxRef;
pub use message::{CloneableMessage, make_cloneable};
pub use scheduler::queue::SchedulingQueue;
pub use scheduler::shared::SharedThreadPool;
pub use system::ThreadActorSystem;
pub use processor::{ActorProcessor, ActorProcessorManager, ProcessorInterface};
pub use actor::ThreadActor;

/// Shared test fixtures for the thread engine's unit tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
    use crate::thread::context::ThreadContext;
    use std::any::Any;

    /// Minimal actor used by context unit tests.
    #[derive(Debug)]
    pub struct DummyActor;

    impl Actor for DummyActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }

        fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
            None
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
}
