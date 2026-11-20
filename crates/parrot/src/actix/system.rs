use super::disposal::{OwnedPayload, RejectedSpawn};
use crate::actix::lifecycle::{ActorControl, ActorExit, Completion, Outcome, await_completion};
use crate::actix::{actor::ActixActor, context::ActixContext, reference::ActixActorRef};
use crate::thread::native_budget::NativeWorkerBudget;
use actix::prelude::ArbiterHandle;
use futures::FutureExt;
use parrot_api::{
    actor::{ActixEngineExt, Actor as ParrotActor, ActorState},
    address::ActorRef,
    context::ActorContext,
    errors::ActorError,
    message::Message,
    system::{SystemError, SystemResources, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

#[path = "system_shutdown.rs"]
mod shutdown;

struct ArbiterOwners {
    workers: Vec<ArbiterHandle>,
    threads: Vec<std::thread::ThreadId>,
    retirement: super::pool_retirement::Retirement,
    distribution: Arc<super::distribution::Distribution>,
}
impl Drop for ArbiterOwners {
    fn drop(&mut self) {
        for worker in &self.workers {
            worker.stop();
        }
        self.retirement.request();
    }
}

/// An owned group of Actix arbiters, shared by clones of one Parrot system.
#[derive(Clone)]
pub struct ArbiterPool {
    inner: Arc<ArbiterOwners>,
}
impl ArbiterPool {
    /// Build workers within the current Actix System. Zero selects one worker.
    /// Panics on invalid settings, a missing host, or worker setup failure after
    /// joining any partially created pool; use `try_new` for errors.
    pub fn new(size: usize) -> Self {
        Self::try_new(size).expect("invalid Actix worker count or missing Actix System")
    }
    /// Validate the worker limit and host before creating any worker. Setup
    /// failures stop and join all previously created workers before returning.
    /// A separate retirement thread is established before returning a live pool.
    /// This synchronous constructor can block while retiring a failed pool.
    pub fn try_new(size: usize) -> Result<Self, SystemError> {
        Self::with_distribution(size, parrot_api::runtime::LoadBalancingStrategy::RoundRobin)
    }
    fn with_distribution(
        size: usize,
        strategy: parrot_api::runtime::LoadBalancingStrategy,
    ) -> Result<Self, SystemError> {
        Self::with_distribution_and_budget(size, strategy, NativeWorkerBudget::global())
    }

    pub(crate) fn with_distribution_and_budget(
        size: usize,
        strategy: parrot_api::runtime::LoadBalancingStrategy,
        native_budget: Arc<crate::thread::native_budget::NativeWorkerBudget>,
    ) -> Result<Self, SystemError> {
        super::settings::validate_workers(size)?;
        if actix::System::try_current().is_none() {
            return Err(SystemError::InitializationError(
                "ArbiterPool requires a running Actix System".into(),
            ));
        }
        let threads = Arc::new(Mutex::new(Vec::with_capacity(size.max(1))));
        let reservations = Arc::new(Mutex::new(Vec::with_capacity(size.max(1))));
        let owners = super::pool_startup::build(size.max(1), |_| {
            let reservation = native_budget
                .try_reserve()
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let threads = threads.clone();
            let owner = actix::Arbiter::try_with_tokio_rt(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                threads.lock().unwrap().push(std::thread::current().id());
                Ok(runtime)
            })?;
            reservations.lock().unwrap().push(reservation);
            Ok(owner)
        })?;
        let threads = std::mem::take(&mut *threads.lock().unwrap());
        let reservations = std::mem::take(&mut *reservations.lock().unwrap());
        let workers = owners.iter().map(actix::Arbiter::handle).collect();
        let retirement =
            super::pool_retirement::Retirement::new_with_reservations(owners, reservations)?;
        Ok(Self {
            inner: Arc::new(ArbiterOwners {
                workers,
                threads,
                retirement,
                distribution: super::distribution::Distribution::new(size.max(1), strategy),
            }),
        })
    }
    pub fn size(&self) -> usize {
        self.inner.workers.len()
    }
    #[cfg(test)]
    pub(crate) fn next_arbiter(&self) -> ArbiterHandle {
        self.reserve_arbiter().worker
    }
    pub(super) fn reserve_arbiter(&self) -> super::distribution::Assignment {
        let lease = self.inner.distribution.reserve();
        super::distribution::Assignment {
            worker: self.inner.workers[lease.index].clone(),
            lease,
        }
    }
    async fn stop_and_join(&self) -> ActorResult<()> {
        for worker in &self.inner.workers {
            worker.stop();
        }
        self.inner.retirement.request();
        self.inner.retirement.join().await
    }
}
impl std::fmt::Debug for ArbiterPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArbiterPool")
            .field("workers", &self.size())
            .finish()
    }
}

#[cfg(test)]
#[path = "pool_retirement/pool_contracts.rs"]
mod retirement_contracts;

#[cfg(test)]
#[path = "pool_retirement/host_exit_contracts.rs"]
mod host_exit_contracts;

#[cfg(test)]
#[path = "pool_retirement/worker_shutdown_contracts.rs"]
mod worker_shutdown_contracts;

#[cfg(test)]
#[path = "system/last_owner_contracts.rs"]
mod last_owner_contracts;
#[cfg(test)]
#[path = "system/native_budget_contracts.rs"]
mod native_budget_contracts;

/// Resource and deadline settings for managed Actix actors.
#[derive(Debug, Clone)]
pub struct ActixSystemSettings {
    /// Owned worker count, at most 4096. Zero preserves the one-worker fallback.
    pub arbiter_count: usize,
    /// Actor placement, counting reserved executions through final disposal.
    /// LeastLoaded counts actors (including idle ones), not messages or CPU.
    pub load_balancing: parrot_api::runtime::LoadBalancingStrategy,
    /// Per-actor mailbox admission limit in 1..=1048576.
    pub mailbox_capacity: usize,
    /// Maximum outstanding user timers and streams per actor, in 0..=65536.
    /// Zero disables both.
    /// One internal receive-timeout worker has a separate reserved slot.
    pub max_context_tasks: usize,
    pub ask_timeout: Duration,
    pub shutdown_timeout: Duration,
}
impl Default for ActixSystemSettings {
    fn default() -> Self {
        Self {
            arbiter_count: num_cpus::get(),
            load_balancing: Default::default(),
            mailbox_capacity: 1024,
            max_context_tasks: crate::context_tasks::DEFAULT_MAX_CONTEXT_TASKS,
            ask_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
        }
    }
}

type CreationReply = Arc<Mutex<Option<tokio::sync::oneshot::Sender<ActorResult<BoxedActorRef>>>>>;
fn creation_reply(reply: &CreationReply, result: ActorResult<BoxedActorRef>) {
    if let Some(reply) = reply.lock().unwrap().take() {
        let _ = reply.send(result);
    }
}
pub(super) struct InstanceTicket {
    pub actor_ref: BoxedActorRef,
    pub control: Arc<ActorControl>,
    activate: Option<tokio::sync::oneshot::Sender<()>>,
    receive: tokio::sync::oneshot::Receiver<ActorResult<BoxedActorRef>>,
    cancel: bool,
}
impl InstanceTicket {
    pub(super) async fn wait(mut self) -> ActorResult<BoxedActorRef> {
        if let Some(activate) = self.activate.take() {
            let _ = activate.send(());
        }
        let result = (&mut self.receive)
            .await
            .map_err(|_| ActorError::InternalError("instance owner lost creation result".into()))?;
        self.cancel = false;
        result
    }
}
impl Drop for InstanceTicket {
    fn drop(&mut self) {
        if self.cancel {
            self.control
                .request_stop(tokio::time::Instant::now() + self.control.timeout);
        }
    }
}

pub(crate) struct Registration {
    pub actor_ref: Arc<dyn ActorRef>,
    pub control: Arc<ActorControl>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.control
            .request_stop(tokio::time::Instant::now() + self.control.timeout);
    }
}
#[derive(Default)]
pub(crate) struct Registry {
    pub actors: HashMap<String, Registration>,
    pub shutting_down: bool,
    stopped: bool,
    shutdown: Option<Completion>,
}

type ReceptionistSlot =
    crate::system::registrations::Registry<Option<Arc<dyn crate::system::ReceptionistGateway>>>;
type RemoteSlot =
    crate::system::registrations::Registry<Option<Arc<dyn crate::system::RemoteGateway>>>;

/// A Parrot system owns its registry and arbiter pool, never the host Actix System.
/// Native systems reserve one of 4096 process-wide cancellation slots until the
/// last system handle and any handed-off cleanup have been released. Releasing
/// the last handle starts the same owned drain as explicit shutdown. Two shared
/// process-lifetime threads perform owned-worker cancellation joins.
pub struct ActixActorSystem {
    leases: Arc<AtomicUsize>,
    cancellation: Arc<super::cancellation_executor::Reservation>,
    pub(crate) operation_timeouts: parrot_api::system::SystemTimeouts,
    pub(crate) root_cohorts: Arc<crate::root_cohort::RootCohorts>,
    pub(crate) registry: Arc<RwLock<Registry>>,
    receptionist: Arc<ReceptionistSlot>,
    remote: Arc<RemoteSlot>,
    pub(crate) settings: ActixSystemSettings,
    arbiters: ArbiterPool,
    // Constructors and retirement outlive the host; business stays on arbiters.
    pub(super) runtime: tokio::runtime::Handle,
    started: Instant,
    pub(super) policy_slots: Arc<tokio::sync::Semaphore>,
    pub(super) constructor_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) factory_admission: Arc<crate::constructor::admission::Admission>,
    pub(super) cleanup_slots: Arc<tokio::sync::Semaphore>,
    pub(super) notification_slots: Arc<tokio::sync::Semaphore>,
}

impl Clone for ActixActorSystem {
    fn clone(&self) -> Self {
        self.downgrade().upgrade().expect("live Actix system lease")
    }
}

impl Drop for ActixActorSystem {
    fn drop(&mut self) {
        if self.leases.fetch_sub(1, Ordering::AcqRel) == 1 {
            drop(self.begin_shutdown());
        }
    }
}

/// Contexts hold weak system resources: retaining an actor never retains its registry.
#[derive(Clone)]
pub(crate) struct WeakSystem {
    leases: Weak<AtomicUsize>,
    cancellation: Weak<super::cancellation_executor::Reservation>,
    operation_timeouts: parrot_api::system::SystemTimeouts,
    root_cohorts: Weak<crate::root_cohort::RootCohorts>,
    registry: Weak<RwLock<Registry>>,
    receptionist: Weak<ReceptionistSlot>,
    remote: Weak<RemoteSlot>,
    owners: Weak<ArbiterOwners>,
    settings: ActixSystemSettings,
    pub(super) runtime: tokio::runtime::Handle,
    started: Instant,
    pub(super) policy_slots: Arc<tokio::sync::Semaphore>,
    pub(super) constructor_slots: Arc<tokio::sync::Semaphore>,
    pub(super) factory_admission: Arc<crate::constructor::admission::Admission>,
    pub(super) cleanup_slots: Arc<tokio::sync::Semaphore>,
    pub(super) notification_slots: Arc<tokio::sync::Semaphore>,
}
impl WeakSystem {
    pub(super) fn receptionist_gateway(
        &self,
    ) -> Option<Arc<dyn crate::system::ReceptionistGateway>> {
        self.receptionist.upgrade()?.read().unwrap().clone()
    }

    pub(crate) fn upgrade(&self) -> ActorResult<ActixActorSystem> {
        let leases = self.leases.upgrade().ok_or(ActorError::Stopped)?;
        let cancellation = self.cancellation.upgrade().ok_or(ActorError::Stopped)?;
        let root_cohorts = self.root_cohorts.upgrade().ok_or(ActorError::Stopped)?;
        let registry = self.registry.upgrade().ok_or(ActorError::Stopped)?;
        let receptionist = self.receptionist.upgrade().ok_or(ActorError::Stopped)?;
        let remote = self.remote.upgrade().ok_or(ActorError::Stopped)?;
        let owners = self.owners.upgrade().ok_or(ActorError::Stopped)?;
        // Resource observers and in-flight cleanup can retain individual Arcs.
        // Only a still-live system lease may authorize another public handle.
        leases
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                if count == 0 {
                    None
                } else {
                    count.checked_add(1)
                }
            })
            .map_err(|_| ActorError::Stopped)?;
        Ok(ActixActorSystem {
            leases,
            cancellation,
            operation_timeouts: self.operation_timeouts.clone(),
            root_cohorts,
            registry,
            receptionist,
            remote,
            arbiters: ArbiterPool { inner: owners },
            settings: self.settings.clone(),
            runtime: self.runtime.clone(),
            started: self.started,
            policy_slots: self.policy_slots.clone(),
            constructor_slots: self.constructor_slots.clone(),
            factory_admission: self.factory_admission.clone(),
            cleanup_slots: self.cleanup_slots.clone(),
            notification_slots: self.notification_slots.clone(),
        })
    }
}

pub(super) fn remove_registration(
    registry: &Weak<RwLock<Registry>>,
    path: &str,
    control: &Arc<ActorControl>,
) {
    if let Some(registry) = registry.upgrade() {
        let mut registry = registry.write().unwrap();
        if registry
            .actors
            .get(path)
            .is_some_and(|entry| Arc::ptr_eq(&entry.control, control))
        {
            registry.actors.remove(path);
        }
    }
}

/// Bridges the public erased actor contract to Actix's concrete context.
///
/// Actix actors are dispatched serially on an arbiter, so the adapter can own
/// the erased actor directly. Every hook is forwarded with a coercion from the
/// concrete `ActixContext` to `dyn ActorContext`; the native fast path remains
/// disabled because the erased API cannot expose an engine-specific context.
pub(super) struct BoxedActixActor {
    pub(super) inner:
        Box<dyn ParrotActor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>,
}

impl ParrotActor for BoxedActixActor {
    type Config = Box<dyn Any + Send>;
    type Context = ActixContext<ActixActor<Self>>;

    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.init(ctx)
    }

    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.receive_message(message, ctx)
    }

    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        self.inner.can_batch_message(message)
    }

    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.receive_batch(messages, ctx)
    }

    fn use_async_handler(&self) -> bool {
        // The erased contract has no `ActixEngineExt` implementation for the
        // inner actor, so routing through Actix's sync probe would report an
        // unhandled message before this adapter gets a chance to delegate.
        true
    }

    fn handle_stream<'a>(
        &'a mut self,
        item: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.handle_stream(item, ctx)
    }

    fn stream_started<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.stream_started(ctx)
    }

    fn stream_finished<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.stream_finished(ctx)
    }

    fn stream_error<'a>(
        &'a mut self,
        error: ActorError,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.stream_error(error, ctx)
    }

    fn before_stop<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.before_stop(ctx)
    }

    fn handle_child_terminated<'a>(
        &'a mut self,
        child: BoxedActorRef,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let ctx: &mut dyn ActorContext = ctx;
        self.inner.handle_child_terminated(child, ctx)
    }

    fn state(&self) -> ActorState {
        self.inner.state()
    }
}

impl ActixEngineExt for BoxedActixActor {}

impl parrot_api::engine::AsyncEngineRuntime for ActixActorSystem {
    fn cohort_runtime(&self) -> Option<&dyn parrot_api::engine::AsyncCohortRuntime> {
        Some(self)
    }
    fn local_runtime(&self) -> Option<&dyn parrot_api::engine::AsyncLocalRuntime> {
        Some(self)
    }
    fn factory_runtime(&self) -> Option<&dyn parrot_api::engine::AsyncFactoryRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        actor: parrot_api::engine::RuntimeActor,
        config: parrot_api::engine::InstanceConfig,
        options: parrot_api::engine::SpawnOptions,
    ) -> parrot_api::types::BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let actor = OwnedPayload::new(actor, &self.runtime, &self.cleanup_slots);
        let config = OwnedPayload::new(config, &self.runtime, &self.cleanup_slots);
        Box::pin(parrot_api::trace::capture(async move {
            if let Err(error) = options.validate() {
                let (actor, config) = tokio::join!(actor.retire(), config.retire());
                actor.and(config).map_err(SystemError::ActorError)?;
                return Err(error);
            }
            match self.spawn_managed_at(
                BoxedActixActor {
                    inner: actor.take(),
                },
                config.take(),
                None,
                None,
                options.path,
            ) {
                Ok(ticket) => ticket.wait().await.map_err(SystemError::ActorError),
                Err(rejected) => {
                    rejected.retired.await.map_err(SystemError::ActorError)?;
                    Err(rejected.error)
                }
            }
        }))
    }

    fn get_actor<'a>(
        &'a self,
        path: &'a str,
    ) -> parrot_api::types::BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async move { self.resolve_actor(path) })
    }

    fn status(&self) -> SystemStatus {
        self.status()
    }

    fn shutdown(&self) -> parrot_api::types::BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(self.clone().shutdown())
    }
}

impl ActixActorSystem {
    /// Create a system with automatic supervision of ordinary root actors.
    pub fn with_guardian(
        settings: ActixSystemSettings,
        guardian: parrot_api::system::GuardianConfig,
    ) -> BoxedFuture<'static, Result<Self, SystemError>> {
        Self::with_guardian_and_timeouts(settings, guardian, Default::default())
    }

    pub(crate) fn with_guardian_and_timeouts(
        mut settings: ActixSystemSettings,
        guardian: parrot_api::system::GuardianConfig,
        timeouts: parrot_api::system::SystemTimeouts,
    ) -> BoxedFuture<'static, Result<Self, SystemError>> {
        let guardian = crate::owned_payload::OwnedPayload::unbound(guardian);
        Box::pin(parrot_api::trace::capture(async move {
            if let Err(error) = crate::system::startup::validate_timeouts(&timeouts) {
                guardian.retire().await?;
                return Err(error);
            }
            if !timeouts.system_shutdown.is_zero() {
                settings.shutdown_timeout = timeouts.system_shutdown;
            }
            if let Err(error) = settings.validate() {
                guardian.retire().await?;
                return Err(error);
            }
            let strategy = crate::root_cohort::guardian_strategy(guardian).await?;
            let strategy = crate::owned_payload::OwnedPayload::unbound(strategy);
            let system = match Self::with_settings(settings).await {
                Ok(mut system) => {
                    system.operation_timeouts = timeouts;
                    system
                }
                Err(error) => {
                    strategy.retire().await?;
                    return Err(error);
                }
            };
            system
                .root_cohorts
                .install_guardian(
                    strategy.take(),
                    system.settings.shutdown_timeout,
                    system.policy_slots.clone(),
                )
                .await?;
            Ok(system)
        }))
    }

    pub async fn new() -> Result<Self, SystemError> {
        Self::with_settings(Default::default()).await
    }
    pub async fn with_arbiter_count(arbiter_count: usize) -> Result<Self, SystemError> {
        Self::with_settings(ActixSystemSettings {
            arbiter_count,
            ..Default::default()
        })
        .await
    }

    /// Spawn an actor supplied through the public type-erased API. The adapter
    /// preserves all lifecycle and message hooks while using the normal
    /// managed Actix admission and cleanup path.
    pub fn spawn_root_boxed(
        &self,
        actor: Box<dyn ParrotActor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn Any + Send>,
    ) -> impl std::future::Future<Output = Result<Box<dyn ActorRef>, SystemError>> + Send + '_ {
        let adapter = BoxedActixActor { inner: actor };
        self.spawn_root_typed(adapter, config)
    }
    /// Validate native resource limits and deadlines before allocating workers.
    /// A running host Actix System is required even for an empty actor registry.
    pub async fn with_settings(mut settings: ActixSystemSettings) -> Result<Self, SystemError> {
        settings.validate()?;
        if actix::System::try_current().is_none() {
            return Err(SystemError::Other(anyhow::anyhow!(
                "ActixActorSystem requires a running Actix System"
            )));
        }
        settings.arbiter_count = settings.arbiter_count.max(1);
        let root_cohorts = Arc::new(crate::root_cohort::RootCohorts::new()?);
        let runtime = root_cohorts.retirement_runtime().clone();
        let cancellation = super::cancellation_executor::Reservation::new()?;
        // Construct on the host thread; later spawns may originate on foreign
        // Tokio threads where Actix's thread-local System is absent.
        let arbiters = ArbiterPool::with_distribution(
            settings.arbiter_count,
            settings.load_balancing.clone(),
        )?;
        Ok(Self {
            leases: Arc::new(AtomicUsize::new(1)),
            cancellation,
            operation_timeouts: Default::default(),
            root_cohorts,
            registry: Arc::new(RwLock::new(Registry::default())),
            receptionist: Arc::new(ReceptionistSlot::new(None)),
            remote: Arc::new(RemoteSlot::new(None)),
            settings,
            arbiters: arbiters.clone(),
            runtime,
            started: Instant::now(),
            policy_slots: Arc::new(tokio::sync::Semaphore::new(arbiters.size())),
            constructor_slots: Arc::new(tokio::sync::Semaphore::new(arbiters.size())),
            factory_admission: crate::constructor::admission::Admission::new(
                crate::constructor::admission::SYSTEM_CAPACITY,
            ),
            cleanup_slots: Arc::new(tokio::sync::Semaphore::new(arbiters.size())),
            notification_slots: Arc::new(tokio::sync::Semaphore::new(1024)),
        })
    }
    pub fn arbiter_size(&self) -> usize {
        self.settings.arbiter_count
    }
    /// Reserved actor executions per arbiter, including queued dispatch and idle
    /// actors through execution-future disposal. This is not a CPU/backlog sample.
    pub fn arbiter_loads(&self) -> Vec<usize> {
        self.arbiters.inner.distribution.loads()
    }

    /// Create an explicit supervision group for roots on this system.
    pub fn root_cohort(
        &self,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> BoxedFuture<'static, ActorResult<crate::root_cohort::RootCohort>> {
        self.root_cohorts.create(
            strategy,
            self.settings.shutdown_timeout,
            self.policy_slots.clone(),
        )
    }
    pub(crate) fn arbiter_pool(&self) -> ArbiterPool {
        self.arbiters.clone()
    }

    /// Install the gateway used by managed actor contexts in this system.
    pub fn set_receptionist_gateway(&self, gateway: Arc<dyn crate::system::ReceptionistGateway>) {
        drop(self.swap_receptionist_gateway(gateway));
    }

    /// Return the displaced owner so facade callers can retire it outside all locks.
    pub(crate) fn swap_receptionist_gateway(
        &self,
        gateway: Arc<dyn crate::system::ReceptionistGateway>,
    ) -> Option<Arc<dyn crate::system::ReceptionistGateway>> {
        self.receptionist.write().unwrap().replace(gateway)
    }

    pub fn receptionist_gateway(&self) -> Option<Arc<dyn crate::system::ReceptionistGateway>> {
        self.receptionist.read().unwrap().clone()
    }

    /// Install a message gateway without running its retired owner's destructor
    /// under an engine lock. Gateways that refer back to the engine must be weak.
    pub fn set_remote_gateway(&self, gateway: Arc<dyn crate::system::RemoteGateway>) {
        drop(self.swap_remote_gateway(gateway));
    }

    pub(crate) fn swap_remote_gateway(
        &self,
        gateway: Arc<dyn crate::system::RemoteGateway>,
    ) -> Option<Arc<dyn crate::system::RemoteGateway>> {
        self.remote.write().unwrap().replace(gateway)
    }

    pub(super) fn downgrade(&self) -> WeakSystem {
        WeakSystem {
            leases: Arc::downgrade(&self.leases),
            cancellation: Arc::downgrade(&self.cancellation),
            operation_timeouts: self.operation_timeouts.clone(),
            root_cohorts: Arc::downgrade(&self.root_cohorts),
            registry: Arc::downgrade(&self.registry),
            receptionist: Arc::downgrade(&self.receptionist),
            remote: Arc::downgrade(&self.remote),
            owners: Arc::downgrade(&self.arbiters.inner),
            settings: self.settings.clone(),
            runtime: self.runtime.clone(),
            started: self.started,
            policy_slots: self.policy_slots.clone(),
            constructor_slots: self.constructor_slots.clone(),
            factory_admission: self.factory_admission.clone(),
            cleanup_slots: self.cleanup_slots.clone(),
            notification_slots: self.notification_slots.clone(),
        }
    }

    pub fn spawn_root_typed<A>(
        &self,
        actor: A,
        config: A::Config,
    ) -> impl std::future::Future<Output = Result<Box<dyn ActorRef>, SystemError>> + Send + '_
    where
        A: ParrotActor<Context = ActixContext<ActixActor<A>>>
            + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
    {
        let actor = OwnedPayload::new(actor, &self.runtime, &self.cleanup_slots);
        let config = OwnedPayload::new(config, &self.runtime, &self.cleanup_slots);
        async move {
            match self.spawn_managed(actor.take(), config.take(), None, None) {
                Ok(ticket) => ticket.wait().await.map_err(SystemError::ActorError),
                Err(rejected) => {
                    rejected.retired.await.map_err(SystemError::ActorError)?;
                    Err(rejected.error)
                }
            }
        }
    }

    /// Spawn an instance with an explicit failure policy. Resume retains this
    /// instance; Restart requires a factory and terminates an instance-only actor.
    pub fn spawn_root_typed_with_strategy<A>(
        &self,
        actor: A,
        config: A::Config,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> impl std::future::Future<Output = Result<Box<dyn ActorRef>, SystemError>> + Send + '_
    where
        A: ParrotActor<Context = ActixContext<ActixActor<A>>>
            + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
    {
        let actor = OwnedPayload::new(actor, &self.runtime, &self.cleanup_slots);
        let config = OwnedPayload::new(config, &self.runtime, &self.cleanup_slots);
        let strategy = OwnedPayload::new(strategy, &self.runtime, &self.cleanup_slots);
        async move {
            if self.root_cohorts.guardian().is_some() {
                strategy.retire().await.map_err(SystemError::ActorError)?;
                return match self.spawn_managed(actor.take(), config.take(), None, None) {
                    Ok(ticket) => ticket.wait().await.map_err(SystemError::ActorError),
                    Err(rejected) => {
                        rejected.retired.await.map_err(SystemError::ActorError)?;
                        Err(rejected.error)
                    }
                };
            }
            let lifetime = crate::children::ChildLifetime::with_policy(
                std::sync::Weak::new(),
                Some(strategy.take()),
                self.policy_slots.clone(),
            );
            match self.spawn_managed(actor.take(), config.take(), None, Some(lifetime.clone())) {
                Ok(ticket) => ticket.wait().await.map_err(SystemError::ActorError),
                Err(rejected) => {
                    // A cancelled admission waiter must not detach capture disposal.
                    self.runtime
                        .spawn(async move {
                            let (inputs, policy) =
                                tokio::join!(rejected.retired, lifetime.dispose_policy());
                            inputs.and(policy).map_err(SystemError::ActorError)?;
                            Err(rejected.error)
                        })
                        .await
                        .map_err(|error| {
                            SystemError::ActorError(ActorError::InternalError(error.to_string()))
                        })?
                }
            }
        }
    }

    /// Admit a dynamic instance at an exact root path in a supervision group.
    pub fn spawn_root_in_cohort<'a, A>(
        &'a self,
        actor: A,
        config: A::Config,
        path: &'a str,
        cohort: &'a crate::root_cohort::RootCohort,
    ) -> impl std::future::Future<Output = ActorResult<BoxedActorRef>> + Send + 'a
    where
        A: ParrotActor<Context = ActixContext<ActixActor<A>>>
            + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
    {
        let mut actor = Some(OwnedPayload::new(actor, &self.runtime, &self.cleanup_slots));
        let mut config = Some(OwnedPayload::new(
            config,
            &self.runtime,
            &self.cleanup_slots,
        ));
        parrot_api::trace::capture(async move {
            let mut ticket = None;
            let mut rejection = None;
            let admitted = cohort.validate(&self.root_cohorts).and_then(|()| {
                cohort.scope.spawn(|lifetime| {
                    let pending = self
                        .spawn_managed_at(
                            actor.take().unwrap().take(),
                            config.take().unwrap().take(),
                            None,
                            Some(lifetime.clone()),
                            Some(path.to_owned()),
                        )
                        .map_err(|rejected| {
                            rejection = Some(rejected);
                            ActorError::Stopped
                        })?;
                    let reference = pending.actor_ref.clone_boxed();
                    let control = pending.control.clone();
                    ticket = Some(pending);
                    Ok((
                        reference,
                        Arc::new(move || {
                            control.request_stop(tokio::time::Instant::now() + control.timeout);
                            let lifetime = lifetime.clone();
                            Box::pin(async move { lifetime.wait().await })
                        }),
                    ))
                })
            });
            if let Err(error) = admitted {
                let actor = async {
                    match actor {
                        Some(actor) => actor.retire().await,
                        None => Ok(()),
                    }
                };
                let config = async {
                    match config {
                        Some(config) => config.retire().await,
                        None => Ok(()),
                    }
                };
                let rejected = async {
                    match rejection {
                        Some(rejected) => {
                            rejected
                                .retired
                                .await
                                .and(Err(ActorError::InitializationError(
                                    rejected.error.to_string(),
                                )))
                        }
                        None => Ok(()),
                    }
                };
                let (actor, config, rejected) = tokio::join!(actor, config, rejected);
                actor.and(config).and(rejected)?;
                return Err(error);
            }
            ticket.expect("admitted root instance").wait().await
        })
    }

    pub(super) fn spawn_managed<A>(
        &self,
        actor: A,
        config: A::Config,
        parent: Option<BoxedActorRef>,
        lifetime: Option<Arc<crate::children::ChildLifetime>>,
    ) -> Result<InstanceTicket, RejectedSpawn>
    where
        A: ParrotActor<Context = ActixContext<ActixActor<A>>>
            + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
    {
        self.spawn_managed_at(actor, config, parent, lifetime, None)
    }

    fn spawn_managed_at<A>(
        &self,
        actor: A,
        config: A::Config,
        parent: Option<BoxedActorRef>,
        lifetime: Option<Arc<crate::children::ChildLifetime>>,
        path: Option<String>,
    ) -> Result<InstanceTicket, RejectedSpawn>
    where
        A: ParrotActor<Context = ActixContext<ActixActor<A>>>
            + parrot_api::actor::ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
    {
        let actor = OwnedPayload::new(actor, &self.runtime, &self.cleanup_slots);
        let config = OwnedPayload::new(config, &self.runtime, &self.cleanup_slots);
        if parent.is_none()
            && lifetime.is_none()
            && let Some(guardian) = self.root_cohorts.guardian()
        {
            let mut actor = Some(actor);
            let mut config = Some(config);
            let mut ticket = None;
            let mut rejection = None;
            let admitted = guardian.scope.spawn(|lifetime| {
                let pending = self
                    .spawn_managed_at(
                        actor.take().unwrap().take(),
                        config.take().unwrap().take(),
                        None,
                        Some(lifetime.clone()),
                        path,
                    )
                    .map_err(|rejected| {
                        rejection = Some(rejected);
                        ActorError::Stopped
                    })?;
                let reference = pending.actor_ref.clone_boxed();
                let control = pending.control.clone();
                ticket = Some(pending);
                Ok((
                    reference,
                    Arc::new(move || {
                        control.request_stop(tokio::time::Instant::now() + control.timeout);
                        let lifetime = lifetime.clone();
                        Box::pin(async move { lifetime.wait().await })
                    }),
                ))
            });
            return match admitted {
                Ok(()) => Ok(ticket.expect("admitted guardian instance")),
                Err(error) => {
                    let (error, rejected_retirement) = match rejection {
                        Some(rejected) => (rejected.error, Some(rejected.retired)),
                        None => (SystemError::ActorError(error), None),
                    };
                    Err(RejectedSpawn {
                        error,
                        retired: Box::pin(async move {
                            let (actor, config, rejected) = tokio::join!(
                                async {
                                    match actor {
                                        Some(actor) => actor.retire().await,
                                        None => Ok(()),
                                    }
                                },
                                async {
                                    match config {
                                        Some(config) => config.retire().await,
                                        None => Ok(()),
                                    }
                                },
                                async {
                                    match rejected_retirement {
                                        Some(retired) => retired.await,
                                        None => Ok(()),
                                    }
                                },
                            );
                            actor.and(config).and(rejected)
                        }),
                    })
                }
            };
        }
        let config_retirement = config.receipt();
        let mut registry = self.registry.write().unwrap();
        let rejection = if registry.shutting_down {
            Some(SystemError::ShuttingDown)
        } else if path
            .as_ref()
            .is_some_and(|path| registry.actors.contains_key(path))
        {
            Some(SystemError::ActorCreationError(format!(
                "Actor path already exists: {}",
                path.as_ref().unwrap()
            )))
        } else {
            None
        };
        if let Some(error) = rejection {
            return Err(RejectedSpawn {
                error,
                retired: {
                    let actor = actor.retire();
                    let config = config.retire();
                    Box::pin(async move {
                        let (actor, config) = tokio::join!(actor, config);
                        actor.and(config)
                    })
                },
            });
        }
        let name = std::any::type_name::<A>()
            .rsplit("::")
            .next()
            .unwrap_or("unknown");
        let path = path.unwrap_or_else(|| {
            parent.as_ref().map_or_else(
                || format!("actix://{name}/{}", Uuid::new_v4().simple()),
                |parent| format!("{}/{}", parent.path(), Uuid::new_v4().simple()),
            )
        });
        let (mut control, finished) = ActorControl::with_lifetime(
            self.settings.shutdown_timeout,
            self.settings.max_context_tasks,
            lifetime,
            self.policy_slots.clone(),
        );
        ActorControl::configure_timeouts(&mut control, &self.operation_timeouts);
        let (ready, gate) = tokio::sync::watch::channel(None);
        Arc::get_mut(&mut control)
            .expect("unpublished instance control")
            .initialization_gate = Some(gate);
        let (created, receive) = tokio::sync::oneshot::channel();
        let created = Arc::new(Mutex::new(Some(created)));
        let (activate, activation) = tokio::sync::oneshot::channel();
        let outcome: crate::actix::actor::ActorOutcome = Default::default();
        let actor_base = ActixActor::managed(
            actor.take(),
            path.clone(),
            control.clone(),
            outcome.clone(),
            self.settings.mailbox_capacity,
            self.settings.ask_timeout,
        )
        .with_spawn_service(self.downgrade(), parent);
        let (sender, receiver) = actix::dev::channel::channel(self.settings.mailbox_capacity);
        let addr = actix::Addr::new(sender);
        let actor_ref =
            Box::new(ActixActorRef::new(addr, path.clone()).with_control(control.clone()))
                as BoxedActorRef;
        let actor_base = OwnedPayload::new(actor_base, &self.runtime, &self.cleanup_slots);
        let unpublished_retirement = actor_base.receipt();
        let holder = Arc::new(Mutex::new(Some(actor_base)));
        let dispatched = holder.clone();
        let (send, result) = tokio::sync::oneshot::channel();
        let tasks = control.tasks.clone();
        let children = control.children.clone();
        let cleanup_control = control.clone();
        if !self.arbiter_pool().reserve_arbiter().spawn_fn(move || {
            let actor_base = dispatched
                .lock()
                .unwrap()
                .take()
                .expect("one instance dispatch")
                .take();
            async move {
                // Construct the !Send Actix context only after arriving on its owner.
                // Await and dispose the complete context future, including its mailbox,
                // before acknowledging stop to the system monitor.
                let execution = std::panic::AssertUnwindSafe(async move {
                    actix::Context::with_receiver(receiver)
                        .into_future(actor_base)
                        .await;
                })
                .catch_unwind()
                .await;
                let cleanup = execution.map_err(|payload| {
                    ActorError::Panic(crate::actix::lifecycle::panic_message(payload))
                });
                let result = outcome.lock().unwrap().take().unwrap_or_else(|| {
                    Err(ActorError::InternalError(
                        "Actix actor ended without lifecycle completion".into(),
                    ))
                });
                tasks.close();
                let tasks_result = tasks.join().await;
                let children_result = children.join().await;
                let stop_result = cleanup_control
                    .cleanup_failure
                    .lock()
                    .unwrap()
                    .take()
                    .map_or(Ok(()), |error| Err(error.into_error()));
                let _ = send.send(ActorExit {
                    outcome: result,
                    cleanup: cleanup
                        .and(tasks_result)
                        .and(children_result)
                        .and(stop_result),
                });
            }
        }) {
            return Err(RejectedSpawn {
                error: SystemError::ActorError(ActorError::InternalError(
                    "Actix arbiter rejected actor execution".into(),
                )),
                retired: {
                    let config = config.retire();
                    Box::pin(async move {
                        let (actor, config) = tokio::join!(unpublished_retirement, config);
                        actor.and(config)
                    })
                },
            });
        }
        registry.actors.insert(
            path.clone(),
            Registration {
                actor_ref: Arc::from(actor_ref.clone_boxed()),
                control: control.clone(),
            },
        );
        let pending = control.clone();
        let reply = created.clone();
        let published = actor_ref.clone_boxed();
        let deadline = tokio::time::Instant::now() + control.creation_timeout;
        self.runtime.spawn(async move {
            if activation.await.is_err() { pending.request_stop(deadline); }
            let retirement = config.retire();
            tokio::pin!(retirement);
            let result = tokio::select! {
                biased;
                result = &mut retirement => result,
                _ = tokio::time::sleep_until(deadline) => {
                    pending.request_stop(deadline);
                    creation_reply(&reply, Err(ActorError::TimeoutDetail("instance configuration cleanup exceeded creation deadline".into())));
                    retirement.await
                }
            };
            // A ready cleanup receipt can win the biased select even after
            // the executor resumes past its deadline. Recheck before activation.
            if tokio::time::Instant::now() >= deadline {
                pending.request_stop(deadline);
                creation_reply(&reply, Err(ActorError::TimeoutDetail("instance configuration cleanup exceeded creation deadline".into())));
            } else if result.is_err() {
                pending.request_stop(deadline);
            }
            if result.is_ok() && !pending.is_stopping() { creation_reply(&reply, Ok(published)); }
            ready.send_replace(Some(Outcome::Stopped));
        });
        let inputs = Box::pin(async move {
            let (actor, config) = tokio::join!(unpublished_retirement, config_retirement);
            actor.and(config)
        });
        self.monitor_actor_with_disposal(
            path,
            control.clone(),
            finished,
            result,
            Some(inputs),
            Some(created),
        );
        Ok(InstanceTicket {
            actor_ref,
            control,
            activate: Some(activate),
            receive,
            cancel: true,
        })
    }

    pub(super) fn monitor_actor_with_disposal(
        &self,
        path: String,
        control: Arc<ActorControl>,
        finished: tokio::sync::watch::Sender<Option<Outcome>>,
        result: tokio::sync::oneshot::Receiver<ActorExit>,
        unpublished: Option<parrot_api::types::BoxedFuture<'static, ActorResult<()>>>,
        creation: Option<CreationReply>,
    ) {
        let registry = Arc::downgrade(&self.registry);
        let pool = self.arbiters.clone();
        let policy_slots = self.policy_slots.clone();
        self.root_cohorts.retirement_runtime().spawn(async move {
            let _pool_owner = pool;
            let cleanup = async {
                let mut execution = result.await;
                if let Some(unpublished) = unpublished {
                    let disposal = unpublished.await;
                    match &mut execution {
                        Ok(exit) => { if let Err(error) = disposal { exit.cleanup = Err(error); } },
                        Err(_) => {
                            control.tasks.close();
                            control.children.close();
                            let tasks = control.tasks.join().await;
                            let children = control.children.join().await;
                            let cleanup = disposal.and(tasks).and(children);
                            let error = ActorError::InternalError("Actix instance execution was cancelled".into());
                            execution = Ok(ActorExit { outcome: Err(error), cleanup });
                        }
                    }
                }
                control.begin_cleanup(tokio::time::Instant::now() + control.timeout);
                match execution {
                    Ok(ActorExit { outcome, cleanup }) => {
                        let escalated = control.escalated.lock().unwrap().take();
                        let cleanup = cleanup.map_err(crate::error_receipt::ActorReceipt::from_error);
                        let mut retirement = cleanup.clone();
                        let planned = control.typed_tree.get().and_then(|tree| tree.take_failure());
                        let error = match outcome {
                            Err(error) => Some(crate::error_receipt::ActorReceipt::from_error(error)),
                            Ok(()) => escalated.clone(),
                        };
                        let mut status = match cleanup {
                            Err(error) => Outcome::Failed(error),
                            Ok(()) => error.as_ref().map_or(Outcome::Stopped, |error| Outcome::Failed(error.clone())),
                        };
                        let mut failure_policy = None;
                        if (control.supervision_failed.load(Ordering::Acquire) || planned.is_some())
                            && !control.explicit_stop.load(Ordering::Acquire)
                            && let Some(error) = planned.as_ref().map(|(error,_)|error.clone()).or(error)
                        {
                            let policy = match planned {
                                Some((_,policy)) => Ok(Some(policy)),
                                None => control.lifetime.failure_policy(error.clone(), policy_slots, control.cleanup_deadline().unwrap_or_else(|| tokio::time::Instant::now() + control.timeout)).await,
                            };
                            match policy {
                                Ok(Some(policy)) if !control.expired_now() && !control.explicit_stop.load(Ordering::Acquire) => {
                                    if let Err(error) = policy.stop_siblings().await && retirement.is_ok() {
                                        let error = crate::error_receipt::ActorReceipt::from_error(error);
                                    retirement = Err(error.clone());
                                        status = Outcome::Failed(error);
                                    }
                                    if policy.decision == parrot_api::supervisor::SupervisionDecision::Escalate
                                        && !control.expired_now() && !control.explicit_stop.load(Ordering::Acquire) {
                                        parrot_api::trace::in_scope(control.failure_context(), || control.lifetime.escalate(error.clone()));
                                    }
                                    if policy.decision == parrot_api::supervisor::SupervisionDecision::Restart && retirement.is_ok() {
                                        status = Outcome::from_result(Err(ActorError::InternalError("supervision requested restart without a repeatable factory".into())));
                                    }
                                    failure_policy = Some(policy);
                                }
                                Ok(_) => {},
                                Err(error) if retirement.is_ok() => {
                                    let error = crate::error_receipt::ActorReceipt::from_error(error);
                                    retirement = Err(error.clone());
                                    status = Outcome::Failed(error);
                                }
                                Err(_) => {}, // Keep the earlier real cleanup failure.
                            }
                        }
                        if let Err(error) = control.lifetime.dispose_policy().await && retirement.is_ok() {
                            let error = crate::error_receipt::ActorReceipt::from_error(error);
                                    retirement = Err(error.clone());
                            status = Outcome::Failed(error);
                        }
                        remove_registration(&registry, &path, &control);
                        drop(failure_policy);
                        let reason = match (&status, escalated) {
                            (Outcome::Failed(current), Some(original)) if current.same_error(&original) => Some(parrot_api::supervisor::DeathReason::Escalated(crate::error_receipt::actor_summary(&original))),
                            _ => None,
                        };
                        Some((status, reason, retirement))
                    }
                    Err(_) => {
                        remove_registration(&registry, &path, &control);
                        None
                    }
                }
            };
            tokio::pin!(cleanup);
            let (execution, timed_out) = tokio::select! {
                biased;
                result = &mut cleanup => (result, false),
                _ = control.expired() => {
                    // Factory recovery may be waiting on a blocked arbiter.
                    // Seal admission from this independent lifecycle owner.
                    control.begin_cleanup(control.cleanup_deadline().unwrap_or_else(tokio::time::Instant::now));
                    let timeout = Outcome::Timeout(format!("Actix actor {path} stop exceeded configured deadline"));
                    finished.send_replace(Some(timeout.clone()));
                    if let Some(reply) = &creation { creation_reply(reply, Err(timeout.result().unwrap_err())); }
                    (cleanup.await, true)
                }
            };
            match execution {
                Some((status, reason, retirement)) => {
                    let outcome = status.clone().result();
                    let notification = control.lifetime.finish(retirement);
                    let reason = reason.unwrap_or_else(|| crate::actix::lifecycle::death_reason_from_result(&outcome, control.deadline_was_enforced()));
                    if !timed_out { finished.send_replace(Some(status.clone())); }
                    if let Some(reply) = &creation { creation_reply(reply, Err(status.result().err().unwrap_or(ActorError::Stopped))); }
                    notification.await;
                    control.death_watch.terminated(parrot_api::context::ActorTerminated {
                        generation: control.death_watch.id(), path: path.clone(), reason,
                    }, control.timeout).await;
                }
                None => {
                    control.death_watch.abandon();
                    if !timed_out {
                        finished.send_replace(Some(Outcome::Failed(crate::error_receipt::ActorReceipt::new(Arc::new(ActorError::InternalError(format!("Actix actor {path} execution was cancelled")))))));
                    }
                }
            }
        });
    }
    // Preserve the legacy signature, including callers using `&"path".into()`.
    // New callers can use resolve_actor(&str) without allocating a String.
    #[allow(clippy::ptr_arg)]
    pub async fn get_actor(&self, path: &String) -> Option<BoxedActorRef> {
        if path.starts_with("parrot://") {
            return self.resolve_actor(path);
        }
        // Legacy local lookup also observes registrations still owned by a
        // timed-out shutdown. Message admission remains the reference's concern.
        self.local_actor(path)
    }

    fn local_actor(&self, path: &str) -> Option<BoxedActorRef> {
        let actor = self
            .registry
            .read()
            .unwrap()
            .actors
            .get(path)?
            .actor_ref
            .clone();
        crate::system::lookup_owned(actor, |actor| Some(actor.clone_boxed()))
    }

    /// Resolve an exact local identity or explicit remote address. Gateway and
    /// reference callbacks execute after all registry/gateway guards are released.
    pub fn resolve_actor(&self, path: &str) -> Option<BoxedActorRef> {
        if self.registry.read().unwrap().shutting_down {
            return None;
        }
        if path.starts_with("parrot://") {
            let gateway = self.remote.read().unwrap().clone()?;
            return crate::system::lookup_owned(gateway, |gateway| gateway.lookup(path));
        }
        self.local_actor(path)
    }
    pub fn stop_actor<'a>(
        &'a self,
        path: &'a str,
    ) -> impl std::future::Future<Output = ActorResult<()>> + Send + 'a {
        parrot_api::trace::capture(async move {
            let actor = self
                .registry
                .read()
                .unwrap()
                .actors
                .get(path)
                .map(|entry| entry.actor_ref.clone())
                .ok_or_else(|| ActorError::ActorNotFound(path.into()))?;
            actor.stop().await
        })
    }
    /// Complete mailbox admission for each recipient and propagate any delivery failure.
    pub fn broadcast<M: Message + Clone + 'static>(
        &self,
        msg: M,
    ) -> impl std::future::Future<Output = Result<(), SystemError>> + Send + '_ {
        let mut source = crate::broadcast::Input::new(
            OwnedPayload::new(msg, &self.runtime, &self.cleanup_slots),
            self.constructor_slots.clone(),
        );
        parrot_api::trace::capture(async move {
            let result = async {
                let actors: Vec<_> = {
                    let registry = self.registry.read().unwrap();
                    if registry.shutting_down {
                        return Err(SystemError::ShuttingDown);
                    }
                    registry
                        .actors
                        .values()
                        .map(|entry| entry.actor_ref.clone())
                        .collect()
                };
                // Keep each prepared copy protected until its delivery future owns it.
                let mut deliveries = Vec::with_capacity(actors.len());
                for actor in actors {
                    let message = source.copy().await?;
                    deliveries.push(async move { actor.deliver(Box::new(message.take())).await });
                }
                let results = futures::future::join_all(deliveries).await;
                for result in results {
                    result?;
                }
                Ok(())
            }
            .await;
            crate::broadcast::finish(source, result, crate::error_receipt::public_system_summary)
                .await
        })
    }
    /// Close admission, stop all actors, and join only this system's workers.
    /// Concurrent callers share one owned operation; cancelling a caller does not cancel cleanup.
    pub async fn shutdown(self) -> Result<(), SystemError> {
        await_completion(self.begin_shutdown())
            .await
            .map_err(SystemError::from)
    }

    fn begin_shutdown(&self) -> tokio::sync::watch::Receiver<Option<Outcome>> {
        self.root_cohorts.close();
        let (completion, start) = {
            let mut registry = self.registry.write().unwrap();
            if let Some(completion) = &registry.shutdown {
                (completion.clone(), None)
            } else {
                registry.shutting_down = true;
                let deadline = tokio::time::Instant::now() + self.settings.shutdown_timeout;
                let (finished, completion) = tokio::sync::watch::channel(None);
                registry.shutdown = Some(completion.clone());
                let actors: Vec<_> = registry
                    .actors
                    .values()
                    .map(|entry| {
                        entry.control.request_stop(deadline);
                        (entry.actor_ref.clone(), entry.control.lifetime.clone())
                    })
                    .collect();
                (completion, Some((finished, deadline, actors)))
            }
        };
        // Retirement must remain owned when the caller's executor exits.
        // Dispatch after releasing the registry lock.
        if let Some((finished, deadline, actors)) = start {
            let owner = shutdown::ShutdownOwner::new(
                self.arbiters.clone(),
                self.registry.clone(),
                deadline,
                finished,
                self.cancellation.clone(),
                self.root_cohorts.clone(),
            );
            let pool = self.arbiters.clone();
            let root_cohorts = self.root_cohorts.clone();
            let timer = owner.timer();
            self.root_cohorts.retirement_runtime().spawn(async move {
                let results = futures::future::join_all(actors.into_iter().map(
                    |(actor, lifetime)| async move {
                        let stopped = actor.stop().await;
                        // A public stop deadline only limits waiting. Keep
                        // the arbiters and registry owned until each actor's
                        // resources (including policy captures) truly retire.
                        let retired = lifetime.wait().await;
                        stopped.and(retired)
                    },
                ))
                .await;
                let cohorts = root_cohorts.join().await;
                let joined = pool.stop_and_join().await;
                let result = joined.and(cohorts).and(
                    results
                        .into_iter()
                        .collect::<ActorResult<Vec<_>>>()
                        .map(|_| ()),
                );
                owner.finish(result);
            });
            self.root_cohorts.retirement_runtime().spawn(timer);
        }
        completion
    }
    pub fn status(&self) -> SystemStatus {
        let registry = self.registry.read().unwrap();
        SystemStatus {
            state: if registry.stopped {
                SystemState::Stopped
            } else if registry.shutting_down {
                SystemState::ShuttingDown
            } else {
                SystemState::Running
            },
            active_actors: registry.actors.len(),
            uptime: self.started.elapsed(),
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: self
                    .arbiters
                    .inner
                    .retirement
                    .owned_threads(self.arbiters.size()),
            },
        }
    }
}

#[cfg(test)]
#[path = "system/creation_deadline_contracts.rs"]
mod creation_deadline_contracts;

#[cfg(test)]
#[path = "system/queued_construction_contracts.rs"]
mod queued_construction_contracts;

#[cfg(test)]
mod lookup_retirement_tests {
    use super::*;
    struct LookupRef {
        clone: Option<Box<dyn Fn() + Send + Sync>>,
        retired: Arc<AtomicUsize>,
    }
    impl std::fmt::Debug for LookupRef {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("LookupRef")
        }
    }
    impl Drop for LookupRef {
        fn drop(&mut self) {
            self.retired.fetch_add(1, Ordering::SeqCst);
            panic!("local lookup ref retirement");
        }
    }
    impl ActorRef for LookupRef {
        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { unreachable!() })
        }
        fn send_with_timeout<'a>(
            &'a self,
            message: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(message)
        }
        fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { unreachable!() })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { unreachable!() })
        }
        fn path(&self) -> String {
            "/lookup".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            if let Some(callback) = &self.clone {
                callback();
            }
            Box::new(Self {
                clone: None,
                retired: self.retired.clone(),
            })
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    #[actix::test]
    async fn local_lookup_protects_last_owner_and_unpublished_clone_retirement() {
        for callback_panics in [false, true] {
            let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
            let registry = Arc::downgrade(&system.registry);
            let count = Arc::new(AtomicUsize::new(0));
            let actor: Arc<dyn ActorRef> = Arc::new(LookupRef {
                clone: Some(Box::new(move || {
                    drop(
                        registry
                            .upgrade()
                            .unwrap()
                            .write()
                            .unwrap()
                            .actors
                            .remove("/lookup"),
                    );
                    if callback_panics {
                        panic!("local lookup clone callback");
                    }
                })),
                retired: count.clone(),
            });
            let (control, _finished) = ActorControl::new(Duration::from_secs(1));
            system.registry.write().unwrap().actors.insert(
                "/lookup".into(),
                Registration {
                    actor_ref: actor,
                    control,
                },
            );
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                system.resolve_actor("/lookup")
            }));
            assert!(
                result.is_err(),
                "retirement failure cannot publish a local clone"
            );
            drop(result);
            assert_eq!(
                count.load(Ordering::SeqCst),
                if callback_panics { 1 } else { 2 }
            );
            assert!(system.resolve_actor("/lookup").is_none());
            system.shutdown().await.unwrap();
        }
    }
}

#[cfg(test)]
mod rejection_tests {
    use super::*;
    use parrot_api::{
        actor::{ActorConfig, ActorState},
        types::{BoxedFuture, BoxedMessage},
    };
    struct ExplodingInput(Arc<AtomicUsize>);
    impl Drop for ExplodingInput {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("intentional rejected input destructor");
        }
    }
    impl ActorConfig for ExplodingInput {}
    struct RejectedActor {
        _resource: ExplodingInput,
    }
    impl parrot_api::actor::ActixEngineExt for RejectedActor {}
    impl ParrotActor for RejectedActor {
        type Config = ExplodingInput;
        type Context = ActixContext<ActixActor<Self>>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            _: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Ok(Box::new(()) as BoxedMessage) })
        }
    }
    #[actix::test]
    async fn rejected_arbiter_submission_owns_actor_and_config_disposal() {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        // Simulate loss of the execution worker while admission is still open.
        system.arbiters.stop_and_join().await.unwrap();
        let actor = Arc::new(AtomicUsize::new(0));
        let config = Arc::new(AtomicUsize::new(0));
        let result = std::panic::AssertUnwindSafe(system.spawn_root_typed(
            RejectedActor {
                _resource: ExplodingInput(actor.clone()),
            },
            ExplodingInput(config.clone()),
        ))
        .catch_unwind()
        .await;
        assert!(
            result.is_ok(),
            "arbiter rejection dropped user inputs on caller"
        );
        assert!(result.unwrap().is_err());
        assert_eq!(actor.load(Ordering::SeqCst), 1);
        assert_eq!(config.load(Ordering::SeqCst), 1);
        assert_eq!(system.status().active_actors, 0);
        system.clone().shutdown().await.unwrap();
    }

    #[actix::test]
    async fn boxed_spawn_executes_erased_actor_lifecycle() {
        struct ErasedEcho {
            initialized: Arc<AtomicUsize>,
            received: Arc<AtomicUsize>,
            stopped: Arc<AtomicUsize>,
        }
        impl ParrotActor for ErasedEcho {
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

        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let initialized = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicUsize::new(0));
        let actor_ref = system
            .spawn_root_boxed(
                Box::new(ErasedEcho {
                    initialized: initialized.clone(),
                    received: received.clone(),
                    stopped: stopped.clone(),
                }),
                Box::new("erased-config") as Box<dyn Any + Send>,
            )
            .await
            .expect("boxed Actix spawn should succeed");

        let reply = actor_ref
            .send(Box::new(7_u64) as BoxedMessage)
            .await
            .expect("boxed Actix actor should process messages");
        assert_eq!(reply.downcast::<u64>().unwrap().as_ref(), &7);
        assert_eq!(initialized.load(Ordering::SeqCst), 1);
        assert_eq!(received.load(Ordering::SeqCst), 1);

        actor_ref
            .stop()
            .await
            .expect("boxed actor should stop cleanly");
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        system.shutdown().await.unwrap();
    }

    #[actix::test]
    async fn shutdown_joins_pool_and_reports_no_live_workers() {
        let system = ActixActorSystem::with_arbiter_count(2).await.unwrap();
        assert_eq!(system.status().resources.thread_count, 3);
        system.clone().shutdown().await.unwrap();
        let status = system.status();
        assert_eq!(status.state, SystemState::Stopped);
        assert_eq!(status.active_actors, 0);
        assert_eq!(status.resources.thread_count, 0);
    }
}

#[cfg(test)]
mod shutdown_deadline_tests {
    use super::*;

    async fn completed_shutdown_at(
        elapsed: Duration,
    ) -> (
        Result<(), SystemError>,
        Result<(), SystemError>,
        SystemState,
    ) {
        let mut system = ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_millis(10),
            ..Default::default()
        })
        .await
        .unwrap();
        // Use the paused test clock for lifecycle work as well.
        Arc::get_mut(&mut system.root_cohorts)
            .unwrap()
            .set_test_retirement_runtime(tokio::runtime::Handle::current());
        // Workers may have retired before shutdown is requested. Make cleanup
        // immediately ready without replacing or mocking the actual owner.
        system.arbiters.stop_and_join().await.unwrap();
        tokio::time::pause();
        let shutdown = system.clone().shutdown();
        tokio::pin!(shutdown);
        assert!(futures::poll!(shutdown.as_mut()).is_pending());
        tokio::time::advance(elapsed).await;
        let first = shutdown.await;
        let repeated = system.clone().shutdown().await;
        let state = system.status().state;
        tokio::time::resume();
        (first, repeated, state)
    }

    #[actix::test]
    async fn completed_shutdown_before_deadline_keeps_success() {
        let (first, repeated, state) = completed_shutdown_at(Duration::ZERO).await;
        assert!(first.is_ok(), "early shutdown: {first:?}");
        assert!(repeated.is_ok(), "repeated early shutdown: {repeated:?}");
        assert_eq!(state, SystemState::Stopped);
    }

    async fn assert_expired_shutdown(elapsed: Duration) {
        let (first, repeated, state) = completed_shutdown_at(elapsed).await;
        assert!(
            matches!(
                first,
                Err(SystemError::ActorError(ActorError::TimeoutDetail(_)))
            ),
            "ready cleanup must not hide the expired shutdown deadline: {first:?}"
        );
        assert!(
            matches!(
                repeated,
                Err(SystemError::ActorError(ActorError::TimeoutDetail(_)))
            ),
            "repeated shutdown must retain the timeout: {repeated:?}"
        );
        assert_eq!(
            state,
            SystemState::Stopped,
            "timeout must not prevent actual retirement"
        );
    }

    #[actix::test]
    async fn completed_shutdown_at_deadline_keeps_timeout() {
        assert_expired_shutdown(Duration::from_millis(10)).await;
    }

    #[actix::test]
    async fn completed_shutdown_after_deadline_keeps_timeout() {
        assert_expired_shutdown(Duration::from_millis(11)).await;
    }
}
