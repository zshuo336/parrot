/// # Parrot-Actix Common Types
/// 
/// This module contains common type definitions used across the Parrot-Actix adapter.
/// Keeping these types in a separate module helps avoid circular dependencies.

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use actix::Message as ActixMessage;

/// Type alias for a boxed trait object that can be sent across threads.
pub type BoxedMessage = Box<dyn Any + Send>;

/// Type alias for a result containing either the BoxedMessage or an error.
pub type MessageResult = Result<BoxedMessage, Box<dyn std::error::Error + Send + Sync>>;

/// Result type returned by Actor handlers.
pub type HandlerResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Type alias for a message envelope held inside an Arc for shared ownership.
pub type SharedEnvelope = Arc<MessageEnvelope>;

/// A unique identifier for an actor instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActorId(pub String);

impl ActorId {
    /// Creates a new ActorId with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        ActorId(name.into())
    }

    /// Returns the name of this actor.
    pub fn name(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A message envelope that wraps the actual message.
/// This allows storing messages of different types in a uniform way.
pub struct MessageEnvelope {
    /// The actual message, boxed as a trait object.
    pub message: BoxedMessage,
    /// The name of the message type.
    pub message_type: &'static str,
}

impl fmt::Debug for MessageEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageEnvelope")
            .field("message_type", &self.message_type)
            .finish()
    }
}

/// An internal message to stop the actor
#[derive(Debug)]
pub struct StopMessage;

// Implement actix::Message for StopMessage
impl ActixMessage for StopMessage {
    type Result = ();
}

/// Type alias for context generic parameter
/// This is used to avoid circular references between modules
pub struct ActorContextData; 