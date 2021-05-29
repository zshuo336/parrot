use async_trait::async_trait;
use std::sync::Arc;
use std::any::Any;
use std::sync::Mutex;

use parrot_api::types::BoxedMessage;
use parrot_api::address::ActorPath;
use parrot_api::actor::Actor;

use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::processor::ActorProcessor;
use crate::thread::processor::ProcessorInterface;
use crate::thread::context::ThreadContext;

pub mod mpsc;
pub mod spsc;
pub mod spsc_ringbuf;


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
    
    /// Checks if the mailbox has more messages (opposite of is_empty).
    async fn has_more_messages(&self) -> bool {
        !self.is_empty().await
    }

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

    /// Closes this mailbox, preventing further messages from being added.
    async fn close(&self);
    
    /// Set the processor for this mailbox
    fn set_processor(&mut self, processor: Arc<Mutex<dyn ProcessorInterface>>);
    
    /// Get the processor for this mailbox
    fn get_processor(&self) -> Option<Arc<Mutex<dyn ProcessorInterface>>>;
    
    /// Check if this mailbox has a processor
    fn has_processor(&self) -> bool;
} 

