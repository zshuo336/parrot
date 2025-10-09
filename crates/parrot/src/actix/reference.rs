use crate::actix::actor::ActixActor;
use crate::actix::context::ActixContext;
use crate::actix::lifecycle::ActorControl;
use crate::actix::message::ActixMessageWrapper;
use actix::Addr;
use actix::prelude::SendError;
use anyhow::anyhow;
use async_trait::async_trait;
use parrot_api::actor::Actor as ParrotActor;
use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::message::{MessageEnvelope, MessageOptions};
use parrot_api::typed_channel::TypedActorLifecycle;
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
    control: Option<Arc<ActorControl>>,
    default_timeout: Option<Duration>,
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
            control: None,
            default_timeout: None,
        }
    }

    pub(crate) fn with_control(
        mut self,
        control: Arc<ActorControl>,
        ask_timeout: Duration,
    ) -> Self {
        self.control = Some(control);
        self.default_timeout = Some(ask_timeout);
        self
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
        Box::pin(async move {
            if self
                .control
                .as_ref()
                .is_some_and(|control| control.channel.is_sealed())
            {
                return Err(ActorError::Stopped);
            }
            let wrapper =
                Self::create_envelope_static(msg, None, MessageOptions::default(), "tell");
            self.addr.try_send(wrapper).map_err(|error| match error {
                SendError::Closed(_) => ActorError::Stopped,
                SendError::Full(_) => {
                    ActorError::MessageHandlingError("Actix mailbox is full".into())
                }
            })
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

        let timeout = timeout.or(self.default_timeout);
        Box::pin(async move {
            let request = async {
                let request = addr.send(wrapper);
                let result = match timeout {
                    Some(duration) => {
                        tokio::time::timeout(duration, request).await.map_err(|_| {
                            ActorError::TimeoutDetail(format!(
                                "Request to actor {} timed out after {}ms",
                                self.path,
                                duration.as_millis()
                            ))
                        })?
                    }
                    None => request.await,
                };
                match result {
                    Ok(Some(result)) => result,
                    Ok(None) => Err(ActorError::MessageHandlingError(
                        "No response from actor".into(),
                    )),
                    Err(e) => Err(ActorError::Other(anyhow!(
                        "Failed to deliver message: {}",
                        e
                    ))),
                }
            };
            if let Some(control) = &self.control {
                tokio::select! { biased; _ = control.channel.closed() => Err(ActorError::Stopped), result = request => result }
            } else {
                request.await
            }
        })
    }

    /// Stop the actor
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if let Some(control) = &self.control {
                return control.stop().await;
            }
            // Raw Actix addresses have no system owner. At least wait for actual
            // closure, rather than returning success after merely queuing stop.
            self.addr.do_send(StopMessage);
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.addr.connected() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .map_err(|_| ActorError::TimeoutDetail("raw Actix actor did not stop".into()))
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
            !self
                .control
                .as_ref()
                .is_some_and(|control| control.channel.is_sealed())
                && addr.connected()
        })
    }

    /// Clone this reference as a boxed ActorRef
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            addr: self.addr.clone(),
            path: self.path.clone(),
            control: self.control.clone(),
            default_timeout: self.default_timeout,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
