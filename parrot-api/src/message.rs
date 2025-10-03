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
//! use parrot_api::message::{Message, MessageEnvelope, MessageOptions, MessagePriority};
//! use std::time::Duration;
//!
//! // Define a message type
//! struct GreetingMsg {
//!     name: String,
//! }
//! impl Message for GreetingMsg { type Result = (); }
//!
//! // Create a message with options
//! let options = MessageOptions {
//!     timeout: Some(Duration::from_secs(5)),
//!     priority: MessagePriority::new(70).expect("valid priority"),
//!     ..Default::default()
//! };
//!
//! let msg = MessageEnvelope::new(
//!     GreetingMsg { name: "World".to_string() },
//!     None,
//!     Some(options)
//! );
//! ```

use crate::address::ActorRef;
use crate::types::{BoxedMessage, SharedMessage};
use std::any::Any;
use std::time::Duration;
use uuid::Uuid;
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
    fn extract_result(
        result: Box<dyn Any + Send>,
    ) -> Result<Self::Result, crate::errors::ActorError> {
        result.downcast::<Self::Result>().map(|b| *b).map_err(|_| {
            crate::errors::ActorError::MessageHandlingError(format!(
                "Failed to downcast message result for {}",
                std::any::type_name::<Self>()
            ))
        })
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
    /// use parrot_api::message::Message;
    /// struct MyMessage {}
    /// impl Message for MyMessage {
    ///     type Result = ();
    /// }
    ///
    /// let msg = MyMessage {};
    /// // Note: message_type returns std::any::type_name, which includes the
    /// // defining module path (e.g. "rust_out::MyMessage" in doctests).
    /// assert!(msg.message_type().ends_with("MyMessage"));
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
    /// use parrot_api::message::{Message, MessagePriority};
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
    /// use parrot_api::message::Message;
    /// struct MyMessage {}
    /// impl Message for MyMessage {
    ///     type Result = ();
    /// }
    ///
    /// let msg = MyMessage {};
    /// let boxed = Message::into_boxed(msg);
    /// ```
    fn into_boxed(msg: Self) -> BoxedMessage
    where
        Self: Sized,
    {
        Box::new(msg)
    }
}

// create a trait for cloneable message
pub trait CloneableMessageTrait: Send {
    fn clone_message(&self) -> Box<dyn CloneableMessageTrait>;
    fn into_boxed_message(self: Box<Self>) -> BoxedMessage;
}

// implement CloneableMessageTrait for any type that implements Message
impl<T: Message + Clone + 'static> CloneableMessageTrait for T {
    fn clone_message(&self) -> Box<dyn CloneableMessageTrait> {
        Box::new(self.clone())
    }
    fn into_boxed_message(self: Box<Self>) -> BoxedMessage {
        // we need to convert Box<dyn CloneableMessageTrait> to BoxedMessage
        // directly return self, avoid extra nesting
        self
    }
}

// create wrapper struct for CloneableMessageTrait
pub struct CloneableMessage {
    message: Box<dyn CloneableMessageTrait>,
}

// implement Clone for CloneableMessage
impl Clone for CloneableMessage {
    fn clone(&self) -> Self {
        CloneableMessage {
            message: self.message.clone_message(),
        }
    }
}

/// create a CloneableMessage from a concrete type
/// let msg = MyMessage { /* ... */ };
/// let cloneable = CloneableMessage::from_message(msg);
/// let cloned = cloneable.clone();  // now we can clone
/// let boxed: BoxedMessage = cloneable.into_boxed();
///
/// try to create a CloneableMessage from a BoxedMessage
/// let boxed_msg: BoxedMessage = /* ... */;
/// if let Some(cloneable) = CloneableMessage::try_from_boxed(&boxed_msg) {
///     // the message can be cloned
///     let cloned = cloneable.clone();
///     // ...
/// }
impl CloneableMessage {
    /// convert a type that implements Message + Clone to CloneableMessage
    pub fn from_message<T>(message: T) -> Self
    where
        T: Message + Clone + 'static,
    {
        CloneableMessage {
            message: Box::new(message) as Box<dyn CloneableMessageTrait>,
        }
    }

    /// get the inner message
    pub fn into_boxed(self) -> BoxedMessage {
        self.message.into_boxed_message()
    }

    // create a CloneableMessage from any cloneable type
    pub fn from_cloneable<T>(value: T) -> Self
    where
        T: Clone + Send + 'static,
    {
        // create a special wrapper type
        struct CloneableValue<T: Clone + Send + 'static>(T);

        impl<T: Clone + Send + 'static> CloneableMessageTrait for CloneableValue<T> {
            fn clone_message(&self) -> Box<dyn CloneableMessageTrait> {
                Box::new(CloneableValue(self.0.clone()))
            }
            fn into_boxed_message(self: Box<Self>) -> BoxedMessage {
                Box::new(self.0) as BoxedMessage
            }
        }

        CloneableMessage {
            message: Box::new(CloneableValue(value)),
        }
    }

    // Enhanced try_from_boxed method to support more types
    pub fn try_from_boxed(boxed: &BoxedMessage) -> Option<Self> {
        // 1. First try common types directly for efficiency
        if let Some(s) = boxed.as_ref().downcast_ref::<String>() {
            return Some(Self::from_cloneable(s.clone()));
        } else if let Some(i) = boxed.as_ref().downcast_ref::<i32>() {
            return Some(Self::from_cloneable(*i));
        } else if let Some(i) = boxed.as_ref().downcast_ref::<i64>() {
            return Some(Self::from_cloneable(*i));
        } else if let Some(u) = boxed.as_ref().downcast_ref::<u32>() {
            return Some(Self::from_cloneable(*u));
        } else if let Some(u) = boxed.as_ref().downcast_ref::<u64>() {
            return Some(Self::from_cloneable(*u));
        } else if let Some(b) = boxed.as_ref().downcast_ref::<bool>() {
            return Some(Self::from_cloneable(*b));
        } else if boxed.as_ref().downcast_ref::<()>().is_some() {
            return Some(Self::from_cloneable(()));
        } else if let Some(f) = boxed.as_ref().downcast_ref::<f32>() {
            return Some(Self::from_cloneable(*f));
        } else if let Some(f) = boxed.as_ref().downcast_ref::<f64>() {
            return Some(Self::from_cloneable(*f));
        } else if let Some(c) = boxed.as_ref().downcast_ref::<char>() {
            return Some(Self::from_cloneable(*c));
        }

        // 2. Try custom message types

        // For TestMessage type (explicit handling for test purposes)
        if std::any::type_name_of_val(boxed.as_ref()).contains("TestMessage") {
            // We can't directly clone custom types without knowing their type
            // In a real implementation, we would need a registry or trait-based solution
            // For now, we'll return None to indicate that custom types aren't supported yet
            return None;
        }

        // Final fallback - we can't reliably clone unknown types
        None
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

enum MessageContainer {
    #[allow(dead_code)]
    Exclusive(BoxedMessage),
    #[allow(dead_code)]
    Shared(SharedMessage),
}

/// Implement From<MessageContainer> for BoxedMessage
///
/// This implementation allows for conversion between MessageContainer and BoxedMessage.
///
/// # Parameters
/// * `container` - The MessageContainer to convert
///
/// # Returns
/// exclusive example:
/// ```ignore
/// let container = MessageContainer::Exclusive(Box::new(Message1));
/// let boxed_message = container.into(); // return type is Box<dyn Any + Send>
/// actor.send(boxed_message);
/// ```
/// shared example:
/// ```ignore
/// let container = MessageContainer::Shared(Arc::new(Message1));
/// let boxed_message = container.into(); // return type is Box<Arc<dyn Any + Send + Sync>>
/// actor.send(boxed_message);
/// ```
impl From<MessageContainer> for BoxedMessage {
    fn from(container: MessageContainer) -> Self {
        match container {
            // return type is Box<dyn Any + Send>
            MessageContainer::Exclusive(boxed) => boxed,
            // return type is Box<Arc<dyn Any + Send + Sync>
            MessageContainer::Shared(arc) => Box::new(arc),
        }
    }
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

/// Trait for cloning BoxedMessage
pub trait BoxedMessageClone: Any + Send {
    fn clone_box(&self) -> Box<dyn Any + Send>;
}

/// Implement BoxedMessageClone for any type that implements Clone
impl<T: 'static + Clone + Send> BoxedMessageClone for T {
    fn clone_box(&self) -> Box<dyn Any + Send> {
        Box::new(self.clone())
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
        let options = options.unwrap_or_else(|| payload.message_options().unwrap_or_default());
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
            options,
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

    // ---------------- Display / TryFrom / new_unchecked ----------------

    #[test]
    fn priority_display_format() {
        assert_eq!(MessagePriority::HIGH.to_string(), "Priority(70)");
        assert_eq!(MessagePriority::new(0).unwrap().to_string(), "Priority(0)");
    }

    #[test]
    fn priority_try_from_valid_and_invalid() {
        let ok: Result<MessagePriority, _> = u8::try_into(0u8);
        assert!(ok.is_ok());
        let ok2: Result<MessagePriority, _> = 100u8.try_into();
        assert!(ok2.is_ok());
        let err: Result<MessagePriority, _> = 101u8.try_into();
        let err = err.unwrap_err();
        assert!(err.contains("between 0 and 100"));
        let bad: Result<MessagePriority, _> = 255u8.try_into();
        assert!(bad.is_err());
    }

    #[test]
    fn priority_new_unchecked_and_value() {
        let p = MessagePriority::new_unchecked(42);
        assert_eq!(p.value(), 42);
    }

    // ---------------- Message 默认方法 ----------------

    struct TestMsg(u32);

    impl Message for TestMsg {
        type Result = u32;
    }

    #[test]
    fn message_default_extract_result_roundtrip_and_mismatch() {
        let boxed: Box<dyn Any + Send> = Box::new(7u32);
        let r = <TestMsg as Message>::extract_result(boxed);
        assert_eq!(r.unwrap(), 7);
        // 类型不匹配 → 错误信息含类型名
        let bad: Box<dyn Any + Send> = Box::new("str");
        let e = <TestMsg as Message>::extract_result(bad).unwrap_err();
        assert!(e.to_string().contains("downcast"));
    }

    #[test]
    fn message_default_validate_is_ok() {
        let r: Result<(), crate::errors::ActorError> = Message::validate(&TestMsg(1));
        assert!(r.is_ok());
    }

    // ---------------- CloneableMessage ----------------

    #[test]
    fn cloneable_from_message_roundtrip() {
        #[derive(Debug, Clone, PartialEq)]
        struct M(u64);
        impl Message for M {
            type Result = ();
        }
        let c = CloneableMessage::from_message(M(5));
        let c2 = c.clone();
        // into_boxed 取回载荷
        let b = c.into_boxed();
        let m = b.downcast_ref::<M>().unwrap();
        assert_eq!(*m, M(5));
        // 克隆独立
        let b2 = c2.into_boxed();
        assert_eq!(b2.downcast_ref::<M>().unwrap().0, 5);
    }

    #[test]
    fn cloneable_from_cloneable_arbitrary_type() {
        let c = CloneableMessage::from_cloneable(vec![1i32, 2, 3]);
        let c2 = c.clone();
        let b = c.into_boxed();
        let v = b.downcast_ref::<Vec<i32>>().unwrap();
        assert_eq!(v, &vec![1, 2, 3]);
        assert_eq!(c2.into_boxed().downcast_ref::<Vec<i32>>().unwrap().len(), 3);
    }

    #[test]
    fn try_from_boxed_common_primitive_types() {
        // String
        let s: BoxedMessage = Box::new("hello".to_string());
        let c = CloneableMessage::try_from_boxed(&s).unwrap();
        assert_eq!(
            c.into_boxed().downcast_ref::<String>().unwrap(),
            "hello"
        );
        // 整数族 / 浮点 / bool / unit / char 逐类型断言
        let b: BoxedMessage = Box::new(7i32);
        assert_eq!(
            CloneableMessage::try_from_boxed(&b)
                .unwrap()
                .into_boxed()
                .downcast_ref::<i32>()
                .unwrap(),
            &7
        );
        let b: BoxedMessage = Box::new(7i64);
        assert_eq!(
            CloneableMessage::try_from_boxed(&b)
                .unwrap()
                .into_boxed()
                .downcast_ref::<i64>()
                .unwrap(),
            &7
        );
        let b: BoxedMessage = Box::new(7u32);
        assert_eq!(
            CloneableMessage::try_from_boxed(&b)
                .unwrap()
                .into_boxed()
                .downcast_ref::<u32>()
                .unwrap(),
            &7
        );
        let b: BoxedMessage = Box::new(7u64);
        assert_eq!(
            CloneableMessage::try_from_boxed(&b)
                .unwrap()
                .into_boxed()
                .downcast_ref::<u64>()
                .unwrap(),
            &7
        );
        let b: BoxedMessage = Box::new(true);
        assert!(
            *CloneableMessage::try_from_boxed(&b)
                .unwrap()
                .into_boxed()
                .downcast_ref::<bool>()
                .unwrap()
        );
        let b: BoxedMessage = Box::new(());
        assert!(CloneableMessage::try_from_boxed(&b).is_some());
        let b: BoxedMessage = Box::new(1.5f32);
        assert_eq!(
            *CloneableMessage::try_from_boxed(&b)
                .unwrap()
                .into_boxed()
                .downcast_ref::<f32>()
                .unwrap(),
            1.5
        );
        let b: BoxedMessage = Box::new(1.5f64);
        assert_eq!(
            *CloneableMessage::try_from_boxed(&b)
                .unwrap()
                .into_boxed()
                .downcast_ref::<f64>()
                .unwrap(),
            1.5
        );
        let b: BoxedMessage = Box::new('x');
        assert_eq!(
            *CloneableMessage::try_from_boxed(&b)
                .unwrap()
                .into_boxed()
                .downcast_ref::<char>()
                .unwrap(),
            'x'
        );
    }

    #[test]
    fn try_from_boxed_unsupported_type_returns_none() {
        struct Custom;
        let b: BoxedMessage = Box::new(Custom);
        assert!(CloneableMessage::try_from_boxed(&b).is_none());
    }

    // ---------------- AnyMessage / MessageContainer ----------------

    #[test]
    fn any_message_trait_object_delegates() {
        struct M;
        impl Message for M {
            type Result = ();
            fn message_type(&self) -> &'static str {
                "M"
            }
            fn validate(&self) -> Result<(), crate::errors::ActorError> {
                Err(crate::errors::ActorError::MessageHandlingError("custom".into()))
            }
            fn priority(&self) -> MessagePriority {
                MessagePriority::CRITICAL
            }
            fn message_options(&self) -> Option<MessageOptions> {
                Some(MessageOptions::default())
            }
        }
        let m = M;
        let am: &dyn AnyMessage = &m;
        assert_eq!(am.message_type(), "M");
        assert!(am.validate().is_err());
        assert_eq!(am.priority(), MessagePriority::CRITICAL);
        assert!(am.message_options().is_some());
        // 默认实现版本
        struct N;
        impl Message for N {
            type Result = ();
        }
        let an: &dyn AnyMessage = &N;
        assert!(an.validate().is_ok());
        assert_eq!(an.priority(), MessagePriority::NORMAL);
        assert!(an.message_options().is_none());
        // 默认 message_type 返回完整 type_name（含 crate 路径）
        assert_eq!(an.message_type(), std::any::type_name::<N>());
    }

    // ---------------- BoxedMessageClone ----------------

    #[test]
    fn boxed_message_clone_for_clone_types() {
        #[derive(Debug, Clone, PartialEq)]
        struct V(u8);
        let v = V(9);
        let bc: &dyn BoxedMessageClone = &v;
        let c = bc.clone_box();
        assert_eq!(c.downcast_ref::<V>().unwrap(), &V(9));
    }

    // ---------------- MessageOptions / MessageEnvelope ----------------

    #[test]
    fn message_options_default() {
        let o = MessageOptions::default();
        assert!(o.timeout.is_none());
        assert!(o.retry_policy.is_none());
        assert_eq!(o.priority, MessagePriority::NORMAL);
    }

    #[test]
    fn envelope_new_with_defaults_from_message() {
        struct M;
        impl Message for M {
            type Result = ();
            fn message_options(&self) -> Option<MessageOptions> {
                Some(MessageOptions {
                    timeout: Some(Duration::from_millis(5)),
                    retry_policy: None,
                    priority: MessagePriority::HIGH,
                })
            }
        }
        let e = MessageEnvelope::new(M, None, None);
        assert_eq!(e.options.priority, MessagePriority::HIGH);
        assert_eq!(e.options.timeout, Some(Duration::from_millis(5)));
        assert!(e.sender.is_none());
        assert_eq!(e.message_type, std::any::type_name::<M>());
    }

    #[test]
    fn envelope_new_with_explicit_options_override() {
        struct M;
        impl Message for M {
            type Result = ();
        }
        let opts = MessageOptions {
            timeout: Some(Duration::from_secs(1)),
            retry_policy: None,
            priority: MessagePriority::CRITICAL,
        };
        let e = MessageEnvelope::new(M, None, Some(opts));
        assert_eq!(e.options.priority, MessagePriority::CRITICAL);
    }

    #[test]
    fn envelope_payload_accessors() {
        #[derive(Debug, PartialEq)]
        struct P(u16);
        impl Message for P {
            type Result = ();
        }
        let mut e = MessageEnvelope::new(P(3), None, None);
        assert_eq!(e.payload::<P>(), Some(&P(3)));
        assert_eq!(e.message::<P>(), Some(&P(3)));
        // 可变访问
        e.payload_mut::<P>().unwrap().0 = 4;
        assert_eq!(e.payload::<P>(), Some(&P(4)));
        e.message_mut::<P>().unwrap().0 = 5;
        assert_eq!(e.message::<P>(), Some(&P(5)));
        // 类型不匹配 → None
        struct Q;
        impl Message for Q {
            type Result = ();
        }
        assert!(e.payload::<Q>().is_none());
        assert!(e.message::<Q>().is_none());
        assert!(e.payload_mut::<Q>().is_none());
        assert!(e.message_mut::<Q>().is_none());
    }

    #[test]
    fn envelope_from_boxed_sets_type_name() {
        let boxed: BoxedMessage = Box::new(42u32);
        let e = MessageEnvelope::from_boxed(boxed, None, MessageOptions::default());
        // 已知限制：type_name_of_val 对 Box<dyn Any> 只能看到擦除后的
        // trait-object 名（"dyn core::any::Any + ..."）。固化此行为；
        // 若未来改为记录具体类型名，此断言会提醒更新调用方文档。
        assert!(e.message_type.contains("dyn"), "was {}", e.message_type);
        // id 唯一
        let e2 = MessageEnvelope::from_boxed(Box::new(1u32), None, MessageOptions::default());
        assert_ne!(e.id, e2.id);
    }

    // ---------------- RetryPolicy / BackoffStrategy ----------------

    #[test]
    fn retry_policy_and_backoff_variants_debug() {
        let p = RetryPolicy {
            max_attempts: 3,
            retry_interval: Duration::from_millis(100),
            backoff_strategy: BackoffStrategy::Exponential {
                base: 2.0,
                max_interval: Duration::from_secs(10),
            },
        };
        assert_eq!(p.max_attempts, 3);
        assert!(format!("{:?}", p).contains("Exponential"));
        assert!(format!("{:?}", BackoffStrategy::Fixed).contains("Fixed"));
        assert!(format!("{:?}", BackoffStrategy::Linear).contains("Linear"));
    }
}
