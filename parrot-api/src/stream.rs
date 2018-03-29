use std::any::Any;
use futures::{Stream, StreamExt};
use async_trait::async_trait;
use crate::errors::ActorError;
use crate::actor::Actor;
use crate::context::ActorContext;

/// Stream handler trait for handling stream items
#[async_trait]
pub trait StreamHandler<S: Stream>: Send 
where
    S::Item: Send + 'static,
{
    /// Called when stream item received
    async fn handle(&mut self, item: S::Item, ctx: &mut dyn ActorContext);

    /// Called when stream is started
    async fn started(&mut self, ctx: &mut dyn ActorContext) {}

    /// Called when stream is finished
    async fn finished(&mut self, ctx: &mut dyn ActorContext) {}

    /// Called when stream has error
    async fn handle_error(&mut self, err: ActorError, ctx: &mut dyn ActorContext) {}
}

/// Stream registry for managing streams
pub trait StreamRegistry: Send {
    /// Add type-erased stream
    fn add_stream_erased(
        &mut self,
        stream: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>,
    ) -> Result<(), ActorError>;

    /// Add type-erased stream with handler
    fn add_stream_with_handler_erased(
        &mut self,
        stream: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>,
        handler: Box<dyn Any + Send>,
    ) -> Result<(), ActorError>;
}

/// Extension trait for StreamRegistry to provide type-safe methods
pub trait StreamRegistryExt: StreamRegistry {
    /// Add stream with type safety
    fn add_stream<S>(&mut self, stream: S) -> Result<(), ActorError>
    where
        S: Stream + Send + 'static,
        S::Item: Send + 'static,
    {
        let stream = Box::new(stream.map(|item| Box::new(item) as Box<dyn Any + Send>));
        self.add_stream_erased(stream)
    }

    /// Add stream with custom handler and type safety
    fn add_stream_with_handler<S, H>(&mut self, stream: S, handler: H) -> Result<(), ActorError>
    where
        S: Stream + Send + 'static,
        S::Item: Send + 'static,
        H: StreamHandler<S> + 'static,
    {
        let stream = Box::new(stream.map(|item| Box::new(item) as Box<dyn Any + Send>));
        self.add_stream_with_handler_erased(stream, Box::new(handler))
    }
}

// Implement StreamRegistryExt for all types that implement StreamRegistry
impl<T: StreamRegistry + ?Sized> StreamRegistryExt for T {}

/// Stream message type for internal use
#[derive(Debug)]
pub(crate) enum StreamMessage<I> {
    /// New stream item
    Item(I),
    /// Stream error
    Error(ActorError),
    /// Stream completed
    Completed,
}

/// Actor stream handler implementation
pub struct ActorStreamHandler<A: Actor> {
    actor: A,
}

impl<A: Actor> ActorStreamHandler<A> {
    pub fn new(actor: A) -> Self {
        Self { actor }
    }
}

#[async_trait]
impl<A: Actor, S: Stream> StreamHandler<S> for ActorStreamHandler<A>
where
    S::Item: Send + 'static,
{
    async fn handle(&mut self, item: S::Item, ctx: &mut dyn ActorContext) {
        let item = Box::new(item) as Box<dyn Any + Send>;
        if let Err(err) = self.actor.handle_stream(item, ctx).await {
            self.actor.stream_error(err, ctx).await.ok();
        }
    }

    async fn started(&mut self, ctx: &mut dyn ActorContext) {
        self.actor.stream_started(ctx).await.ok();
    }

    async fn finished(&mut self, ctx: &mut dyn ActorContext) {
        self.actor.stream_finished(ctx).await.ok();
    }

    async fn handle_error(&mut self, err: ActorError, ctx: &mut dyn ActorContext) {
        self.actor.stream_error(err, ctx).await.ok();
    }
} 