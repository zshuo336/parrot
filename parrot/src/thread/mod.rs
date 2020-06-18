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
pub use address::{ThreadActorRef, WeakMailboxRef, StrongMailboxRef};
pub use context::{ThreadContext, SystemRef, WeakSystemRef};
pub use message::{BoxedMessageExt, CloneableMessage, make_cloneable, wrap_as_cloneable};
pub use scheduler::queue::SchedulingQueue;
pub use scheduler::shared::SharedThreadPool;
pub use system::ThreadActorSystem;
pub use processor::ActorProcessor;

// TODO: Add the following modules as they are implemented:
// pub mod system;
// pub use system::ThreadActorSystem; 