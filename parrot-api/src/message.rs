use std::time::Duration;
use std::any::Any;
use std::sync::Arc;
use uuid::Uuid;
use crate::address::ActorRef;

/// Message ID type
pub type MessageId = Uuid;

/// Message trait
pub trait Message: Send + 'static {
    /// Result type of the message
    type Result: Send + 'static;

    /// Extract result from Any box
    fn extract_result(result: Box<dyn Any + Send>) -> Result<Self::Result, crate::errors::ActorError> {
        result
            .downcast::<Self::Result>()
            .map(|b| *b)
            .map_err(|_| crate::errors::ActorError::MessageHandlingError(
                "Failed to downcast message result".to_string()
            ))
    }
}

/// Message envelope
pub struct MessageEnvelope {
    /// Unique message identifier
    pub id: MessageId,
    /// Message payload
    pub payload: Box<dyn Any + Send>,
    /// Sender reference
    pub sender: Option<Arc<dyn ActorRef>>,
    /// Message options
    pub options: MessageOptions,
}

impl std::fmt::Debug for MessageEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageEnvelope")
            .field("id", &self.id)
            .field("payload", &"<dyn Any>")
            .field("sender", &self.sender.as_ref().map(|_| "<dyn ActorRef>"))
            .field("options", &self.options)
            .finish()
    }
}

/// Message options
#[derive(Clone, Debug)]
pub struct MessageOptions {
    /// Message processing timeout
    pub timeout: Option<Duration>,
    /// Retry policy
    pub retry_policy: Option<RetryPolicy>,
    /// Message priority
    pub priority: MessagePriority,
}

impl Default for MessageOptions {
    fn default() -> Self {
        Self {
            timeout: None,
            retry_policy: None,
            priority: MessagePriority::Normal,
        }
    }
}

/// Message priority
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessagePriority {
    Low,
    Normal,
    High,
    Critical,
}

/// Retry policy
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Maximum retry attempts
    pub max_attempts: u32,
    /// Retry interval
    pub retry_interval: Duration,
    /// Backoff strategy
    pub backoff_strategy: BackoffStrategy,
}

/// Backoff strategy
#[derive(Clone, Debug)]
pub enum BackoffStrategy {
    /// Fixed interval
    Fixed,
    /// Linear growth
    Linear,
    /// Exponential growth
    Exponential {
        /// Exponential base
        base: f64,
        /// Maximum interval
        max_interval: Duration,
    },
}

impl MessageEnvelope {
    /// Create a new message envelope
    pub fn new<M: Message>(
        payload: M,
        sender: Option<Arc<dyn ActorRef>>,
        options: Option<MessageOptions>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            payload: Box::new(payload),
            sender,
            options: options.unwrap_or_default(),
        }
    }

    /// Get message payload
    pub fn payload<M: Message>(&self) -> Option<&M> {
        self.payload.downcast_ref()
    }

    /// Get mutable message payload
    pub fn payload_mut<M: Message>(&mut self) -> Option<&mut M> {
        self.payload.downcast_mut()
    }
} 