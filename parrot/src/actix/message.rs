use actix::{Message as ActixMessage};
use parrot_api::{
    message::{Message, MessageEnvelope},
    types::{BoxedMessage, ActorResult},
    errors::ActorError
};
use std::fmt::Debug;

/// Wrapper for MessageEnvelope to implement Actix Message trait
#[derive(Debug)]
pub struct ActixMessageWrapper {
    pub envelope: MessageEnvelope,
}

impl ActixMessage for ActixMessageWrapper {
    type Result = Result<BoxedMessage, ActorError>;
}

/// Helper trait for message envelope conversion
pub trait MessageConverter {
    fn to_actix_message<M: Message + 'static>(msg: M) -> ActixMessageWrapper {
        let envelope = MessageEnvelope::new(msg, None, None);
        ActixMessageWrapper { envelope }
    }
    
    fn from_actix_message<M: Message + 'static>(wrapper: ActixMessageWrapper) -> Result<M, ActorError> {
        let payload = wrapper.envelope.payload;
        
        match payload.downcast::<M>() {
            Ok(boxed) => Ok(*boxed),
            Err(_) => Err(ActorError::MessageHandlingError(
                format!("Failed to downcast message to {}", std::any::type_name::<M>())
            )),
        }
    }
    
    fn extract_result<M: Message + 'static>(result: BoxedMessage) -> Result<M::Result, ActorError> {
        M::extract_result(result)
    }
}

/// Handler for Parrot messages
pub trait ActixMessageHandler {
    /// Handle message wrapper and return response
    fn handle_message<M: Message + 'static>(&mut self, msg: M) -> ActorResult<M::Result>;
}

/// Extension methods for MessageEnvelope
pub trait MessageEnvelopeExt {
    /// Create a new MessageEnvelope from any message
    fn new<M: Message + 'static>(
        msg: M,
        sender: Option<Box<dyn parrot_api::address::ActorRef>>,
        options: Option<parrot_api::message::MessageOptions>,
    ) -> Self;
    
    /// Create a new MessageEnvelope from a generic BoxedMessage
    fn new_generic(
        msg: BoxedMessage,
        sender: Option<Box<dyn parrot_api::address::ActorRef>>,
        options: Option<parrot_api::message::MessageOptions>,
    ) -> Self;
}

impl MessageEnvelopeExt for MessageEnvelope {
    fn new<M: Message + 'static>(
        msg: M,
        sender: Option<Box<dyn parrot_api::address::ActorRef>>,
        options: Option<parrot_api::message::MessageOptions>,
    ) -> Self {
        let msg_options = options.unwrap_or_default();
        
        MessageEnvelope {
            id: uuid::Uuid::new_v4(),
            payload: Box::new(msg),
            sender,
            options: msg_options,
            message_type: std::any::type_name::<M>(),
        }
    }
    
    fn new_generic(
        msg: BoxedMessage,
        sender: Option<Box<dyn parrot_api::address::ActorRef>>,
        options: Option<parrot_api::message::MessageOptions>,
    ) -> Self {
        let msg_options = options.unwrap_or_default();
        let message_type = "unknown"; // Not ideal, but we don't know the type name here
        
        MessageEnvelope {
            id: uuid::Uuid::new_v4(),
            payload: msg,
            sender,
            options: msg_options,
            message_type,
        }
    }
}
