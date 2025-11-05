use crate::actix::actor::ActixActor;
use crate::actix::context::ActixContext;
use crate::actix::lifecycle::ActorControl;
use crate::actix::message::ActixMessageWrapper;
use actix::Addr;
use actix::prelude::SendError;
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

pub(crate) struct IdleRecipient<A: actix::Actor> {
    pub(crate) addr: actix::WeakAddr<A>,
}
impl<A> crate::receive_timeout::IdleRecipient for IdleRecipient<A>
where
    A: actix::Actor + actix::Handler<ActixMessageWrapper>,
    A::Context: actix::dev::ToEnvelope<A, ActixMessageWrapper>,
{
    fn deliver(
        &self,
        signal: crate::receive_timeout::IdleSignal,
    ) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let addr = self.addr.upgrade().ok_or(ActorError::Stopped)?;
            let wrapper = ActixMessageWrapper {
                envelope: MessageEnvelope {
                    trace_context: None,
                    id: uuid::Uuid::new_v4(),
                    payload: Box::new(signal),
                    sender: None,
                    options: MessageOptions::default(),
                    message_type: "ReceiveTimeout",
                },
            };
            // The clock waits for capacity without do_send. Claiming the token
            // also wakes/cancels this wait; user handling errors do not disable
            // future idle intervals. Execution stays on the owning arbiter.
            let _ = addr.send(wrapper).await.map_err(|_| ActorError::Stopped)?;
            Ok(())
        })
    }
}

pub(crate) struct ChildRecipient<A: actix::Actor> {
    pub(crate) addr: actix::WeakAddr<A>,
}
impl<A> crate::children::ChildRecipient for ChildRecipient<A>
where
    A: actix::Actor + actix::Handler<ActixMessageWrapper>,
    A::Context: actix::dev::ToEnvelope<A, ActixMessageWrapper>,
{
    fn deliver(&self, event: crate::children::ChildTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let addr = self.addr.upgrade().ok_or(ActorError::Stopped)?;
            let wrapper = ActixMessageWrapper {
                envelope: MessageEnvelope {
                    trace_context: None,
                    id: uuid::Uuid::new_v4(),
                    payload: Box::new(event),
                    sender: None,
                    options: MessageOptions::default(),
                    message_type: "ChildTerminated",
                },
            };
            // Bounded actor mailbox; the worker also waits for its processing receipt.
            let _ = addr.send(wrapper).await.map_err(|_| ActorError::Stopped)?;
            Ok(())
        })
    }
}

pub(crate) struct StreamRecipient<A: actix::Actor> {
    pub(crate) addr: actix::WeakAddr<A>,
}
impl<A> crate::streams::StreamRecipient for StreamRecipient<A>
where
    A: actix::Actor + actix::Handler<ActixMessageWrapper>,
    A::Context: actix::dev::ToEnvelope<A, ActixMessageWrapper>,
{
    fn deliver(&self, event: crate::streams::StreamEvent) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let addr = self.addr.upgrade().ok_or(ActorError::Stopped)?;
            let wrapper = ActixMessageWrapper {
                envelope: MessageEnvelope {
                    trace_context: None,
                    id: uuid::Uuid::new_v4(),
                    payload: Box::new(event),
                    sender: None,
                    options: MessageOptions::default(),
                    message_type: "StreamEvent",
                },
            };
            // Bounded actor mailbox; the worker also waits for its processing receipt.
            let _ = addr.send(wrapper).await.map_err(|_| ActorError::Stopped)?;
            Ok(())
        })
    }
}

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
        }
    }

    pub(crate) fn with_control(mut self, control: Arc<ActorControl>) -> Self {
        // Only the initial published reference installs the endpoint. Self
        // references built during started() share the same generation handle.
        let _ = control.death_watch.set_recipient(Arc::new(WatchRecipient {
            addr: self.addr.downgrade(),
        }));
        self.control = Some(control);
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
            trace_context: parrot_api::trace::TraceContext::current(),
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
            trace_context: parrot_api::trace::TraceContext::current(),
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
        Box::pin(parrot_api::trace::capture(async move {
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
        }))
    }

    fn deliver_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            options.validate_single_attempt()?;
            options.validate_fifo()?;
            // Native try_send has no admission wait. Its receipt never claims
            // that handler execution finished within the caller's timeout.
            self.deliver(msg).await
        }))
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

        Box::pin(parrot_api::trace::capture(async move {
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
                    Err(actix::MailboxError::Closed) => Err(ActorError::Stopped),
                    Err(actix::MailboxError::Timeout) => Err(ActorError::TimeoutDetail(format!(
                        "Native request to actor {} timed out",
                        self.path
                    ))),
                }
            };
            if let Some(control) = &self.control {
                tokio::select! { biased; result = request => result, _ = control.channel.closed() => Err(ActorError::Stopped) }
            } else {
                request.await
            }
        }))
    }

    fn send_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            options.validate_single_attempt()?;
            options.validate_fifo()?;
            // Reuse the generation close fence as well as its deadline logic.
            self.send_with_timeout(msg, options.timeout).await
        }))
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
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        self.control
            .as_ref()
            .map(|control| control.death_watch.clone())
    }
}

// No strong Addr or ActorControl is retained by a subscription endpoint.
struct WatchRecipient<A: actix::Actor> {
    addr: actix::WeakAddr<A>,
}
impl<A> parrot_api::death_watch::TerminationRecipient for WatchRecipient<A>
where
    A: actix::Actor + actix::Handler<ActixMessageWrapper>,
    A::Context: actix::dev::ToEnvelope<A, ActixMessageWrapper>,
{
    fn deliver(
        &self,
        event: parrot_api::context::ActorTerminated,
    ) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let addr = self.addr.upgrade().ok_or(ActorError::Stopped)?;
            let wrapper = ActixMessageWrapper {
                envelope: MessageEnvelope {
                    trace_context: None,
                    id: uuid::Uuid::new_v4(),
                    payload: Box::new(event),
                    sender: None,
                    options: MessageOptions::default(),
                    message_type: "ActorTerminated",
                },
            };
            // Actix Request waits for capacity. Its cancellation removes a
            // still-waiting request; no unbounded do_send escape hatch is used.
            addr.send(wrapper)
                .await
                .map_err(|_| ActorError::Stopped)?
                .ok_or_else(|| {
                    ActorError::MessageHandlingError(
                        "termination notification had no response".into(),
                    )
                })??;
            Ok(())
        })
    }
}
