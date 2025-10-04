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
//! use parrot_api::actor::{Actor, ActorState, EmptyConfig};
//! use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
//!
//! struct MyActor {
//!     counter: u64,
//! }
//!
//! impl Actor for MyActor {
//!     type Config = EmptyConfig;
//!     type Context = dyn parrot_api::context::ActorContext;
//!
//!     fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
//!         Box::pin(async { Ok(()) })
//!     }
//!
//!     fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context)
//!         -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
//!         // Handle messages here
//!         Box::pin(async move { Ok(msg) })
//!     }
//!
//!     fn state(&self) -> ActorState {
//!         ActorState::Running
//!     }
//! }
//! ```
//!

use crate::errors::ActorError;
use crate::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::any::Any;
use std::ptr::NonNull;
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

/// Safe wrapper around the raw engine context pointer passed to
/// [`Actor::receive_message_with_engine`] (ADR-2 mitigation).
///
/// # Overview
/// The engine (currently only the Actix adapter) constructs this handle
/// from the live native context pointer for the duration of the synchronous
/// dispatch call. The pointer is guaranteed by the engine to be:
///
/// - **dereferenceable** for the whole call (the native context outlives
///   the handler invocation), and
/// - **not written concurrently** (actor dispatch is serial within one
///   actor).
///
/// The only `unsafe` involved is a single [`EngineContextHandle::from_raw`]
/// call inside the engine adapter, carrying the contract documented on that
/// method. User code receives the handle and only uses the safe
/// `downcast_ref` / `downcast_mut` borrows — no raw pointers, no lifetime
/// escape.
///
/// # Usage
/// Replace raw-pointer boilerplate in engine handlers:
///
/// ```ignore
/// // before
/// let engine_ctx_ref = unsafe { engine_ctx.as_ref() };
/// if let Some(ctx) = engine_ctx_ref.downcast_ref::<MyEngineCtx>() { ... }
///
/// // after
/// if let Some(ctx) = engine_ctx.downcast_ref::<MyEngineCtx>() { ... }
/// ```
///
/// # Engine support
/// - **actix**: the handle wraps `&mut ActixActor<...>::Context` (the
///   native actix context). Downcasting to that type grants access to
///   timers, spawning, address book, and stream registration.
/// - **thread**: the engine never calls `receive_message_with_engine`, so
///   no handle is ever produced.
pub struct EngineContextHandle {
    ptr: NonNull<dyn Any>,
}

impl std::fmt::Debug for EngineContextHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineContextHandle")
            .finish_non_exhaustive()
    }
}

impl EngineContextHandle {
    /// Create a handle from a raw engine context pointer.
    ///
    /// This is the single `unsafe` boundary of the ADR-2 mitigation and is
    /// meant to be called once by the engine adapter, not by user code.
    ///
    /// # Safety
    /// The caller must uphold:
    /// 1. `ptr` is dereferenceable for the duration of the synchronous
    ///    `receive_message_with_engine` call this handle is passed into.
    /// 2. No other thread accesses the pointee mutably while the handle
    ///    is alive (actor dispatch is serial).
    /// 3. The handle must not outlive the call it was created for.
    pub unsafe fn from_raw(ptr: NonNull<dyn Any>) -> Self {
        Self { ptr }
    }

    /// Try to borrow the engine context as `&T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        // SAFETY: `from_raw` contract guarantees dereferenceability for the
        // dispatch call scope, within which this handle lives.
        unsafe { self.ptr.as_ref() }.downcast_ref::<T>()
    }

    /// Try to borrow the engine context as `&mut T`.
    ///
    /// The engine guarantees exclusive access during the synchronous
    /// dispatch, so a mutable borrow is sound as long as the handler does
    /// not stash the reference beyond the call.
    pub fn downcast_mut<T: Any>(&mut self) -> Option<&mut T> {
        // SAFETY: see `from_raw`; dispatch is exclusive within the actor.
        unsafe { self.ptr.as_mut() }.downcast_mut::<T>()
    }
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
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Whether this actor wants its messages dispatched through the async
    /// [`receive_message`](Self::receive_message) path.
    ///
    /// # Overview
    /// Engines consult this flag to select the dispatch path:
    ///
    /// - `false` (default): the Actix engine probes
    ///   `receive_message_with_engine` first (sync fast path). A `None`
    ///   return drops the message (legacy behavior).
    /// - `true`: the Actix engine skips the sync probe entirely and routes
    ///   every message through `receive_message`. The handler may `.await`
    ///   freely (real IO); the actor stays logically serial — subsequent
    ///   messages queue behind the in-flight one — while the arbiter thread
    ///   is released to poll other actors during the await.
    ///
    /// The thread engine always uses `receive_message`, so setting this to
    /// `true` makes actor behavior identical across both engines.
    fn use_async_handler(&self) -> bool {
        false
    }

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
    fn handle_stream<'a>(
        &'a mut self,
        item: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
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
    fn stream_started<'a>(
        &'a mut self,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Called when a stream completes successfully.
    ///
    /// Override to perform cleanup work after stream processing.
    ///
    /// # Parameters
    /// * `ctx` - Mutable reference to actor context
    fn stream_finished<'a>(
        &'a mut self,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Called when a stream encounters an error.
    ///
    /// Override to handle stream processing errors.
    ///
    /// # Parameters
    /// * `err` - The error that occurred
    /// * `ctx` - Mutable reference to actor context
    fn stream_error<'a>(
        &'a mut self,
        err: ActorError,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
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
    fn before_stop<'a>(
        &'a mut self,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Handle termination of a child actor.
    ///
    /// Override to implement supervision strategies.
    ///
    /// # Parameters
    /// * `child` - Reference to the terminated child actor
    /// * `ctx` - Mutable reference to actor context
    fn handle_child_terminated<'a>(
        &'a mut self,
        _child: BoxedActorRef,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
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

/// Actix 引擎侧扩展 trait：同步快路径（M6 从规范 `Actor` trait 移出）。
///
/// behaviour/runtime 二分原则：引擎特有的能力（actix 原生 Context 直访）
/// 归引擎 crate 的扩展 trait；规范 trait 只留 `receive_message`。
///
/// trait 方法带默认体（`None` = 未处理）：只跑 thread 引擎的 actor 无需
/// 实现本 trait；跑 actix 引擎的 actor 由适配层 bound 要求实现（derive
/// 宏 `engine = "actix"` 分支自动生成，转发到用户的 `handle_message_engine`；
/// 手写 actor 用空 impl `impl ActixEngineExt for X {}` 取默认值或自行覆盖）。
///
/// 注意：**故意不加 blanket impl**——stable Rust 无 specialization，
/// blanket 会与 derive 生成的逐类型 impl 冲突（E0119）。
pub trait ActixEngineExt: Actor {
    /// 同步快路径：`Some(..)` 完全处理消息；`None` = 未处理
    /// （引擎回退到 async `receive_message`）。
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
        engine_ctx: EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        let _ = (msg, ctx, engine_ctx);
        None
    }
}

#[cfg(test)]
mod engine_context_handle_tests {
    use super::*;

    /// A representative engine context payload (like actix's Context).
    #[derive(Debug, PartialEq)]
    struct FakeEngineCtx {
        counter: u32,
        label: String,
    }

    fn mint(data: &mut FakeEngineCtx) -> EngineContextHandle {
        let raw: NonNull<dyn Any> = NonNull::from(data);
        // SAFETY (test-side engine simulation): `data` outlives this scope's
        // use; single-threaded access; handle not stored beyond calls.
        unsafe { EngineContextHandle::from_raw(raw) }
    }

    #[test]
    fn downcast_ref_matches_type_and_value() {
        let mut ctx = FakeEngineCtx {
            counter: 7,
            label: "arbiter-0".into(),
        };
        let h = mint(&mut ctx);
        assert_eq!(h.downcast_ref::<FakeEngineCtx>().unwrap().counter, 7);
        assert_eq!(
            h.downcast_ref::<FakeEngineCtx>().unwrap().label,
            "arbiter-0"
        );
    }

    #[test]
    fn downcast_ref_wrong_type_returns_none() {
        let mut ctx = FakeEngineCtx {
            counter: 1,
            label: String::new(),
        };
        let h = mint(&mut ctx);
        assert!(h.downcast_ref::<u32>().is_none());
        assert!(h.downcast_ref::<String>().is_none());
        assert!(h.downcast_ref::<Vec<u8>>().is_none());
        assert!(h.downcast_ref::<()>().is_none());
    }

    #[test]
    fn downcast_mut_allows_mutation_through_handle() {
        let mut ctx = FakeEngineCtx {
            counter: 0,
            label: "a".into(),
        };
        {
            let mut h = mint(&mut ctx);
            h.downcast_mut::<FakeEngineCtx>().unwrap().counter = 99;
            h.downcast_mut::<FakeEngineCtx>().unwrap().label = "b".into();
        }
        assert_eq!(ctx.counter, 99);
        assert_eq!(ctx.label, "b");
    }

    #[test]
    fn repeated_borrow_is_stable() {
        let mut ctx = FakeEngineCtx {
            counter: 3,
            label: "x".into(),
        };
        let h = mint(&mut ctx);
        for _ in 0..100 {
            assert_eq!(h.downcast_ref::<FakeEngineCtx>().unwrap().counter, 3);
        }
    }

    #[test]
    fn debug_impl_does_not_dereference() {
        let mut ctx = FakeEngineCtx {
            counter: 0,
            label: String::new(),
        };
        let h = mint(&mut ctx);
        // Debug must not panic / must not require accessing the pointee's
        // internals through the raw pointer (finish_non_exhaustive).
        let s = format!("{:?}", h);
        assert!(s.contains("EngineContextHandle"));
    }

    #[test]
    fn zero_sized_and_unit_types() {
        let mut unit = ();
        let raw: NonNull<dyn Any> = NonNull::from(&mut unit);
        let h = unsafe { EngineContextHandle::from_raw(raw) };
        assert!(h.downcast_ref::<()>().is_some());
        assert!(h.downcast_ref::<FakeEngineCtx>().is_none());
    }

    // -----------------------------------------------------------------------
    // Trait default-method coverage (100% of Actor's default surface)
    // -----------------------------------------------------------------------

    /// A minimal actor whose ONLY impl is the required
    /// `receive_message_with_engine`/`receive_message` pair — everything
    /// else exercises the trait defaults.
    struct DefaultsOnlyActor {
        received: Vec<u32>,
    }

    impl Actor for DefaultsOnlyActor {
        type Config = EmptyConfig;
        type Context = dyn crate::context::ActorContext;

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(v) = msg.downcast_ref::<u32>() {
                    self.received.push(*v);
                }
                Ok(msg)
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    #[tokio::test]
    async fn default_init_returns_ok() {
        let mut a = DefaultsOnlyActor { received: vec![] };
        let mut ctx: Box<dyn crate::context::ActorContext> = Box::new(NoopContext {
            registry: NoopStreamRegistry,
            spawner_impl: NoopSpawner,
        });
        assert!(a.init(ctx.as_mut()).await.is_ok());
    }

    #[tokio::test]
    async fn default_handle_stream_forwards_to_receive_message() {
        let mut a = DefaultsOnlyActor { received: vec![] };
        let mut ctx: Box<dyn crate::context::ActorContext> = Box::new(NoopContext {
            registry: NoopStreamRegistry,
            spawner_impl: NoopSpawner,
        });
        let r = a
            .handle_stream(Box::new(7u32), ctx.as_mut())
            .await
            .expect("forwarded receive must succeed");
        assert_eq!(*r.downcast::<u32>().unwrap(), 7);
        assert_eq!(a.received, vec![7], "the forwarded handler must run");
    }

    #[tokio::test]
    async fn default_stream_lifecycle_hooks() {
        let mut a = DefaultsOnlyActor { received: vec![] };
        let mut ctx: Box<dyn crate::context::ActorContext> = Box::new(NoopContext {
            registry: NoopStreamRegistry,
            spawner_impl: NoopSpawner,
        });
        assert!(a.stream_started(ctx.as_mut()).await.is_ok());
        assert!(a.stream_finished(ctx.as_mut()).await.is_ok());
    }

    #[tokio::test]
    async fn default_stream_error_propagates_err() {
        let mut a = DefaultsOnlyActor { received: vec![] };
        let mut ctx: Box<dyn crate::context::ActorContext> = Box::new(NoopContext {
            registry: NoopStreamRegistry,
            spawner_impl: NoopSpawner,
        });
        let e = crate::errors::ActorError::MessageHandlingError("stream-err".into());
        let r = a.stream_error(e, ctx.as_mut()).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("stream-err"));
    }

    #[tokio::test]
    async fn default_before_stop_returns_ok() {
        let mut a = DefaultsOnlyActor { received: vec![] };
        let mut ctx: Box<dyn crate::context::ActorContext> = Box::new(NoopContext {
            registry: NoopStreamRegistry,
            spawner_impl: NoopSpawner,
        });
        assert!(a.before_stop(ctx.as_mut()).await.is_ok());
    }

    #[tokio::test]
    async fn default_handle_child_terminated_returns_ok() {
        let mut a = DefaultsOnlyActor { received: vec![] };
        let mut ctx: Box<dyn crate::context::ActorContext> = Box::new(NoopContext {
            registry: NoopStreamRegistry,
            spawner_impl: NoopSpawner,
        });
        let dead: BoxedActorRef = Box::new(crate::address::DeadTargetRef);
        assert!(a.handle_child_terminated(dead, ctx.as_mut()).await.is_ok());
    }

    #[test]
    fn default_use_async_handler_is_false() {
        let a = DefaultsOnlyActor { received: vec![] };
        assert!(!a.use_async_handler());
    }

    /// Noop stream registry satisfying `ActorContext::stream_registry`.
    struct NoopStreamRegistry;

    impl crate::stream::StreamRegistry for NoopStreamRegistry {
        fn add_stream_erased(
            &mut self,
            _stream: Box<dyn futures::Stream<Item = Box<dyn std::any::Any + Send>> + Send>,
        ) -> Result<(), crate::errors::ActorError> {
            Ok(())
        }

        fn add_stream_with_handler_erased(
            &mut self,
            _stream: Box<dyn futures::Stream<Item = Box<dyn std::any::Any + Send>> + Send>,
            _handler: Box<dyn std::any::Any + Send>,
        ) -> Result<(), crate::errors::ActorError> {
            Ok(())
        }
    }

    /// Noop spawner satisfying `ActorContext::spawner`.
    struct NoopSpawner;

    impl crate::context::ActorSpawner for NoopSpawner {
        fn spawn<'a>(
            &'a self,
            _actor: BoxedMessage,
            _config: BoxedMessage,
        ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
            Box::pin(async {
                Err(crate::errors::ActorError::ActorNotFound(
                    "noop-spawner".into(),
                ))
            })
        }

        fn spawn_with_strategy<'a>(
            &'a self,
            _actor: BoxedMessage,
            _config: BoxedMessage,
            _strategy: crate::supervisor::SupervisorStrategyType,
        ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
            Box::pin(async {
                Err(crate::errors::ActorError::ActorNotFound(
                    "noop-spawner".into(),
                ))
            })
        }
    }

    /// Noop ActorContext implementation for default-method tests.
    struct NoopContext {
        registry: NoopStreamRegistry,
        spawner_impl: NoopSpawner,
    }

    impl crate::context::ActorContext for NoopContext {
        fn get_self_ref(&self) -> BoxedActorRef {
            Box::new(crate::address::DeadTargetRef)
        }

        fn send<'a>(
            &'a self,
            _target: BoxedActorRef,
            _msg: BoxedMessage,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn ask<'a>(
            &'a self,
            _target: BoxedActorRef,
            _msg: BoxedMessage,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Err(crate::errors::ActorError::ActorNotFound("noop".into())) })
        }

        fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn schedule_once<'a>(
            &'a self,
            _target: BoxedActorRef,
            _msg: BoxedMessage,
            _delay: std::time::Duration,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn schedule_periodic<'a>(
            &'a self,
            _target: BoxedActorRef,
            _msg: crate::message::CloneableMessage,
            _initial_delay: std::time::Duration,
            _interval: std::time::Duration,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn watch<'a>(&'a mut self, _target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn unwatch<'a>(&'a mut self, _target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn set_parent(&mut self, _parent: BoxedActorRef) {}

        fn parent(&self) -> Option<BoxedActorRef> {
            None
        }

        fn add_child(&mut self, _child: BoxedActorRef) {}

        fn remove_child(&mut self, _child: BoxedActorRef) {}

        fn children(&self) -> Option<crate::context::ReadOnlyChildrenVec> {
            None
        }

        fn set_receive_timeout(&mut self, _timeout: Option<std::time::Duration>) {}

        fn receive_timeout(&self) -> Option<std::time::Duration> {
            None
        }

        fn set_supervisor_strategy(
            &mut self,
            _strategy: crate::supervisor::SupervisorStrategyType,
        ) {
        }

        fn path(&self) -> &crate::address::ActorPath {
            static NOOP_PATH: std::sync::OnceLock<crate::address::ActorPath> =
                std::sync::OnceLock::new();
            NOOP_PATH.get_or_init(|| crate::address::ActorPath::placeholder("noop"))
        }

        fn stream_registry(&mut self) -> &mut dyn crate::stream::StreamRegistry {
            &mut self.registry
        }

        fn spawner(&mut self) -> &mut dyn crate::context::ActorSpawner {
            &mut self.spawner_impl
        }
    }

    /// Drive every NoopContext method once so the whole default surface is
    /// exercised (context plumbing behind the trait defaults).
    #[tokio::test]
    async fn noop_context_full_surface() {
        use crate::context::ActorSpawner as _;
        use crate::stream::StreamRegistry as _;
        let mut ctx: Box<dyn crate::context::ActorContext> = Box::new(NoopContext {
            registry: NoopStreamRegistry,
            spawner_impl: NoopSpawner,
        });
        let dead: BoxedActorRef = Box::new(crate::address::DeadTargetRef);

        assert!(ctx.send(dead.clone_boxed(), Box::new(1u32)).await.is_ok());
        assert!(ctx.ask(dead.clone_boxed(), Box::new(1u32)).await.is_err());
        assert!(ctx
            .schedule_once(
                dead.clone_boxed(),
                Box::new(1u32),
                std::time::Duration::from_millis(1)
            )
            .await
            .is_ok());
        #[derive(Clone)]
        struct PeriodicTick(#[allow(dead_code)] u32);
        impl crate::message::Message for PeriodicTick {
            type Result = ();
        }
        assert!(ctx
            .schedule_periodic(
                dead.clone_boxed(),
                crate::message::CloneableMessage::from_message(PeriodicTick(1)),
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(1),
            )
            .await
            .is_ok());
        assert!(ctx.watch(dead.clone_boxed()).await.is_ok());
        assert!(ctx.unwatch(dead.clone_boxed()).await.is_ok());
        ctx.set_parent(dead.clone_boxed());
        assert!(ctx.parent().is_none());
        ctx.add_child(dead.clone_boxed());
        ctx.remove_child(dead.clone_boxed());
        assert!(ctx.children().is_none());
        ctx.set_receive_timeout(Some(std::time::Duration::from_secs(1)));
        assert!(ctx.receive_timeout().is_none());
        ctx.set_supervisor_strategy(crate::supervisor::SupervisorStrategyType::default());
        assert!(!ctx.path().path.is_empty());
        assert!(ctx.stop().await.is_ok());

        // stream_registry / spawner accessors return usable trait objects.
        let _reg: &mut dyn crate::stream::StreamRegistry = ctx.stream_registry();
        let _sp: &mut dyn crate::context::ActorSpawner = ctx.spawner();

        // Noop impls themselves: exercise their bodies.
        let mut reg = NoopStreamRegistry;
        let empty: Box<dyn futures::Stream<Item = Box<dyn std::any::Any + Send>> + Send> =
            Box::new(futures::stream::empty());
        assert!(reg.add_stream_erased(empty).is_ok());
        let empty2: Box<dyn futures::Stream<Item = Box<dyn std::any::Any + Send>> + Send> =
            Box::new(futures::stream::empty());
        assert!(reg
            .add_stream_with_handler_erased(empty2, Box::new(()))
            .is_ok());

        let sp = NoopSpawner;
        let r = sp.spawn(Box::new(0u32), Box::new(0u32)).await;
        assert!(r.is_err());
        let r = sp
            .spawn_with_strategy(
                Box::new(0u32),
                Box::new(0u32),
                crate::supervisor::SupervisorStrategyType::default(),
            )
            .await;
        assert!(r.is_err());
    }

    /// Directly drive the DefaultsOnlyActor dispatch arms (the same paths
    /// the engines invoke), completing coverage of the required methods.
    #[tokio::test]
    async fn defaults_only_actor_dispatch_arms() {
        let mut a = DefaultsOnlyActor { received: vec![] };
        let mut ctx: Box<dyn crate::context::ActorContext> = Box::new(NoopContext {
            registry: NoopStreamRegistry,
            spawner_impl: NoopSpawner,
        });

        // receive_message: known type recorded, unknown passes through.
        let r = a
            .receive_message(Box::new(5u32), ctx.as_mut())
            .await
            .unwrap();
        assert_eq!(*r.downcast::<u32>().unwrap(), 5);
        let r = a
            .receive_message(Box::new("str"), ctx.as_mut())
            .await
            .unwrap();
        assert!(r.downcast::<&str>().is_ok());
        assert_eq!(a.received, vec![5]);

        // M6: `receive_message_with_engine` 已从规范 `Actor` trait 移出，
        // 归 `ActixEngineExt` 扩展 trait。DefaultsOnlyActor 未实现该扩展，
        // 其默认契约（None）由扩展 trait 的默认方法体覆盖（见
        // engine.rs / actix 适配层测试）。此处验证规范面只剩
        // `receive_message` 派发。

        // get_self_ref goes through the NoopContext path once.
        let _ = ctx.get_self_ref();
    }

    #[test]
    fn defaults_only_actor_state_reports_running() {
        let a = DefaultsOnlyActor { received: vec![] };
        assert_eq!(a.state(), ActorState::Running);
    }
}
