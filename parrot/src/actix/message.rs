use actix::{Message as ActixMessage, Handler, Actor as ActixActor};
use parrot_api::{
    message::{Message, MessageEnvelope, MessageOptions},
    types::{BoxedMessage, ActorResult},
};
use std::fmt::Debug;

// Wrapper around Parrot messages to implement Actix Message trait
pub struct ActixMessageWrapper {
    pub envelope: MessageEnvelope,
}

impl Debug for ActixMessageWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActixMessageWrapper")
            .field("envelope", &self.envelope)
            .finish()
    }
}

impl ActixMessage for ActixMessageWrapper {
    type Result = ActorResult<BoxedMessage>;
}

pub fn wrap_message(envelope: MessageEnvelope) -> ActixMessageWrapper {
    ActixMessageWrapper {
        envelope,
    }
}

// Handler implementation for converting Parrot messages to Actix messages
pub trait ActixMessageHandler: ActixActor {
    fn handle_parrot_message(&mut self, msg: MessageEnvelope) -> ActorResult<BoxedMessage>;
}

pub fn unwrap_message<M: Message>(wrapper: ActixMessageWrapper) -> (M, Option<MessageOptions>) {
    let message = wrapper.envelope.payload.downcast::<M>()
        .expect("Failed to downcast message to expected type");
    (*message, Some(wrapper.envelope.options))
}

// Helper trait for message envelope conversion
pub trait EnvelopeConverter {
    fn to_actix(&self, envelope: MessageEnvelope) -> ActixMessageWrapper;
    fn from_actix(wrapper: ActixMessageWrapper) -> MessageEnvelope;
}
