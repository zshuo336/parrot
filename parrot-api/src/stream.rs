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
//! ```rust
//! use parrot_api::stream::{StreamHandler, StreamRegistry};
//! use futures::stream::Stream;
//!
//! struct MyStreamHandler;
//!
//! #[async_trait]
//! impl<S: Stream> StreamHandler<S, MyContext> for MyStreamHandler
//! where
//!     S::Item: Send + 'static,
//! {
//!     async fn handle(&mut self, item: S::Item, ctx: &mut MyContext) {
//!         // Process stream item
//!     }
//!
//!     async fn finished(&mut self, ctx: &mut MyContext) {
//!         // Handle stream completion
//!     }
//! }
//!
//! // Register stream with actor
//! actor.context.stream_registry().add_stream_with_handler(
//!     my_stream,
//!     MyStreamHandler::new()
//! )?;
//! ```

use std::any::Any;
use futures::{Stream, StreamExt};
use async_trait::async_trait;
use crate::errors::ActorError;
use crate::actor::Actor;
use crate::context::ActorContext;

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
    /// * `handler` - Boxed custom stream handler
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
    /// * `C` - Context type
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
        C: ?Sized + Send + 'static,
    {
        let stream = Box::new(stream.map(|item| Box::new(item) as Box<dyn Any + Send>));
        self.add_stream_with_handler_erased(stream, Box::new(handler))
    }
}

impl<T: StreamRegistry + ?Sized> StreamRegistryExt for T {}

/// Internal message type for stream processing.
///
/// Used to communicate stream events between the stream
/// processor and the actor system.
#[derive(Debug)]
pub(crate) enum StreamMessage<I> {
    /// New item received from stream
    Item(I),
    /// Error occurred during processing
    Error(ActorError),
    /// Stream has completed
    Completed,
}

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
        self.actor.stream_started(ctx).await.ok();
    }

    async fn finished(&mut self, ctx: &mut A::Context) {
        self.actor.stream_finished(ctx).await.ok();
    }

    async fn handle_error(&mut self, err: ActorError, ctx: &mut A::Context) {
        self.actor.stream_error(err, ctx).await.ok();
    }
} 