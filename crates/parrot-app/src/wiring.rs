//! Generation-bound application references. Connections grant message access;
//! component references retain the underlying target's stop capability.
//! Closing a generation rejects subsequent polling of component stops and sends,
//! including futures created before closure but not yet polled.
//! Message futures retain their construction-time trace context (including None)
//! through polling and disposal. A connection adds no synthetic handler span.
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct WiredRef {
    source: String,
    target: BoxedActorRef,
    admitted: Arc<AtomicBool>,
    access: Access,
}
#[derive(Clone, Copy, Debug)]
enum Access {
    Connection,
    Component,
}
impl WiredRef {
    pub(crate) fn connection(
        source: String,
        target: BoxedActorRef,
        admitted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            source,
            target,
            admitted,
            access: Access::Connection,
        }
    }
    pub(crate) fn component(
        source: String,
        target: BoxedActorRef,
        admitted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            source,
            target,
            admitted,
            access: Access::Component,
        }
    }
    fn check(&self) -> ActorResult<()> {
        if self.admitted.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(ActorError::Stopped)
        }
    }
}
impl ActorRef for WiredRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.check()?;
            self.target.send(message).await
        }))
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.check()?;
            self.target.send_with_timeout(message, timeout).await
        }))
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.check()?;
            self.target.deliver(message).await
        }))
    }
    fn send_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.check()?;
            self.target.send_with_options(message, options).await
        }))
    }
    fn deliver_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.check()?;
            self.target.deliver_with_options(message, options).await
        }))
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            match self.access {
                Access::Connection => Err(ActorError::MessageHandlingError("a wiring connection does not own its target; stop the application through its assembling context".into())),
                Access::Component => {
                    self.check()?;
                    self.target.stop().await
                }
            }
        })
    }
    fn path(&self) -> String {
        self.source.clone()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move {
            self.check().is_ok() && self.target.is_alive().await && self.check().is_ok()
        })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            source: self.source.clone(),
            target: self.target.clone_boxed(),
            admitted: self.admitted.clone(),
            access: self.access,
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
