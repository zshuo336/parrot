use crate::actix::actor::ActixActor;
use crate::actix::context::ActixContext;
use crate::actix::message::ActixMessageWrapper;
use actix::Addr;
use actix::prelude::SendError;
use anyhow::anyhow;
use async_trait::async_trait;
use parrot_api::actor::Actor as ParrotActor;
use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::message::{MessageEnvelope, MessageOptions};
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
/// An internal message to stop the actor
#[derive(Debug)]
pub struct StopMessage;

// Implement actix::Message for StopMessage
impl actix::Message for StopMessage {
    type Result = ();
}

/// ActixActorRef implements the ActorRef trait for Actix addresses
///
/// # Overview
/// Provides a reference to an actor in the Actix system
///
/// # Key Responsibilities
/// - Send messages to the referenced actor
/// - Track actor address information
/// - Convert between Parrot and Actix message types
///
/// # Implementation Details
/// - Wraps an actix::Addr
/// - Handles message conversion and type safety
/// - Manages both sync and async message delivery
pub struct ActixActorRef<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
        + std::marker::Unpin
        + 'static,
{
    /// The underlying Actix address
    addr: Arc<Addr<ActixActor<A>>>,
    /// Path to the actor in the system
    path: String,
}

impl<A> ActixActorRef<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
        + std::marker::Unpin
        + 'static,
{
    /// Create a new ActixActorRef from an Actix address
    pub fn new(addr: Addr<ActixActor<A>>, path: String) -> Self {
        Self {
            addr: Arc::new(addr),
            path,
        }
    }

    pub fn get_addr(&self) -> &Addr<ActixActor<A>> {
        &self.addr
    }

    pub fn get_path(&self) -> &String {
        &self.path
    }

    /// Build an envelope with default options (associated helper for
    /// static contexts like `deliver`).
    pub fn create_envelope_for(msg: BoxedMessage) -> ActixMessageWrapper {
        Self::create_envelope_static(msg, None, MessageOptions::default(), "unknown")
    }

    fn create_envelope_static(
        msg: BoxedMessage,
        sender: Option<BoxedActorRef>,
        options: MessageOptions,
        message_type: &'static str,
    ) -> ActixMessageWrapper {
        let envelope = MessageEnvelope {
            id: uuid::Uuid::new_v4(),
            payload: msg,
            sender,
            options,
            message_type,
        };
        ActixMessageWrapper { envelope }
    }

    pub fn create_envelope(
        &self,
        msg: BoxedMessage,
        sender: Option<BoxedActorRef>,
        options: MessageOptions,
        message_type: &'static str,
    ) -> ActixMessageWrapper {
        let envelope = MessageEnvelope {
            id: uuid::Uuid::new_v4(),
            payload: msg,
            sender,
            options,
            message_type,
        };

        // Create ActixMessageWrapper

        ActixMessageWrapper { envelope }
    }

    /// Sends a message unconditionally, ignoring any potential errors.
    ///
    /// The message is always queued, even if the mailbox for the receiver is full. If the mailbox
    /// is closed, the message is silently dropped.
    pub fn do_send(&self, msg: BoxedMessage) {
        let wrapper = self.create_envelope(msg, None, MessageOptions::default(), "unknown");
        self.addr.do_send(wrapper);
    }

    /// Tries to send a message.
    ///
    /// This method fails if actor's mailbox is full or closed. This
    /// method registers the current task in the receiver's queue.
    pub fn try_send(&self, msg: BoxedMessage) -> Result<(), SendError<BoxedMessage>> {
        let wrapper = self.create_envelope(msg, None, MessageOptions::default(), "unknown");
        self.addr.try_send(wrapper).map_err(|e| match e {
            SendError::Full(wrapper) => SendError::Full(wrapper.envelope.payload),
            SendError::Closed(wrapper) => SendError::Closed(wrapper.envelope.payload),
        })
    }
}

impl<A> std::fmt::Debug for ActixActorRef<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
        + std::marker::Unpin
        + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActixActorRef")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<A> ActorRef for ActixActorRef<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
        + std::marker::Unpin
        + 'static,
{
    /// Send a message to the actor and wait for a response
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send_with_timeout(msg, None)
    }

    /// Fire-and-forget delivery: enqueue and return immediately.
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        let addr = self.addr.clone();
        Box::pin(async move {
            // do_send never blocks: actix mailboxes are unbounded; closed
            // mailboxes silently drop (documented actix semantics). We
            // surface closure via connected() for observability.
            if !addr.connected() {
                return Err(ActorError::InternalError(
                    "Actor mailbox closed (actor stopped)".to_string(),
                ));
            }
            let wrapper =
                Self::create_envelope_static(msg, None, MessageOptions::default(), "tell");
            addr.do_send(wrapper);
            Ok(())
        })
    }

    /// Send a message to the actor with a timeout
    ///
    /// # Parameters
    /// - `msg`: The message to send
    /// - `timeout`: The timeout duration
    ///
    /// # Returns
    /// A future that resolves to the actor result
    ///
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        // Create message envelope
        let wrapper = self.create_envelope(msg, None, MessageOptions::default(), "unknown");
        let addr = self.addr.clone();

        // Send the message via Actix
        Box::pin(async move {
            let mut result_fut = addr.send(wrapper);
            if let Some(timeout) = timeout {
                result_fut = result_fut.timeout(timeout);
            }
            match result_fut.await {
                Ok(Some(result)) => result,
                Ok(None) => Err(ActorError::MessageHandlingError(
                    "No response from actor".to_string(),
                )),
                Err(e) => Err(ActorError::Other(anyhow!(
                    "Failed to deliver message: {}",
                    e
                ))),
            }
        })
    }

    /// Stop the actor
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        let addr = self.addr.clone();

        Box::pin(async move {
            // Wrap StopMessage in an ActixMessageWrapper
            let msg = Box::new(StopMessage) as BoxedMessage;
            let envelope = MessageEnvelope {
                id: uuid::Uuid::new_v4(),
                payload: msg,
                sender: None,
                options: Default::default(),
                message_type: "stop",
            };
            let wrapper = ActixMessageWrapper { envelope };
            addr.do_send(wrapper);
            Ok(())
        })
    }

    /// Get the actor's path
    fn path(&self) -> String {
        self.path.clone()
    }

    /// Check if the actor is alive
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        let addr = self.addr.clone();

        Box::pin(async move {
            // In Actix, we can check if the address is connected
            addr.connected()
        })
    }

    /// Clone this reference as a boxed ActorRef
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            addr: self.addr.clone(),
            path: self.path.clone(),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
