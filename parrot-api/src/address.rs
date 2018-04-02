//! # Actor Addressing and Reference Management
//! 
//! This module provides the addressing and reference management system for the Parrot actor framework.
//! It defines how actors can be uniquely identified, located, and communicated with in a distributed system.
//!
//! ## Design Philosophy
//!
//! The addressing system follows these principles:
//! - Location Transparency: Actors can communicate without knowing each other's physical location
//! - Reference Safety: Strong and weak references prevent memory leaks and dangling references
//! - Path-based Addressing: Hierarchical naming scheme for actor identification
//! - Type-safe Communication: Generic traits ensure type safety in message passing
//!
//! ## Core Components
//!
//! - `ActorPath`: Unique identifier for actors in the system
//! - `ActorRef`: Interface for sending messages to actors
//! - `ActorRefExt`: Type-safe message sending extensions
//! - `WeakActorRef`: Non-owning reference to prevent reference cycles
//!
//! ## Usage Example
//!
//! ```rust
//! use parrot_api::address::{ActorRef, ActorRefExt};
//! use parrot_api::message::Message;
//!
//! // Define a message
//! #[derive(Message)]
//! struct Ping(String);
//!
//! async fn example(actor: impl ActorRef) {
//!     // Type-safe message sending
//!     let response = actor.ask(Ping("Hello".to_string())).await;
//!     
//!     // Fire-and-forget message
//!     actor.tell(Ping("Notification".to_string()));
//! }
//! ```

use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use crate::errors::ActorError;
use crate::types::{BoxedActorRef, BoxedMessage, ActorResult, BoxedFuture, WeakActorTarget};
use crate::message::{Message, MessageEnvelope};
use std::sync::Arc;

/// Unique identifier and location information for an actor in the system.
///
/// `ActorPath` combines:
/// - A weak reference to the actual actor target
/// - A string path representing the actor's location in the hierarchy
///
/// The path format follows a URI-like structure:
/// `protocol://system/user/child1/child2`
///
/// # Examples
///
/// ```rust
/// use parrot_api::address::ActorPath;
///
/// let path = ActorPath {
///     target: actor_target,
///     path: "local://system1/user/worker1".to_string()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct ActorPath {
    /// Weak reference to the actual actor implementation
    pub target: WeakActorTarget,
    
    /// String representation of the actor's path in the system
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

/// Core interface for interacting with actors through message passing.
///
/// `ActorRef` provides the fundamental operations for:
/// - Sending messages to actors
/// - Managing actor lifecycle
/// - Querying actor status
///
/// This trait is the primary way to interact with actors in the system,
/// providing location transparency and type safety.
///
/// # Thread Safety
///
/// All methods are thread-safe and can be called from multiple threads.
///
/// # Implementation Notes
///
/// Implementors must ensure that:
/// - Message sending is asynchronous
/// - Actor lifecycle operations are atomic
/// - Path information is immutable
pub trait ActorRef: Send + Sync + Debug {
    /// Sends a message to the actor and waits for a response.
    ///
    /// This is the core message passing method that:
    /// - Delivers the message to the actor
    /// - Waits for processing
    /// - Returns the response
    ///
    /// # Parameters
    /// * `msg` - The message envelope containing the message and metadata
    ///
    /// # Returns
    /// A future that resolves to the actor's response or an error
    fn send<'a>(&'a self, msg: MessageEnvelope) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Initiates graceful shutdown of the actor.
    ///
    /// The shutdown process:
    /// 1. Stops accepting new messages
    /// 2. Processes remaining messages in the mailbox
    /// 3. Calls cleanup handlers
    /// 4. Terminates the actor
    ///
    /// # Returns
    /// A future that completes when the actor has stopped
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>>;

    /// Returns the string representation of the actor's path.
    ///
    /// The path uniquely identifies the actor in the system hierarchy.
    fn path(&self) -> String;

    /// Checks if the actor is still alive and processing messages.
    ///
    /// # Returns
    /// A future that resolves to:
    /// - `true` if the actor is alive
    /// - `false` if the actor has terminated
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool>;

    /// Creates a new boxed reference to this actor.
    ///
    /// Used internally for type erasure and dynamic dispatch.
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
        let envelope = MessageEnvelope::new(msg, None, None);
        let this = self.clone_boxed();
        Box::pin(async move {
            let result = this.send(envelope).await?;
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
        let envelope = MessageEnvelope::new(msg, None, None);
        let actor_ref = self.clone_boxed();
        tokio::spawn(async move {
            let _ = actor_ref.send(envelope).await;
        });
    }
}

// Implement ActorRefExt for all types that implement ActorRef
impl<T: ActorRef + ?Sized> ActorRefExt for T {}

/// Non-owning reference to an actor that doesn't prevent garbage collection.
///
/// `WeakActorRef` is used to:
/// - Break reference cycles in actor hierarchies
/// - Create temporary references
/// - Implement supervision without memory leaks
///
/// # Examples
///
/// ```rust
/// use parrot_api::address::WeakActorRef;
///
/// async fn example(weak_ref: WeakActorRef) {
///     if let Some(strong_ref) = weak_ref.upgrade().await {
///         // Use the strong reference
///     }
/// }
/// ```
#[derive(Clone, Debug)]
pub struct WeakActorRef {
    /// The path identifying the referenced actor
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