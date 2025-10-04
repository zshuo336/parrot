use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::runtime::Handle;

use crate::logging;
use crate::thread::address::ThreadActorRef;
use crate::thread::config::{BackpressureStrategy, SupervisorStrategy};
use parrot_api::actor::Actor;
use parrot_api::address::ActorPath;
use parrot_api::context::{ActorContext, ActorSpawner, ReadOnlyChildrenVec};
use parrot_api::errors::ActorError;
use parrot_api::message::CloneableMessage;
use parrot_api::supervisor::SupervisorStrategyType;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::sync::RwLock;

/// Weak reference to the actor system to avoid circular references.
///
/// Concrete (not `dyn`) so the context gets full typed spawn capabilities.
pub type WeakSystemRef = Weak<crate::thread::system::ThreadActorSystem>;

/// ThreadContext provides the execution context for an actor instance.
///
/// It implements ActorContext from parrot-api and provides access to:
/// - The actor's own reference (self)
/// - Parent and children references
/// - The actor system
/// - Actor configuration and lifecycle management
/// - Methods for spawning child actors
#[derive(Debug)]
pub struct ThreadContext<A: Actor + Send + Sync + 'static> {
    /// Weak reference to the actor system
    system: WeakSystemRef,

    /// Handle to the Tokio runtime
    runtime_handle: Handle,

    /// Actor's own reference
    self_ref: Option<BoxedActorRef>,

    /// Reference to parent actor
    parent_ref: Option<BoxedActorRef>,

    /// References to child actors
    children_refs: Option<Arc<RwLock<HashMap<String, BoxedActorRef>>>>,

    /// Actor's path (full form, including the actor target reference)
    path: ActorPath,

    /// Supervision strategy for child actors
    supervisor_strategy: SupervisorStrategy,

    /// Receive timeout for ask operations
    receive_timeout: Option<Duration>,

    /// Default backpressure strategy
    backpressure_strategy: BackpressureStrategy,

    /// Phantom data to associate with actor type
    _phantom: PhantomData<A>,
}

/// M1 derive-decouple: thread 引擎的中立 Context 别名。
///
/// derive 宏生成代码经 `__parrot_engine::EngineContext<Self>` 引用 context；
/// 本引擎的具体类型是 `ThreadContext<Self>`。
pub type EngineContext<A> = ThreadContext<A>;

impl<A: Actor + Send + Sync + 'static> ThreadContext<A> {
    /// Creates a new ThreadContext.
    ///
    /// # Parameters
    /// * `system` - Weak reference to the actor system
    /// * `runtime_handle` - Handle to the Tokio runtime
    /// * `path` - The actor's path
    /// * `parent_ref` - Optional reference to parent actor
    /// * `supervisor_strategy` - Supervision strategy for child actors
    pub fn new(
        system: WeakSystemRef,
        runtime_handle: Handle,
        path: ActorPath,
        parent_ref: Option<BoxedActorRef>,
        supervisor_strategy: SupervisorStrategy,
    ) -> Self {
        Self {
            system,
            runtime_handle,
            self_ref: None,
            parent_ref,
            children_refs: None,
            path,
            supervisor_strategy,
            receive_timeout: None,
            backpressure_strategy: BackpressureStrategy::Block,
            _phantom: PhantomData,
        }
    }

    /// Creates a context for unit tests (no system attached).
    pub fn new_for_test(path: &str) -> Self {
        Self::new_for_test_with(path, Weak::default())
    }

    /// Creates a test context bound to a specific system weak reference.
    pub fn new_for_test_with(path: &str, system: WeakSystemRef) -> Self {
        Self {
            system,
            runtime_handle: Handle::current(),
            self_ref: None,
            parent_ref: None,
            children_refs: None,
            path: ActorPath::for_test(path),
            supervisor_strategy: SupervisorStrategy::default(),
            receive_timeout: None,
            backpressure_strategy: BackpressureStrategy::Block,
            _phantom: PhantomData,
        }
    }

    /// Sets the self reference.
    pub fn set_self_ref(&mut self, actor_ref: BoxedActorRef) {
        self.self_ref = Some(actor_ref);
    }

    /// get self reference.
    ///
    /// # Returns
    /// The actor's own reference, if it is set. otherwise, it will panic.
    pub fn get_self_ref(&self) -> BoxedActorRef {
        self.self_ref
            .as_ref()
            .expect("Self reference not set")
            .clone_boxed()
    }

    /// Whether the self reference has been set.
    pub fn has_self_ref(&self) -> bool {
        self.self_ref.is_some()
    }

    /// Get self reference without panicking; `None` when not set.
    pub fn get_self_ref_opt(&self) -> Option<BoxedActorRef> {
        self.self_ref.as_ref().map(|r| r.clone_boxed())
    }

    fn set_parent(&mut self, parent: BoxedActorRef) {
        self.parent_ref = Some(parent);
    }

    /// Upgrades the weak system reference to a strong reference if available.
    fn system(&self) -> Option<Arc<crate::thread::system::ThreadActorSystem>> {
        self.system.upgrade()
    }

    /// Get the weak system reference (M3 supervision hook plumbing).
    pub fn system_weak_ref(&self) -> WeakSystemRef {
        self.system.clone()
    }

    /// Adds a child actor reference.
    pub fn add_child(&mut self, child_ref: BoxedActorRef) {
        let path_str = child_ref.path();
        if self.children_refs.is_none() {
            self.children_refs = Some(Arc::new(RwLock::new(HashMap::new())));
        }
        self.children_refs
            .as_mut()
            .unwrap()
            .write()
            .unwrap()
            .insert(path_str, child_ref);
    }

    /// Removes a child actor reference by path.
    pub fn remove_child_by_path(&mut self, path: &str) -> Option<BoxedActorRef> {
        self.children_refs.as_mut()?.write().unwrap().remove(path)
    }
    /// Removes a child actor reference.
    pub fn remove_child_by_ref(&mut self, child: &BoxedActorRef) -> Option<BoxedActorRef> {
        self.children_refs
            .as_mut()?
            .write()
            .unwrap()
            .remove(&child.path())
    }

    /// Returns the children references.
    pub fn children(&self) -> Option<ReadOnlyChildrenVec> {
        self.children_refs.as_ref().map(|children| {
            let boxed_children = children
                .read()
                .unwrap()
                .values()
                .map(|r| r.clone_boxed())
                .collect::<Vec<_>>();
            ReadOnlyChildrenVec::new(Arc::new(RwLock::new(boxed_children)))
        })
    }

    /// Sets the backpressure strategy.
    pub fn set_backpressure_strategy(&mut self, strategy: BackpressureStrategy) {
        self.backpressure_strategy = strategy;
    }

    /// Gets the current backpressure strategy.
    pub fn backpressure_strategy(&self) -> &BackpressureStrategy {
        &self.backpressure_strategy
    }

    /// Gets the effective supervisor strategy (thread-engine internal form).
    ///
    /// Mostly useful for tests and diagnostics; the mapping from the public
    /// `SupervisorStrategyType` happens in
    /// [`ActorContext::set_supervisor_strategy`].
    pub fn supervisor_strategy_internal(&self) -> &SupervisorStrategy {
        &self.supervisor_strategy
    }

    /// Get the actor's own typed reference, if set and of the right type.
    pub fn typed_self_ref(&self) -> Option<ThreadActorRef<A>>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        // ThreadActorRef<A> is stored boxed as self_ref only when the actor
        // engine created it; downcast through as_any.
        self.self_ref
            .as_ref()
            .and_then(|r| r.as_any().downcast_ref::<ThreadActorRef<A>>().cloned())
    }

    /// Actor path string.
    pub fn path_str(&self) -> &str {
        self.path.path()
    }
}

impl<A: Actor + Send + Sync + 'static> ActorContext for ThreadContext<A> {
    fn get_self_ref(&self) -> BoxedActorRef {
        self.get_self_ref()
    }

    fn set_parent(&mut self, parent: BoxedActorRef) {
        self.set_parent(parent);
    }

    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if let Some(self_ref) = &self.self_ref {
                self_ref.stop().await?;

                // Stop all children
                if let Some(children) = &self.children_refs {
                    let children_to_stop: Vec<_> = children
                        .read()
                        .unwrap()
                        .values()
                        .map(|r| r.clone_boxed())
                        .collect();

                    for child in children_to_stop {
                        let result = child.stop().await;
                        if let Err(e) = result {
                            logging::error!("Failed to stop child actor-{}: {}", child.path(), e);
                        }
                    }
                }

                Ok(())
            } else {
                Err(ActorError::InternalError(
                    "Self reference not set".to_string(),
                ))
            }
        })
    }

    fn send<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            target.send(msg).await?;
            Ok(())
        })
    }

    /// Ask a target actor for a response using the universal ask envelope.
    fn ask<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let timeout_duration = self.receive_timeout.unwrap_or_else(|| {
            self.system()
                .map(|s| s.config().default_ask_timeout)
                .unwrap_or_else(|| Duration::from_secs(5))
        });

        Box::pin(async move {
            let (envelope, reply_rx) = crate::thread::envelope::AskEnvelope::new(msg);
            target.send(Box::new(envelope) as BoxedMessage).await?;

            match tokio::time::timeout(timeout_duration, reply_rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(ActorError::ReplyChannelError(
                    "Reply channel closed for ask".to_string(),
                )),
                Err(_) => Err(ActorError::TimeoutDetail(format!(
                    "Ask timed out after {}ms",
                    timeout_duration.as_millis()
                ))),
            }
        })
    }

    fn schedule_once<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: BoxedMessage,
        delay: Duration,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let runtime = self.runtime_handle.clone();

        Box::pin(async move {
            let target_clone = target.clone_boxed();

            runtime.spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = target_clone.send(msg).await;
            });

            Ok(())
        })
    }

    fn schedule_periodic<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: CloneableMessage,
        initial_delay: Duration,
        interval: Duration,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let runtime = self.runtime_handle.clone();

        Box::pin(async move {
            let target_clone = target.clone_boxed();

            runtime.spawn(async move {
                tokio::time::sleep(initial_delay).await;

                let _ = target_clone.send(msg.clone().into_boxed()).await;

                let mut interval_timer = tokio::time::interval(interval);
                interval_timer.tick().await; // first tick fires immediately; skip duplicate

                loop {
                    interval_timer.tick().await;

                    let target_copy = target_clone.clone_boxed();
                    if target_copy.send(msg.clone().into_boxed()).await.is_err() {
                        break;
                    }
                }
            });

            Ok(())
        })
    }

    fn watch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        let system = self.system().map(|s| Arc::downgrade(&s));
        let watcher_path = self.path.path();

        Box::pin(async move {
            if let Some(weak) = system
                && let Some(sys) = weak.upgrade()
            {
                sys.watch(watcher_path.to_string(), target.path()).await?;
            }
            Ok(())
        })
    }

    fn unwatch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        let system = self.system().map(|s| Arc::downgrade(&s));
        let watcher_path = self.path.path();

        Box::pin(async move {
            if let Some(weak) = system
                && let Some(sys) = weak.upgrade()
            {
                sys.unwatch(watcher_path.to_string(), target.path()).await?;
            }
            Ok(())
        })
    }

    fn parent(&self) -> Option<BoxedActorRef> {
        self.parent_ref.as_ref().map(|r| r.clone_boxed())
    }

    fn add_child(&mut self, child: BoxedActorRef) {
        self.add_child(child);
    }

    fn remove_child(&mut self, child: BoxedActorRef) {
        self.remove_child_by_ref(&child);
    }

    fn children(&self) -> Option<ReadOnlyChildrenVec> {
        self.children()
    }

    fn set_receive_timeout(&mut self, timeout: Option<Duration>) {
        self.receive_timeout = timeout;
    }

    fn receive_timeout(&self) -> Option<Duration> {
        self.receive_timeout
    }

    // ---------------- K2 receptionist 转发（DEV_02 §3.1 依赖注入） ----------------

    fn receptionist_register(
        &mut self,
        key: parrot_api::receptionist::ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        let gw = self.system().and_then(|s| s.receptionist_gateway());
        let path = self.path.path().to_string();
        Box::pin(async move {
            match gw {
                Some(g) => g.register(key, path).await,
                None => Err(ActorError::InternalError("receptionist not enabled".into())),
            }
        })
    }

    fn receptionist_deregister(
        &mut self,
        key: &parrot_api::receptionist::ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        let gw = self.system().and_then(|s| s.receptionist_gateway());
        let key = key.clone();
        let path = self.path.path().to_string();
        Box::pin(async move {
            match gw {
                Some(g) => g.deregister(key, path).await,
                None => Err(ActorError::InternalError("receptionist not enabled".into())),
            }
        })
    }

    fn receptionist_subscribe<'a>(
        &'a mut self,
        key: parrot_api::receptionist::ReceptionistKey,
    ) -> BoxedFuture<'a, ActorResult<parrot_api::receptionist::ReceptionistStream>> {
        let gw = self.system().and_then(|s| s.receptionist_gateway());
        Box::pin(async move {
            match gw {
                Some(g) => g.subscribe(key).await,
                None => Err(ActorError::InternalError("receptionist not enabled".into())),
            }
        })
    }

    fn set_supervisor_strategy(&mut self, strategy: SupervisorStrategyType) {
        use parrot_api::supervisor::{DefaultStrategy, OneForAllStrategy, OneForOneStrategy};
        self.supervisor_strategy = match strategy {
            SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure) => {
                SupervisorStrategy::Stop
            }
            SupervisorStrategyType::Default(DefaultStrategy::RestartOnFailure) => {
                SupervisorStrategy::Restart {
                    max_retries: 3,
                    within: Duration::from_secs(10),
                }
            }
            SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure) => {
                SupervisorStrategy::Resume
            }
            SupervisorStrategyType::Default(DefaultStrategy::EscalateFailure) => {
                SupervisorStrategy::Escalate
            }
            SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts,
                within,
                ..
            }) => SupervisorStrategy::Restart {
                max_retries: max_restarts as usize,
                within,
            },
            SupervisorStrategyType::OneForAll(OneForAllStrategy {
                max_restarts,
                within,
                ..
            }) => SupervisorStrategy::Restart {
                max_retries: max_restarts as usize,
                within,
            },
        };
    }

    fn path(&self) -> &ActorPath {
        &self.path
    }

    fn stream_registry(&mut self) -> &mut dyn parrot_api::stream::StreamRegistry {
        panic!("Stream registry not yet implemented for ThreadContext")
    }

    fn spawner(&mut self) -> &mut dyn ActorSpawner {
        self
    }
}

#[async_trait::async_trait]
impl<A: Actor + Send + Sync + 'static> ActorSpawner for ThreadContext<A> {
    fn spawn<'a>(
        &'a self,
        actor: BoxedMessage,
        config: BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        Box::pin(async move {
            if let Some(system) = self.system() {
                system
                    .spawn_erased_actor(actor, config, Some(self.supervisor_strategy.clone()))
                    .await
            } else {
                Err(ActorError::InternalError(
                    "Actor system not available".to_string(),
                ))
            }
        })
    }

    fn spawn_with_strategy<'a>(
        &'a self,
        actor: BoxedMessage,
        config: BoxedMessage,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        use parrot_api::supervisor::{DefaultStrategy, OneForAllStrategy, OneForOneStrategy};
        let internal = match strategy {
            SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure) => {
                SupervisorStrategy::Stop
            }
            SupervisorStrategyType::Default(DefaultStrategy::RestartOnFailure) => {
                SupervisorStrategy::Restart {
                    max_retries: 3,
                    within: Duration::from_secs(10),
                }
            }
            SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure) => {
                SupervisorStrategy::Resume
            }
            SupervisorStrategyType::Default(DefaultStrategy::EscalateFailure) => {
                SupervisorStrategy::Escalate
            }
            SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts,
                within,
                ..
            }) => SupervisorStrategy::Restart {
                max_retries: max_restarts as usize,
                within,
            },
            SupervisorStrategyType::OneForAll(OneForAllStrategy {
                max_restarts,
                within,
                ..
            }) => SupervisorStrategy::Restart {
                max_retries: max_restarts as usize,
                within,
            },
        };

        Box::pin(async move {
            if let Some(system) = self.system() {
                system
                    .spawn_erased_actor(actor, config, Some(internal))
                    .await
            } else {
                Err(ActorError::InternalError(
                    "Actor system not available".to_string(),
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::tests_support::DummyActor;
    use parrot_api::types::BoxedActorRef;

    /// A minimal boxed actor ref used to exercise child/parent bookkeeping.
    struct FakeRef(String);

    impl std::fmt::Debug for FakeRef {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FakeRef").field("path", &self.0).finish()
        }
    }

    impl parrot_api::address::ActorRef for FakeRef {
        fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn send<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Err(ActorError::ActorNotFound("fake".into())) })
        }

        fn send_with_timeout<'a>(
            &'a self,
            _msg: BoxedMessage,
            _timeout: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Err(ActorError::ActorNotFound("fake".into())) })
        }

        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn path(&self) -> String {
            self.0.clone()
        }

        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }

        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(FakeRef(self.0.clone()))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn boxed_fake(path: &str) -> BoxedActorRef {
        Box::new(FakeRef(path.to_string()))
    }

    #[tokio::test]
    async fn test_new_for_test_context() {
        let ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/ctx");
        assert_eq!(ctx.path_str(), "test/ctx");
        assert!(!ctx.has_self_ref());
        assert!(ctx.parent().is_none());
        assert!(ctx.children().is_none());
        assert_eq!(ctx.receive_timeout(), None);
    }

    #[tokio::test]
    async fn test_self_ref_set_and_get() {
        let mut ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/self");
        assert!(!ctx.has_self_ref());

        ctx.set_self_ref(boxed_fake("test/self"));
        assert!(ctx.has_self_ref());
        assert_eq!(ctx.get_self_ref().path(), "test/self");
        assert!(ctx.get_self_ref_opt().is_some());
    }

    #[tokio::test]
    async fn test_get_self_ref_panics_when_missing() {
        let ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/panic");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.get_self_ref();
        }));
        assert!(result.is_err(), "get_self_ref must panic when unset");
    }

    #[tokio::test]
    async fn test_parent_set_via_trait() {
        let mut ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/child");
        use parrot_api::context::ActorContext as _;
        ctx.set_parent(boxed_fake("test/parent"));

        let parent = ctx.parent().expect("parent set");
        assert_eq!(parent.path(), "test/parent");
    }

    #[tokio::test]
    async fn test_children_management() {
        let mut ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/parent");

        // No children initially.
        assert!(ctx.children().is_none());

        // Add two children.
        ctx.add_child(boxed_fake("/user/c1"));
        ctx.add_child(boxed_fake("/user/c2"));

        let children = ctx.children().expect("children present");
        let all = children.read_all();
        assert_eq!(all.len(), 2);
        let paths: Vec<String> = all.iter().map(|c| c.path()).collect();
        assert!(paths.contains(&"/user/c1".to_string()));
        assert!(paths.contains(&"/user/c2".to_string()));

        // Remove one by path.
        let removed = ctx.remove_child_by_path("/user/c1");
        assert!(removed.is_some());
        assert_eq!(ctx.children().unwrap().read_all().len(), 1);

        // Remove a missing one.
        assert!(ctx.remove_child_by_path("/user/c1").is_none());

        // Remove the other by ref.
        let c2 = boxed_fake("/user/c2");
        let removed_by_ref = ctx.remove_child_by_ref(&c2);
        assert!(removed_by_ref.is_some());
        assert_eq!(ctx.children().unwrap().read_all().len(), 0);
    }

    #[tokio::test]
    async fn test_backpressure_strategy_get_set() {
        let mut ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/bp");
        assert!(matches!(
            ctx.backpressure_strategy(),
            BackpressureStrategy::Block
        ));

        ctx.set_backpressure_strategy(BackpressureStrategy::DropOldest);
        assert!(matches!(
            ctx.backpressure_strategy(),
            BackpressureStrategy::DropOldest
        ));
    }

    #[tokio::test]
    async fn test_receive_timeout_set_clear() {
        let mut ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/timeout");
        assert_eq!(ctx.receive_timeout(), None);

        ctx.set_receive_timeout(Some(Duration::from_millis(250)));
        assert_eq!(ctx.receive_timeout(), Some(Duration::from_millis(250)));

        ctx.set_receive_timeout(None);
        assert_eq!(ctx.receive_timeout(), None);
    }

    #[tokio::test]
    async fn test_supervisor_strategy_mapping() {
        use parrot_api::supervisor::{DefaultStrategy, SupervisorStrategyType};

        let mut ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/sup");

        ctx.set_supervisor_strategy(SupervisorStrategyType::Default(
            DefaultStrategy::StopOnFailure,
        ));
        assert!(matches!(
            ctx.supervisor_strategy_internal(),
            SupervisorStrategy::Stop
        ));

        ctx.set_supervisor_strategy(SupervisorStrategyType::Default(
            DefaultStrategy::ResumeOnFailure,
        ));
        assert!(matches!(
            ctx.supervisor_strategy_internal(),
            SupervisorStrategy::Resume
        ));

        ctx.set_supervisor_strategy(SupervisorStrategyType::Default(
            DefaultStrategy::EscalateFailure,
        ));
        assert!(matches!(
            ctx.supervisor_strategy_internal(),
            SupervisorStrategy::Escalate
        ));

        ctx.set_supervisor_strategy(SupervisorStrategyType::Default(
            DefaultStrategy::RestartOnFailure,
        ));
        assert!(matches!(
            ctx.supervisor_strategy_internal(),
            SupervisorStrategy::Restart { max_retries: 3, .. }
        ));
    }

    #[tokio::test]
    async fn test_path_accessor() {
        let ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/path");
        use parrot_api::context::ActorContext as _;
        assert_eq!(ctx.path().path(), "test/path");
    }
}
