//! # Actor Address Module
//!
//! ## Key Concepts
//! - ActorPath: Unique identifier and location for actors
//! - ActorRef: Core message passing interface
//! - WeakActorRef: Legacy path wrapper retaining a shared target handle
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

use crate::message::{Message, MessageOptions};
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
/// - Retains a shared actor-reference handle
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
/// // Placeholder targets are inert handles; managed actors construct their
/// // real path from an engine-provided reference.
/// let path = ActorPath::placeholder("local://system1/user/worker1");
/// assert_eq!(path.path, "local://system1/user/worker1");
/// ```
#[derive(Debug, Clone)]
pub struct ActorPath {
    /// Shared reference handle (`Arc<dyn ActorRef>`), despite the historical
    /// `WeakActorTarget` alias. Cloning the path retains this handle and any
    /// resources it owns. Actor termination follows the engine's lifecycle;
    /// reference retention alone does not establish instance liveness.
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

    /// Creates a path with an inert target that rejects message delivery.
    ///
    /// Useful for tests and bootstrap metadata. The target is a shared
    /// `DeadTargetRef`, not a weak pointer; it has no `upgrade` operation and
    /// does not resolve automatically when an actor with this path is created.
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
    /// [`ActorRef::send`] (unbounded ask). `Some(d)` bounds the whole exchange,
    /// including waiting for mailbox capacity. Expiry cancels the caller's wait;
    /// it does not roll back business processing. An already accepted message
    /// may still be processed, depending on the engine's cancellation semantics.
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Sends a type-erased message while preserving its delivery metadata.
    ///
    /// Timeout bounds admission and response waiting, without undoing accepted
    /// work. The default adapter enforces timeout and rejects unsupported high
    /// priority and retry options before sending. Override to provide priority.
    /// Retry requires an explicit replayable typed request; see `ask_retrying`.
    fn send_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let mut msg = crate::owned_input::Input::new(msg);
        Box::pin(crate::trace::capture(async move {
            let result = async {
                options.validate_single_attempt()?;
                options.validate_fifo()?;
                let delegate = crate::owned_input::call(|| {
                    self.send_with_timeout(msg.take(), options.timeout)
                })?;
                crate::owned_input::resolve(delegate).await
            }
            .await;
            msg.finish(result)
        }))
    }

    /// Fire-and-forget delivery: enqueues the message and returns
    /// immediately after the mailbox accepts it.
    ///
    /// **Unified semantics**: `deliver` never waits for the actor to process
    /// the message. It resolves as soon as the mailbox push completes
    /// (subject to the engine's backpressure strategy), returning a unit
    /// receipt. This is the explicit tell path; [`ActorRefExt::tell`] is
    /// built on it.
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>>;

    /// Fire-and-forget delivery with preserved message metadata.
    fn deliver_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let mut msg = crate::owned_input::Input::new(msg);
        Box::pin(crate::trace::capture(async move {
            let result = async {
                options.validate_single_attempt()?;
                options.validate_fifo()?;
                let delegate = crate::owned_input::resolve(crate::owned_input::call(|| {
                    self.deliver(msg.take())
                })?);
                match options.timeout {
                    Some(duration) => tokio::time::timeout(duration, delegate)
                        .await
                        .map_err(|_| crate::errors::ActorError::Timeout)?,
                    None => delegate.await,
                }
            }
            .await;
            msg.finish(result)
        }))
    }

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

    /// Observes actor liveness/admission according to the reference's engine.
    ///
    /// This observation can change before the next operation. In managed local
    /// engines, `false` may mean the mailbox is closed while owned cleanup is
    /// still running. It is not a retirement receipt. Await `stop()` to join
    /// cleanup and inspect its result; remote reachability is not termination.
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool>;

    /// Creates boxed clone of reference
    ///
    /// ## Thread Safety
    /// Safe to call from any thread
    fn clone_boxed(&self) -> BoxedActorRef;

    /// Returns a reference to the actor as a `dyn Any`
    fn as_any(&self) -> &dyn Any;

    /// Generation-specific lifecycle capability. Raw or unsupported references
    /// return None; engines must expose the same handle through every clone.
    fn death_watch(&self) -> Option<crate::death_watch::DeathWatch> {
        None
    }

    /// Register a managed watcher. Closed generations return Stopped; repeated
    /// registrations coalesce. Notification delivery failures are observable
    /// through the target handle. This does not imply durable delivery.
    ///
    /// The default adapter takes ownership immediately but invokes capability
    /// callbacks only when polled. Callback and cleanup panics become errors;
    /// cancellation also isolates watcher cleanup. An error from final cleanup
    /// does not roll back a registration that already succeeded. Custom
    /// implementations are responsible for their own cancellation guarantees.
    fn watch<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        crate::watch_input::change(self, watcher, false)
    }

    /// Cancel a watch; an already admitted notification cannot be recalled.
    /// The default adapter has the same input ownership and panic boundaries as
    /// [`ActorRef::watch`]; a cleanup error does not restore a removed watch.
    fn unwatch<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        crate::watch_input::change(self, watcher, true)
    }

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
        let mut msg = crate::owned_input::Input::new(msg);
        // Preserve eager clone/metadata evaluation, but keep both owned inputs
        // outside the borrowed callback unwind boundary.
        let prepared = crate::owned_input::call(|| self.clone_boxed())
            .map(crate::owned_input::Input::new)
            .and_then(|this| {
                crate::owned_input::call(|| MessageOptions::for_message(msg.value()))
                    .map(|options| (this, options))
            });
        Box::pin(crate::trace::capture(async move {
            let result = match prepared {
                Ok((this, options)) => {
                    let result = async {
                        let delegate = crate::owned_input::call(|| {
                            this.value()
                                .send_with_options(Box::new(msg.take()) as BoxedMessage, options)
                        })?;
                        let result = crate::owned_input::resolve(delegate).await?;
                        crate::owned_input::call(|| M::extract_result(result))?
                    }
                    .await;
                    this.finish(result)
                }
                Err(error) => Err(error),
            };
            msg.finish(result)
        }))
    }

    /// Ask with explicit replay consent. `MessageOptions::retry_policy` sets the
    /// maximum number of *retries*, in addition to the initial attempt. Each
    /// attempt gets its own timeout; delays use fixed, linear or capped
    /// exponential backoff. The predicate decides which transport/handler errors
    /// may be replayed. It is never called for response decoding errors.
    ///
    /// Requires a Clone payload and a Tokio runtime. Cancellation prevents new
    /// attempts, but previously admitted work may still run. A retry may repeat
    /// side effects: callers must supply idempotency or explicitly accept that
    /// risk. Actor supervision still applies to each failed attempt.
    fn ask_retrying<'a, M, F>(
        &'a self,
        msg: M,
        retry_if: F,
    ) -> BoxedFuture<'a, ActorResult<M::Result>>
    where
        M: Message + Clone + Sync,
        F: Fn(&crate::errors::ActorError) -> bool + Send + 'a,
    {
        let msg = crate::owned_input::Input::new(msg);
        let mut retry_if = crate::owned_input::Input::new(retry_if);
        Box::pin(crate::trace::capture(async move {
            let result = async {
                let mut options =
                    crate::owned_input::call(|| MessageOptions::for_message(msg.value()))?;
                let policy = options.retry_policy.take();
                if let Some(policy) = &policy {
                    tokio::runtime::Handle::try_current().map_err(|_| {
                        crate::errors::ActorError::InternalError(
                            "ask_retrying requires a Tokio runtime".into(),
                        )
                    })?;
                    // Validate before any business side effect, including a base on
                    // a zero-budget policy and clock overflow on the longest delay.
                    policy.delay(1)?;
                    if policy.max_attempts > 0
                        && tokio::time::Instant::now()
                            .checked_add(policy.delay(policy.max_attempts)?)
                            .is_none()
                    {
                        return Err(crate::errors::ActorError::MessageHandlingError(
                            "retry delay exceeds clock range".into(),
                        ));
                    }
                }
                let mut retries = 0;
                loop {
                    let attempt = crate::owned_input::call(|| msg.value().clone())?;
                    let result = match crate::owned_input::call(|| {
                        self.send_with_options(Box::new(attempt), options.clone())
                    }) {
                        Ok(delegate) => crate::owned_input::resolve(delegate).await,
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(reply) => return crate::owned_input::call(|| M::extract_result(reply))?,
                        Err(error) => {
                            let mut error = crate::owned_input::Input::new(error);
                            let Some(policy) = &policy else {
                                return Err(error.take());
                            };
                            if retries >= policy.max_attempts
                                || !crate::owned_input::call(|| {
                                    retry_if.value_mut()(error.value())
                                })?
                            {
                                return Err(error.take());
                            }
                            error.retire()?;
                            retries += 1;
                            tokio::time::sleep(policy.delay(retries)?).await;
                        }
                    }
                }
            }
            .await;
            let result = msg.finish(result);
            retry_if.finish(result)
        }))
    }

    /// Delivers a typed message and awaits admission, not a business response.
    ///
    /// The returned future must be polled. It uses [`ActorRef::deliver_with_options`]
    /// without spawning a task. A declared timeout requires a Tokio timer. The
    /// underlying reference still determines executor requirements and admission
    /// policy: a full mailbox may wait or return an error. For a remote reference,
    /// success means admission to the local transport queue, not remote receipt.
    ///
    /// Sequential `tell(message).await?` calls preserve admission order through
    /// this adapter. Concurrent futures have no call-order guarantee; mailbox
    /// priority and remote routing retain their own ordering contracts.
    /// Dropping an unpolled future does not deliver the message. Cancellation
    /// while waiting follows `deliver`; it cannot undo an accepted message.
    #[must_use = "tell must be awaited to deliver the message and observe admission errors"]
    fn tell<'a, M: Message>(&'a self, msg: M) -> BoxedFuture<'a, ActorResult<()>> {
        let mut msg = crate::owned_input::Input::new(msg);
        let options = crate::owned_input::call(|| MessageOptions::for_message(msg.value()));
        Box::pin(crate::trace::capture(async move {
            let result = async {
                let options = options?;
                let delegate = crate::owned_input::call(|| {
                    self.deliver_with_options(Box::new(msg.take()) as BoxedMessage, options)
                })?;
                crate::owned_input::resolve(delegate).await
            }
            .await;
            msg.finish(result)
        }))
    }
}

// Implement ActorRefExt for all types that implement ActorRef
impl<T: ActorRef + ?Sized> ActorRefExt for T {}

/// Legacy path wrapper retaining an [`ActorPath`] and its shared target handle.
///
/// Despite its historical name, this is not a `std::sync::Weak` pointer and
/// does not break reference cycles. Cloning it clones the path's `Arc` target.
/// There is no upgrade or automatic lookup operation. Use `Arc::downgrade`
/// on an actual shared reference when weak ownership is required; resolve
/// actor paths through the owning system when a fresh reference is needed.
///
/// ```rust
/// use parrot_api::address::{ActorPath, WeakActorRef};
///
/// let reference = WeakActorRef::new(ActorPath::placeholder("local://sys/user/a"));
/// assert_eq!(reference.path.path, "local://sys/user/a");
/// ```
#[derive(Clone, Debug)]
pub struct WeakActorRef {
    /// Actor path information
    /// - **Lifecycle**: Outlives actor instance
    /// - **Thread Safety**: Safe to share
    pub path: ActorPath,
}

impl WeakActorRef {
    /// Wraps an actor path without changing its target's ownership.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ActorError;
    use crate::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};

    // ---------------- ActorPath ----------------

    #[test]
    fn actor_path_new_and_accessors() {
        let p = ActorPath::placeholder("local://s/user/a");
        let p2 = ActorPath::new(p.target.clone(), "local://s/user/b".into());
        assert_eq!(p2.path(), "local://s/user/b");
        assert_eq!(p2.path, "local://s/user/b");
        // target 访问器
        assert_eq!(p2.target().path(), "dead://placeholder");
    }

    #[test]
    fn actor_path_placeholder_uses_inert_target() {
        let p = ActorPath::placeholder("x://y");
        assert_eq!(p.target.path(), "dead://placeholder");
    }

    #[test]
    fn actor_path_for_test_alias() {
        let p = ActorPath::for_test("/t/1");
        assert_eq!(p.path, "/t/1");
    }

    #[test]
    fn actor_path_equality_requires_same_target_and_path() {
        let a = ActorPath::placeholder("p://a");
        let b = ActorPath::placeholder("p://a");
        // 两个独立 placeholder 的 target 是不同 Arc 实例（均指向死 target）
        // path 相同、target.path 相同 → 相等
        assert_eq!(a, b);
        let c = ActorPath::placeholder("p://c");
        assert_ne!(a, c);
    }

    #[test]
    fn actor_path_hash_consistent_with_eq() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ActorPath::placeholder("p://a"));
        // 相等路径（不同实例）hash 一致 → insert 去重
        let before = set.len();
        set.insert(ActorPath::placeholder("p://a"));
        assert_eq!(set.len(), before);
        set.insert(ActorPath::placeholder("p://b"));
        assert_eq!(set.len(), before + 1);
    }

    #[test]
    fn actor_path_display_is_path_string() {
        let p = ActorPath::placeholder("proto://sys/user/worker");
        assert_eq!(p.to_string(), "proto://sys/user/worker");
    }

    // ---------------- DeadTargetRef 语义（placeholder target） ----------------

    #[tokio::test]
    async fn dead_target_send_returns_not_found() {
        let p = ActorPath::placeholder("x://y");
        let t = p.target.clone();
        let r = t.send(Box::new(1u32) as BoxedMessage).await;
        match r {
            Err(ActorError::ActorNotFound(m)) => assert!(m.contains("dead://")),
            other => panic!("expected ActorNotFound, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn dead_target_send_with_timeout_none_and_some() {
        let t = ActorPath::placeholder("x://y").target.clone();
        // timeout=None（无界语义）也应同样失败
        assert!(t
            .send_with_timeout(Box::new(()) as BoxedMessage, None)
            .await
            .is_err());
        assert!(t
            .send_with_timeout(
                Box::new(()) as BoxedMessage,
                Some(Duration::from_millis(10))
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn dead_target_deliver_fails_and_stop_fails_and_not_alive() {
        let t = ActorPath::placeholder("x://y").target.clone();
        assert!(t.deliver(Box::new(()) as BoxedMessage).await.is_err());
        assert!(t.stop().await.is_err());
        assert!(!(t.is_alive().await));
    }

    #[test]
    fn dead_target_path_and_clone_boxed_and_as_any() {
        let t = ActorPath::placeholder("x://y").target.clone();
        assert_eq!(t.path(), "dead://placeholder");
        let c = t.clone_boxed();
        assert_eq!(c.path(), "dead://placeholder");
        // as_any 可用于 downcast 校验
        assert!(c.as_any().is::<DeadTargetRef>());
        // eq / eq_path 默认实现
        assert!(t.eq(c.as_ref()));
        assert!(t.eq_path("dead://placeholder"));
        assert!(!t.eq_path("other"));
    }

    // ---------------- ActorRefExt（默认实现语义） ----------------

    #[derive(Debug)]
    struct PingPong;

    #[derive(Debug)]
    struct PingMsg;
    impl crate::message::Message for PingMsg {
        type Result = u32;
        fn extract_result(r: BoxedMessage) -> ActorResult<u32> {
            r.downcast::<u32>()
                .map(|b| *b)
                .map_err(|_| ActorError::MessageHandlingError("type".into()))
        }
    }

    #[async_trait]
    impl ActorRef for PingPong {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if msg.downcast_ref::<PingMsg>().is_some() {
                    Ok(Box::new(7u32) as BoxedMessage)
                } else {
                    Err(ActorError::MessageHandlingError("unknown".into()))
                }
            })
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            t: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            // 模拟超时：t=Some(0) 即失败
            if t == Some(Duration::ZERO) {
                return Box::pin(async { Err(ActorError::Timeout) });
            }
            self.send(msg)
        }
        fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "test://pingpong".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(PingPong)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn actor_ref_ext_ask_roundtrip() {
        let p = PingPong;
        let v = p.ask(PingMsg).await.unwrap();
        assert_eq!(v, 7u32);
    }

    #[tokio::test]
    async fn actor_ref_ext_ask_type_mismatch_is_error() {
        struct WrongMsg;
        impl crate::message::Message for WrongMsg {
            type Result = String;
            fn extract_result(r: BoxedMessage) -> ActorResult<String> {
                r.downcast::<String>()
                    .map(|b| *b)
                    .map_err(|_| ActorError::MessageHandlingError("type".into()))
            }
        }
        let p = PingPong;
        // PingPong 对 PingMsg 回 u32；WrongMsg 的 extract 期待 String → 类型错
        let r = p.ask(WrongMsg).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn actor_ref_ext_tell_reports_admission() {
        let p = PingPong;
        p.tell(PingMsg).await.unwrap();
    }

    #[tokio::test]
    async fn actor_ref_eq_and_eq_path_defaults() {
        let a = PingPong;
        let b = PingPong;
        assert!(a.eq(&b));
        assert!(a.eq_path("test://pingpong"));
        assert!(!a.eq_path("test://other"));
    }

    // ---------------- WeakActorRef ----------------

    #[test]
    fn weak_actor_ref_carries_path() {
        let w = WeakActorRef::new(ActorPath::placeholder("local://sys/user/a"));
        assert_eq!(w.path.path, "local://sys/user/a");
        // Clone 保留 path
        let w2 = w.clone();
        assert_eq!(w2.path, w.path);
    }
}
