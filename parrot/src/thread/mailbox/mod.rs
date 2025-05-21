use async_trait::async_trait;
use parrot_api::types::BoxedMessage;
use parrot_api::address::ActorPath;

use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;

pub mod mpsc;
pub mod spsc;

/// Abstract interface for an actor's message queue.
/// Implementors must guarantee FIFO ordering.
#[async_trait]
pub trait Mailbox: Send + Sync + std::fmt::Debug {
    /// Asynchronously pushes a message into the mailbox, applying the specified backpressure strategy.
    async fn push(&self, msg: BoxedMessage, strategy: BackpressureStrategy) -> Result<(), MailboxError>;

    /// Asynchronously pops a message from the mailbox.
    /// Returns `None` if the mailbox is closed or empty after potentially waiting.
    /// 
    /// This method must be implemented to be thread-safe through internal synchronization
    /// mechanisms since it's called with `&self` but conceptually needs mutable access.
    async fn pop(&self) -> Option<BoxedMessage>;

    /// Checks if the mailbox is currently empty (snapshot in time).
    /// 
    /// This method should avoid blocking operations like tokio::runtime::Handle::current().block_on
    /// to prevent deadlocks in nested runtime contexts.
    async fn is_empty(&self) -> bool;

    /// Signals that this mailbox might have work and should be considered for scheduling.
    /// Primarily used by MPSC mailboxes in the SharedPool scheduler.
    /// SPSC mailbox implementations can be a no-op.
    async fn signal_ready(&self);

    /// Returns the path of the actor this mailbox belongs to.
    fn path(&self) -> &ActorPath;

    /// Returns the configured capacity of the mailbox.
    fn capacity(&self) -> usize;

    /// Returns the current number of messages in the mailbox (snapshot in time).
    /// 
    /// This method should avoid blocking operations like tokio::runtime::Handle::current().block_on
    /// to prevent deadlocks in nested runtime contexts.
    async fn len(&self) -> usize;

    /// Closes the mailbox, preventing further pushes and cleaning up resources.
    /// This should be called during actor shutdown to prevent memory leaks.
    async fn close(&self);
} 