//! # Actor Message System
//! 
//! This module defines the message passing infrastructure for the Parrot actor system.
//! It provides the core types and traits for type-safe, reliable message communication
//! between actors.
//!
//! ## Design Philosophy
//!
//! The message system is built on these principles:
//! - Type Safety: Messages and their responses are strongly typed
//! - Reliability: Built-in support for timeouts, retries, and priorities
//! - Flexibility: Extensible message envelopes for metadata
//! - Performance: Efficient message passing with minimal overhead
//!
//! ## Core Components
//!
//! - `Message`: Trait for defining actor messages
//! - `MessageEnvelope`: Container for messages with metadata
//! - `MessageOptions`: Configuration for message delivery
//! - `RetryPolicy`: Message retry handling
//!
//! ## Usage Example
//!
//! ```rust
//! use parrot_api::message::{Message, MessageEnvelope, MessageOptions};
//! use std::time::Duration;
//!
//! // Define a message type
//! #[derive(Message)]
//! struct GreetingMsg {
//!     name: String,
//! }
//!
//! // Create a message with options
//! let options = MessageOptions {
//!     timeout: Some(Duration::from_secs(5)),
//!     priority: MessagePriority::High,
//!     ..Default::default()
//! };
//!
//! let msg = MessageEnvelope::new(
//!     GreetingMsg { name: "World".to_string() },
//!     None,
//!     Some(options)
//! );
//! ```

use std::time::Duration;
use std::any::Any;
use std::sync::Arc;
use uuid::Uuid;
use crate::address::ActorRef;

/// Unique identifier for messages in the system.
/// 
/// Uses UUID v4 to ensure uniqueness across distributed systems.
pub type MessageId = Uuid;

/// Core trait for defining actor messages.
///
/// This trait must be implemented by all message types in the system.
/// It provides type safety and result type mapping for message handling.
///
/// # Type Parameters
///
/// * `Result`: The type returned when this message is processed
///
/// # Examples
///
/// ```rust
/// use parrot_api::message::Message;
///
/// #[derive(Message)]
/// struct GetUserProfile {
///     user_id: String,
/// }
///
/// impl Message for GetUserProfile {
///     type Result = Option<UserProfile>;
/// }
/// ```
pub trait Message: Send + 'static {
    /// The type returned when this message is processed by an actor
    type Result: Send + 'static;

    /// Extracts the typed result from a type-erased response.
    ///
    /// This method handles the type conversion from the generic
    /// message handling system back to the concrete result type.
    ///
    /// # Parameters
    /// * `result` - Type-erased result box from message processing
    ///
    /// # Returns
    /// * `Ok(Result)` - Successfully extracted result
    /// * `Err(ActorError)` - Type conversion failed
    fn extract_result(result: Box<dyn Any + Send>) -> Result<Self::Result, crate::errors::ActorError> {
        result
            .downcast::<Self::Result>()
            .map(|b| *b)
            .map_err(|_| crate::errors::ActorError::MessageHandlingError(
                "Failed to downcast message result".to_string()
            ))
    }
}

/// Container for messages with metadata and delivery options.
///
/// `MessageEnvelope` wraps a message with:
/// - Unique identifier
/// - Type-erased payload
/// - Sender information
/// - Delivery options
///
/// This structure enables the actor system to:
/// - Track message flow
/// - Implement delivery guarantees
/// - Handle message priorities
/// - Manage timeouts and retries
pub struct MessageEnvelope {
    /// Unique identifier for this message instance
    pub id: MessageId,
    
    /// Type-erased message content
    pub payload: Box<dyn Any + Send>,
    
    /// Optional reference to the sending actor
    pub sender: Option<Arc<dyn ActorRef>>,
    
    /// Delivery and processing options
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

/// Configuration options for message delivery and processing.
///
/// These options control how the message is handled by the actor system,
/// including timing, retries, and priority.
#[derive(Clone, Debug)]
pub struct MessageOptions {
    /// Maximum time allowed for message processing
    pub timeout: Option<Duration>,
    
    /// Policy for handling message delivery failures
    pub retry_policy: Option<RetryPolicy>,
    
    /// Relative importance of the message
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

/// Priority levels for message processing.
///
/// Messages with higher priority are processed before
/// lower priority messages in the actor's mailbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessagePriority {
    /// Background or bulk processing tasks
    Low,
    /// Standard message priority
    Normal,
    /// Time-sensitive operations
    High,
    /// System or emergency messages
    Critical,
}

/// Configuration for message retry behavior.
///
/// Defines how the system should handle message delivery
/// or processing failures through retries.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    
    /// Base interval between retry attempts
    pub retry_interval: Duration,
    
    /// Strategy for adjusting retry intervals
    pub backoff_strategy: BackoffStrategy,
}

/// Strategies for adjusting retry intervals between attempts.
///
/// Different backoff strategies can be used to handle various
/// types of failures and network conditions.
#[derive(Clone, Debug)]
pub enum BackoffStrategy {
    /// Constant interval between retries
    Fixed,
    
    /// Interval increases linearly with each attempt
    Linear,
    
    /// Interval increases exponentially with each attempt
    Exponential {
        /// Multiplier for interval growth
        base: f64,
        /// Upper limit for retry interval
        max_interval: Duration,
    },
}

impl MessageEnvelope {
    /// Creates a new message envelope with the given payload and options.
    ///
    /// # Type Parameters
    /// * `M`: Message type implementing the `Message` trait
    ///
    /// # Parameters
    /// * `payload`: The message content
    /// * `sender`: Optional reference to the sending actor
    /// * `options`: Optional delivery configuration
    ///
    /// # Examples
    ///
    /// ```rust
    /// let msg = MessageEnvelope::new(
    ///     MyMessage { data: "hello" },
    ///     None,
    ///     Some(MessageOptions::default())
    /// );
    /// ```
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

    /// Attempts to access the message payload as a specific type.
    ///
    /// # Type Parameters
    /// * `M`: Expected message type
    ///
    /// # Returns
    /// * `Some(&M)`: Reference to the payload if type matches
    /// * `None`: If the payload is not of type M
    pub fn payload<M: Message>(&self) -> Option<&M> {
        self.payload.downcast_ref()
    }

    /// Attempts to access the message payload as a specific type mutably.
    ///
    /// # Type Parameters
    /// * `M`: Expected message type
    ///
    /// # Returns
    /// * `Some(&mut M)`: Mutable reference to the payload if type matches
    /// * `None`: If the payload is not of type M
    pub fn payload_mut<M: Message>(&mut self) -> Option<&mut M> {
        self.payload.downcast_mut()
    }
} 