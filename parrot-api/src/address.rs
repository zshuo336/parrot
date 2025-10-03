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

use crate::message::Message;
use crate::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage, WeakActorTarget};
use async_trait::async_trait;
use std::any::Any;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

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
/// use parrot_api::address::ActorPath;
///
/// // Placeholder targets are weak refs to dropped refs; typical actors
/// // construct their real path from a live target.
/// let path = ActorPath::placeholder("local://system1/user/worker1");
/// assert_eq!(path.path, "local://system1/user/worker1");
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

impl ActorPath {
    pub fn new(target: WeakActorTarget, path: String) -> Self {
        Self { target, path }
    }

    /// Creates a path with a dead placeholder target (never upgrades).
    ///
    /// Useful for tests and for bootstrap phases where the real target
    /// reference is created after the path. The placeholder is a weak
    /// reference to a dropped dead ref, so `upgrade` always yields `None`.
    pub fn placeholder(path: impl Into<String>) -> Self {
        Self {
            target: Arc::new(DeadTargetRef) as WeakActorTarget,
            path: path.into(),
        }
    }

    /// Test-only alias of [`ActorPath::placeholder`].
    pub fn for_test(path: &str) -> Self {
        Self::placeholder(path.to_string())
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn target(&self) -> &WeakActorTarget {
        &self.target
    }
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
    /// Sends a type-erased message and awaits the actor's response.
    ///
    /// **Unified semantics (2026-10-02, per stress report §9)**: `send` is an
    /// *unbounded ask* — the future resolves when the actor processes the
    /// message and produces a result. It never applies an implicit engine
    /// default timeout; use [`ActorRef::send_with_timeout`] to bound the
    /// wait. Fire-and-forget delivery is [`ActorRef::deliver`].
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

    /// Sends a type-erased message and awaits response, with an optional
    /// timeout.
    ///
    /// **Unified semantics**: `timeout == None` behaves exactly like
    /// [`ActorRef::send`] (unbounded ask). `Some(d)` bounds the wait; on
    /// expiry the *caller* gives up — the message stays enqueued and the
    /// actor may still process it later.
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Fire-and-forget delivery: enqueues the message and returns
    /// immediately after the mailbox accepts it.
    ///
    /// **Unified semantics**: `deliver` never waits for the actor to process
    /// the message. It resolves as soon as the mailbox push completes
    /// (subject to the engine's backpressure strategy), returning a unit
    /// receipt. This is the explicit tell path; [`ActorRefExt::tell`] is
    /// built on it.
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>>;

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

    /// Returns a reference to the actor as a `dyn Any`
    fn as_any(&self) -> &dyn Any;

    /// Compares two actor references for equality
    ///
    /// ## Returns
    /// - `true`: References point to same actor
    /// - `false`: References point to different actors
    fn eq(&self, other: &dyn ActorRef) -> bool {
        self.path() == other.path()
    }

    fn eq_path(&self, path: &str) -> bool {
        self.path() == path
    }
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
            let _ = actor_ref.deliver(Box::new(msg) as BoxedMessage).await;
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
/// use parrot_api::address::{ActorPath, WeakActorRef};
///
/// # async fn example() {
/// let weak_ref = WeakActorRef::new(ActorPath::placeholder("local://sys/user/a"));
/// // A weak ref carries the path even after the actor is gone.
/// assert_eq!(weak_ref.path.path, "local://sys/user/a");
/// # }
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

    // M6 清理：`upgrade`（恒返回 None 的占位）从未被调用，删除。
    // 远程/集群方向的弱引用重建走 ActorPath 解析（TECH_DESIGN_04）。
}
/// Dead actor ref used as a placeholder target in [`ActorPath::placeholder`].
///
/// Never alive; `send`/`stop` return `NotFound`-style errors.
#[derive(Debug, Clone)]
pub(crate) struct DeadTargetRef;

#[async_trait]
impl ActorRef for DeadTargetRef {
    fn send<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Err(crate::errors::ActorError::ActorNotFound(
                "dead://placeholder".to_string(),
            ))
        })
    }

    fn send_with_timeout<'a>(
        &'a self,
        _msg: BoxedMessage,
        _timeout_duration: Option<std::time::Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Err(crate::errors::ActorError::ActorNotFound(
                "dead://placeholder".to_string(),
            ))
        })
    }

    fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            Err(crate::errors::ActorError::ActorNotFound(
                "dead://placeholder".to_string(),
            ))
        })
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            Err(crate::errors::ActorError::ActorNotFound(
                "dead://placeholder".to_string(),
            ))
        })
    }

    fn path(&self) -> String {
        "dead://placeholder".to_string()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move { false })
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
