use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::runtime::Handle;

use crate::thread::address::ThreadActorRef;
use crate::thread::config::{BackpressureStrategy, SupervisorStrategy};
use parrot_api::actor::Actor;
use parrot_api::address::ActorPath;
use parrot_api::context::{
    ActorContext, ActorContextScheduler, ActorSpawner, ReadOnlyChildrenVec, ScheduledTask,
};
use parrot_api::errors::ActorError;
use parrot_api::message::{CloneableMessage, Message};
use parrot_api::supervisor::SupervisorStrategyType;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::sync::RwLock;

struct ThreadChildRecipient(crate::thread::mailbox::WeakMailboxRef);
impl crate::children::ChildRecipient for ThreadChildRecipient {
    fn deliver(&self, event: crate::children::ChildTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0
                .upgrade()
                .ok_or(ActorError::Stopped)?
                .push_with_priority(Box::new(event), BackpressureStrategy::Block, false)
                .await
                .map(|_| ())
                .map_err(|e| ActorError::InternalError(e.to_string()))
        })
    }
}

impl<A> parrot_api::context::ActorSpawnContext<A> for ThreadContext<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    fn wrap_spawn(actor: A) -> BoxedMessage {
        crate::thread::system::TypedSpawnPayload::new(actor).into_boxed()
    }
}

struct ThreadStreamRecipient(crate::thread::mailbox::WeakMailboxRef);
impl crate::streams::StreamRecipient for ThreadStreamRecipient {
    fn deliver(&self, event: crate::streams::StreamEvent) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let mailbox = self.0.upgrade().ok_or(ActorError::Stopped)?;
            mailbox
                .push_with_priority(Box::new(event), BackpressureStrategy::Block, false)
                .await
                .map(|_| ())
                .map_err(|error| ActorError::InternalError(error.to_string()))
        })
    }
}

struct ThreadIdleRecipient(crate::thread::mailbox::WeakMailboxRef);
impl crate::receive_timeout::IdleRecipient for ThreadIdleRecipient {
    fn deliver(
        &self,
        signal: crate::receive_timeout::IdleSignal,
    ) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let mailbox = self.0.upgrade().ok_or(ActorError::Stopped)?;
            mailbox
                .push_with_priority(Box::new(signal), BackpressureStrategy::Block, false)
                .await
                .map(|_| ())
                .map_err(|error| ActorError::InternalError(error.to_string()))
        })
    }
}

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
///
/// `ActorContext::stop` initiates this actor's shutdown. Its own cleanup hook
/// runs after the current handler releases the actor; callers outside the
/// actor can await `ActorRef::stop` to observe lifecycle completion.
#[derive(Debug)]
pub struct ThreadContext<A: Actor + Send + Sync + 'static> {
    /// Weak reference to the actor system
    system: WeakSystemRef,

    /// Handle to the Tokio runtime
    runtime_handle: Handle,
    tasks: Arc<crate::context_tasks::ContextTasks>,

    /// Actor's own reference
    self_ref: Option<BoxedActorRef>,

    /// This generation's mailbox, used by non-blocking self-stop requests.
    mailbox: Option<crate::thread::mailbox::WeakMailboxRef>,

    /// Reference to parent actor
    parent_ref: Option<BoxedActorRef>,

    /// References to child actors
    pub(crate) owned_children: Arc<crate::children::ChildScope>,
    children_refs: Option<Arc<RwLock<HashMap<String, BoxedActorRef>>>>,

    /// Actor's path (full form, including the actor target reference)
    path: ActorPath,

    /// Supervision strategy for child actors
    supervisor_strategy: SupervisorStrategy,

    /// Idle timeout for this concrete actor generation; independent of asks.
    pub(crate) idle: Arc<crate::receive_timeout::ReceiveTimeout>,

    /// Default backpressure strategy
    backpressure_strategy: BackpressureStrategy,

    /// Phantom data to associate with actor type
    _phantom: PhantomData<A>,
    stream_registry: crate::streams::OwnedStreams,
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
        let limit = system
            .upgrade()
            .map_or(crate::context_tasks::DEFAULT_MAX_CONTEXT_TASKS, |system| {
                system.config().max_context_tasks
            });
        let tasks = crate::context_tasks::ContextTasks::new(limit);
        let slots = system
            .upgrade()
            .map(|s| s.typed_slots.clone())
            .unwrap_or_else(|| Arc::new(tokio::sync::Semaphore::new(1)));
        let stream_registry = crate::streams::OwnedStreams::new::<Self>(
            tasks.clone(),
            crate::streams::StreamExecutor::Thread {
                handle: runtime_handle.clone(),
                slots: slots.clone(),
            },
        );
        Self {
            tasks,
            system: system.clone(),
            runtime_handle,
            self_ref: None,
            mailbox: None,
            parent_ref,
            owned_children: crate::children::ChildScope::new(
                system
                    .upgrade()
                    .map_or(Duration::from_secs(5), |s| s.config().shutdown_timeout),
                slots,
            ),
            children_refs: None,
            path,
            supervisor_strategy,
            idle: crate::receive_timeout::ReceiveTimeout::new(),
            backpressure_strategy: BackpressureStrategy::Block,
            _phantom: PhantomData,
            stream_registry,
        }
    }

    fn spawn_instance_child<'a>(
        &'a self,
        actor: BoxedMessage,
        config: BoxedMessage,
        strategy: Option<SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let system = self.system();
        let slots = system
            .as_ref()
            .map(|system| system.cleanup_slots.clone())
            .unwrap_or_else(|| Arc::new(tokio::sync::Semaphore::new(1)));
        let actor = crate::owned_payload::OwnedPayload::new(actor, &self.runtime_handle, &slots);
        let config = crate::owned_payload::OwnedPayload::new(config, &self.runtime_handle, &slots);
        let strategy =
            crate::owned_payload::OwnedPayload::new(strategy, &self.runtime_handle, &slots);
        Box::pin(parrot_api::trace::capture(async move {
            if self.mailbox.is_none() || system.is_none() {
                let (actor, config, strategy) =
                    tokio::join!(actor.retire(), config.retire(), strategy.retire());
                actor.and(config).and(strategy)?;
                return Err(ActorError::InternalError(
                    "Thread spawner is not bound to a live managed actor".into(),
                ));
            }
            system
                .expect("checked live system")
                .spawn_child_erased(
                    actor.take(),
                    config.take(),
                    strategy.take(),
                    self.path.target.clone_boxed(),
                    &self.owned_children,
                )
                .await
        }))
    }

    /// Creates a context for unit tests (no system attached).
    pub fn new_for_test(path: &str) -> Self {
        Self::new_for_test_with(path, Weak::default())
    }

    /// Creates a test context bound to a specific system weak reference.
    pub fn new_for_test_with(path: &str, system: WeakSystemRef) -> Self {
        Self::new(
            system,
            Handle::current(),
            ActorPath::for_test(path),
            None,
            SupervisorStrategy::default(),
        )
    }

    pub(crate) fn tasks(&self) -> Arc<crate::context_tasks::ContextTasks> {
        self.tasks.clone()
    }

    fn own_schedule_input<T: Send + 'static>(
        &self,
        value: T,
    ) -> crate::owned_payload::OwnedPayload<T> {
        match self.system() {
            Some(system) => crate::owned_payload::OwnedPayload::new(
                value,
                &self.runtime_handle,
                &system.cleanup_slots,
            ),
            None => crate::owned_payload::OwnedPayload::unbound(value),
        }
    }

    fn spawn_context_task(&self, task: crate::context_tasks::OwnedTask) {
        let slots = self
            .system()
            .map(|system| system.typed_slots.clone())
            .unwrap_or_else(|| Arc::new(tokio::sync::Semaphore::new(1)));
        self.runtime_handle.spawn(async move {
            if let Ok(permit) = slots.clone().acquire_owned().await {
                let _ = crate::thread::scheduler::shared::poll_isolated(
                    task,
                    permit,
                    slots,
                    Arc::new(tokio::sync::Notify::new()),
                )
                .await;
            }
        });
    }

    /// Sets the self reference.
    pub fn set_self_ref(&mut self, actor_ref: BoxedActorRef) {
        self.self_ref = Some(actor_ref);
    }

    pub(crate) fn set_mailbox(&mut self, mailbox: crate::thread::mailbox::WeakMailboxRef) {
        self.idle
            .bind(Arc::new(ThreadIdleRecipient(mailbox.clone())));
        self.stream_registry
            .bind(Arc::new(ThreadStreamRecipient(mailbox.clone())));
        self.owned_children
            .bind(Arc::new(ThreadChildRecipient(mailbox.clone())));
        let system = self.system.clone();
        let parent_mailbox = mailbox.clone();
        let path = self.path.path.clone();
        self.owned_children.bind_escalator(Arc::new(move |error| {
            if let Some(system) = system.upgrade() {
                system.runtime_spawn_escalation(path.clone(), parent_mailbox.clone(), error);
            }
        }));
        self.mailbox = Some(mailbox);
    }

    pub(crate) fn generation_mailbox(&self) -> Option<crate::thread::mailbox::WeakMailboxRef> {
        self.mailbox.clone()
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
        if self.mailbox.is_some() {
            return Some(self.owned_children.view());
        }
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
    fn resolve_actor(&self, path: &str) -> Option<BoxedActorRef> {
        self.system()?.resolve_actor(path)
    }

    fn get_self_ref(&self) -> BoxedActorRef {
        self.get_self_ref()
    }

    fn set_parent(&mut self, parent: BoxedActorRef) {
        self.set_parent(parent);
    }

    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.idle.close();
            self.tasks.close();
            if let Some(self_ref) = &self.self_ref {
                if let Some(system) = self.system() {
                    let mailbox: Arc<dyn crate::thread::mailbox::Mailbox> = self
                        .mailbox
                        .as_ref()
                        .and_then(|mailbox| mailbox.upgrade())
                        .ok_or(ActorError::Stopped)?;
                    // The actor/context lock is held by this handler. Accept
                    // the stop now and let the hook run after it returns.
                    system.request_stop(&self.path.path, Some(&mailbox))?;
                } else {
                    self_ref.stop().await?;
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
        let target = self.own_schedule_input(target);
        let msg = self.own_schedule_input(msg);
        Box::pin(crate::context_message::call(
            target,
            msg,
            &self.tasks,
            |target, msg| target.deliver(msg),
        ))
    }

    /// Ask through the target reference, which owns engine-specific framing.
    fn ask<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let timeout_duration = self
            .system()
            .map(|s| s.config().default_ask_timeout)
            .unwrap_or_else(|| Duration::from_secs(5));

        let target = self.own_schedule_input(target);
        let msg = self.own_schedule_input(msg);
        Box::pin(crate::context_message::call(
            target,
            msg,
            &self.tasks,
            move |target, msg| target.send_with_timeout(msg, Some(timeout_duration)),
        ))
    }

    fn schedule_once<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: BoxedMessage,
        delay: Duration,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.own_schedule_input(target);
        let msg = self.own_schedule_input(msg);
        Box::pin(parrot_api::trace::capture(async move {
            let task = self.tasks.once(target.take(), msg.take(), delay)?;
            self.spawn_context_task(task);
            Ok(())
        }))
    }

    fn schedule_periodic<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: CloneableMessage,
        initial_delay: Duration,
        interval: Duration,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.own_schedule_input(target);
        let msg = self.own_schedule_input(msg);
        Box::pin(parrot_api::trace::capture(async move {
            let task = self
                .tasks
                .periodic(target.take(), msg.take(), initial_delay, interval)?;
            self.spawn_context_task(task);
            Ok(())
        }))
    }

    fn watch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.own_schedule_input(target);
        let watcher = self.own_schedule_input(self.get_self_ref());
        Box::pin(crate::context_message::call(
            target,
            watcher,
            &self.tasks,
            |target, watcher| target.watch(watcher),
        ))
    }

    fn unwatch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.own_schedule_input(target);
        let watcher = self.own_schedule_input(self.get_self_ref());
        Box::pin(crate::context_message::call(
            target,
            watcher,
            &self.tasks,
            |target, watcher| target.unwatch(watcher),
        ))
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
        if let Some(task) = self.idle.configure(timeout, &self.tasks) {
            self.spawn_context_task(task);
        }
    }

    fn receive_timeout(&self) -> Option<Duration> {
        self.idle.get()
    }

    // ---------------- K2 receptionist 转发（DEV_02 §3.1 依赖注入） ----------------

    fn receptionist_register(
        &mut self,
        key: parrot_api::receptionist::ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        let gw = self.system().and_then(|s| s.receptionist_gateway());
        let scope = gw
            .as_ref()
            .map(|_| self.tasks.registration_scope(&self.runtime_handle));
        let path = self.path.path().to_string();
        Box::pin(async move {
            match (gw, scope) {
                (Some(g), Some(scope)) => g.register_scoped(key, path, scope).await,
                _ => Err(ActorError::InternalError("receptionist not enabled".into())),
            }
        })
    }

    fn receptionist_deregister(
        &mut self,
        key: &parrot_api::receptionist::ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        let gw = self.system().and_then(|s| s.receptionist_gateway());
        let scope = gw
            .as_ref()
            .map(|_| self.tasks.registration_scope(&self.runtime_handle));
        let key = key.clone();
        let path = self.path.path().to_string();
        Box::pin(async move {
            match (gw, scope) {
                (Some(g), Some(scope)) => g.deregister_scoped(key, path, scope).await,
                _ => Err(ActorError::InternalError("receptionist not enabled".into())),
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
        self.owned_children.set_strategy(strategy.clone());
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
        &mut self.stream_registry
    }

    fn spawner(&mut self) -> &mut dyn ActorSpawner {
        self
    }
    fn async_spawner(&self) -> Option<&dyn parrot_api::engine::AsyncActorSpawner> {
        Some(self)
    }
}

impl<A: Actor + Send + Sync + 'static> ActorContextScheduler for ThreadContext<A> {
    fn schedule<'life0, 'async_trait, M>(
        &'life0 self,
        msg: M,
        delay: Duration,
        interval: Option<Duration>,
    ) -> BoxedFuture<'async_trait, ActorResult<ScheduledTask>>
    where
        M: Message + Clone + Sync + 'async_trait,
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let msg = self.own_schedule_input(msg);
        Box::pin(parrot_api::trace::capture(async move {
            // Validate both clock domains before reserving a slot or launching
            // a timer. A rejected input must be fully retired before returning.
            let schedule_time = match std::time::SystemTime::now().checked_add(delay) {
                Some(time) => time,
                None => {
                    msg.retire().await?;
                    return Err(ActorError::InternalError(
                        "scheduled time exceeds clock range".into(),
                    ));
                }
            };
            // Generated references are owned inputs too. Keep each one
            // outside callback unwinding, and retire all inputs before reporting
            // a rejected registration. Callback failure is the primary error.
            let target = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.get_self_ref()
            })) {
                Ok(target) => self.own_schedule_input(target),
                Err(payload) => {
                    let error = ActorError::Panic(crate::owned_payload::panic_message(payload));
                    let _ = msg.retire().await;
                    return Err(error);
                }
            };
            let target_for_task =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    target.value().clone_boxed()
                })) {
                    Ok(target) => target,
                    Err(payload) => {
                        let error = ActorError::Panic(crate::owned_payload::panic_message(payload));
                        let _ = tokio::join!(target.retire(), msg.retire());
                        return Err(error);
                    }
                };
            let registration = match interval {
                Some(interval) => self.tasks.periodic_with_id(
                    target_for_task,
                    CloneableMessage::from_message(msg.take()),
                    delay,
                    interval,
                ),
                None => self
                    .tasks
                    .once_with_id(target_for_task, Box::new(msg.take()), delay),
            };
            let (id, task) = match registration {
                Ok(registered) => registered,
                Err(error) => {
                    let _ = target.retire().await;
                    return Err(error);
                }
            };
            let target: Arc<dyn parrot_api::address::ActorRef> = Arc::from(target.take());
            self.spawn_context_task(task);
            let tasks = self.tasks.clone();
            let cancel = Arc::new(move || tasks.cancel_schedule(id)) as Arc<dyn Fn() + Send + Sync>;
            Ok(ScheduledTask::with_cancel(
                id,
                target,
                schedule_time,
                cancel,
            ))
        }))
    }

    fn cancel_schedule(&self, task: ScheduledTask) {
        if task.target.path() == self.path.path() {
            task.cancel();
        }
    }
}

#[async_trait::async_trait]
impl<A: Actor + Send + Sync + 'static> ActorSpawner for ThreadContext<A> {
    fn spawn_factory_erased<'a>(
        &'a self,
        factory: BoxedMessage,
        strategy: Option<SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let system = self.system();
        let slots = system
            .as_ref()
            .map(|system| system.cleanup_slots.clone())
            .unwrap_or_else(|| Arc::new(tokio::sync::Semaphore::new(1)));
        let factory =
            crate::owned_payload::OwnedPayload::new(factory, &self.runtime_handle, &slots);
        let strategy =
            crate::owned_payload::OwnedPayload::new(strategy, &self.runtime_handle, &slots);
        Box::pin(async move {
            let error = if self.mailbox.is_none() || system.is_none() {
                Some(ActorError::InitializationError(
                    "Thread spawner is not bound to a live managed actor".into(),
                ))
            } else if !factory
                .value()
                .is::<crate::thread::factory::ErasedChildFactory>()
            {
                Some(ActorError::InitializationError(
                    "Thread factory payload type mismatch".into(),
                ))
            } else {
                None
            };
            if let Some(error) = error {
                let (factory, strategy) = tokio::join!(factory.retire(), strategy.retire());
                factory.and(strategy)?;
                return Err(error);
            }
            let factory = factory
                .take()
                .downcast::<crate::thread::factory::ErasedChildFactory>()
                .expect("validated Thread factory payload");
            factory
                .spawn(
                    system.expect("checked live system"),
                    self.path.target.clone_boxed(),
                    self.owned_children.clone(),
                    strategy.take(),
                )
                .await
        })
    }

    fn spawn<'a>(
        &'a self,
        actor: BoxedMessage,
        config: BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        self.spawn_instance_child(actor, config, None)
    }
    fn spawn_with_strategy<'a>(
        &'a self,
        actor: BoxedMessage,
        config: BoxedMessage,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        self.spawn_instance_child(actor, config, Some(strategy))
    }
}

impl<A: Actor + Send + Sync + 'static> parrot_api::engine::AsyncActorSpawner for ThreadContext<A> {
    fn spawn<'a>(
        &'a self,
        actor: parrot_api::engine::RuntimeActor,
        config: parrot_api::engine::InstanceConfig,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let actor = (super::system::BoxedThreadActor::new)(actor);
        let payload = <ThreadContext<super::system::BoxedThreadActor> as parrot_api::context::ActorSpawnContext<super::system::BoxedThreadActor>>::wrap_spawn(actor);
        match strategy {
            Some(strategy) => {
                ActorSpawner::spawn_with_strategy(self, payload, Box::new(config), strategy)
            }
            None => ActorSpawner::spawn(self, payload, Box::new(config)),
        }
    }
    fn spawn_factory<'a>(
        &'a self,
        factory: parrot_api::engine::RuntimeFactory,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let payload = <ThreadContext<super::system::BoxedThreadActor> as parrot_api::context::ActorFactoryContext<super::system::BoxedThreadActor>>::wrap_factory(move || (super::system::BoxedThreadActor::new)(factory()));
        ActorSpawner::spawn_factory_erased(self, payload, strategy)
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

    #[tokio::test]
    async fn detached_stream_registry_rejects_registration_without_a_mailbox() {
        use parrot_api::context::ActorContext as _;
        let mut ctx: ThreadContext<DummyActor> = ThreadContext::new_for_test("test/stream");
        let stream: Box<dyn futures::Stream<Item = Box<dyn std::any::Any + Send>> + Send> =
            Box::new(futures::stream::empty());
        let result = ctx.stream_registry().add_stream_erased(stream);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("stream registry"));
    }
}
