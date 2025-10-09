//! # Thread Actor System
//!
//! This module provides the core implementation of the thread-based actor system.
//! `ThreadActorSystem` is the central component that manages actor lifecycles,
//! maintains the actor registry, and coordinates the different scheduler implementations.
//!
//! ## Key Concepts
//! - Actor registry: Central storage of actor references and mailboxes
//! - Actor lifecycle management: Spawning, stopping, and monitoring actors
//! - Scheduler coordination: Managing shared and dedicated thread pools
//! - System shutdown: Graceful termination of all actors and threads
//!
//! ## Design Principles
//! - Thread safety: Uses Arc, RwLock, and atomic types for concurrent access
//! - Resource efficiency: Proper cleanup of actors and threads
//! - Flexibility: Support for different actor scheduling strategies
//! - Reliability: Robust error handling and supervision

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tracing::info;

use parrot_api::actor::Actor;
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::context::ActorContext;
use parrot_api::errors::ActorError;
use parrot_api::system::{
    ActorSystem, ActorSystemConfig, SystemResources, SystemState, SystemStatus,
};
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};

use crate::thread::actor::ThreadActor;
use crate::thread::address::ThreadActorRef;
use crate::thread::config::{
    BackpressureStrategy, SchedulingMode, SupervisorStrategy, ThreadActorConfig,
    ThreadActorSystemConfig,
};
use crate::thread::context::ThreadContext;
use crate::thread::error::{SpawnError, SystemError};
use crate::thread::mailbox::Mailbox;
use crate::thread::mailbox::WeakMailboxRef;
use crate::thread::mailbox::mpsc::MpscMailbox;
use crate::thread::mailbox::spsc_ringbuf::SpscRingbufMailbox;
use crate::thread::processor::ActorProcessor;
use crate::thread::scheduler::SchedulerGroup;
use crate::thread::scheduler::ThreadScheduler;
use crate::thread::scheduler::ThreadSchedulerFactory;
use crate::thread::scheduler::dedicated_thread::TypedThreadSchedulerExt;
use parrot_api::message::Message;

/// Cloneable outcome shared by concurrent stop callers.
#[derive(Clone)]
pub(crate) enum StopOutcome {
    Stopped,
    TimedOut(String),
    Failed(String),
}
impl StopOutcome {
    pub(crate) fn from_result(result: Result<(), SystemError>) -> Self {
        match result {
            Ok(()) => Self::Stopped,
            Err(SystemError::Timeout(message)) => Self::TimedOut(message),
            Err(error) => Self::Failed(error.to_string()),
        }
    }
    fn into_result(self) -> Result<(), SystemError> {
        match self {
            Self::Stopped => Ok(()),
            Self::TimedOut(message) => Err(SystemError::Timeout(message)),
            Self::Failed(message) => Err(SystemError::ShutdownError(message)),
        }
    }
}
pub(crate) type StopCompletion = tokio::sync::watch::Receiver<Option<StopOutcome>>;

pub(crate) async fn await_stop(mut completion: StopCompletion) -> Result<(), SystemError> {
    loop {
        if let Some(outcome) = completion.borrow_and_update().clone() {
            return outcome.into_result();
        }
        completion.changed().await.map_err(|_| {
            SystemError::ShutdownError("stop task ended without reporting completion".into())
        })?;
    }
}

/// Draining may invoke arbitrary user destructors. Keep cleanup owned even if
/// its caller reaches a deadline; do not run a late before_stop along with it.
pub(crate) fn drain_mailbox(
    mailbox: Arc<dyn Mailbox>,
    slots: Arc<tokio::sync::Semaphore>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let permit = slots.clone().acquire_owned().await?;
        crate::thread::scheduler::shared::poll_isolated(
            async move {
                mailbox.close().await;
                Ok(())
            },
            permit,
            slots,
            Arc::new(Notify::new()),
        )
        .await
    })
}

/// Entry in the actor registry
pub(crate) struct DynamicActorEntry {
    /// Reference to the actor for sending messages
    actor_ref: Arc<dyn ActorRef>,
    /// Logical identity; reused only by supervised replacements.
    address_cell: Arc<Mutex<WeakMailboxRef>>,

    /// Reference to the actor's mailbox
    pub(crate) mailbox: Arc<dyn Mailbox>,

    /// Actor configuration
    pub(crate) config: ThreadActorConfig,
    pub(crate) supervision: Option<Arc<crate::thread::supervisor_exec::SupervisionEntry>>,
    pub(crate) restart_cancel: Option<Arc<crate::thread::supervisor_exec::RestartControl>>,
    /// Kept while stopping so duplicate spawns cannot reuse this generation.
    pub(crate) stop_completion: Option<StopCompletion>,
}

pub(crate) enum ActorRegistryEntry {
    Dynamic(DynamicActorEntry),
    Typed(crate::thread::typed::TypedRegistration),
}
impl ActorRegistryEntry {
    pub(crate) fn dynamic(&self) -> Option<&DynamicActorEntry> {
        match self {
            Self::Dynamic(entry) => Some(entry),
            Self::Typed(_) => None,
        }
    }
    pub(crate) fn dynamic_mut(&mut self) -> Option<&mut DynamicActorEntry> {
        match self {
            Self::Dynamic(entry) => Some(entry),
            Self::Typed(_) => None,
        }
    }
    fn actor_ref(&self) -> Arc<dyn ActorRef> {
        match self {
            Self::Dynamic(entry) => entry.actor_ref.clone(),
            Self::Typed(entry) => entry.actor_ref.clone(),
        }
    }
}
impl fmt::Debug for ActorRegistryEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActorRegistryEntry")
            .field("path", &self.actor_ref().path())
            .finish()
    }
}

/// Watch request sent through the registry to register interest in termination.
#[derive(Debug, Clone)]
pub(crate) struct WatchRequest {
    pub watcher_path: String,
}

/// Public alias for the erased spawn box so supervision factories can name
/// their return type without touching private plumbing.
pub type ErasedSpawnBoxTyped = ErasedSpawnBox;

/// Unwatch request removing a previously registered watcher.
#[derive(Debug, Clone)]
pub(crate) struct UnwatchRequest {
    pub watcher_path: String,
}

/// Death notification delivered to watchers when an actor terminates.
///
/// M3 (semantic change #2): now public and carrying a `DeathReason` so
/// DeathWatch consumers can pattern-match on how the actor died
/// (parity GAP-1 closed).
#[derive(Debug, Clone)]
pub struct Terminated {
    pub path: String,
    pub reason: parrot_api::supervisor::DeathReason,
}
impl Message for Terminated {
    type Result = ();
}

/// Thread-based implementation of the Parrot actor system.
///
/// The system is always handled through `Arc<ThreadActorSystem>`; a
/// `Weak<ThreadActorSystem>` is distributed to contexts so actors can spawn
/// children and register watches without creating reference cycles.
pub struct ThreadActorSystem {
    /// System configuration
    config: Arc<ThreadActorSystemConfig>,

    /// Registry of all active actors
    pub(crate) registry: Arc<RwLock<HashMap<String, ActorRegistryEntry>>>,

    /// Scheduler group (shared pool + dedicated threads)
    pub(crate) scheduler_group: Arc<SchedulerGroup>,

    pub(crate) typed_slots: Arc<tokio::sync::Semaphore>,

    /// Runtime handle for spawning async tasks
    pub(crate) runtime_handle: Handle,

    /// Signal for system shutdown
    shutdown_signal: Arc<Notify>,
    shutdown_completion: Mutex<Option<StopCompletion>>,

    /// Flag indicating if the system is shutting down
    is_shutting_down: Arc<AtomicBool>,

    /// Watch registry: watched path -> watcher paths
    watch_registry: Arc<Mutex<HashMap<String, Vec<String>>>>,

    /// System start time for uptime reporting
    started_at: Instant,

    /// Weak self reference used by contexts (set right after construction)
    self_weak: Mutex<Option<Weak<ThreadActorSystem>>>,

    /// K2 receptionist 网关（可选——facade 注册后注入；context 三方法出口）
    receptionist: RwLock<Option<Arc<dyn crate::system::ReceptionistGateway>>>,
}

impl fmt::Debug for ThreadActorSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let registry = self.registry.read().unwrap();
        f.debug_struct("ThreadActorSystem")
            .field("config", &self.config)
            .field("actor_count", &registry.len())
            .field(
                "is_shutting_down",
                &self.is_shutting_down.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl ThreadActorSystem {
    /// Create a new thread actor system.
    ///
    /// After construction call [`ThreadActorSystem::init`] once to install
    /// the weak self reference; spawns go through [`ThreadActorSystem::shared`]
    /// which returns an `Arc`.
    pub fn new(config: ThreadActorSystemConfig) -> Self {
        Self::with_runtime_handle(config, Handle::current())
    }

    /// Create a new system bound to a specific runtime handle.
    pub fn with_runtime_handle(config: ThreadActorSystemConfig, runtime_handle: Handle) -> Self {
        let config = Arc::new(config);

        let factory = ThreadSchedulerFactory::new(runtime_handle.clone());
        let pool_config = crate::thread::scheduler::shared::SharedThreadPoolConfig {
            pool_size: config.shared_pool_size,
            burst_workers_max: config.shared_burst_workers_max,
            burst_backlog_threshold: std::time::Duration::from_millis(
                config.shared_burst_backlog_threshold_ms,
            ),
            burst_idle_timeout: std::time::Duration::from_millis(
                config.shared_burst_idle_timeout_ms,
            ),
            ..Default::default()
        };
        let dedicated_config = crate::thread::scheduler::dedicated_thread::DedicatedThreadConfig {
            max_threads: config.max_dedicated_threads,
            ..Default::default()
        };
        let scheduler_group =
            Arc::new(factory.create_scheduler_group(Some(pool_config), Some(dedicated_config)));

        Self {
            typed_slots: Arc::new(tokio::sync::Semaphore::new(config.shared_pool_size.max(1))),
            config,
            registry: Arc::new(RwLock::new(HashMap::new())),
            scheduler_group,
            runtime_handle,
            shutdown_signal: Arc::new(Notify::new()),
            shutdown_completion: Mutex::new(None),
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            watch_registry: Arc::new(Mutex::new(HashMap::new())),
            started_at: Instant::now(),
            self_weak: Mutex::new(None),
            receptionist: RwLock::new(None),
        }
    }

    /// K2：注入 receptionist 网关（facade 注册时下发）。
    pub fn set_receptionist_gateway(&self, gw: Arc<dyn crate::system::ReceptionistGateway>) {
        *self.receptionist.write().unwrap() = Some(gw);
    }

    /// K2：receptionist 网关句柄（context 转发用）。
    pub fn receptionist_gateway(&self) -> Option<Arc<dyn crate::system::ReceptionistGateway>> {
        self.receptionist.read().unwrap().clone()
    }

    /// Create and initialize a shared (`Arc`) system.
    pub fn shared(config: ThreadActorSystemConfig) -> Arc<Self> {
        let system = Arc::new(Self::new(config));
        system.set_self_weak(Arc::downgrade(&system));
        system
    }

    /// Create and initialize a shared system bound to a runtime handle.
    pub fn shared_with_handle(
        config: ThreadActorSystemConfig,
        runtime_handle: Handle,
    ) -> Arc<Self> {
        let system = Arc::new(Self::with_runtime_handle(config, runtime_handle));
        system.set_self_weak(Arc::downgrade(&system));
        system
    }

    /// Install the weak self reference. Call once after wrapping in Arc.
    ///
    /// `shared()` does this automatically; if you constructed the system via
    /// `ActorSystem::start`, wrap it in an `Arc` and call
    /// `set_self_weak(Arc::downgrade(&arc))` yourself.
    pub fn set_self_weak(&self, weak: Weak<ThreadActorSystem>) {
        *self.self_weak.lock().unwrap() = Some(weak);
    }

    /// Get the system configuration.
    pub fn config(&self) -> &ThreadActorSystemConfig {
        &self.config
    }

    /// Get the runtime handle.
    pub fn runtime_handle(&self) -> &Handle {
        &self.runtime_handle
    }

    /// Get the weak system reference, if initialized.
    pub(crate) fn self_weak(&self) -> Option<Weak<ThreadActorSystem>> {
        self.self_weak.lock().unwrap().clone()
    }

    /// Upgrade the weak self reference.
    #[allow(dead_code)] // 预留：自引用升级入口
    pub(crate) fn self_arc(&self) -> Option<Arc<ThreadActorSystem>> {
        self.self_weak().and_then(|w| w.upgrade())
    }

    /// Get the scheduler group.
    pub fn scheduler_group(&self) -> &Arc<SchedulerGroup> {
        &self.scheduler_group
    }

    /// Whether the system is shutting down.
    pub fn is_shutting_down(&self) -> bool {
        self.is_shutting_down.load(Ordering::Relaxed)
    }

    /// M3 supervision plumbing: spawn the supervision decision for a
    /// panicked actor as a detached task on the system runtime.
    pub(crate) fn runtime_spawn_supervision(
        self: &Arc<Self>,
        path: String,
        mailbox: WeakMailboxRef,
        panic_msg: String,
    ) {
        let system = self.clone();
        self.runtime_handle.spawn(async move {
            system.on_child_panic(&path, mailbox, panic_msg).await;
        });
    }

    /// Register a watch: `watcher_path` wants notifications about `watched_path`.
    ///
    /// Idempotent (matching Akka's `context.watch` semantics): registering
    /// the same (watcher, watched) pair twice does not duplicate the
    /// termination notification.
    pub async fn watch(
        &self,
        watcher_path: String,
        watched_path: String,
    ) -> Result<(), ActorError> {
        let mut registry = self.watch_registry.lock().unwrap();
        let watchers = registry.entry(watched_path).or_default();
        if !watchers.contains(&watcher_path) {
            watchers.push(watcher_path);
        }
        Ok(())
    }

    /// Remove a previously registered watch.
    pub async fn unwatch(
        &self,
        watcher_path: String,
        watched_path: String,
    ) -> Result<(), ActorError> {
        let mut registry = self.watch_registry.lock().unwrap();
        if let Some(watchers) = registry.get_mut(&watched_path) {
            watchers.retain(|w| w != &watcher_path);
            if watchers.is_empty() {
                registry.remove(&watched_path);
            }
        }
        Ok(())
    }

    /// Notify all watchers under the caller's operation deadline.
    pub(crate) async fn notify_termination_until(
        &self,
        path: &str,
        reason: parrot_api::supervisor::DeathReason,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        let watchers = {
            let mut registry = self.watch_registry.lock().unwrap();
            registry.remove(path).unwrap_or_default()
        };

        if watchers.is_empty() {
            return Ok(());
        }

        let targets = {
            let registry = self.registry.read().unwrap();
            watchers
                .iter()
                .filter_map(|path| registry.get(path))
                .map(|entry| {
                    (
                        entry.actor_ref(),
                        entry.dynamic().map(|entry| entry.mailbox.clone()),
                    )
                })
                .collect::<Vec<_>>()
        };
        let deliveries = targets.into_iter().map(|(actor_ref, mailbox)| {
            let notification = Box::new(Terminated {
                path: path.to_string(),
                reason: reason.clone(),
            }) as BoxedMessage;
            async move {
                tokio::time::timeout_at(deadline, async {
                    match mailbox {
                        Some(mailbox) => mailbox
                            .push_with_priority(notification, BackpressureStrategy::Block, true)
                            .await
                            .map(|_| ())
                            .map_err(|error| SystemError::ShutdownError(error.to_string())),
                        None => actor_ref
                            .deliver(notification)
                            .await
                            .map_err(|error| SystemError::ShutdownError(error.to_string())),
                    }
                })
                .await
                .map_err(|_| {
                    SystemError::Timeout(format!(
                        "termination notification to {} exceeded configured deadline",
                        actor_ref.path()
                    ))
                })?
            }
        });
        let results: Vec<Result<(), SystemError>> = futures::future::join_all(deliveries).await;
        results.into_iter().collect()
    }

    /// Spawn a typed root actor on this system (returns a typed ThreadActorRef).
    ///
    /// This is the engine-specific entry point with the required context bound.
    pub async fn spawn_root_typed_thread<A>(
        self: &Arc<Self>,
        actor: A,
        _config: A::Config,
    ) -> Result<ThreadActorRef<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        let thread_config = ThreadActorConfig::default();
        self.spawn_at::<A>(
            actor,
            &format!("/user/{}", uuid_v4_simple()),
            None,
            thread_config,
        )
        .await
    }

    /// Spawn a typed root actor with an explicit thread-engine config.
    pub async fn spawn_typed_with_config<A>(
        self: &Arc<Self>,
        actor: A,
        _config: A::Config,
        thread_config: ThreadActorConfig,
    ) -> Result<ThreadActorRef<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        if self.is_shutting_down() {
            return Err(SpawnError::SchedulerError("System is shutting down".into()));
        }

        let path_str = format!("/user/{}", uuid_v4_simple());
        self.spawn_at::<A>(actor, &path_str, None, thread_config)
            .await
    }

    /// Spawn a typed actor at an explicit path with an optional parent.
    pub async fn spawn_at<A>(
        self: &Arc<Self>,
        actor: A,
        path: &str,
        parent: Option<BoxedActorRef>,
        thread_config: ThreadActorConfig,
    ) -> Result<ThreadActorRef<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        self.spawn_at_internal(actor, path, parent, thread_config, None, None)
    }

    pub(crate) fn spawn_at_internal<A>(
        self: &Arc<Self>,
        actor: A,
        path: &str,
        parent: Option<BoxedActorRef>,
        thread_config: ThreadActorConfig,
        supervision: Option<Arc<crate::thread::supervisor_exec::SupervisionEntry>>,
        replacing: Option<&Arc<dyn Mailbox>>,
    ) -> Result<ThreadActorRef<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        if self.is_shutting_down() {
            return Err(SpawnError::SchedulerError("System is shutting down".into()));
        }

        // Fast existence check; registration and scheduling are committed
        // together below, before a worker can observe an unregistered actor.
        {
            let registry = self.registry.read().unwrap();
            if replacing.is_none() && registry.contains_key(path) {
                return Err(SpawnError::ActorPathAlreadyExists(path.to_string()));
            }
        }

        // 1. Create mailbox + typed actor ref first (ref holds only weak mailbox).
        let actor_ref = ThreadActorRef::<A>::new(
            ActorPath::for_test(path),
            Weak::<MpscMailbox>::new() as WeakMailboxRef,
            thread_config
                .backpressure_strategy
                .clone()
                .unwrap_or(self.config.default_backpressure_strategy.clone()),
            thread_config
                .ask_timeout
                .unwrap_or(self.config.default_ask_timeout),
            None,
        )
        .with_system(Arc::downgrade(self));
        let actor_ref = Arc::new(match &supervision {
            Some(entry) => actor_ref.with_mailbox_cell(entry.mailbox.clone()),
            None => actor_ref,
        });

        let actor_path = ActorPath {
            target: actor_ref.clone() as parrot_api::types::WeakActorTarget,
            path: path.to_string(),
        };

        // 2. Create the mailbox bound to the typed ref.
        let capacity = thread_config
            .mailbox_capacity
            .unwrap_or(self.config.default_mailbox_capacity);
        let mode = thread_config
            .scheduling_mode
            .clone()
            .unwrap_or_else(|| self.config.default_scheduling_mode.clone());
        let mailbox: Arc<dyn Mailbox> = match &mode {
            SchedulingMode::DedicatedThread => {
                Arc::new(SpscRingbufMailbox::new(capacity, actor_path.clone()))
            }
            _ => Arc::new(MpscMailbox::new(capacity, actor_path.clone())),
        };

        // Fix up the actor ref's weak mailbox pointer now that mailbox exists.
        if supervision.is_none() {
            actor_ref.set_mailbox(Arc::downgrade(&mailbox) as WeakMailboxRef);
        }

        // 3. Create context + processor.
        let system_weak = self.self_weak().ok_or_else(|| {
            SpawnError::SchedulerError("System self reference not initialized".into())
        })?;

        let mut context = ThreadContext::<A>::new(
            system_weak.clone(),
            self.runtime_handle.clone(),
            actor_path.clone(),
            parent,
            thread_config
                .supervisor_strategy
                .clone()
                .unwrap_or_else(|| self.config.default_supervisor_strategy.clone()),
        );

        context.set_self_ref(actor_ref.clone_boxed());
        context.set_mailbox(Arc::downgrade(&mailbox) as WeakMailboxRef);

        let processor = Arc::new(ActorProcessor::<A>::new(
            ThreadActor::new(actor, actor_path.clone()),
            context,
            path.to_string(),
            thread_config.clone(),
        ));

        mailbox.set_processor(processor.clone());

        // Publish registration before scheduling so init/panic callbacks can
        // find their actor. Both operations are synchronous under this guard:
        // no duplicate spawn or shutdown drain can interleave between them.
        {
            let mut registry = self.registry.write().unwrap();
            if self.is_shutting_down() {
                return Err(SpawnError::SystemShutdown);
            }
            if let Some(expected) = replacing {
                let valid = registry
                    .get(path)
                    .and_then(ActorRegistryEntry::dynamic)
                    .is_some_and(|entry| {
                        Arc::ptr_eq(expected, &entry.mailbox)
                            && entry
                                .restart_cancel
                                .as_ref()
                                .is_some_and(|control| !control.is_cancelled())
                    });
                if !valid {
                    return Err(SpawnError::SystemShutdown);
                }
            } else if registry.contains_key(path) {
                return Err(SpawnError::ActorPathAlreadyExists(path.to_string()));
            }
            // Lock the logical address through publication and scheduling.
            // Existing references cannot observe an unscheduled generation.
            let mut cell = supervision
                .as_ref()
                .map(|entry| entry.mailbox.lock().unwrap());
            let old_mailbox = cell.as_mut().map(|cell| {
                std::mem::replace(&mut **cell, Arc::downgrade(&mailbox) as WeakMailboxRef)
            });
            let previous = registry.insert(
                path.to_string(),
                ActorRegistryEntry::Dynamic(DynamicActorEntry {
                    actor_ref: actor_ref.clone() as Arc<dyn ActorRef>,
                    address_cell: actor_ref.mailbox_cell(),
                    mailbox: mailbox.clone(),
                    config: thread_config.clone(),
                    stop_completion: None,
                    supervision: supervision.clone(),
                    restart_cancel: None,
                }),
            );
            let mode = thread_config
                .scheduling_mode
                .clone()
                .unwrap_or_else(|| self.config.default_scheduling_mode.clone());
            let scheduled = match &mode {
                SchedulingMode::DedicatedThread => self
                    .scheduler_group
                    .dedicated_scheduler
                    .schedule_typed_by_processor::<A>(
                        path,
                        mailbox.clone(),
                        processor,
                        thread_config.clone(),
                    )
                    .map_err(|error| SpawnError::SchedulerError(error.to_string())),
                SchedulingMode::Sharded { affinity_key } => self
                    .scheduler_group
                    .sharded()
                    .schedule_affinity(path, mailbox.clone(), affinity_key)
                    .map_err(SpawnError::SchedulerError),
                SchedulingMode::SharedPool { .. } => self
                    .scheduler_group
                    .shared_scheduler
                    .schedule_now(path, mailbox.clone(), Some(thread_config.clone()))
                    .map_err(|error| SpawnError::SchedulerError(error.to_string())),
            };
            if let Err(error) = scheduled {
                registry.remove(path);
                if let Some(previous) = previous {
                    registry.insert(path.to_owned(), previous);
                }
                if let (Some(cell), Some(old)) = (cell.as_mut(), old_mailbox) {
                    **cell = old;
                }
                return Err(error);
            }
        }

        Ok((*actor_ref).clone())
    }

    /// M3: spawn a supervised child with a respawn factory.
    ///
    /// The factory produces fresh actor instances for restarts (Akka
    /// "props" semantics). On panic, the supervision state machine decides
    /// Restart / Stop / Escalate (see `supervisor_exec.rs`); a Restart
    /// consumes one unit of the strategy's windowed budget.
    pub async fn spawn_supervised<A, F>(
        self: &Arc<Self>,
        factory: F,
        path: &str,
        strategy: SupervisorStrategy,
    ) -> Result<ThreadActorRef<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_supervised_with_config(
            factory,
            path,
            ThreadActorConfig {
                supervisor_strategy: Some(strategy),
                ..Default::default()
            },
        )
        .await
    }

    /// Spawn a supervised logical actor. All references and watches survive
    /// successful restarts, and each generation retains this configuration.
    /// Messages queued in a failed generation are discarded, not replayed.
    /// Restart factories run in isolation under the configured shutdown deadline.
    pub async fn spawn_supervised_with_config<A, F>(
        self: &Arc<Self>,
        factory: F,
        path: &str,
        thread_config: ThreadActorConfig,
    ) -> Result<ThreadActorRef<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
        F: Fn() -> A + Send + Sync + 'static,
    {
        use crate::thread::supervisor_exec::{Respawn, SupervisionEntry};
        let first = factory();
        let factory = Arc::new(factory);
        let restart_path = path.to_owned();
        let restart_config = thread_config.clone();
        let respawn: Respawn = Arc::new(move |system, entry, old_mailbox| {
            let factory = factory.clone();
            let path = restart_path.clone();
            let config = restart_config.clone();
            Box::pin(async move {
                let slots = Arc::new(tokio::sync::Semaphore::new(1));
                let permit = slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|error| SystemError::ActorCreationError(error.to_string()))?;
                let actor = crate::thread::scheduler::shared::poll_isolated(
                    async move { Ok(factory()) },
                    permit,
                    slots,
                    Arc::new(Notify::new()),
                )
                .await
                .map_err(|error| SystemError::ActorCreationError(error.to_string()))?;
                system
                    .spawn_at_internal(actor, &path, None, config, Some(entry), Some(&old_mailbox))
                    .map_err(|error| SystemError::ActorCreationError(error.to_string()))?;
                Ok(())
            })
        });
        let supervision = Arc::new(SupervisionEntry {
            strategy: thread_config
                .supervisor_strategy
                .clone()
                .unwrap_or_else(|| self.config.default_supervisor_strategy.clone()),
            restarts: Mutex::new(Vec::new()),
            mailbox: Arc::new(Mutex::new(Weak::<MpscMailbox>::new() as WeakMailboxRef)),
            respawn,
        });
        self.spawn_at_internal(first, path, None, thread_config, Some(supervision), None)
    }

    /// Spawn an erased actor from boxed components (used by ThreadContext::spawn).
    pub async fn spawn_erased_actor(
        self: &Arc<Self>,
        actor: BoxedMessage,
        config: BoxedMessage,
        strategy: Option<SupervisorStrategy>,
    ) -> ActorResult<BoxedActorRef> {
        // The boxed actor payload is an `ErasedSpawnBox` (see
        // TypedSpawnPayload::into_boxed) erased to BoxedMessage.
        let payload = actor.downcast::<ErasedSpawnBox>().map_err(|_| {
            ActorError::InternalError("Spawn payload is not a boxed spawnable actor".into())
        })?;

        let mut thread_config = ThreadActorConfig::default();
        if let Some(strategy) = strategy {
            thread_config.supervisor_strategy = Some(strategy);
        }

        payload
            .0
            .spawn_on(self.clone(), config, thread_config)
            .await
    }

    /// Look up an actor ref by path.
    pub fn get_actor_ref(&self, path: &str) -> Option<Arc<dyn ActorRef>> {
        self.registry
            .read()
            .unwrap()
            .get(path)
            .map(ActorRegistryEntry::actor_ref)
    }

    /// Look up an actor's mailbox by path.
    pub fn get_mailbox(&self, path: &str) -> Option<Arc<dyn Mailbox>> {
        self.registry
            .read()
            .unwrap()
            .get(path)
            .and_then(|e| e.dynamic().map(|e| e.mailbox.clone()))
    }

    /// Stop an actor with the system's configured deadline. Concurrent callers
    /// await the same operation; cancelling a caller does not abandon cleanup.
    /// A notification error does not undo termination of the actor itself.
    pub async fn stop_actor(&self, path: &str) -> Result<(), SystemError> {
        await_stop(self.request_stop(path, None)?).await
    }

    /// Reserve an owned stop synchronously. Context self-stop only requests
    /// this operation: waiting for it inside a handler would deadlock the hook.
    /// Contexts supply their mailbox to reject obsolete actor generations.
    pub(crate) fn request_stop(
        &self,
        path: &str,
        expected: Option<&Arc<dyn Mailbox>>,
    ) -> Result<StopCompletion, SystemError> {
        self.request_stop_matching(path, |entry| {
            expected.is_none_or(|mailbox| {
                entry
                    .dynamic()
                    .is_some_and(|entry| Arc::ptr_eq(mailbox, &entry.mailbox))
            })
        })
    }

    /// References stop the logical actor atomically with restart publication.
    /// Context self-stop still uses its generation-specific mailbox above.
    pub(crate) fn request_stop_ref(
        &self,
        path: &str,
        address: &Arc<Mutex<WeakMailboxRef>>,
    ) -> Result<StopCompletion, SystemError> {
        self.request_stop_matching(path, |entry| {
            entry
                .dynamic()
                .is_some_and(|entry| Arc::ptr_eq(address, &entry.address_cell))
        })
    }

    fn request_stop_matching(
        &self,
        path: &str,
        matches: impl FnOnce(&ActorRegistryEntry) -> bool,
    ) -> Result<StopCompletion, SystemError> {
        let system = self.self_arc().ok_or(SystemError::NotRunning)?;
        let completion = {
            let mut registry = self.registry.write().unwrap();
            let entry = registry
                .get_mut(path)
                .ok_or_else(|| SystemError::ActorNotFound(path.into()))?;
            if !matches(entry) {
                return Err(SystemError::ActorNotFound(path.into()));
            }
            if let ActorRegistryEntry::Typed(entry) = entry {
                entry
                    .control
                    .request_stop(tokio::time::Instant::now() + self.config.shutdown_timeout);
                return Ok(entry.control.completion());
            }
            let entry = entry.dynamic_mut().expect("dynamic entry checked above");
            if let Some(completion) = &entry.stop_completion {
                if let Some(control) = &entry.restart_cancel {
                    control.cancel();
                }
                completion.clone()
            } else {
                let (sender, receiver) = tokio::sync::watch::channel(None);
                entry.stop_completion = Some(receiver.clone());
                let mailbox = entry.mailbox.clone();
                let mode = entry
                    .config
                    .scheduling_mode
                    .clone()
                    .unwrap_or_else(|| self.config.default_scheduling_mode.clone());
                mailbox.seal();
                let path = path.to_owned();
                let deadline = tokio::time::Instant::now() + self.config.shutdown_timeout;
                self.runtime_handle.spawn(async move {
                    let result = system
                        .stop_registered_actor(&path, mailbox.clone(), mode, deadline)
                        .await;
                    // The tombstone stays until descheduling and notifications
                    // complete. Never remove a replacement generation.
                    let mut registry = system.registry.write().unwrap();
                    if registry
                        .get(&path)
                        .and_then(ActorRegistryEntry::dynamic)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.mailbox, &mailbox))
                    {
                        registry.remove(&path);
                    }
                    sender.send_replace(Some(StopOutcome::from_result(result)));
                });
                receiver
            }
        };
        Ok(completion)
    }

    async fn stop_registered_actor(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        mode: SchedulingMode,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        mailbox.seal();
        let processor = mailbox.get_processor();
        if let Some(processor) = &processor {
            processor.cancel_context_tasks();
        }
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let drained = drain_mailbox(mailbox.clone(), slots.clone());
        let graceful = async {
            drained
                .await
                .map_err(|error| SystemError::ShutdownError(error.to_string()))?
                .map_err(|error| SystemError::ShutdownError(error.to_string()))?;
            if let Some(processor) = processor.clone() {
                self.scheduler_group
                    .stop_hook(path, &mode, processor, slots, deadline)
                    .await?;
            }
            Ok(())
        };
        let stopped = tokio::time::timeout_at(deadline, graceful)
            .await
            .unwrap_or_else(|_| {
                Err(SystemError::Timeout(format!(
                    "actor {path} stop exceeded configured deadline"
                )))
            });
        // Release suspended actor futures in all execution modes, including
        // a handler that prevented before_stop from acquiring the actor lock.
        if let Some(processor) = &processor {
            processor.cancel_processing();
        }
        let _ = self.scheduler_group.shared_scheduler.deschedule(path).await;
        if let Some(sharded) = self
            .scheduler_group
            .sharded_scheduler
            .lock()
            .unwrap()
            .as_ref()
        {
            let _ = sharded.deschedule(path);
        }
        let dedicated = self
            .scheduler_group
            .dedicated_scheduler
            .deschedule_until(path, deadline)
            .await;
        let notified = self
            .notify_termination_until(path, parrot_api::supervisor::DeathReason::Normal, deadline)
            .await;
        stopped?;
        match dedicated {
            Ok(()) | Err(SystemError::ActorNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        notified
    }

    /// Get actor count.
    pub fn actor_count(&self) -> usize {
        self.registry.read().unwrap().len()
    }

    /// List all registered actor paths (B2/DEV_09：admin-v2 Status/Stop
    /// 前缀匹配需要枚举 registry 键；只读快照，排序保证确定性).
    pub fn actor_paths(&self) -> Vec<String> {
        let registry = self.registry.read().unwrap();
        let mut paths: Vec<String> = registry.keys().cloned().collect();
        paths.sort();
        paths
    }

    /// Broadcast a message to all registered actors.
    pub async fn broadcast_message(&self, msg: BoxedMessage) -> Result<(), SystemError> {
        if self.is_shutting_down() {
            return Err(SystemError::ShuttingDown);
        }

        let targets = {
            let registry = self.registry.read().unwrap();
            registry
                .values()
                .map(|entry| {
                    (
                        entry.actor_ref(),
                        entry.dynamic().map(|entry| entry.mailbox.clone()),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (actor, mailbox) in targets {
            let message = msg_clone(&msg)?;
            match mailbox {
                Some(mailbox) => mailbox
                    .push(message, BackpressureStrategy::DropNewest)
                    .await
                    .map_err(|error| SystemError::ShutdownError(error.to_string()))?,
                None => tokio::time::timeout(self.config.shutdown_timeout, actor.deliver(message))
                    .await
                    .map_err(|_| {
                        SystemError::Timeout(format!("broadcast to {} timed out", actor.path()))
                    })?
                    .map_err(|error| SystemError::ShutdownError(error.to_string()))?,
            }
        }
        Ok(())
    }

    /// Internal shutdown: stop all actors, deschedule, close mailboxes.
    /// Snapshot of shared-pool scheduler metrics (incl. elastic burst
    /// workers). Returns `None` if the shared scheduler does not expose
    /// detailed metrics.
    pub fn scheduler_metrics(&self) -> Option<crate::thread::scheduler::shared::SchedulerMetrics> {
        // SharedThreadPool::metrics is on the concrete type; downcast via
        // as_any-free path: the scheduler group stores Arc<SharedThreadPool>.
        Some(self.scheduler_group.shared_scheduler.metrics())
    }

    /// Join one owned shutdown operation. Every caller observes completion;
    /// dropping a caller does not cancel resource cleanup or erase errors.
    pub async fn shutdown_internal(&self) -> Result<(), SystemError> {
        let completion = {
            let mut state = self.shutdown_completion.lock().unwrap();
            if let Some(completion) = state.as_ref() {
                completion.clone()
            } else {
                self.is_shutting_down.store(true, Ordering::SeqCst);
                let (sender, receiver) = tokio::sync::watch::channel(None);
                let work = self
                    .shutdown_resources(tokio::time::Instant::now() + self.config.shutdown_timeout);
                self.runtime_handle.spawn(async move {
                    sender.send_replace(Some(StopOutcome::from_result(work.await)));
                });
                *state = Some(receiver.clone());
                receiver
            }
        };
        await_stop(completion).await
    }

    fn shutdown_resources(
        &self,
        deadline: tokio::time::Instant,
    ) -> parrot_api::types::BoxedFuture<'static, Result<(), SystemError>> {
        let registry = self.registry.clone();
        let config = self.config.clone();
        let scheduler_group = self.scheduler_group.clone();
        let watch_registry = self.watch_registry.clone();
        let shutdown_signal = self.shutdown_signal.clone();
        Box::pin(async move {
            info!("ThreadActorSystem shutting down");

            // Remove all entries.
            let entries: Vec<(String, ActorRegistryEntry)> = {
                let mut registry = registry.write().unwrap();
                registry.drain().collect()
            };

            // Seal immediately without invoking user message destructors on
            // the runtime thread. Owned drain tasks do disposal below.
            for (_, entry) in &entries {
                if let ActorRegistryEntry::Typed(entry) = entry {
                    entry.control.request_stop(deadline);
                    continue;
                }
                let entry = entry.dynamic().unwrap();
                if let Some(control) = &entry.restart_cancel {
                    control.cancel();
                }
                entry.mailbox.seal();
                if let Some(processor) = entry.mailbox.get_processor() {
                    processor.cancel_context_tasks();
                }
            }

            // Stop independently: a wedged actor must not prevent other actors'
            // lifecycle hooks from running. All phases share ONE configured deadline.
            let stop_slots = Arc::new(tokio::sync::Semaphore::new(config.shared_pool_size.max(1)));
            let stops = entries.iter().map(|(path, entry)| {
                let slots = stop_slots.clone();
                let schedulers = scheduler_group.clone();
                let dynamic = entry.dynamic();
                let mode = dynamic
                    .and_then(|entry| entry.config.scheduling_mode.clone())
                    .unwrap_or_else(|| config.default_scheduling_mode.clone());
                let completion = match entry {
                    ActorRegistryEntry::Dynamic(entry) => entry.stop_completion.clone(),
                    ActorRegistryEntry::Typed(entry) => Some(entry.control.completion()),
                };
                let processor = dynamic.and_then(|entry| entry.mailbox.get_processor());
                let drained = if completion.is_none() {
                    dynamic.map(|entry| drain_mailbox(entry.mailbox.clone(), slots.clone()))
                } else {
                    None
                };
                async move {
                    if let Some(completion) = completion {
                        return await_stop(completion).await;
                    }
                    if let Some(drained) = drained {
                        drained
                            .await
                            .map_err(|error| {
                                SystemError::ShutdownError(format!("mailbox {path}: {error}"))
                            })?
                            .map_err(|error| {
                                SystemError::ShutdownError(format!("mailbox {path}: {error}"))
                            })?;
                    }
                    let Some(processor) = processor else {
                        return Ok(());
                    };
                    schedulers
                        .stop_hook(path, &mode, processor, slots, deadline)
                        .await
                }
            });
            let stopped = tokio::time::timeout_at(deadline, futures::future::join_all(stops)).await;
            let timed_out = stopped.is_err();
            let stop_error = stopped
                .ok()
                .and_then(|results| results.into_iter().find_map(Result::err));

            for (_, entry) in &entries {
                if let Some(processor) = entry
                    .dynamic()
                    .and_then(|entry| entry.mailbox.get_processor())
                {
                    processor.cancel_processing();
                }
            }

            // Every scheduler receives cancellation even if the hook deadline has
            // elapsed. Async waits are cancelled; uncooperative synchronous code
            // cannot be forcibly terminated safely inside this process.
            let sharded = scheduler_group.sharded_scheduler.lock().unwrap().clone();
            let remaining_ms = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_millis() as u64;
            let (_, dedicated, shards) = tokio::join!(
                scheduler_group.shared_scheduler.shutdown(remaining_ms),
                scheduler_group.dedicated_scheduler.shutdown_until(deadline),
                async {
                    match sharded {
                        Some(scheduler) => scheduler.shutdown_until(deadline).await,
                        None => Ok(()),
                    }
                }
            );
            watch_registry.lock().unwrap().clear();

            shutdown_signal.notify_waiters();
            if timed_out {
                return Err(SystemError::Timeout(
                    "actor shutdown exceeded configured deadline".into(),
                ));
            }
            dedicated?;
            shards?;
            if let Some(error) = stop_error {
                return Err(error);
            }
            info!("ThreadActorSystem stopped ({} actors)", entries.len());
            Ok(())
        })
    }
}

/// Clone a boxed message via its Clone impl when available.
fn msg_clone(msg: &BoxedMessage) -> Result<BoxedMessage, SystemError> {
    // Box<dyn Any + Send> cannot be cloned generically; broadcast therefore
    // requires cloneable payloads (see CloneableMessage::try_from_boxed).
    match parrot_api::message::CloneableMessage::try_from_boxed(msg) {
        Some(cloneable) => Ok(cloneable.into_boxed()),
        None => Err(SystemError::Other(anyhow::anyhow!(
            "Broadcast requires a cloneable message payload"
        ))),
    }
}

/// Erased spawn payload: a boxed closure able to spawn on a system.
///
/// The system reference is captured as an `Arc` clone so the returned future
/// is 'static.
#[async_trait]
pub trait ErasedSpawnable: Send + Sync + 'static {
    async fn spawn_on(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        config: BoxedMessage,
        thread_config: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef>;

    /// Spawn at an explicit path (supervision respawn keeps the child path).
    async fn spawn_on_path(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        path: String,
        thread_config: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        // Default: ignore the path request, fall back to spawn_on.
        let _ = path;
        self.spawn_on(system, Box::new(()), thread_config).await
    }
}

/// Wrapper holding an erased spawnable so the outer box stays a concrete
/// type and can be downcast from `BoxedMessage` reliably.
///
/// (Double boxing like `Box<Box<dyn Trait>>` cannot be downcast back: the
/// unsize coercion to `Box<dyn Any + Send>` erases the inner trait object's
/// concrete `TypeId`.)
pub struct ErasedSpawnBox(Box<dyn ErasedSpawnable>);

impl std::fmt::Debug for ErasedSpawnBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedSpawnBox").finish()
    }
}

/// Erased spawn closure for a typed actor with ThreadContext.
pub struct TypedSpawnPayload<A> {
    actor: Option<A>,
}

impl<A> TypedSpawnPayload<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// Create an erased spawn payload wrapping a typed actor.
    pub fn new(actor: A) -> Self {
        Self { actor: Some(actor) }
    }

    /// Erase into a BoxedMessage ready for `spawn_erased_actor`.
    ///
    /// The concrete `ErasedSpawnBox` wrapper keeps the outer type downcastable.
    pub fn into_boxed(self) -> BoxedMessage {
        Box::new(ErasedSpawnBox(Box::new(self) as Box<dyn ErasedSpawnable>))
    }
}

/// Convenience constructor + spawn access for supervision factories.
impl ErasedSpawnBox {
    pub fn from_typed<A>(actor: A) -> Self
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        ErasedSpawnBox(Box::new(TypedSpawnPayload::new(actor)))
    }

    /// Spawn the wrapped actor at an explicit path (supervision respawn).
    pub async fn spawn_at_path(
        self,
        system: Arc<ThreadActorSystem>,
        path: String,
        thread_config: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        self.0.spawn_on_path(system, path, thread_config).await
    }

    /// Downcast-friendly BoxedMessage form.
    pub fn from_typed_boxed<A>(actor: A) -> BoxedMessage
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        TypedSpawnPayload::new(actor).into_boxed()
    }
}

#[async_trait]
impl<A> ErasedSpawnable for TypedSpawnPayload<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    async fn spawn_on(
        mut self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        _config: BoxedMessage,
        thread_config: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        let actor = self
            .actor
            .take()
            .ok_or_else(|| ActorError::InternalError("Spawn payload already consumed".into()))?;

        let actor_ref = system
            .spawn_at::<A>(
                actor,
                &format!("/user/{}", uuid_v4_simple()),
                None,
                thread_config,
            )
            .await
            .map_err(|e| ActorError::InternalError(e.to_string()))?;

        Ok(Box::new(actor_ref))
    }

    /// Supervision respawn: keep the child's original path (the supervisor
    /// tree shape is stable across restarts).
    async fn spawn_on_path(
        mut self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        path: String,
        thread_config: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        let actor = self
            .actor
            .take()
            .ok_or_else(|| ActorError::InternalError("Spawn payload already consumed".into()))?;

        let actor_ref = system
            .spawn_at::<A>(actor, &path, None, thread_config)
            .await
            .map_err(|e| ActorError::InternalError(e.to_string()))?;

        Ok(Box::new(actor_ref))
    }
}

/// Simple UUID v4-ish generator without external deps.
fn uuid_v4_simple() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{:x}-{:x}", ms, n)
}

/// Adapter exposing `Arc<dyn ActorRef>` as `Box<dyn ActorRef>`.
struct ArcActorRef(Arc<dyn ActorRef>);

impl std::fmt::Debug for ArcActorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArcActorRef")
            .field("path", &self.0.path())
            .finish()
    }
}

impl ArcActorRef {
    fn boxed(inner: Arc<dyn ActorRef>) -> Box<dyn ActorRef> {
        Box::new(ArcActorRef(inner))
    }
}

#[async_trait]
impl ActorRef for ArcActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send(msg)
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send_with_timeout(msg, timeout)
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.deliver(msg)
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.stop()
    }

    fn path(&self) -> String {
        self.0.path()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.0.is_alive()
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        self.0.clone_boxed()
    }

    fn as_any(&self) -> &dyn Any {
        self.0.as_any()
    }
}

#[async_trait]
impl ActorSystem for ThreadActorSystem {
    async fn start(config: ActorSystemConfig) -> Result<Self, parrot_api::system::SystemError> {
        let runtime_handle = Handle::try_current().map_err(|_| {
            parrot_api::system::SystemError::InitializationError(
                "ThreadActorSystem::start must be called within a Tokio runtime".to_string(),
            )
        })?;

        let thread_config = ThreadActorSystemConfig {
            name: if config.name.is_empty() {
                "parrot-thread-system".to_string()
            } else {
                config.name
            },
            ..Default::default()
        };

        let system = Self::with_runtime_handle(thread_config, runtime_handle);
        Ok(system)
    }

    async fn spawn_root_typed<A>(
        &self,
        _actor: A,
        _config: A::Config,
    ) -> Result<Box<dyn ActorRef>, parrot_api::system::SystemError>
    where
        A: Actor + 'static,
    {
        // Generic (context-erased) spawn cannot construct a ThreadContext<A>
        // without the context bound; use the engine-specific entry point.
        Err(parrot_api::system::SystemError::ActorCreationError(
            "Generic actor creation not supported directly. Use spawn_root_typed_thread on the ThreadActorSystem.".to_string(),
        ))
    }

    async fn spawn_root_boxed(
        &self,
        _actor: Box<dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>,
        _config: Box<dyn Any + Send>,
    ) -> Result<Box<dyn ActorRef>, parrot_api::system::SystemError> {
        Err(parrot_api::system::SystemError::ActorCreationError(
            "spawn_root_boxed is not supported by the thread engine; use spawn_root_typed"
                .to_string(),
        ))
    }

    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        self.get_actor_ref(&path.path).map(ArcActorRef::boxed)
    }

    async fn broadcast<M: Message + Clone>(
        &self,
        msg: M,
    ) -> Result<(), parrot_api::system::SystemError> {
        self.broadcast_message(Box::new(msg))
            .await
            .map_err(|e| parrot_api::system::SystemError::Other(anyhow::anyhow!(e.to_string())))
    }

    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: if self.is_shutting_down() {
                SystemState::ShuttingDown
            } else {
                SystemState::Running
            },
            active_actors: self.actor_count(),
            uptime: self.started_at.elapsed(),
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: self.scheduler_group.shared_scheduler.metrics_thread_count(),
            },
        }
    }

    async fn shutdown(self) -> Result<(), parrot_api::system::SystemError> {
        self.shutdown_internal()
            .await
            .map_err(|e| parrot_api::system::SystemError::Other(anyhow::anyhow!(e.to_string())))
    }
}

/// Free function performing the typed spawn used by `spawn_root_typed`.
///
/// Casts the context requirement down to ThreadContext<A> via a helper trait.
#[async_trait]
pub trait TypedSpawnHelper<A>: Send + Sync + 'static
where
    A: Actor + Send + Sync + 'static,
{
    async fn spawn_on_system(
        &self,
        system: &Arc<ThreadActorSystem>,
        actor: A,
        config: A::Config,
    ) -> Result<BoxedActorRef, SpawnError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::tests_support::DummyActor;
    use parrot_api::actor::ActorState;
    use parrot_api::actor::EmptyConfig;
    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

    /// Echo actor recording the messages it received.
    #[derive(Debug, Default)]
    struct RecvRecorder {
        received: std::sync::Mutex<Vec<u64>>,
    }

    impl Actor for RecvRecorder {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(v) = msg.downcast_ref::<u64>() {
                    self.received.lock().unwrap().push(*v);
                    return Ok(Box::new(*v) as BoxedMessage);
                }
                Ok(msg)
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    fn shared_system() -> Arc<ThreadActorSystem> {
        ThreadActorSystem::shared(ThreadActorSystemConfig::default())
    }

    #[tokio::test]
    async fn test_shared_installs_self_weak() {
        let sys = shared_system();
        assert!(sys.self_weak().is_some());
        assert!(sys.self_arc().is_some());
        assert!(!sys.is_shutting_down());
    }

    #[tokio::test]
    async fn test_shared_with_handle_uses_provided_runtime() {
        let sys = ThreadActorSystem::shared_with_handle(
            ThreadActorSystemConfig::default(),
            tokio::runtime::Handle::current(),
        );
        assert!(sys.self_weak().is_some());
        assert!(!sys.is_shutting_down());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_at_registers_actor_and_ref() {
        let sys = shared_system();

        let actor_ref = sys
            .spawn_at::<DummyActor>(
                DummyActor,
                "/user/test-spawn",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .expect("spawn should succeed");

        assert_eq!(actor_ref.path(), "/user/test-spawn");
        assert_eq!(sys.actor_count(), 1);
        assert!(sys.get_actor_ref("/user/test-spawn").is_some());
        assert!(sys.get_mailbox("/user/test-spawn").is_some());

        sys.shutdown_internal().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_at_duplicate_path_fails() {
        let sys = shared_system();

        let first = sys
            .spawn_at::<DummyActor>(DummyActor, "/user/dup", None, ThreadActorConfig::default())
            .await;
        assert!(first.is_ok());

        let second = sys
            .spawn_at::<DummyActor>(DummyActor, "/user/dup", None, ThreadActorConfig::default())
            .await;
        assert!(matches!(second, Err(SpawnError::ActorPathAlreadyExists(_))));

        sys.shutdown_internal().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_root_typed_thread_uses_uuid_path() {
        let sys = shared_system();

        let actor_ref = sys
            .spawn_root_typed_thread(DummyActor, EmptyConfig)
            .await
            .expect("root spawn should succeed");

        assert!(actor_ref.path().starts_with("/user/"));
        assert_eq!(sys.actor_count(), 1);

        sys.shutdown_internal().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_erased_actor_through_payload() {
        let sys = shared_system();

        let payload = TypedSpawnPayload::new(DummyActor);
        let boxed = payload.into_boxed();
        // The boxed payload must downcast back to the erased spawnable.
        let inner = boxed
            .downcast::<ErasedSpawnBox>()
            .expect("payload boxing round-trip");
        drop(inner);

        let payload2 = TypedSpawnPayload::new(DummyActor);
        let boxed_ref = sys
            .spawn_erased_actor(
                payload2.into_boxed(),
                Box::new(EmptyConfig) as BoxedMessage,
                None,
            )
            .await
            .expect("erased spawn should succeed");

        assert!(boxed_ref.path().starts_with("/user/"));
        assert_eq!(sys.actor_count(), 1);

        sys.shutdown_internal().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_rejects_payload_of_wrong_type() {
        let sys = shared_system();

        let result = sys
            .spawn_erased_actor(
                Box::new("not-a-spawn-payload") as BoxedMessage,
                Box::new(EmptyConfig) as BoxedMessage,
                None,
            )
            .await;

        assert!(result.is_err());

        sys.shutdown_internal().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stop_actor_removes_from_registry() {
        let sys = shared_system();

        sys.spawn_at::<DummyActor>(
            DummyActor,
            "/user/stop-me",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();

        assert_eq!(sys.actor_count(), 1);
        sys.stop_actor("/user/stop-me").await.unwrap();
        assert_eq!(sys.actor_count(), 0);
        assert!(sys.get_actor_ref("/user/stop-me").is_none());

        // Stopping a non-existent actor errors.
        let err = sys.stop_actor("/user/ghost").await;
        assert!(matches!(err, Err(SystemError::ActorNotFound(_))));

        sys.shutdown_internal().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_broadcast_delivers_to_all_mailboxes() {
        let sys = shared_system();

        for name in ["/user/bcast-1", "/user/bcast-2"] {
            sys.spawn_at::<RecvRecorder>(
                RecvRecorder::default(),
                name,
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        }

        sys.broadcast_message(Box::new(42u64) as BoxedMessage)
            .await
            .unwrap();

        // Wait until both mailboxes have drained the broadcast.
        for _ in 0..200 {
            let empty1 = sys.get_mailbox("/user/bcast-1").unwrap().is_empty().await;
            let empty2 = sys.get_mailbox("/user/bcast-2").unwrap().is_empty().await;
            if empty1 && empty2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            sys.get_mailbox("/user/bcast-1").unwrap().is_empty().await,
            "broadcast should be drained by the shared pool"
        );
        assert!(
            sys.get_mailbox("/user/bcast-2").unwrap().is_empty().await,
            "broadcast should be drained by the shared pool"
        );

        sys.shutdown_internal().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_fails_when_system_is_shutting_down() {
        let sys = shared_system();
        sys.shutdown_internal().await.unwrap();
        assert!(sys.is_shutting_down());

        let result = sys
            .spawn_at::<DummyActor>(DummyActor, "/user/late", None, ThreadActorConfig::default())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_watch_and_unwatch_registry() {
        let sys = shared_system();

        sys.spawn_at::<DummyActor>(
            DummyActor,
            "/user/watched",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();

        // Register interest in termination.
        sys.watch("/user/watcher".to_string(), "/user/watched".to_string())
            .await
            .unwrap();

        // Remove interest again.
        sys.unwatch("/user/watcher".to_string(), "/user/watched".to_string())
            .await
            .unwrap();

        sys.shutdown_internal().await.unwrap();
    }
}
