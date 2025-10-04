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
    fn actor_path_placeholder_never_upgrades() {
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
        fn send<'a>(
            &'a self,
            msg: BoxedMessage,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
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
    async fn actor_ref_ext_tell_is_fire_and_forget() {
        let p = PingPong;
        // tell 内部 tokio::spawn，不 panic 即通过；短暂让出确保任务执行
        p.tell(PingMsg);
        tokio::time::sleep(Duration::from_millis(10)).await;
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
