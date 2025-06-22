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
use crate::types::BoxedMessage;
/// Message ID type
pub type MessageId = Uuid;

/// # Message Priority
///
/// ## Overview
/// Represents message processing priority in the actor system
///
/// ## Key Characteristics
/// - Range: 0-100 (inclusive)
/// - Higher value means higher priority
/// - Default is 50 (normal priority)
///
/// ## Priority Ranges
/// - 0-19: Background tasks (lowest priority)
/// - 20-39: Low priority tasks
/// - 40-59: Normal priority tasks
/// - 60-79: High priority tasks
/// - 80-100: Critical tasks (highest priority)
///
/// ## Thread Safety
/// - Implements Send + Sync
/// - Copy semantic for efficient passing
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MessagePriority(u8);

impl MessagePriority {
    /// Creates a new MessagePriority with the specified value
    ///
    /// ## Parameters
    /// - `priority`: Priority value between 0 and 100
    ///
    /// ## Returns
    /// - `Some(MessagePriority)`: If value is in valid range
    /// - `None`: If value is greater than 100
    pub fn new(priority: u8) -> Option<Self> {
        if priority <= 100 {
            Some(MessagePriority(priority))
        } else {
            None
        }
    }

    /// Creates a new MessagePriority without checking the range
    ///
    /// ## Safety
    /// - Caller must ensure value is <= 100
    /// - Panics in debug mode if value > 100
    pub fn new_unchecked(priority: u8) -> Self {
        debug_assert!(priority <= 100, "Priority must be <= 100");
        MessagePriority(priority)
    }

    /// Returns the priority value
    pub fn value(&self) -> u8 {
        self.0
    }

    /// Predefined priority: Background (10)
    pub const BACKGROUND: MessagePriority = MessagePriority(10);
    
    /// Predefined priority: Low (30)
    pub const LOW: MessagePriority = MessagePriority(30);
    
    /// Predefined priority: Normal (50)
    pub const NORMAL: MessagePriority = MessagePriority(50);
    
    /// Predefined priority: High (70)
    pub const HIGH: MessagePriority = MessagePriority(70);
    
    /// Predefined priority: Critical (90)
    pub const CRITICAL: MessagePriority = MessagePriority(90);

    /// Checks if priority is in background range (0-19)
    pub fn is_background(&self) -> bool {
        self.0 <= 19
    }

    /// Checks if priority is in low range (20-39)
    pub fn is_low(&self) -> bool {
        (20..=39).contains(&self.0)
    }

    /// Checks if priority is in normal range (40-59)
    pub fn is_normal(&self) -> bool {
        (40..=59).contains(&self.0)
    }

    /// Checks if priority is in high range (60-79)
    pub fn is_high(&self) -> bool {
        (60..=79).contains(&self.0)
    }

    /// Checks if priority is in critical range (80-100)
    pub fn is_critical(&self) -> bool {
        self.0 >= 80
    }
}

impl Default for MessagePriority {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl std::fmt::Display for MessagePriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Priority({})", self.0)
    }
}

impl TryFrom<u8> for MessagePriority {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value).ok_or("Priority must be between 0 and 100")
    }
}

/// Message trait for type-safe message passing
pub trait Message: Send + 'static {
    /// Message response type
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
                format!("Failed to downcast message result for {}", std::any::type_name::<Self>())
            ))
    }

    /// Validates the message content.
    ///
    /// This method should check if the message content is valid
    /// according to business rules.
    ///
    /// # Returns
    /// * `Ok(())` - Message is valid
    /// * `Err(ActorError)` - Message is invalid
    fn validate(&self) -> Result<(), crate::errors::ActorError> {
        Ok(())
    }

    /// Returns the message type name.
    ///
    /// This is typically the struct name of the message type.
    /// 
    /// # Returns
    /// * `&'static str` - The type name of the message as a static string
    ///
    /// # Example
    /// ```
    /// struct MyMessage {}
    /// impl Message for MyMessage {
    ///     type Result = ();
    /// }
    /// 
    /// let msg = MyMessage {};
    /// assert_eq!(msg.message_type(), "MyMessage");
    /// ```
    fn message_type(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Returns the message priority level.
    ///
    /// Default implementation returns Normal priority.
    /// 
    /// # Returns
    /// * `MessagePriority` - The priority level for this message
    ///
    /// # Example
    /// ```
    /// struct MyMessage {}
    /// impl Message for MyMessage {
    ///     type Result = ();
    /// }
    /// 
    /// let msg = MyMessage {};
    /// assert_eq!(msg.priority(), MessagePriority::NORMAL);
    /// ```
    fn priority(&self) -> MessagePriority {
        MessagePriority::NORMAL
    }

    /// Returns the message options for this message.
    ///
    /// Default implementation returns default message options.
    /// 
    /// # Returns
    /// * `MessageOptions` - The options for this message
    fn message_options(&self) -> Option<MessageOptions> {
        None
    }


    /// Converts message into a boxed message
    ///
    /// Boxes the message for type erasure in the actor system.
    /// 
    /// # Parameters
    /// * `msg` - The message to box
    ///
    /// # Returns
    /// * `BoxedMessage` - Type-erased boxed message
    ///
    /// # Example
    /// ```
    /// struct MyMessage {}
    /// impl Message for MyMessage {
    ///     type Result = ();
    /// }
    /// 
    /// let msg = MyMessage {};
    /// let boxed = Message::into_boxed(msg);
    /// ```
    fn into_boxed(msg: Self) -> BoxedMessage where Self: Sized {
        Box::new(msg)
    }

}

/// Trait combining Any and Message functionalities, but making it object safe
/// by erasing the associated types with runtime type checks.
pub trait AnyMessage: Any + Send {
    /// Get message type for runtime type information
    fn message_type(&self) -> &'static str;
    
    /// Message validation
    fn validate(&self) -> Result<(), crate::errors::ActorError>;
    
    /// Message priority
    fn priority(&self) -> crate::message::MessagePriority;
    
    /// Message options
    fn message_options(&self) -> Option<crate::message::MessageOptions>;
}

/// Implement AnyMessage for any type that implements Message
impl<T: Any + Message + Send> AnyMessage for T {
    fn message_type(&self) -> &'static str {
        <T as Message>::message_type(self)
    }
    
    fn validate(&self) -> Result<(), crate::errors::ActorError> {
        <T as Message>::validate(self)
    }
    
    fn priority(&self) -> crate::message::MessagePriority {
        <T as Message>::priority(self)
    }
    
    fn message_options(&self) -> Option<crate::message::MessageOptions> {
        <T as Message>::message_options(self)
    }
}

/// Message options for controlling delivery and processing
#[derive(Debug)]
pub struct MessageOptions {
    /// Message processing timeout
    pub timeout: Option<Duration>,
    /// Retry policy for failed processing
    pub retry_policy: Option<RetryPolicy>,
    /// Message priority level
    pub priority: MessagePriority,
}

impl Default for MessageOptions {
    fn default() -> Self {
        Self {
            timeout: None,
            retry_policy: None,
            priority: MessagePriority::NORMAL,
        }
    }
}

/// Message envelope for type-erased message passing
#[derive(Debug)]
pub struct MessageEnvelope {
    /// Unique message identifier
    pub id: MessageId,
    /// Message payload
    pub payload: Box<dyn Any + Send>,
    /// Message sender reference
    pub sender: Option<Box<dyn ActorRef>>,
    /// Message processing options
    pub options: MessageOptions,
    /// The type name of the message
    pub message_type: &'static str,
}

impl MessageEnvelope {
    /// Creates a new message envelope
    pub fn new<M: Message>(
        payload: M,
        sender: Option<Box<dyn ActorRef>>,
        options: Option<MessageOptions>,
    ) -> Self {
        // if options are provided, use them, otherwise use the message options
        let options = options.unwrap_or_else(
            || payload.message_options().unwrap_or_default()
        );
        Self {
            id: Uuid::new_v4(),
            payload: Box::new(payload),
            sender,
            options,
            message_type: std::any::type_name::<M>(),
        }
    }

    /// Extracts message payload
    pub fn payload<M: Message>(&self) -> Option<&M> {
        self.payload.downcast_ref()
    }

    /// Extracts mutable message payload
    pub fn payload_mut<M: Message>(&mut self) -> Option<&mut M> {
        self.payload.downcast_mut()
    }

    pub fn message<M: Message>(&self) -> Option<&M> {
        self.payload.downcast_ref()
    }

    pub fn message_mut<M: Message>(&mut self) -> Option<&mut M> {
        self.payload.downcast_mut()
    }

    /// Creates a new message envelope from a boxed message
    pub fn from_boxed(
        boxed_msg: BoxedMessage,
        sender: Option<Box<dyn ActorRef>>,
        options: MessageOptions,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            message_type: std::any::type_name_of_val(&*boxed_msg),
            payload: boxed_msg,
            sender,
            options
        }
    }

}

/// Configuration for message retry behavior.
///
/// Defines how the system should handle message delivery
/// or processing failures through retries.
#[derive(Debug)]
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
#[derive(Debug)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_ranges() {
        assert!(MessagePriority::new(0).unwrap().is_background());
        assert!(MessagePriority::new(15).unwrap().is_background());
        assert!(MessagePriority::new(30).unwrap().is_low());
        assert!(MessagePriority::new(50).unwrap().is_normal());
        assert!(MessagePriority::new(70).unwrap().is_high());
        assert!(MessagePriority::new(90).unwrap().is_critical());
    }

    #[test]
    fn test_priority_ordering() {
        assert!(MessagePriority::CRITICAL > MessagePriority::HIGH);
        assert!(MessagePriority::HIGH > MessagePriority::NORMAL);
        assert!(MessagePriority::NORMAL > MessagePriority::LOW);
        assert!(MessagePriority::LOW > MessagePriority::BACKGROUND);
    }

    #[test]
    fn test_invalid_priority() {
        assert!(MessagePriority::new(101).is_none());
        assert!(MessagePriority::new(255).is_none());
    }

    #[test]
    fn test_default_priority() {
        assert_eq!(MessagePriority::default(), MessagePriority::NORMAL);
    }
} 