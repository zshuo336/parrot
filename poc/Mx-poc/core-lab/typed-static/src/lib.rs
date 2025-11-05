//! Owned actor workers with a genuinely typed mailbox and optional dynamic edges.
//!
//! Static messages/replies stay generic: no Any conversion or message boxing is
//! required by this path. Queues and reply channels can still allocate. The
//! `typed_actor!` macro dispatches several typed protocols to one actor state.
mod bridge;
mod engine;
mod mailbox;
mod protocol;

pub use bridge::{BoxedMessage, BoxedResult, DynActorRef, TypedToDyn};
pub use engine::SimpleEngine;
pub use mailbox::TypedActorRef;
pub use protocol::{ProtocolMessage, ProtocolRef};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

pub struct SendError<M> {
    pub reason: String,
    pub message: M,
}
impl<M> std::fmt::Debug for SendError<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
pub trait TypedAskRef<M: Send + 'static>: Send + Sync {
    type Reply: Send + 'static;
    fn ask(&self, msg: M) -> Result<Self::Reply, String> {
        self.ask_with_timeout(msg, Duration::from_secs(5))
    }
    fn ask_with_timeout(&self, msg: M, timeout: Duration) -> Result<Self::Reply, String>;
    /// Admission only; on failure the original message is returned.
    fn tell(&self, msg: M) -> Result<(), SendError<M>>;
}
pub trait TypedReceive<M>: Send + 'static {
    type Reply: Send + 'static;
    fn receive_typed(&mut self, msg: M) -> Result<Self::Reply, String>;
}
fn deadline(timeout: Duration) -> Result<Instant, String> {
    if timeout.is_zero() {
        return Err("timeout must be positive".into());
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "timeout is too large".into())
}
fn safe_drop<T>(value: T) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
            std::mem::forget(secondary);
        }
    }
}
struct Owned<T>(Option<T>);
impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            safe_drop(value);
        }
    }
}
fn panic_error(payload: Box<dyn std::any::Any + Send>) -> String {
    let text = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "opaque panic".into()
    };
    safe_drop(payload);
    format!("actor or adapter panicked: {text}")
}
