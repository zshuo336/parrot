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

use parrot_api::actor::{Actor, ActorState};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::context::ActorContext;
use parrot_api::errors::ActorError;
use parrot_api::message::MessageOptions;
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

/// Adapter for the public type-erased actor API.
///
/// `ThreadActor` needs a concrete `ThreadContext<A>`, while the public boxed
/// API deliberately exposes `dyn ActorContext`.  The adapter keeps the user
/// actor behind an async mutex and forwards every actor hook after coercing the
/// concrete thread context to the erased context trait.  The mutex is required
/// because the erased `Actor` object is only guaranteed to be `Send`; holding
/// its mutable borrow in the returned future also preserves the engine's
/// serial dispatch semantics.
type ErasedActor = dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>;

pub(super) struct BoxedThreadActor {
    inner: tokio::sync::Mutex<Box<ErasedActor>>,
}

impl parrot_api::engine::AsyncEngineRuntime for ThreadActorSystem {
    fn factory_runtime(&self) -> Option<&dyn parrot_api::engine::AsyncFactoryRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        actor: parrot_api::engine::RuntimeActor,
        config: parrot_api::engine::InstanceConfig,
        options: parrot_api::engine::SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, parrot_api::system::SystemError>> {
        // Install ownership before returning the future, including unpolled drop.
        let actor = crate::owned_payload::OwnedPayload::new(
            actor,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        let config = crate::owned_payload::OwnedPayload::new(
            config,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        Box::pin(async move {
            let admission = options.validate().and_then(|()| {
                self.self_arc().ok_or_else(|| {
                    parrot_api::system::SystemError::InitializationError(
                        "ThreadActorSystem requires its managed Arc identity".into(),
                    )
                })
            });
            let system = match admission {
                Ok(system) => system,
                Err(error) => {
                    let (actor, config) = tokio::join!(actor.retire(), config.retire());
                    actor
                        .and(config)
                        .map_err(parrot_api::system::SystemError::ActorError)?;
                    return Err(error);
                }
            };
            let reference = system
                .spawn_owned_instance_at(
                    BoxedThreadActor::new(actor.take()),
                    config.take(),
                    Default::default(),
                    options.path,
                )
                .await
                .map_err(|error| match error {
                    SpawnError::SystemShutdown => parrot_api::system::SystemError::ShuttingDown,
                    error => parrot_api::system::SystemError::ActorCreationError(error.to_string()),
                })?;
            Ok(Box::new(reference) as BoxedActorRef)
        })
    }

    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async move { self.resolve_actor(path) })
    }

    fn status(&self) -> SystemStatus {
        <Self as ActorSystem>::status(self)
    }

    fn shutdown(&self) -> BoxedFuture<'_, Result<(), parrot_api::system::SystemError>> {
        Box::pin(async move {
            self.shutdown_internal().await.map_err(|error| {
                parrot_api::system::SystemError::Other(anyhow::anyhow!(error.to_string()))
            })
        })
    }
}

impl BoxedThreadActor {
    pub(super) fn new(actor: Box<ErasedActor>) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(actor),
        }
    }
}

impl Actor for BoxedThreadActor {
    // The caller's erased config follows the same owned creation retirement
    // as ordinary typed instances; init itself has no config parameter.
    type Config = Box<dyn Any + Send>;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.init(ctx).await
        })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.receive_message(msg, ctx).await
        })
    }

    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        self.inner
            .try_lock()
            .map(|actor| actor.can_batch_message(message))
            .unwrap_or(false)
    }

    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.receive_batch(messages, ctx).await
        })
    }

    fn use_async_handler(&self) -> bool {
        self.inner
            .try_lock()
            .map(|actor| actor.use_async_handler())
            .unwrap_or(true)
    }

    fn handle_stream<'a>(
        &'a mut self,
        item: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.handle_stream(item, ctx).await
        })
    }

    fn stream_started<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.stream_started(ctx).await
        })
    }

    fn stream_finished<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.stream_finished(ctx).await
        })
    }

    fn stream_error<'a>(
        &'a mut self,
        error: ActorError,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.stream_error(error, ctx).await
        })
    }

    fn before_stop<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.before_stop(ctx).await
        })
    }

    fn handle_child_terminated<'a>(
        &'a mut self,
        child: BoxedActorRef,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let mut actor = self.inner.lock().await;
            let ctx: &mut dyn ActorContext = ctx;
            actor.handle_child_terminated(child, ctx).await
        })
    }

    fn state(&self) -> ActorState {
        self.inner
            .try_lock()
            .map(|actor| actor.state())
            .unwrap_or(ActorState::Running)
    }
}

/// Cloneable outcome shared by concurrent stop callers.
#[derive(Debug, Clone)]
pub(crate) enum StopOutcome {
    Stopped,
    TimedOut(String),
    Panic(String),
    Failed(crate::error_receipt::SystemReceipt),
}
impl StopOutcome {
    pub(crate) fn from_result(result: Result<(), SystemError>) -> Self {
        match result {
            Ok(()) => Self::Stopped,
            Err(SystemError::Timeout(message)) => Self::TimedOut(message),
            Err(SystemError::Panic(message)) => Self::Panic(message),
            Err(error) => Self::Failed(crate::error_receipt::SystemReceipt::new(error)),
        }
    }
    fn into_result(self) -> Result<(), SystemError> {
        match self {
            Self::Stopped => Ok(()),
            Self::TimedOut(message) => Err(SystemError::Timeout(message)),
            Self::Panic(message) => Err(SystemError::Panic(message)),
            Self::Failed(error) => Err(error.into_error()),
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
    pub(super) stop_request: super::address::StopRequest,

    /// Reference to the actor's mailbox
    pub(crate) mailbox: Arc<dyn Mailbox>,
    pub(crate) watches: crate::thread::death_watch::ThreadWatches,

    /// Actor configuration
    pub(crate) config: ThreadActorConfig,
    pub(crate) supervision: Option<Arc<crate::thread::supervisor_exec::SupervisionEntry>>,
    pub(crate) restart_cancel: Option<Arc<crate::thread::supervisor_exec::RestartControl>>,
    /// Kept while stopping so duplicate spawns cannot reuse this generation.
    pub(crate) stop_completion: Option<StopCompletion>,
    pub(crate) retirement_completion: Option<StopCompletion>,
    pub(crate) children: Arc<crate::children::ChildScope>,
    pub(crate) lifetime: Arc<crate::children::ChildLifetime>,
}

pub(crate) enum ActorRegistryEntry {
    Pending(Arc<crate::thread::factory::PendingFactory>),
    Instance(Arc<super::instance::PendingInstance>),
    Dynamic(Box<DynamicActorEntry>),
    Typed(crate::thread::typed::TypedRegistration),
}
impl ActorRegistryEntry {
    pub(crate) fn dynamic(&self) -> Option<&DynamicActorEntry> {
        match self {
            Self::Dynamic(entry) => Some(entry),
            Self::Typed(_) | Self::Pending(_) | Self::Instance(_) => None,
        }
    }
    pub(crate) fn dynamic_mut(&mut self) -> Option<&mut DynamicActorEntry> {
        match self {
            Self::Dynamic(entry) => Some(entry),
            Self::Typed(_) | Self::Pending(_) | Self::Instance(_) => None,
        }
    }
    fn legacy_watch(&self) -> parrot_api::death_watch::DeathWatch {
        match self {
            Self::Dynamic(entry) => entry.watches.legacy.clone(),
            Self::Typed(entry) => entry.control.legacy_watch.clone(),
            Self::Pending(entry) => entry.supervision.watches.legacy.clone(),
            Self::Instance(entry) => entry.watches.legacy.clone(),
        }
    }
    fn actor_ref(&self) -> Arc<dyn ActorRef> {
        match self {
            Self::Dynamic(entry) => entry.actor_ref.clone(),
            Self::Typed(entry) => entry.actor_ref.clone(),
            Self::Pending(entry) => entry.reference.clone(),
            Self::Instance(entry) => entry.reference.clone(),
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
    // A per-system scheduling rendezvous for deterministic stop-race tests.
    // Absent from production builds; it never changes a stop result.
    #[cfg(test)]
    reference_stop_probe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// System configuration
    config: Arc<ThreadActorSystemConfig>,

    /// Registry of all active actors
    pub(crate) registry: Arc<RwLock<HashMap<String, ActorRegistryEntry>>>,

    /// Scheduler group (shared pool + dedicated threads)
    pub(crate) scheduler_group: Arc<SchedulerGroup>,

    pub(crate) typed_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) cleanup_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) factory_slots: Arc<tokio::sync::Semaphore>,

    /// Runtime handle for spawning async tasks
    pub(crate) runtime_handle: Handle,

    /// Signal for system shutdown
    shutdown_signal: Arc<Notify>,
    shutdown_completion: Mutex<Option<StopCompletion>>,

    /// Flag indicating if the system is shutting down
    is_shutting_down: Arc<AtomicBool>,
    /// True only after actual actor retirement and scheduler joins, independent
    /// of the public shutdown receipt (which may already contain a timeout).
    stopped: Arc<AtomicBool>,

    /// System start time for uptime reporting
    started_at: Instant,

    /// Weak self reference used by contexts (set right after construction)
    self_weak: Mutex<Option<Weak<ThreadActorSystem>>>,

    /// K2 receptionist 网关（可选——facade 注册后注入；context 三方法出口）
    receptionist: RwLock<Option<Arc<dyn crate::system::ReceptionistGateway>>>,
    remote: RwLock<Option<Arc<dyn crate::system::RemoteGateway>>>,
}

impl Drop for ThreadActorSystem {
    fn drop(&mut self) {
        self.is_shutting_down.store(true, Ordering::Release);
        if self.shutdown_completion.lock().unwrap().is_none() {
            let work =
                self.shutdown_resources(tokio::time::Instant::now() + self.config.shutdown_timeout);
            self.runtime_handle.spawn(async move {
                let _ = work.await;
            });
        }
    }
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
    /// Prefer [`ThreadActorSystem::shared`], which returns an `Arc` and installs
    /// its weak self reference. When wrapping `new` in an `Arc` manually, call
    /// [`ThreadActorSystem::set_self_weak`] before spawning actors that use context
    /// operations requiring their owning system.
    pub fn new(config: ThreadActorSystemConfig) -> Self {
        Self::with_runtime_handle(config, Handle::current())
    }

    /// Create a new system bound to a specific runtime handle.
    pub fn with_runtime_handle(config: ThreadActorSystemConfig, runtime_handle: Handle) -> Self {
        let config = Arc::new(config);

        let factory = ThreadSchedulerFactory::new(runtime_handle.clone());
        let pool_config = crate::thread::scheduler::shared::SharedThreadPoolConfig {
            pool_size: config.shared_pool_size,
            max_queue_capacity: config.shared_queue_capacity,
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
            cleanup_slots: Arc::new(tokio::sync::Semaphore::new(config.shared_pool_size.max(1))),
            factory_slots: Arc::new(tokio::sync::Semaphore::new(config.shared_pool_size.max(1))),
            config,
            registry: Arc::new(RwLock::new(HashMap::new())),
            scheduler_group,
            runtime_handle,
            shutdown_signal: Arc::new(Notify::new()),
            shutdown_completion: Mutex::new(None),
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
            self_weak: Mutex::new(None),
            #[cfg(test)]
            reference_stop_probe: Mutex::new(None),
            receptionist: RwLock::new(None),
            remote: RwLock::new(None),
        }
    }

    /// K2：注入 receptionist 网关（facade 注册时下发）。
    pub fn set_receptionist_gateway(&self, gw: Arc<dyn crate::system::ReceptionistGateway>) {
        drop(self.swap_receptionist_gateway(gw));
    }

    /// Swap only; callers holding facade locks must defer the returned owner's
    /// destruction until their outer locks have also been released.
    pub(crate) fn swap_receptionist_gateway(
        &self,
        gw: Arc<dyn crate::system::ReceptionistGateway>,
    ) -> Option<Arc<dyn crate::system::ReceptionistGateway>> {
        self.receptionist.write().unwrap().replace(gw)
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
        error: Arc<ActorError>,
    ) {
        let system = self.clone();
        self.runtime_handle
            .spawn(parrot_api::trace::capture(async move {
                let reason = match error.as_ref() {
                    ActorError::Panic(message) => {
                        parrot_api::supervisor::DeathReason::Panic(message.clone())
                    }
                    error => parrot_api::supervisor::DeathReason::Failed(
                        crate::error_receipt::actor_summary(error),
                    ),
                };
                system.on_child_failure(&path, mailbox, error, reason).await;
            }));
    }

    pub(crate) fn runtime_spawn_escalation(
        self: &Arc<Self>,
        path: String,
        mailbox: WeakMailboxRef,
        error: Arc<ActorError>,
    ) {
        let Some(expected) = mailbox.upgrade() else {
            return;
        };
        let expected: Arc<dyn Mailbox> = expected;
        let (processor, decision) = {
            let registry = self.registry.read().unwrap();
            let Some(entry) = registry.get(&path).and_then(|entry| entry.dynamic()) else {
                return;
            };
            if !Arc::ptr_eq(&entry.mailbox, &expected)
                || entry.stop_completion.is_some()
                || entry.lifetime.is_stopping()
            {
                return;
            }
            let Some(processor) = entry.mailbox.get_processor() else {
                return;
            };
            if !processor.begin_escalation(error.clone()) {
                return;
            }
            let fallback = entry
                .config
                .supervisor_strategy
                .as_ref()
                .unwrap_or(&self.config().default_supervisor_strategy);
            let decision = entry.lifetime.start_escalation(
                error.clone(),
                tokio::time::Instant::now() + self.config().shutdown_timeout,
                matches!(fallback, crate::thread::config::SupervisorStrategy::Resume),
                &self.runtime_handle,
            );
            (processor, decision)
        };
        let system = self.clone();
        self.runtime_handle
            .spawn(parrot_api::trace::capture(async move {
                if decision.await {
                    let registry = system.registry.read().unwrap();
                    if registry
                        .get(&path)
                        .and_then(|entry| entry.dynamic())
                        .is_some_and(|entry| {
                            Arc::ptr_eq(&entry.mailbox, &expected)
                                && entry.stop_completion.is_none()
                                && !entry.lifetime.is_stopping()
                                && entry.lifetime.recovery().is_none()
                        })
                    {
                        processor.resume_after_escalation(&error);
                    }
                    return;
                }
                processor.fail_from_escalation(error.clone());
                let reason = parrot_api::supervisor::DeathReason::Escalated(
                    crate::error_receipt::actor_summary(&error),
                );
                system.on_child_failure(&path, mailbox, error, reason).await;
            }));
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
        let registry = self.registry.read().unwrap();
        let target = registry
            .get(&watched_path)
            .ok_or_else(|| ActorError::ActorNotFound(watched_path.clone()))?
            .legacy_watch();
        let watcher = registry
            .get(&watcher_path)
            .ok_or_else(|| ActorError::ActorNotFound(watcher_path.clone()))?
            .legacy_watch();
        target.watch(&watcher)
    }

    /// Remove a legacy path watch between the currently registered logical actors.
    pub async fn unwatch(
        &self,
        watcher_path: String,
        watched_path: String,
    ) -> Result<(), ActorError> {
        let registry = self.registry.read().unwrap();
        if let (Some(target), Some(watcher)) =
            (registry.get(&watched_path), registry.get(&watcher_path))
        {
            target.legacy_watch().unwatch(&watcher.legacy_watch());
        }
        Ok(())
    }

    pub(crate) fn forget_termination_watchers(&self, path: &str) {
        if let Some(entry) = self.registry.read().unwrap().get(path) {
            entry.legacy_watch().abandon();
        }
    }

    /// Notify all watchers under the caller's operation deadline.
    pub(crate) async fn notify_termination_until(
        &self,
        path: &str,
        reason: parrot_api::supervisor::DeathReason,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        notify_termination(&self.registry, path, reason, deadline).await
    }

    /// Spawn a typed root actor on this system (returns a typed ThreadActorRef).
    ///
    /// This is the engine-specific entry point with the required context bound.
    pub fn spawn_root_typed_thread<A>(
        self: &Arc<Self>,
        actor: A,
        config: A::Config,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + '_
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        self.spawn_owned_instance(actor, config, ThreadActorConfig::default())
    }

    /// Spawn an instance after owning and retiring its creation configuration.
    pub fn spawn_typed_with_config<A>(
        self: &Arc<Self>,
        actor: A,
        config: A::Config,
        thread_config: ThreadActorConfig,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + '_
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        self.spawn_owned_instance(actor, config, thread_config)
    }

    /// Spawn a typed actor at an explicit path with an optional parent.
    pub fn spawn_at<'a, A>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
        parent: Option<BoxedActorRef>,
        thread_config: ThreadActorConfig,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + 'a
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        self.spawn_owned_instance_with_parent(
            actor,
            None,
            thread_config,
            Some(path.to_owned()),
            parent,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_at_internal<A>(
        self: &Arc<Self>,
        actor: A,
        path: &str,
        parent: Option<BoxedActorRef>,
        thread_config: ThreadActorConfig,
        supervision: Option<Arc<crate::thread::supervisor_exec::SupervisionEntry>>,
        replacing: Option<crate::thread::factory::Replacement<'_>>,
        lifetime: Arc<crate::children::ChildLifetime>,
        group: Option<Arc<crate::children::recovery::Recovery>>,
    ) -> Result<ThreadActorRef<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        use crate::owned_payload::PublicationInput;
        let mut actor = PublicationInput::new(actor);
        let mut parent = PublicationInput::new(parent);
        // Every caller owns this logical lifetime through publication failure.
        // Replace raw policy clones before creating mailbox/processor/scheduler
        // state; the caller joins lifetime disposal on rejection.
        let mut engine = PublicationInput::new(thread_config);
        lifetime.bind_admission(
            engine.value_mut(),
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        let thread_config = engine.value();
        let mut unpublished = None;
        let result = (|| {
            if self.is_shutting_down() {
                return Err(SpawnError::SchedulerError("System is shutting down".into()));
            }

            if thread_config
                .message_batch_size
                .is_some_and(|n| !(1..=256).contains(&n))
            {
                return Err(SpawnError::SchedulerError(
                    "message_batch_size must be in 1..=256".into(),
                ));
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
            let instance = match &replacing {
                Some(crate::thread::factory::Replacement::Instance(instance)) => Some(*instance),
                _ => None,
            };
            let (address, stop_request) = match &supervision {
                Some(entry) => (entry.mailbox.clone(), entry.stop_request.clone()),
                None => match instance {
                    Some(instance) => (instance.address.clone(), instance.stop_request.clone()),
                    None => (
                        Arc::new(Mutex::new(Weak::<MpscMailbox>::new() as WeakMailboxRef)),
                        Default::default(),
                    ),
                },
            };
            let actor_ref = ThreadActorRef::<A>::registered(
                path.to_owned(),
                address,
                stop_request,
                Arc::downgrade(self),
                thread_config
                    .backpressure_strategy
                    .clone()
                    .unwrap_or(self.config.default_backpressure_strategy.clone()),
                thread_config
                    .ask_timeout
                    .unwrap_or(self.config.default_ask_timeout),
            );
            let watches = supervision
                .as_ref()
                .map(|entry| entry.watches.clone())
                .unwrap_or_else(|| {
                    instance.map_or_else(
                        || {
                            crate::thread::death_watch::ThreadWatches::dynamic(
                                &actor_ref.mailbox_cell(),
                            )
                        },
                        |instance| instance.watches.clone(),
                    )
                });
            let actor_ref = Arc::new(actor_ref.with_death_watch(watches.common.clone()));

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
            let mailbox: Arc<dyn Mailbox> = match &thread_config.message_admission {
                Some(policy) => Arc::new(crate::thread::mailbox::admission::AdmissionMailbox::new(
                    mailbox,
                    policy.clone(),
                )),
                None => mailbox,
            };

            // Fix up the actor ref's weak mailbox pointer now that mailbox exists.
            if supervision.is_none() && instance.is_none() {
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
                parent.take(),
                thread_config
                    .supervisor_strategy
                    .clone()
                    .unwrap_or_else(|| self.config.default_supervisor_strategy.clone()),
            );

            context.set_self_ref(actor_ref.clone_boxed());
            context.set_mailbox(Arc::downgrade(&mailbox) as WeakMailboxRef);

            let children = context.owned_children.clone();
            let processor = Arc::new(
                ActorProcessor::<A>::new(
                    ThreadActor::new(actor.take(), actor_path.clone()),
                    context,
                    path.to_string(),
                    thread_config.clone(),
                )
                .with_group_activation(group.clone()),
            );

            unpublished = Some(processor.clone());
            mailbox.set_processor(processor.clone());

            // Publish registration before scheduling so init/panic callbacks can
            // find their actor. Both operations are synchronous under this guard:
            // no duplicate spawn or shutdown drain can interleave between them.
            {
                let mut registry = self.registry.write().unwrap();
                if self.is_shutting_down() {
                    return Err(SpawnError::SystemShutdown);
                }
                if let Some(expected) = &replacing {
                    let valid = match expected {
                        crate::thread::factory::Replacement::Generation(expected) => {
                            registry
                                .get(path)
                                .and_then(ActorRegistryEntry::dynamic)
                                .is_some_and(|entry| {
                                    Arc::ptr_eq(expected, &entry.mailbox)
                                        && match &group {
                                            Some(group) => {
                                                !group.control.is_cancelled()
                                                    && !entry.lifetime.is_stopping()
                                                    && entry.lifetime.recovery().is_some_and(
                                                        |assigned| assigned.id == group.id,
                                                    )
                                            }
                                            None => {
                                                entry.lifetime.recovery().is_none()
                                                    && entry.restart_cancel.as_ref().is_some_and(
                                                        |control| !control.is_cancelled(),
                                                    )
                                            }
                                        }
                                })
                        }
                        crate::thread::factory::Replacement::Instance(expected) => {
                            matches!(registry.get(path), Some(ActorRegistryEntry::Instance(entry)) if Arc::ptr_eq(expected, entry) && !entry.control.is_cancelled())
                        }
                        crate::thread::factory::Replacement::Pending(expected) => {
                            matches!(registry.get(path), Some(ActorRegistryEntry::Pending(entry)) if Arc::ptr_eq(expected, entry) && match &group {
                                Some(group) => !group.control.is_cancelled() && !entry.lifetime.is_stopping() && entry.lifetime.recovery().is_some_and(|assigned| assigned.id == group.id),
                                None => !entry.control.is_cancelled() && entry.lifetime.recovery().is_none(),
                            })
                        }
                    };
                    if !valid {
                        return Err(SpawnError::Cancelled);
                    }
                } else if registry.contains_key(path) {
                    return Err(SpawnError::ActorPathAlreadyExists(path.to_string()));
                }
                // Lock the logical address through publication and scheduling.
                // Existing references cannot observe an unscheduled generation.
                let mut cell = supervision
                    .as_ref()
                    .map(|entry| entry.mailbox.lock().unwrap())
                    .or_else(|| instance.map(|entry| entry.address.lock().unwrap()));
                let old_mailbox = cell.as_mut().map(|cell| {
                    std::mem::replace(&mut **cell, Arc::downgrade(&mailbox) as WeakMailboxRef)
                });
                let transition = group
                    .as_ref()
                    .and_then(|_| registry.get(path))
                    .map(|entry| match entry {
                        ActorRegistryEntry::Dynamic(entry) => (
                            entry.stop_completion.clone(),
                            entry.retirement_completion.clone(),
                            entry.restart_cancel.clone(),
                        ),
                        ActorRegistryEntry::Pending(entry) => (
                            Some(entry.finished.subscribe()),
                            Some(entry.retired.subscribe()),
                            Some(entry.control.clone()),
                        ),
                        _ => unreachable!("only factories join recovery"),
                    });
                let previous = registry.insert(
                    path.to_string(),
                    ActorRegistryEntry::Dynamic(Box::new(DynamicActorEntry {
                        actor_ref: actor_ref.clone() as Arc<dyn ActorRef>,
                        address_cell: actor_ref.mailbox_cell(),
                        stop_request: actor_ref.stop_request(),
                        mailbox: mailbox.clone(),
                        watches: watches.clone(),
                        config: thread_config.clone(),
                        stop_completion: transition.as_ref().and_then(|t| t.0.clone()),
                        retirement_completion: transition.as_ref().and_then(|t| t.1.clone()),
                        children,
                        lifetime,
                        supervision: supervision.clone(),
                        restart_cancel: transition.as_ref().and_then(|t| t.2.clone()),
                    })),
                );
                let mode = thread_config
                    .scheduling_mode
                    .clone()
                    .unwrap_or_else(|| self.config.default_scheduling_mode.clone());
                let scheduled = watches
                    .resume()
                    .map_err(|error| SpawnError::SchedulerError(error.to_string()))
                    .and_then(|()| match &mode {
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
                    });
                if let Err(error) = scheduled {
                    watches.suspend();
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
        })();
        // No scheduler owns a failed publication. Registry/address guards and
        // temporary mailboxes are gone before arbitrary user destructors run.
        let storage = match unpublished {
            Some(processor) if result.is_err() => processor.retire_unpublished(),
            _ => Ok(()),
        };
        let actor = actor.retire();
        let parent = parent.retire();
        let engine = engine.retire();
        storage
            .and(actor)
            .and(parent)
            .and(engine)
            .map_err(|error| SpawnError::DisposalFailed(error.to_string()))?;
        result
    }

    /// Spawn a supervised logical root with a repeatable factory.
    ///
    /// The factory produces fresh actor instances for restarts (Akka
    /// "props" semantics). On panic, the supervision state machine decides
    /// Restart / Stop / Escalate (see `supervisor_exec.rs`); a Restart
    /// consumes one unit of the strategy's windowed budget.
    /// Factory captures and explicit engine configuration/policy are owned at
    /// call time. Dropping an unpolled creation future retires inputs without
    /// invoking the constructor or registering an actor.
    pub fn spawn_supervised<'a, A, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
        strategy: SupervisorStrategy,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + 'a
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
    }

    /// Spawn a supervised logical actor. All references and watches survive
    /// successful restarts, and each generation retains this configuration.
    /// Messages queued in a failed generation are discarded, not replayed.
    /// Initial and restart factories run on bounded workers under the configured
    /// shutdown deadline. Pending creation reserves the path; timeout/caller
    /// cancellation does not release it before actual factory/output disposal.
    pub fn spawn_supervised_with_config<'a, A, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
        thread_config: ThreadActorConfig,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + 'a
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_owned_factory(factory, path, thread_config)
    }

    /// Spawn an erased actor from boxed components (used by ThreadContext::spawn).
    pub fn spawn_erased_actor<'a>(
        self: &'a Arc<Self>,
        actor: BoxedMessage,
        config: BoxedMessage,
        strategy: Option<SupervisorStrategy>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let actor = crate::owned_payload::OwnedPayload::new(
            actor,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        let config = crate::owned_payload::OwnedPayload::new(
            config,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        Box::pin(async move {
            if !actor.value().is::<ErasedSpawnBox>() {
                let (actor, config) = tokio::join!(actor.retire(), config.retire());
                actor.and(config)?;
                return Err(ActorError::InternalError(
                    "Spawn payload is not a boxed spawnable actor".into(),
                ));
            }
            let payload = match actor.take().downcast::<ErasedSpawnBox>() {
                Ok(payload) => payload,
                Err(_) => unreachable!("validated erased payload"),
            };
            let mut thread_config = ThreadActorConfig::default();
            if let Some(strategy) = strategy {
                thread_config.supervisor_strategy = Some(strategy);
            }
            payload
                .0
                .spawn_on(self.clone(), config.take(), thread_config)
                .await
        })
    }

    /// Inject the gateway used for explicit `parrot://` paths. Gateways that
    /// refer back to this engine must use weak ownership to avoid a cycle.
    pub fn set_remote_gateway(&self, gateway: Arc<dyn crate::system::RemoteGateway>) {
        let retired = self.remote.write().unwrap().replace(gateway);
        drop(retired);
    }

    /// Resolve a local path or an explicit remote path. No engine lock is held
    /// while invoking the gateway. Resolution does not establish remote liveness.
    pub fn resolve_actor(&self, path: &str) -> Option<BoxedActorRef> {
        if self.is_shutting_down() {
            return None;
        }
        if path.starts_with("parrot://") {
            let gateway = self.remote.read().unwrap().clone()?;
            return gateway.lookup(path);
        }
        self.get_actor_ref(path).map(ArcActorRef::boxed)
    }

    /// Look up a local actor ref by its exact registry path. Remote gateways
    /// are deliberately excluded from inbound host lookup and stop operations.
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
        .map(|(completion, _)| completion)
    }

    /// References stop the logical actor atomically with restart publication.
    /// Context self-stop still uses its generation-specific mailbox above.
    pub(crate) fn request_stop_ref(
        &self,
        path: &str,
        address: &Arc<Mutex<WeakMailboxRef>>,
    ) -> Result<StopCompletion, SystemError> {
        #[cfg(test)]
        {
            let probe = self.reference_stop_probe.lock().unwrap().take();
            if let Some(probe) = probe {
                probe();
            }
        }
        self.request_stop_matching(path, |entry| match entry {
            ActorRegistryEntry::Pending(entry) => Arc::ptr_eq(address, &entry.supervision.mailbox),
            ActorRegistryEntry::Instance(entry) => Arc::ptr_eq(address, &entry.address),
            _ => entry
                .dynamic()
                .is_some_and(|entry| Arc::ptr_eq(address, &entry.address_cell)),
        })
        .map(|(completion, _)| completion)
    }

    fn request_stop_matching(
        &self,
        path: &str,
        matches: impl FnOnce(&ActorRegistryEntry) -> bool,
    ) -> Result<(StopCompletion, StopCompletion), SystemError> {
        let completion = {
            let mut registry = self.registry.write().unwrap();
            let entry = registry
                .get_mut(path)
                .ok_or_else(|| SystemError::ActorNotFound(path.into()))?;
            if !matches(entry) {
                return Err(SystemError::ActorNotFound(path.into()));
            }
            if let ActorRegistryEntry::Pending(entry) = entry {
                entry.lifetime.cancel_recovery();
                entry.control.cancel();
                entry.supervision.watches.close();
                let completion = entry.finished.subscribe();
                let _ = entry.supervision.stop_request.set(completion.clone());
                return Ok((completion, entry.retired.subscribe()));
            }
            if let ActorRegistryEntry::Instance(entry) = entry {
                entry.control.cancel();
                entry.watches.close();
                let completion = entry.finished.subscribe();
                let _ = entry.stop_request.set(completion.clone());
                return Ok((completion, entry.retired.subscribe()));
            }
            if let ActorRegistryEntry::Typed(entry) = entry {
                entry
                    .control
                    .request_stop(tokio::time::Instant::now() + self.config.shutdown_timeout);
                return Ok((entry.control.completion(), entry.control.retirement()));
            }
            let entry = entry.dynamic_mut().expect("dynamic entry checked above");
            if entry.lifetime.cancel_recovery() {
                // The group committed before stop acquired its transition. Stop
                // now owns the replacement, never the retiring owner's receipt.
                entry.stop_completion = None;
                entry.retirement_completion = None;
                entry.restart_cancel = None;
            }
            entry.watches.close();
            entry.children.close();
            let completion = if let Some(completion) = &entry.stop_completion {
                if let Some(control) = &entry.restart_cancel {
                    control.cancel();
                }
                (
                    completion.clone(),
                    entry
                        .retirement_completion
                        .as_ref()
                        .expect("stop owner has retirement receipt")
                        .clone(),
                )
            } else {
                let (sender, receiver) = tokio::sync::watch::channel(None);
                entry.stop_completion = Some(receiver.clone());
                let (retired, retirement) = tokio::sync::watch::channel(None);
                entry.retirement_completion = Some(retirement.clone());
                let mailbox = entry.mailbox.clone();
                let children = entry.children.clone();
                let lifetime = entry.lifetime.clone();
                let factory = entry
                    .supervision
                    .as_ref()
                    .map(|entry| entry.factory.clone());
                let mode = entry
                    .config
                    .scheduling_mode
                    .clone()
                    .unwrap_or_else(|| self.config.default_scheduling_mode.clone());
                mailbox.seal();
                let path = path.to_owned();
                let deadline = tokio::time::Instant::now() + self.config.shutdown_timeout;
                let registry = self.registry.clone();
                let watches = entry.watches.clone();
                let schedulers = self.scheduler_group.clone();
                let timeout = self.config.shutdown_timeout;
                let slots = self.cleanup_slots.clone();
                self.runtime_handle.spawn(async move {
                    let work = async {
                        let (children_result, result) = tokio::join!(
                            children.join(),
                            stop_actor_resources(
                                &path,
                                mailbox.clone(),
                                mode,
                                schedulers,
                                slots,
                                deadline
                            )
                        );
                        let policy_disposal = lifetime
                            .dispose_policy()
                            .await
                            .map_err(SystemError::policy_disposal);
                        let factory_disposal = match factory {
                            Some(factory) => factory.dispose().await,
                            None => Ok(()),
                        };
                        let result = factory_disposal
                            .and(result)
                            .and(
                                children_result
                                    .map_err(|e| SystemError::Other(anyhow::Error::new(e))),
                            )
                            .and(policy_disposal);
                        let reason = match &result {
                            Ok(()) => parrot_api::supervisor::DeathReason::Normal,
                            Err(SystemError::Timeout(_)) => {
                                parrot_api::supervisor::DeathReason::Killed
                            }
                            Err(SystemError::Panic(message)) => {
                                parrot_api::supervisor::DeathReason::Panic(message.clone())
                            }
                            Err(error) => parrot_api::supervisor::DeathReason::Failed(
                                crate::error_receipt::system_summary(error),
                            ),
                        };
                        let delivery_deadline = if tokio::time::Instant::now() >= deadline {
                            tokio::time::Instant::now() + timeout
                        } else {
                            deadline
                        };
                        let event = parrot_api::context::ActorTerminated {
                            generation: watches.common.id(),
                            path: path.clone(),
                            reason: reason.clone(),
                        };
                        let (_, notified) = tokio::join!(
                            watches.common.terminated(
                                event,
                                delivery_deadline
                                    .saturating_duration_since(tokio::time::Instant::now())
                            ),
                            crate::thread::death_watch::notify_legacy(
                                &watches.legacy,
                                &path,
                                reason,
                                delivery_deadline
                                    .saturating_duration_since(tokio::time::Instant::now())
                            )
                        );
                        {
                            let mut registry = registry.write().unwrap();
                            if registry
                                .get(&path)
                                .and_then(ActorRegistryEntry::dynamic)
                                .is_some_and(|entry| Arc::ptr_eq(&entry.mailbox, &mailbox))
                            {
                                registry.remove(&path);
                            }
                        }
                        result.and(notified)
                    };
                    let (result, timed_out) =
                        super::stop_deadline::join_until(work, deadline, || {
                            sender.send_replace(Some(StopOutcome::TimedOut(format!(
                                "actor {path} stop exceeded configured deadline"
                            ))));
                        })
                        .await;
                    let result = result.map_err(crate::error_receipt::SystemReceipt::new);
                    let notification = lifetime.finish(
                        result
                            .as_ref()
                            .map(|_| ())
                            .map_err(|error| error.actor_receipt()),
                    );
                    let outcome = StopOutcome::from_result(
                        result.map_err(crate::error_receipt::SystemReceipt::into_error),
                    );
                    if !timed_out {
                        sender.send_replace(Some(outcome.clone()));
                    }
                    retired.send_replace(Some(outcome));
                    notification.await;
                });
                (receiver, retirement)
            };
            let _ = entry.stop_request.set(completion.0.clone());
            completion
        };
        Ok(completion)
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
        self.broadcast_cloning(move || msg_clone(&msg)).await
    }

    async fn broadcast_cloning<F>(&self, mut copy: F) -> Result<(), SystemError>
    where
        F: FnMut() -> Result<BoxedMessage, SystemError> + Send,
    {
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
            let message = copy()?;
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
                let deadline = tokio::time::Instant::now() + self.config.shutdown_timeout;
                let work = self.shutdown_resources(deadline);
                self.runtime_handle.spawn(async move {
                    let (result, timed_out) =
                        super::stop_deadline::join_until(work, deadline, || {
                            sender.send_replace(Some(StopOutcome::TimedOut(
                                "actor shutdown exceeded configured deadline".into(),
                            )));
                        })
                        .await;
                    if !timed_out {
                        sender.send_replace(Some(StopOutcome::from_result(result)));
                    }
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
        // Reserve all stops while the registry still retains every generation.
        // Existing stop/supervision operations own separate true retirement receipts.
        let completions: Vec<_> = self
            .actor_paths()
            .into_iter()
            .filter_map(|path| {
                self.request_stop_matching(&path, |_| true)
                    .ok()
                    .map(|(_, retirement)| retirement)
            })
            .collect();
        let scheduler_group = self.scheduler_group.clone();
        let shutdown_signal = self.shutdown_signal.clone();
        let stopped = self.stopped.clone();
        let timeout = self.config.shutdown_timeout;
        Box::pin(async move {
            let results = futures::future::join_all(completions.into_iter().map(await_stop)).await;
            // Affinity schedulers remain alive until every actor was disposed.
            let cleanup_deadline = deadline.max(tokio::time::Instant::now() + timeout);
            let sharded = scheduler_group.sharded_scheduler.lock().unwrap().clone();
            let (shared, dedicated, shards) = tokio::join!(
                scheduler_group
                    .shared_scheduler
                    .shutdown(timeout.as_millis().min(u64::MAX as u128) as u64),
                async {
                    let scheduler = &scheduler_group.dedicated_scheduler;
                    let result = scheduler.shutdown_until(cleanup_deadline).await;
                    // A bounded join can expire before the last OS thread
                    // exits. The owned operation must still reap it later.
                    while scheduler.worker_count() != 0 {
                        let _ = scheduler
                            .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
                            .await;
                    }
                    result
                },
                async {
                    match sharded {
                        Some(scheduler) => {
                            let result = scheduler.shutdown_until(cleanup_deadline).await;
                            while scheduler.owned_thread_count() != 0 {
                                let _ = scheduler
                                    .shutdown_until(
                                        tokio::time::Instant::now() + Duration::from_secs(1),
                                    )
                                    .await;
                            }
                            result
                        }
                        None => Ok(()),
                    }
                }
            );
            // User stop hooks can fail after resources have been released.
            // Preserve that error without misreporting an already joined system.
            stopped.store(true, Ordering::Release);
            shutdown_signal.notify_waiters();
            shared?;
            dedicated?;
            shards?;
            results.into_iter().collect()
        })
    }
}

/// Resource completion follows drain, cancellation and both user destructors.
pub(crate) async fn stop_actor_resources(
    path: &str,
    mailbox: Arc<dyn Mailbox>,
    mode: SchedulingMode,
    schedulers: Arc<SchedulerGroup>,
    slots: Arc<tokio::sync::Semaphore>,
    deadline: tokio::time::Instant,
) -> Result<(), SystemError> {
    mailbox.seal();
    let processor = mailbox.get_processor();
    if let Some(processor) = &processor {
        processor.cancel_context_tasks();
    }
    let mut drained = drain_mailbox(mailbox, slots.clone());
    let drain = tokio::select! {
        biased;
        result = &mut drained => result.map_err(|e| SystemError::ShutdownError(e.to_string()))
            .and_then(|result| result.map_err(|e| SystemError::ShutdownError(e.to_string()))),
        _ = tokio::time::sleep_until(deadline) => {
            if let Some(processor) = &processor { processor.cancel_processing(); }
            let _ = drained.await;
            Err(SystemError::Timeout(format!("actor {path} mailbox disposal exceeded deadline")))
        }
    };
    let stopped = match (&processor, drain) {
        (Some(processor), Ok(())) => tokio::time::timeout_at(
            deadline,
            schedulers.stop_hook(path, &mode, processor.clone(), slots.clone(), deadline),
        )
        .await
        .unwrap_or_else(|_| {
            Err(SystemError::Timeout(format!(
                "actor {path} stop hook exceeded deadline"
            )))
        }),
        (_, result) => result,
    };
    let retired = if let Some(processor) = processor {
        schedulers
            .retire_actor(path, &mode, processor, slots, deadline)
            .await
    } else {
        Ok(())
    };
    let _ = schedulers.shared_scheduler.deschedule(path).await;
    if let Some(sharded) = schedulers.sharded_scheduler.lock().unwrap().as_ref() {
        let _ = sharded.deschedule(path);
    }
    // A destructor panic is material even when an earlier hook also failed.
    retired.and(stopped)
}

async fn notify_termination(
    registry: &Arc<RwLock<HashMap<String, ActorRegistryEntry>>>,
    path: &str,
    reason: parrot_api::supervisor::DeathReason,
    deadline: tokio::time::Instant,
) -> Result<(), SystemError> {
    let handle = registry
        .read()
        .unwrap()
        .get(path)
        .map(ActorRegistryEntry::legacy_watch);
    if let Some(handle) = handle {
        crate::thread::death_watch::notify_legacy(
            &handle,
            path,
            reason,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
    } else {
        Ok(())
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

/// Internal lifecycle receipt supplied only by the owning parent scope.
#[doc(hidden)]
pub struct ChildSpawnReceipt(
    pub(crate) Arc<super::instance::PendingInstance>,
    pub(crate) Arc<std::sync::atomic::AtomicBool>,
);

/// Erased spawn payload: a boxed closure able to spawn on a system.
///
/// The system reference is captured as an `Arc` clone so the returned future
/// is 'static.
#[async_trait]
pub trait ErasedSpawnable: Send + Sync + 'static {
    #[doc(hidden)]
    fn accepts_child_config(&self, _config: &BoxedMessage) -> bool {
        false
    }
    #[doc(hidden)]
    fn spawn_child_on(
        self: Box<Self>,
        _system: Arc<ThreadActorSystem>,
        _thread_config: ThreadActorConfig,
        _parent: BoxedActorRef,
        _lifetime: ChildSpawnReceipt,
    ) -> ActorResult<BoxedActorRef> {
        Err(ActorError::InternalError(
            "custom spawn payload has no child creation bridge".into(),
        ))
    }

    async fn spawn_on(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        config: BoxedMessage,
        thread_config: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef>;

    /// Spawn at an explicit path (supervision respawn keeps the child path).
    ///
    /// Custom payloads must override this method to support explicit paths.
    /// The default retires both inputs and rejects the operation without calling
    /// `spawn_on`: an automatically generated path cannot satisfy this contract.
    /// A cleanup failure takes precedence over the unsupported-operation error.
    fn spawn_on_path<'async_trait>(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        path: String,
        thread_config: ThreadActorConfig,
    ) -> BoxedFuture<'async_trait, ActorResult<BoxedActorRef>>
    where
        Self: 'async_trait,
    {
        let payload = crate::owned_payload::OwnedPayload::new(
            self,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        let engine = crate::owned_payload::OwnedPayload::new(
            thread_config,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        Box::pin(async move {
            let (payload, engine) = tokio::join!(payload.retire(), engine.retire());
            payload.and(engine)?;
            Err(ActorError::InternalError(format!(
                "custom spawn payload does not support explicit path {path:?}; override spawn_on_path"
            )))
        })
    }
}

/// Wrapper holding an erased spawnable so the outer box stays a concrete
/// type and can be downcast from `BoxedMessage` reliably.
///
/// The wrapper supplies one known concrete downcast target and preserves the
/// callable trait object inside it. Boxing a raw actor as `Any` alone does not
/// provide a way to recover its engine-specific creation operation.
pub struct ErasedSpawnBox(pub(crate) Box<dyn ErasedSpawnable>);

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
    pub fn spawn_at_path(
        self,
        system: Arc<ThreadActorSystem>,
        path: String,
        thread_config: ThreadActorConfig,
    ) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
        let payload = crate::owned_payload::OwnedPayload::new(
            self,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        let engine = crate::owned_payload::OwnedPayload::new(
            thread_config,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        Box::pin(async move {
            payload
                .take()
                .0
                .spawn_on_path(system, path, engine.take())
                .await
        })
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
    fn accepts_child_config(&self, config: &BoxedMessage) -> bool {
        config.is::<A::Config>()
    }
    fn spawn_child_on(
        mut self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        thread_config: ThreadActorConfig,
        parent: BoxedActorRef,
        lifetime: ChildSpawnReceipt,
    ) -> ActorResult<BoxedActorRef> {
        let actor = self
            .actor
            .take()
            .ok_or_else(|| ActorError::InternalError("Spawn payload already consumed".into()))?;
        let path = lifetime.0.reference.path();
        system
            .spawn_at_internal(
                actor,
                &path,
                Some(parent),
                thread_config,
                None,
                Some(crate::thread::factory::Replacement::Instance(&lifetime.0)),
                lifetime.0.lifetime.clone(),
                None,
            )
            .map(|actor| {
                lifetime.1.store(true, Ordering::Release);
                Box::new(actor) as BoxedActorRef
            })
            .map_err(|e| ActorError::InternalError(e.to_string()))
    }

    fn spawn_on<'async_trait>(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        config: BoxedMessage,
        thread_config: ThreadActorConfig,
    ) -> BoxedFuture<'async_trait, ActorResult<BoxedActorRef>>
    where
        Self: 'async_trait,
    {
        let payload = crate::owned_payload::OwnedPayload::new(
            self,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        let config = crate::owned_payload::OwnedPayload::new(
            config,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        let engine = crate::owned_payload::OwnedPayload::new(
            thread_config,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        Box::pin(async move {
            if !config.value().is::<A::Config>() || payload.value().actor.is_none() {
                let (payload, config, engine) =
                    tokio::join!(payload.retire(), config.retire(), engine.retire());
                payload.and(config).and(engine)?;
                return Err(ActorError::InitializationError(
                    "actor configuration type mismatch or consumed payload".into(),
                ));
            }
            let mut payload = payload.take();
            let actor = payload.actor.take().expect("validated actor input");
            let config = match config.take().downcast::<A::Config>() {
                Ok(config) => *config,
                Err(_) => unreachable!("validated actor config"),
            };
            let reference = system
                .spawn_owned_instance(actor, config, engine.take())
                .await
                .map_err(ActorError::from)?;
            Ok(Box::new(reference) as BoxedActorRef)
        })
    }

    /// Supervision respawn: keep the child's original path (the supervisor
    /// tree shape is stable across restarts).
    fn spawn_on_path<'async_trait>(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        path: String,
        thread_config: ThreadActorConfig,
    ) -> BoxedFuture<'async_trait, ActorResult<BoxedActorRef>>
    where
        Self: 'async_trait,
    {
        let payload = crate::owned_payload::OwnedPayload::new(
            self,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        let engine = crate::owned_payload::OwnedPayload::new(
            thread_config,
            &system.runtime_handle,
            &system.cleanup_slots,
        );
        Box::pin(async move {
            if payload.value().actor.is_none() {
                let (payload, engine) = tokio::join!(payload.retire(), engine.retire());
                payload.and(engine)?;
                return Err(ActorError::InternalError(
                    "Spawn payload already consumed".into(),
                ));
            }
            let actor = payload.take().actor.take().expect("validated actor input");
            let reference = system
                .spawn_at::<A>(actor, &path, None, engine.take())
                .await
                .map_err(|error| ActorError::InternalError(error.to_string()))?;
            Ok(Box::new(reference) as BoxedActorRef)
        })
    }
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

    fn send_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send_with_options(msg, options)
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.deliver(msg)
    }

    fn deliver_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.deliver_with_options(msg, options)
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

    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        self.0.death_watch()
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

    fn spawn_root_typed<'life0, 'async_trait, A>(
        &'life0 self,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<
        'async_trait,
        Result<Box<dyn ActorRef>, parrot_api::system::SystemError>,
    >
    where
        A: Actor + 'static + 'async_trait,
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let actor = crate::owned_payload::OwnedPayload::new(
            actor,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        let config = crate::owned_payload::OwnedPayload::new(
            config,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        Box::pin(async move {
            let (actor, config) = tokio::join!(actor.retire(), config.retire());
            actor
                .and(config)
                .map_err(parrot_api::system::SystemError::ActorError)?;
            Err(parrot_api::system::SystemError::ActorCreationError(
                "Generic actor creation not supported directly. Use spawn_root_typed_thread on the ThreadActorSystem.".to_string(),
            ))
        })
    }

    fn spawn_root_boxed<'life0, 'async_trait>(
        &'life0 self,
        actor: Box<dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn Any + Send>,
    ) -> parrot_api::types::BoxedFuture<
        'async_trait,
        Result<Box<dyn ActorRef>, parrot_api::system::SystemError>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let actor = crate::owned_payload::OwnedPayload::new(
            actor,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        let config = crate::owned_payload::OwnedPayload::new(
            config,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        Box::pin(async move {
            let Some(system) = self.self_arc() else {
                let (actor, config) = tokio::join!(actor.retire(), config.retire());
                actor
                    .and(config)
                    .map_err(parrot_api::system::SystemError::ActorError)?;
                return Err(parrot_api::system::SystemError::InitializationError(
                "ThreadActorSystem must be wrapped in Arc and initialized with set_self_weak before spawning boxed actors".to_string(),
            ));
            };
            let actor = BoxedThreadActor::new(actor.take());
            let typed = system
                .spawn_root_typed_thread(actor, config.take())
                .await
                .map_err(|error| {
                    parrot_api::system::SystemError::ActorCreationError(error.to_string())
                })?;
            Ok(Box::new(typed) as Box<dyn ActorRef>)
        })
    }

    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        self.resolve_actor(&path.path)
    }

    async fn broadcast<M: Message + Clone>(
        &self,
        msg: M,
    ) -> Result<(), parrot_api::system::SystemError> {
        self.broadcast_cloning(move || Ok(Box::new(msg.clone()) as BoxedMessage))
            .await
            .map_err(|e| parrot_api::system::SystemError::Other(anyhow::anyhow!(e.to_string())))
    }

    fn status(&self) -> SystemStatus {
        let stopped = self.stopped.load(Ordering::Acquire);
        let shards = self
            .scheduler_group
            .sharded_scheduler
            .lock()
            .unwrap()
            .clone();
        SystemStatus {
            state: if stopped {
                SystemState::Stopped
            } else if self.is_shutting_down() {
                SystemState::ShuttingDown
            } else {
                SystemState::Running
            },
            active_actors: self.actor_count(),
            uptime: self.started_at.elapsed(),
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: if stopped {
                    0
                } else {
                    self.scheduler_group
                        .dedicated_scheduler
                        .metrics_thread_count()
                        .saturating_add(shards.map_or(0, |s| s.owned_thread_count()))
                },
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
/// Casts the context requirement down to `ThreadContext<A>` via a helper trait.
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

    /// The public erased API must execute an actor whose context is only the
    /// `ActorContext` trait object. This catches regressions where the method
    /// merely returns the old "not supported" placeholder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_root_boxed_executes_messages_and_stop_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ErasedEcho {
            initialized: Arc<AtomicUsize>,
            received: Arc<AtomicUsize>,
            stopped: Arc<AtomicUsize>,
        }

        impl Actor for ErasedEcho {
            type Config = Box<dyn Any + Send>;
            type Context = dyn ActorContext;

            fn init<'a>(
                &'a mut self,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                self.initialized.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }

            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                self.received.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(msg) })
            }

            fn before_stop<'a>(
                &'a mut self,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                self.stopped.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }

            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        let sys = shared_system();
        let initialized = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicUsize::new(0));
        let actor = ErasedEcho {
            initialized: initialized.clone(),
            received: received.clone(),
            stopped: stopped.clone(),
        };

        let actor_ref = <ThreadActorSystem as ActorSystem>::spawn_root_boxed(
            &*sys,
            Box::new(actor),
            Box::new("erased-config") as Box<dyn Any + Send>,
        )
        .await
        .expect("boxed root spawn should execute on the thread engine");

        let result = actor_ref
            .send(Box::new(41_u64) as BoxedMessage)
            .await
            .expect("boxed actor should process messages");
        assert_eq!(result.downcast::<u64>().unwrap().as_ref(), &41);
        assert_eq!(initialized.load(Ordering::SeqCst), 1);
        assert_eq!(received.load(Ordering::SeqCst), 1);

        actor_ref
            .stop()
            .await
            .expect("boxed actor should stop cleanly");
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
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

        assert!(
            sys.watch("/user/missing".into(), "/user/watched".into())
                .await
                .is_err()
        );
        sys.spawn_at::<DummyActor>(
            DummyActor,
            "/user/watcher",
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expired_shutdown_budget_keeps_ownership_until_all_shards_join() {
        let engine = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            shutdown_timeout: Duration::ZERO,
            ..Default::default()
        });
        // This fixture exercises shutdown with already admitted storage. The
        // public async creation API correctly rejects a zero creation budget.
        let actor = engine
            .spawn_at_internal(
                RecvRecorder::default(),
                "/zero-budget",
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(SchedulingMode::Sharded {
                        affinity_key: "zero".into(),
                    }),
                    ..Default::default()
                },
                None,
                None,
                crate::children::ChildLifetime::new(Weak::new()),
                None,
            )
            .unwrap();
        assert!(actor.send(Box::new(42_u64)).await.is_ok());
        let shards = engine.scheduler_group.sharded();
        assert!(
            shards.owned_thread_count() > 0,
            "fixture must actually own shard threads"
        );
        let _ = engine.shutdown_internal().await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while engine.status().state != parrot_api::system::SystemState::Stopped {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(engine.status().resources.thread_count, 0);
        assert_eq!(
            shards.owned_thread_count(),
            0,
            "reported status must match actual joined threads"
        );
    }
}

#[cfg(test)]
mod reservation_race {
    use super::*;
    use std::sync::{Condvar, atomic::AtomicUsize};
    struct Gate {
        entered: tokio::sync::Notify,
        released: Mutex<bool>,
        changed: Condvar,
    }
    impl Gate {
        fn pause(&self) {
            self.entered.notify_one();
            let (_guard, result) = self
                .changed
                .wait_timeout_while(
                    self.released.lock().unwrap(),
                    Duration::from_secs(5),
                    |open| !*open,
                )
                .unwrap();
            assert!(!result.timed_out(), "stop-race fixture was not released");
        }
        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    struct Probe {
        fail: bool,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Actor for Probe {
        type Config = parrot_api::actor::EmptyConfig;
        type Context = ThreadContext<Self>;
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Ok(msg) })
        }
        fn before_stop<'a>(
            &'a mut self,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                if self.fail {
                    Err(ActorError::InternalError(
                        "original concurrent stop failure".into(),
                    ))
                } else {
                    Ok(())
                }
            })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    async fn run(mode: SchedulingMode, fail: bool) {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shutdown_timeout: Duration::from_secs(3),
            ..Default::default()
        });
        let drops = Arc::new(AtomicUsize::new(0));
        let path = "/stop-reservation-race";
        let actor = system
            .spawn_at(
                Probe {
                    fail,
                    drops: drops.clone(),
                },
                path,
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        actor
            .ask_with_strategy_and_timeout(
                Box::new(()),
                BackpressureStrategy::Block,
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let gate = Arc::new(Gate {
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            changed: Condvar::new(),
        });
        let release = Release(gate.clone());
        let paused = gate.clone();
        *system.reference_stop_probe.lock().unwrap() = Some(Arc::new(move || paused.pause()));
        let reference = actor.clone();
        let runtime = Handle::current();
        // Pause inside the actual public reference stop operation, immediately
        // before it requests the registry. Another OS thread can preempt at
        // this same boundary without the cfg(test) scheduling rendezvous.
        let waiting = tokio::task::spawn_blocking(move || runtime.block_on(reference.stop()));
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        let system_result = system.stop_actor(path).await;
        assert_eq!(
            system.actor_count(),
            0,
            "system stop must finish removal before reference proceeds"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(release);
        let reference_result = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        if fail {
            let original = system_result.unwrap_err().to_string();
            assert!(original.contains("original concurrent stop failure"));
            let reference = reference_result.unwrap_err().to_string();
            assert!(
                reference.contains("original concurrent stop failure"),
                "lost original receipt: {reference}"
            );
            assert_eq!(actor.stop().await.unwrap_err().to_string(), reference);
        } else {
            system_result.unwrap();
            reference_result.unwrap();
            actor.stop().await.unwrap();
        }
        assert!(actor.send(Box::new(())).await.is_err());
        system.shutdown_internal().await.unwrap();
    }
    macro_rules! cases { ($($name:ident:$mode:expr,$fail:expr;)*)=>{$(
        #[tokio::test(flavor="multi_thread",worker_threads=2)]
        async fn $name(){run($mode,$fail).await;}
    )*}; }
    cases! {
        shared_success:SchedulingMode::SharedPool{max_messages_per_run:10},false;
        shared_failure:SchedulingMode::SharedPool{max_messages_per_run:10},true;
        dedicated_success:SchedulingMode::DedicatedThread,false;
        dedicated_failure:SchedulingMode::DedicatedThread,true;
        sharded_success:SchedulingMode::Sharded{affinity_key:"race".into()},false;
        sharded_failure:SchedulingMode::Sharded{affinity_key:"race".into()},true;
    }
}
