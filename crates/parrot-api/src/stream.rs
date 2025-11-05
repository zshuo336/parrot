//! # Actor Stream Processing
//!
//! This module provides stream processing capabilities for the Parrot actor system.
//! It enables actors to handle continuous streams of data with backpressure and
//! error handling.
//!
//! ## Design Philosophy
//!
//! The stream processing system is built on these principles:
//! - Type Safety: Generic stream handling with compile-time type checking
//! - Backpressure: Natural flow control through async streams
//! - Error Handling: Comprehensive error management and recovery
//! - Flexibility: Support for custom stream handlers and processors
//!
//! ## Core Components
//!
//! - `StreamHandler`: Interface for processing stream items
//! - `StreamRegistry`: Stream lifecycle management
//! - `StreamRegistryExt`: Type-safe stream registration
//! - `ActorStreamHandler`: Default actor stream processing
//!
//! ## Usage Example
//!
//! ```
//! use futures::stream;
//! use parrot_api::{errors::ActorError, stream::{StreamRegistry, StreamRegistryExt}};
//!
//! // Call from actor initialization or a handler using ctx.stream_registry().
//! fn register(registry: &mut dyn StreamRegistry) -> Result<(), ActorError> {
//!     registry.add_stream(stream::iter([1_u32, 2, 3]))
//! }
//! ```
//!
//! Managed Thread and Actix backends execute stream hooks serially with actor
//! messages and wait for each item's processing before polling the next item.
//! Registration is bounded by the creator's context-task quota and stop owns
//! cancellation/disposal. Detached contexts reject registration. Custom typed
//! registrations preserve a callable handler through `StreamHandlerRegistration`;
//! raw erased handlers must use that wrapper rather than arbitrary `Any`.

use crate::actor::Actor;
use crate::errors::ActorError;
pub use crate::stream_dispatch::{
    StreamCallback, StreamContextAccess, StreamContextView, StreamHandlerContext,
    StreamHandlerRegistration,
};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use std::any::Any;

/// Core trait for processing items from a stream.
///
/// This trait defines how actors handle streaming data, including:
/// - Item processing
/// - Stream lifecycle events
/// - Error handling
///
/// # Type Parameters
///
/// * `S`: Stream type being handled
/// * `C`: Context type for processing
///
/// # Implementation Notes
///
/// Implementors should handle:
/// - Backpressure through async processing
/// - Resource cleanup in lifecycle methods
/// - Error recovery in error handler
#[async_trait]
pub trait StreamHandler<S: Stream, C: ?Sized + Send>: Send
where
    S::Item: Send + 'static,
{
    /// Processes a single item from the stream.
    ///
    /// This is the main processing method called for each stream item.
    /// Implementation should handle backpressure naturally through
    /// async processing.
    ///
    /// # Parameters
    /// * `item` - The item to process
    /// * `ctx` - Mutable reference to processing context
    async fn handle(&mut self, item: S::Item, ctx: &mut C);

    /// Called when stream processing begins.
    ///
    /// Use this method to:
    /// - Initialize resources
    /// - Set up state
    /// - Prepare for processing
    ///
    /// # Parameters
    /// * `ctx` - Mutable reference to processing context
    async fn started(&mut self, _ctx: &mut C) {}

    /// Called when stream completes successfully.
    ///
    /// Use this method to:
    /// - Clean up resources
    /// - Finalize state
    /// - Notify completion
    ///
    /// # Parameters
    /// * `ctx` - Mutable reference to processing context
    async fn finished(&mut self, _ctx: &mut C) {}

    /// Called when stream encounters an error.
    ///
    /// Use this method to:
    /// - Handle error conditions
    /// - Attempt recovery
    /// - Clean up resources
    ///
    /// # Parameters
    /// * `err` - The error that occurred
    /// * `ctx` - Mutable reference to processing context
    async fn handle_error(&mut self, _err: ActorError, _ctx: &mut C) {}
}

/// Interface for managing stream lifecycle and registration.
///
/// This trait provides the core functionality for:
/// - Adding new streams
/// - Connecting streams to handlers
/// - Managing stream lifecycle
pub trait StreamRegistry: Send {
    /// Registers a type-erased stream for processing.
    ///
    /// # Parameters
    /// * `stream` - Boxed stream with type-erased items
    ///
    /// # Returns
    /// Result indicating success or failure of registration
    fn add_stream_erased(
        &mut self,
        stream: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>,
    ) -> Result<(), ActorError>;

    /// Registers a type-erased stream with a custom handler.
    ///
    /// # Parameters
    /// * `stream` - Boxed stream with type-erased items
    /// * `handler` - Boxed `StreamHandlerRegistration` for runtime backends
    ///
    /// # Returns
    /// Result indicating success or failure of registration
    fn add_stream_with_handler_erased(
        &mut self,
        stream: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>,
        handler: Box<dyn Any + Send>,
    ) -> Result<(), ActorError>;
}

/// Type-safe extension methods for stream registration.
///
/// This trait provides convenience methods that preserve
/// type information when registering streams.
pub trait StreamRegistryExt: StreamRegistry {
    /// Registers a typed stream for processing.
    ///
    /// # Type Parameters
    /// * `S` - Stream type with Send items
    ///
    /// # Parameters
    /// * `stream` - The stream to process
    ///
    /// # Returns
    /// Result indicating success or failure of registration
    fn add_stream<S>(&mut self, stream: S) -> Result<(), ActorError>
    where
        S: Stream + Send + 'static,
        S::Item: Send + 'static,
    {
        let stream = Box::new(stream.map(|item| Box::new(item) as Box<dyn Any + Send>));
        self.add_stream_erased(stream)
    }

    /// Registers a typed stream with a custom handler.
    ///
    /// # Type Parameters
    /// * `S` - Stream type
    /// * `H` - Handler type
    /// * `C` - Actual context type, or `dyn ActorContext`; other unsized
    ///   interfaces must implement `StreamHandlerContext`
    ///
    /// # Parameters
    /// * `stream` - The stream to process
    /// * `handler` - Custom handler for the stream
    ///
    /// # Returns
    /// Result indicating success or failure of registration
    fn add_stream_with_handler<S, H, C>(&mut self, stream: S, handler: H) -> Result<(), ActorError>
    where
        S: Stream + Send + 'static,
        S::Item: Send + 'static,
        H: StreamHandler<S, C> + 'static,
        C: ?Sized + StreamHandlerContext + 'static,
    {
        let stream = Box::new(stream.map(|item| Box::new(item) as Box<dyn Any + Send>));
        self.add_stream_with_handler_erased(
            stream,
            Box::new(StreamHandlerRegistration::new::<S, H, C>(handler)),
        )
    }
}

impl<T: StreamRegistry + ?Sized> StreamRegistryExt for T {}

/// Default stream handler implementation for actors.
///
/// This handler delegates stream processing to the actor's
/// stream handling methods.
pub struct ActorStreamHandler<A: Actor> {
    /// The actor that will process the stream
    actor: A,
}

impl<A: Actor> ActorStreamHandler<A> {
    /// Creates a new handler for the specified actor.
    ///
    /// # Parameters
    /// * `actor` - The actor that will handle the stream
    pub fn new(actor: A) -> Self {
        Self { actor }
    }
}

#[async_trait]
impl<A: Actor, S: Stream> StreamHandler<S, A::Context> for ActorStreamHandler<A>
where
    S::Item: Send + 'static,
{
    async fn handle(&mut self, item: S::Item, ctx: &mut A::Context) {
        let item = Box::new(item) as Box<dyn Any + Send>;
        if let Err(err) = self.actor.handle_stream(item, ctx).await {
            self.actor.stream_error(err, ctx).await.ok();
        }
    }

    async fn started(&mut self, ctx: &mut A::Context) {
        if let Err(error) = self.actor.stream_started(ctx).await {
            self.actor.stream_error(error, ctx).await.ok();
        }
    }

    async fn finished(&mut self, ctx: &mut A::Context) {
        if let Err(error) = self.actor.stream_finished(ctx).await {
            self.actor.stream_error(error, ctx).await.ok();
        }
    }

    async fn handle_error(&mut self, err: ActorError, ctx: &mut A::Context) {
        self.actor.stream_error(err, ctx).await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{Actor, ActorState, EmptyConfig};
    use crate::types::{ActorResult, BoxedFuture, BoxedMessage};
    use futures::stream::{self, StreamExt};

    #[derive(Debug, Default)]
    struct RecActor {
        items: Vec<u64>,
        started: bool,
        finished: bool,
        errored: Option<String>,
        fail_hooks: bool,
    }

    impl Actor for RecActor {
        type Config = EmptyConfig;
        type Context = NullCtx;
        fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _c: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                let value = *msg.downcast::<u64>().unwrap();
                if value == 0 {
                    return Err(ActorError::MessageHandlingError("zero item".into()));
                }
                self.items.push(value);
                Ok(Box::new(()) as BoxedMessage)
            })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn stream_started<'a>(
            &'a mut self,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.started = true;
                if self.fail_hooks {
                    Err(ActorError::InitializationError("start failure".into()))
                } else {
                    Ok(())
                }
            })
        }
        fn stream_finished<'a>(
            &'a mut self,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.finished = true;
                if self.fail_hooks {
                    Err(ActorError::MessageHandlingError("finish failure".into()))
                } else {
                    Ok(())
                }
            })
        }
        fn stream_error<'a>(
            &'a mut self,
            error: ActorError,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.errored = Some(error.to_string());
                Ok(())
            })
        }
    }

    struct NullCtx;

    // ---------------- StreamHandler 生命周期默认实现 ----------------

    struct CollectHandler {
        items: Vec<u64>,
        started: bool,
        finished: bool,
        err: Option<String>,
    }

    #[async_trait]
    impl StreamHandler<futures::stream::Iter<std::vec::IntoIter<u64>>, NullCtx> for CollectHandler {
        async fn handle(&mut self, item: u64, _ctx: &mut NullCtx) {
            self.items.push(item);
        }
        async fn started(&mut self, _ctx: &mut NullCtx) {
            self.started = true;
        }
        async fn finished(&mut self, _ctx: &mut NullCtx) {
            self.finished = true;
        }
        async fn handle_error(&mut self, err: ActorError, _ctx: &mut NullCtx) {
            self.err = Some(err.to_string());
        }
    }

    struct DefaultHooksHandler;

    #[async_trait]
    impl<S: Stream<Item = u64>, C: ?Sized + Send> StreamHandler<S, C> for DefaultHooksHandler
    where
        S::Item: Send + 'static,
    {
        async fn handle(&mut self, _item: u64, _ctx: &mut C) {}
    }

    #[tokio::test]
    async fn handler_full_lifecycle() {
        let mut h = CollectHandler {
            items: vec![],
            started: false,
            finished: false,
            err: None,
        };
        let mut ctx = NullCtx;
        h.started(&mut ctx).await;
        h.handle(1, &mut ctx).await;
        h.handle(2, &mut ctx).await;
        h.finished(&mut ctx).await;
        assert!(h.started && h.finished);
        assert_eq!(h.items, vec![1, 2]);
        assert!(h.err.is_none());
    }

    #[tokio::test]
    async fn handler_default_lifecycle_hooks_are_noops() {
        let mut h = DefaultHooksHandler;
        let mut ctx = NullCtx;
        // 默认 started/finished/handle_error 均为空实现，不 panic 即通过
        StreamHandler::<futures::stream::Iter<std::vec::IntoIter<u64>>, NullCtx>::started(
            &mut h, &mut ctx,
        )
        .await;
        StreamHandler::<futures::stream::Iter<std::vec::IntoIter<u64>>, NullCtx>::finished(
            &mut h, &mut ctx,
        )
        .await;
        StreamHandler::<futures::stream::Iter<std::vec::IntoIter<u64>>, NullCtx>::handle_error(
            &mut h,
            ActorError::Timeout,
            &mut ctx,
        )
        .await;
    }

    #[tokio::test]
    async fn handler_receives_error() {
        let mut h = CollectHandler {
            items: vec![],
            started: false,
            finished: false,
            err: None,
        };
        let mut ctx = NullCtx;
        h.handle_error(ActorError::Panic("x".into()), &mut ctx)
            .await;
        assert_eq!(h.err.as_deref(), Some("Panic: x"));
    }

    // ---------------- StreamRegistry（erased + typed ext） ----------------

    struct MemRegistry {
        streams: Vec<Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>>,
        handlers: Vec<Box<dyn Any + Send>>,
    }

    impl StreamRegistry for MemRegistry {
        fn add_stream_erased(
            &mut self,
            stream: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>,
        ) -> Result<(), ActorError> {
            self.streams.push(stream);
            Ok(())
        }
        fn add_stream_with_handler_erased(
            &mut self,
            stream: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>,
            handler: Box<dyn Any + Send>,
        ) -> Result<(), ActorError> {
            self.streams.push(stream);
            self.handlers.push(handler);
            Ok(())
        }
    }

    #[tokio::test]
    async fn registry_ext_wraps_typed_stream() {
        let mut reg = MemRegistry {
            streams: vec![],
            handlers: vec![],
        };
        // add_stream：typed → erased 装箱
        reg.add_stream(stream::iter(vec![1u64, 2, 3])).unwrap();
        assert_eq!(reg.streams.len(), 1);
        // 可从 erased 流中拉出装箱 item 并 downcast 回类型（into_pin 轮询）
        let s2 = reg.streams.remove(0);
        let mut pinned = Box::into_pin(s2);
        let item = pinned.next().await;
        let item = match item {
            Some(i) => i,
            None => panic!("stream should have item"),
        };
        let v = item.downcast::<u64>().unwrap();
        assert_eq!(*v, 1);

        // add_stream_with_handler：handler 也装箱（具体化 S/C 类型）
        reg.add_stream_with_handler::<_, _, NullCtx>(stream::iter(vec![9u64]), DefaultHooksHandler)
            .unwrap();
        assert_eq!(reg.streams.len(), 1);
        assert_eq!(reg.handlers.len(), 1);
    }

    #[tokio::test]
    async fn registry_erased_api_direct() {
        let mut reg = MemRegistry {
            streams: vec![],
            handlers: vec![],
        };
        let boxed: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send> =
            Box::new(stream::iter(vec![Box::new(5u8) as Box<dyn Any + Send>]));
        reg.add_stream_erased(boxed).unwrap();
        assert_eq!(reg.streams.len(), 1);
        reg.add_stream_with_handler_erased(
            Box::new(stream::iter(vec![Box::new(6u8) as Box<dyn Any + Send>])),
            Box::new(7u32),
        )
        .unwrap();
        assert_eq!(reg.handlers.len(), 1);
        // handler 的 Any 可 downcast
        let h = reg.handlers.remove(0);
        assert_eq!(*h.downcast::<u32>().unwrap(), 7);
    }

    #[tokio::test]
    async fn actor_stream_handler_delegates_items_and_all_hook_errors() {
        type Source = futures::stream::Iter<std::vec::IntoIter<u64>>;
        let mut handler = ActorStreamHandler::new(RecActor {
            fail_hooks: true,
            ..Default::default()
        });
        let mut ctx = NullCtx;
        StreamHandler::<Source, NullCtx>::started(&mut handler, &mut ctx).await;
        assert!(handler.actor.started);
        assert!(handler
            .actor
            .errored
            .as_ref()
            .unwrap()
            .contains("start failure"));
        StreamHandler::<Source, NullCtx>::handle(&mut handler, 7, &mut ctx).await;
        StreamHandler::<Source, NullCtx>::handle(&mut handler, 0, &mut ctx).await;
        assert_eq!(handler.actor.items, vec![7]);
        assert!(handler
            .actor
            .errored
            .as_ref()
            .unwrap()
            .contains("zero item"));
        StreamHandler::<Source, NullCtx>::finished(&mut handler, &mut ctx).await;
        assert!(handler.actor.finished);
        assert!(handler
            .actor
            .errored
            .as_ref()
            .unwrap()
            .contains("finish failure"));
    }
}
