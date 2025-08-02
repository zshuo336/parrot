//! # Actor Core API
//! 
//! This module defines the core Actor trait and related components that form the foundation of the Parrot actor system.
//! The Actor trait provides the primary interface for implementing actors, defining their lifecycle, message handling,
//! and stream processing capabilities.
//!
//! ## Design Philosophy
//!
//! The Actor model in Parrot follows these key principles:
//! - Message-driven: All communication between actors is done through message passing
//! - Encapsulation: Actors maintain private state and can only be influenced through messages
//! - Supervision: Actors form a hierarchy where parent actors supervise their children
//! - Asynchronous: All operations are non-blocking by default
//!
//! ## Implementation Guide
//!
//! To implement an actor:
//! 1. Define your actor struct
//! 2. Implement the Actor trait
//! 3. Define message types
//! 4. Implement message handling logic
//!
//! ```rust
//! use parrot_api::actor::{Actor, ActorState};
//! use parrot_api::context::ActorContext;
//!
//! struct MyActor {
//!     state: ActorState,
//!     counter: u64,
//! }
//!
//! impl Actor for MyActor {
//!     type Config = ();
//!     type Context = ActorContext;
//!
//!     async fn receive_message(&mut self, msg: BoxedMessage, ctx: &mut Self::Context) 
//!         -> ActorResult<BoxedMessage> {
//!         // Handle messages here
//!         Ok(msg)
//!     }
//!
//!     fn state(&self) -> ActorState {
//!         self.state
//!     }
//! }
//! ```

use std::any::Any;
use std::future::Future;
use std::ptr::NonNull;
use crate::context::ActorContext;
use crate::address::ActorRef;
use crate::errors::ActorError;
use crate::types::{BoxedMessage, BoxedFuture, ActorResult, BoxedActorRef};
use crate::message::Message;
/// Actor lifecycle states that represent the current status of an actor in the system.
/// 
/// The state transitions typically follow this order:
/// 1. `Starting`: Initial state when actor is being created
/// 2. `Running`: Normal operation state
/// 3. `Stopping`: Actor is performing cleanup
/// 4. `Stopped`: Actor has terminated
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorState {
    /// Actor is initializing
    Starting,
    /// Actor is processing messages normally
    Running,
    /// Actor is performing cleanup before stopping
    Stopping,
    /// Actor has stopped and will not process more messages
    Stopped,
}

/// Default empty configuration for actors that don't need any configuration.
/// 
/// This type is provided as a convenience to avoid having to create an empty
/// configuration type for every actor.
#[derive(Debug, Default, Clone)]
pub struct EmptyConfig;

/// Configuration trait for actor initialization.
/// 
/// Implement this trait to define custom configuration parameters for your actor.
/// The configuration is passed to the actor during creation through the `ActorFactory`.
pub trait ActorConfig: Send + Sync + 'static {}

/// Implement ActorConfig for the EmptyConfig type
impl ActorConfig for EmptyConfig {}

/// Factory trait for creating actor instances.
/// 
/// This trait enables dependency injection and custom actor initialization.
/// Implementations should create and return a new actor instance with the given configuration.
/// 
/// # Type Parameters
/// 
/// * `A` - The actor type this factory creates
pub trait ActorFactory<A: Actor>: Send + 'static {
    /// Creates a new instance of the actor with the specified configuration
    fn create(&self, config: A::Config) -> A;
}

/// Core trait that defines an actor's behavior and lifecycle.
/// 
/// This trait is the foundation of the actor system, defining how actors:
/// - Process messages
/// - Handle streams
/// - Manage lifecycle events
/// - Interact with child actors
/// 
/// # Type Parameters
/// 
/// * `Config`: Configuration type for actor initialization
/// * `Context`: Context type providing actor system services
/// 
/// # Implementation Requirements
/// 
/// Implementors must define:
/// - Message handling logic in `receive_message`
/// - State management through `state`
/// 
/// Other methods have default implementations that can be overridden as needed.
pub trait Actor: Send + 'static {
    /// Configuration type for actor initialization
    type Config: ActorConfig;
    
    /// Context type providing access to actor system services
    type Context: ?Sized + Send;


    /// Initialize the actor with system resources and configuration.
    /// 
    /// Called once before the actor starts processing messages. Use this method to:
    /// - Set up initial state
    /// - Initialize resources
    /// - Register for system events
    /// 
    /// # Parameters
    /// * `ctx` - Mutable reference to actor context
    /// 
    /// # Returns
    /// `ActorResult<()>` indicating success or failure of initialization
    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Process an incoming message and produce a response.
    /// 
    /// This is the core message handling method that defines the actor's behavior.
    /// Implement this to:
    /// - Pattern match on message types
    /// - Update internal state
    /// - Produce responses
    /// - Interact with other actors
    /// 
    /// # Parameters
    /// * `msg` - Incoming message to process
    /// * `ctx` - Mutable reference to actor context
    /// 
    /// # Returns
    /// `ActorResult<BoxedMessage>` containing the response message or error
    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Process an incoming message and produce a response.
    /// 
    /// This is the core message handling method that defines the actor's behavior.
    /// Implement this to:
    /// - Pattern match on message types
    /// - Update internal state
    /// 
    /// # Parameters
    /// * `msg` - Incoming message to process
    /// * `ctx` - Mutable reference to actor context
    /// * `engine_ctx` - Mutable Pointer(`NonNull<dyn Any>`) to engine context like actix context etc.
    /// 
    /// # Returns
    /// `Option<ActorResult<BoxedMessage>>` containing the response message or error
    /// 
    /// # Note
    /// This method is only used on the engine side.
    /// 
    /// # Engine Context
    /// The engine context is a mutable pointer to the actix actor's context type.
    /// It is used to access the engine's resources and services.
    /// 
    /// # Context
    /// The context is a mutable reference to the parrot actor's context.
    /// It is used to access the actor's resources and services.
    fn receive_message_with_engine<'a>(&'a mut self, msg: BoxedMessage, ctx: &'a mut Self::Context, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>>;

    /// Handle an item from a stream.
    /// 
    /// Default implementation forwards to `receive_message`. Override to provide
    /// custom stream processing logic.
    /// 
    /// # Parameters
    /// * `item` - Stream item to process
    /// * `ctx` - Mutable reference to actor context
    /// 
    /// # Returns
    /// `ActorResult<BoxedMessage>` containing the processing result
    fn handle_stream<'a>(&'a mut self, item: BoxedMessage, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // Forward the result from receive_message
            self.receive_message(item, ctx).await
        })
    }

    /// Called when a stream starts.
    /// 
    /// Override to perform setup work for stream processing.
    /// 
    /// # Parameters
    /// * `ctx` - Mutable reference to actor context
    fn stream_started<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Called when a stream completes successfully.
    /// 
    /// Override to perform cleanup work after stream processing.
    /// 
    /// # Parameters
    /// * `ctx` - Mutable reference to actor context
    fn stream_finished<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Called when a stream encounters an error.
    /// 
    /// Override to handle stream processing errors.
    /// 
    /// # Parameters
    /// * `err` - The error that occurred
    /// * `ctx` - Mutable reference to actor context
    fn stream_error<'a>(&'a mut self, err: ActorError, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { Err(err) })
    }

    /// Perform cleanup before actor stops.
    /// 
    /// Override to:
    /// - Clean up resources
    /// - Save state
    /// - Notify other actors
    /// 
    /// # Parameters
    /// * `ctx` - Mutable reference to actor context
    fn before_stop<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Handle termination of a child actor.
    /// 
    /// Override to implement supervision strategies.
    /// 
    /// # Parameters
    /// * `child` - Reference to the terminated child actor
    /// * `ctx` - Mutable reference to actor context
    fn handle_child_terminated<'a>(&'a mut self, _child: BoxedActorRef, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Get the current state of the actor.
    /// 
    /// This method should return the actor's current lifecycle state.
    /// Used by the system for:
    /// - Supervision
    /// - Resource management
    /// - Message routing
    fn state(&self) -> ActorState;
} 
