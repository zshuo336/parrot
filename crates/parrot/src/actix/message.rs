use actix::Message as ActixMessage;
use parrot_api::errors::ActorError;
use parrot_api::message::{Message, MessageEnvelope};
use parrot_api::types::{ActorResult, BoxedMessage};

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

impl ActixMessageWrapper {
    /// Transfer the envelope to the caller, including its cleanup ownership.
    pub fn into_envelope(mut self) -> MessageEnvelope {
        std::mem::replace(
            &mut self.envelope,
            MessageEnvelope {
                trace_context: None,
                id: uuid::Uuid::nil(),
                payload: Box::new(()),
                sender: None,
                options: Default::default(),
                message_type: "()",
            },
        )
    }

    pub(crate) fn into_payload(mut self) -> BoxedMessage {
        std::mem::replace(&mut self.envelope.payload, Box::new(()))
    }
}

impl Drop for ActixMessageWrapper {
    fn drop(&mut self) {
        // Each user value gets its own unwind boundary, including sender refs.
        let payload = std::mem::replace(&mut self.envelope.payload, Box::new(()));
        let sender = self.envelope.sender.take();
        for result in [
            crate::owned_payload::PublicationInput::new(payload).retire(),
            crate::owned_payload::PublicationInput::new(sender).retire(),
        ] {
            if let Err(error) = result {
                tracing::error!(
                    error = %crate::error_receipt::actor_summary(&error),
                    "Actix input disposal failed"
                );
            }
        }
    }
}

impl ActixMessage for ActixMessageWrapper {
    type Result = ActixReply;
}

/// Owns a reply until the request caller explicitly takes its contents.
///
/// Actix drops replies both when a request is cancelled after publication and
/// when its response sender finds a closed receiver. Keeping the guard in the
/// channel value protects both paths, including native `Addr::send` callers.
pub struct ActixReply {
    result: Option<ActorResult<BoxedMessage>>,
}

impl ActixReply {
    /// Protect a response before handing it to Actix's response channel.
    pub fn new(result: Option<ActorResult<BoxedMessage>>) -> Self {
        Self { result }
    }

    /// Transfer the response to the caller, who now owns its eventual cleanup.
    pub fn into_inner(mut self) -> Option<ActorResult<BoxedMessage>> {
        self.result.take()
    }
}

impl std::fmt::Debug for ActixReply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ActixReply")
            .field(&self.result)
            .finish()
    }
}

impl Drop for ActixReply {
    fn drop(&mut self) {
        if let Err(error) = crate::owned_payload::PublicationInput::new(self.result.take()).retire()
        {
            tracing::error!(
                error = %crate::error_receipt::actor_summary(&error),
                "Actix reply disposal failed"
            );
        }
    }
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
            Err(original) => Err(ActorError::MessageHandlingError(format!(
                "Failed to downcast message: expected type {}, got {}",
                std::any::type_name::<M>(),
                std::any::type_name_of_val(&*original)
            ))),
        }
    }

    fn downcast_ref<M: 'static>(&self) -> Option<&M> {
        // Deref through the Box first: `self` is `&Box<dyn Any>`, and the
        // pointee (`dyn Any`) is what carries the concrete type. Calling
        // `downcast_ref` on `&Box<dyn Any>` directly would test whether the
        // reference itself is an `M` — always None.
        (**self).downcast_ref::<M>()
    }

    fn downcast_mut<M: 'static>(&mut self) -> Option<&mut M> {
        (**self).downcast_mut::<M>()
    }
}

/// Helper function to create a message envelope from a message
pub fn create_envelope<M: Message>(msg: M) -> MessageEnvelope {
    MessageEnvelope::new(msg, None, None)
}
