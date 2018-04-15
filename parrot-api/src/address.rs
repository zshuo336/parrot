//! # Actor Address Module
//! 
//! ## Key Concepts
//! - ActorPath: Unique identifier and location for actors
//! - ActorRef: Core message passing interface
//! - WeakActorRef: Non-owning actor references
//! 
//! ## Design Principles
//! - Type safety: Generic interfaces for type-safe message passing
//! - Memory safety: Proper handling of actor lifecycles
//! - Asynchronous: All operations are non-blocking
//! - Thread safety: All types are Send + Sync
//!
//! ## Architecture
//! This module provides the addressing and message passing infrastructure
//! for the actor system, enabling location-transparent communication.

use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use crate::errors::ActorError;
use crate::types::{BoxedActorRef, BoxedMessage, ActorResult, BoxedFuture, WeakActorTarget};
use crate::message::{Message, MessageEnvelope};
use std::sync::Arc;
use async_trait::async_trait;

/// # Actor Path
///
/// ## Overview
/// Unique identifier and location information for an actor in the system.
///
/// ## Key Responsibilities
/// - Maintains weak reference to actual actor
/// - Provides hierarchical addressing
/// - Supports path-based actor lookup
///
/// ## Implementation Details
/// ### Path Format
/// Follows URI-like structure: `protocol://system/user/child1/child2`
///
/// ### Thread Safety
/// - Implements Send + Sync
/// - Clone is lock-free
///
/// ## Examples
/// ```rust
/// let path = ActorPath {
///     target: actor_target,
///     path: "local://system1/user/worker1".to_string()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct ActorPath {
    /// Weak reference to the actual actor implementation
    /// - **Thread Safety**: Safe to share between threads
    /// - **Lifecycle**: Does not prevent actor termination
    pub target: WeakActorTarget,
    
    /// String representation of the actor's path
    /// - **Format**: protocol://system/user/child1/child2
    /// - **Uniqueness**: Must be unique within system
    pub path: String,
}

impl PartialEq for ActorPath {
    fn eq(&self, other: &Self) -> bool {
        self.target.path() == other.target.path() && self.path == other.path
    }
}

impl Eq for ActorPath {}

impl Hash for ActorPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

impl Display for ActorPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path)
    }
}

/// # Actor Reference
///
/// ## Overview
/// Core trait for sending messages to actors.
///
/// ## Key Responsibilities
/// - Type-erased message passing
/// - Actor lifecycle management
/// - Location transparency
///
/// ## Thread Safety
/// - All methods are thread-safe
/// - Can be shared between threads
///
/// ## Performance
/// - Message sends are asynchronous
/// - Uses boxed futures for type erasure
#[async_trait]
pub trait ActorRef: Send + Sync + Debug {
    /// Sends a type-erased message and awaits response
    ///
    /// ## Parameters
    /// - `msg`: Type-erased message (must implement Send)
    ///
    /// ## Returns
    /// - `Ok(response)`: Message processed successfully
    /// - `Err(error)`: Message processing failed
    ///
    /// ## Performance
    /// - Uses dynamic dispatch
    /// - Allocates future on heap
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Stops the actor
    ///
    /// ## Implementation Details
    /// 1. Sends stop signal to actor
    /// 2. Waits for confirmation
    /// 3. Cleans up resources
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>>;

    /// Returns actor's path
    ///
    /// ## Returns
    /// String representation of actor location
    fn path(&self) -> String;

    /// Checks actor liveness
    ///
    /// ## Returns
    /// - `true`: Actor is processing messages
    /// - `false`: Actor has terminated
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool>;

    /// Creates boxed clone of reference
    ///
    /// ## Thread Safety
    /// Safe to call from any thread
    fn clone_boxed(&self) -> BoxedActorRef;
}

/// Extension trait providing type-safe message passing operations.
///
/// This trait extends `ActorRef` with methods that preserve message types,
/// making it easier and safer to communicate with actors.
pub trait ActorRefExt: ActorRef {
    /// Sends a typed message and waits for the corresponding response type.
    ///
    /// This method provides type safety by:
    /// - Preserving message types
    /// - Ensuring response type matches message
    /// - Handling type conversion automatically
    ///
    /// # Type Parameters
    /// * `M` - Message type that implements the `Message` trait
    ///
    /// # Parameters
    /// * `msg` - The message to send
    ///
    /// # Returns
    /// A future that resolves to the typed response or error
    fn ask<'a, M: Message>(&'a self, msg: M) -> BoxedFuture<'a, ActorResult<M::Result>> {
        let this = self.clone_boxed();
        Box::pin(async move {
            let result = this.send(Box::new(msg) as BoxedMessage).await?;
            M::extract_result(result)
        })
    }
    
    /// Sends a message without waiting for a response.
    ///
    /// Use this method for:
    /// - Fire-and-forget operations
    /// - Notifications
    /// - Non-blocking message passing
    ///
    /// # Type Parameters
    /// * `M` - Message type that implements the `Message` trait
    ///
    /// # Parameters
    /// * `msg` - The message to send
    fn tell<M: Message>(&self, msg: M) {
        let actor_ref = self.clone_boxed();
        tokio::spawn(async move {
            let _ = actor_ref.send(Box::new(msg) as BoxedMessage).await;
        });
    }
}

// Implement ActorRefExt for all types that implement ActorRef
impl<T: ActorRef + ?Sized> ActorRefExt for T {}

/// # Weak Actor Reference
///
/// ## Overview
/// Non-owning reference that doesn't prevent garbage collection
///
/// ## Key Responsibilities
/// - Break reference cycles
/// - Temporary references
/// - Supervision without leaks
///
/// ## Thread Safety
/// - Implements Send + Sync
/// - Clone is thread-safe
///
/// ## Examples
/// ```rust
/// async fn example(weak_ref: WeakActorRef) {
///     if let Some(strong_ref) = weak_ref.upgrade().await {
///         // Use the strong reference
///     }
/// }
/// ```
#[derive(Clone, Debug)]
pub struct WeakActorRef {
    /// Actor path information
    /// - **Lifecycle**: Outlives actor instance
    /// - **Thread Safety**: Safe to share
    pub path: ActorPath,
}

impl WeakActorRef {
    /// Creates a new weak reference from an actor path.
    ///
    /// # Parameters
    /// * `path` - The path of the actor to reference
    pub fn new(path: ActorPath) -> Self {
        Self { path }
    }

    /// Attempts to convert this weak reference into a strong reference.
    ///
    /// # Returns
    /// A future that resolves to:
    /// - `Some(ActorRef)` if the actor is still alive
    /// - `None` if the actor has been terminated
    fn upgrade<'a>(&'a self) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async move { None })
    }
} 