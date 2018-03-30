use std::future::Future;
use crate::context::ActorContext;
use crate::address::ActorRef;
use crate::errors::ActorError;
use crate::types::{BoxedMessage, BoxedFuture, ActorResult, BoxedActorRef};

/// Actor lifecycle state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorState {
    Starting,
    Running,
    Stopping,
    Stopped,
}

/// Actor configuration trait
pub trait ActorConfig: Send + Sync + 'static {}

/// Actor factory trait
pub trait ActorFactory<A: Actor>: Send + 'static {
    fn create(&self, config: A::Config) -> A;
}

/// Core Actor trait
pub trait Actor: Send + 'static {
    /// Actor configuration type
    type Config: ActorConfig;
    /// Actor context type
    type Context: ?Sized + Send;

    /// Initialize the Actor
    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Handle incoming message
    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Handle stream item
    fn handle_stream<'a>(&'a mut self, item: BoxedMessage, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // Forward the result from receive_message
            self.receive_message(item, ctx).await
        })
    }

    /// Called when stream is started
    fn stream_started<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Called when stream is finished
    fn stream_finished<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Called when stream has error
    fn stream_error<'a>(&'a mut self, err: ActorError, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { Err(err) })
    }

    /// Cleanup work before Actor stops
    fn before_stop<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Handle child Actor termination
    fn handle_child_terminated<'a>(&'a mut self, _child: BoxedActorRef, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Get Actor state
    fn state(&self) -> ActorState;
} 
