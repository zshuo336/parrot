use std::any::Any;
use uuid::Uuid;
use actix::Message as ActixMessage;
use parrot_api::message::{MessageEnvelope, Message, MessageOptions};
use parrot_api::types::BoxedMessage;
use parrot_api::errors::ActorError;

/// Wrapper for MessageEnvelope to implement actix::Message
/// 
/// # Overview
/// This is necessary since we can't directly implement external traits
/// for external types (orphan rule).
#[derive(Debug)]
pub struct ActixMessageWrapper {
    /// The wrapped message envelope
    pub envelope: MessageEnvelope,
}

impl ActixMessage for ActixMessageWrapper {
    type Result = Result<BoxedMessage, ActorError>;
}

/// Helper traits and functions for message handling
pub trait MessageDowncast {
    /// Downcast the message to a specific type
    fn downcast<M: 'static>(self) -> Result<M, ActorError>;
    
    /// Get a reference to the message as a specific type
    fn downcast_ref<M: 'static>(&self) -> Option<&M>;
    
    /// Get a mutable reference to the message as a specific type
    fn downcast_mut<M: 'static>(&mut self) -> Option<&mut M>;
}

impl MessageDowncast for BoxedMessage {
    fn downcast<M: 'static>(self) -> Result<M, ActorError> {
        match self.downcast::<M>() {
            Ok(boxed) => Ok(*boxed),
            Err(original) => Err(ActorError::MessageHandlingError(
                format!("Failed to downcast message: expected type {}, got {}",
                        std::any::type_name::<M>(),
                        std::any::type_name_of_val(&*original))
            )),
        }
    }
    
    fn downcast_ref<M: 'static>(&self) -> Option<&M> {
        <dyn Any>::downcast_ref::<M>(self)
    }
    
    fn downcast_mut<M: 'static>(&mut self) -> Option<&mut M> {
        <dyn Any>::downcast_mut::<M>(self)
    }
}

/// Helper function to create a message envelope from a message
pub fn create_envelope<M: Message>(msg: M) -> MessageEnvelope {
    MessageEnvelope {
        id: Uuid::new_v4(),
        payload: Box::new(msg) as Box<dyn Any + Send>,
        sender: None,
        options: MessageOptions::default(),
        message_type: std::any::type_name::<M>(),
    }
} 