#![doc = " Thread-based actor system implementation for Parrot."]

pub mod actor;
pub mod address;
pub mod common;
pub mod config;
pub mod context;
pub mod envelope;
pub mod error;
pub mod mailbox;
pub mod reply;
pub mod scheduler;
pub mod system;

// Re-export key types for easier usage
pub use config::{ThreadActorSystemConfig, ThreadActorConfig, SchedulingMode, BackpressureStrategy, SupervisorStrategy};
pub use error::{MailboxError, ActorRefError, AskError, SpawnError, SystemError, SupervisorError};
pub use system::ThreadActorSystem;
// Potentially re-export context and address types if needed at the top level
pub use context::ThreadContext;
pub use address::ThreadActorRef;

// TODO: Add feature gates if needed (e.g., for specific mailbox impls) 