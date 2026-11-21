use futures::{StreamExt, stream::FuturesUnordered};
use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use super::worker::{IsolatedCleanupTracker, Worker, WorkerConfig};
use super::worker_manager::WorkerManager;
use crate::thread::config::ThreadActorConfig;
use crate::thread::error::SystemError;
use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::ThreadScheduler;
use crate::thread::scheduler::queue::SchedulingQueue;

#[cfg(test)]
#[path = "configuration_contracts.rs"]
mod configuration_contracts;
#[cfg(test)]
#[path = "poll_retirement_contracts.rs"]
mod poll_retirement_contracts;
#[cfg(test)]
#[path = "shutdown_contracts.rs"]
mod shutdown_contracts;

#[path = "shutdown.rs"]
mod shutdown;
use shutdown::{Failures, MailboxClose, Receipt};

/// Configuration for the shared thread pool
#[derive(Debug, Clone)]
pub struct SharedThreadPoolConfig {
    /// Configured core worker tasks on the caller-owned Tokio runtime
    pub pool_size: usize,

    /// Maximum resident ready entries shared by both priority lanes
    pub max_queue_capacity: usize,
    /// Worker-local FIFO requeues and stealing; false keeps central FIFO selection.
    /// The capacity is shared across global, high-priority and local ready entries.
    pub work_stealing: bool,

    /// Maximum number of messages to process in one batch
    pub max_messages_per_batch: usize,

    /// Duration to sleep when idle before checking for work again
    pub idle_sleep_duration: Duration,

    /// Whether to yield to the scheduler after processing each message
    pub yield_after_each_message: bool,

    /// Whether to log detailed processing metrics
    pub enable_detailed_logging: bool,

    /// Elastic-scaling: maximum number of *temporary* burst workers.
    ///
    /// Burst workers are spawned when the scheduling queue stays backlogged
    /// while all core workers are busy (e.g. every core worker is stuck in a
    /// minute-level CPU task) and exit after `burst_idle_timeout` of
    /// continuous idleness. The worker-task count is bounded by
    /// `pool_size + burst_workers_max`, bounding elastic task growth.
    pub burst_workers_max: usize,

    /// Elastic-scaling: how long the queue must stay backlogged (with no
    /// idle core worker) before a burst worker is spawned.
    /// Compared in whole microseconds without narrowing the duration.
    pub burst_backlog_threshold: Duration,

    /// Elastic-scaling: how long a burst worker idles before exiting.
    pub burst_idle_timeout: Duration,
}

impl Default for SharedThreadPoolConfig {
    fn default() -> Self {
        Self {
            pool_size: num_cpus::get(),
            max_queue_capacity: 10000,
            work_stealing: false,
            max_messages_per_batch: 10,
            idle_sleep_duration: Duration::from_millis(10),
            yield_after_each_message: false,
            enable_detailed_logging: false,
            // Elastic defaults: up to `num_cpus` extra worker tasks, requiring
            // the queue to stay backlogged ≥100ms before scaling out, and
            // reaping burst workers after 5s of idleness.
            burst_workers_max: num_cpus::get(),
            burst_backlog_threshold: Duration::from_millis(100),
            burst_idle_timeout: Duration::from_secs(5),
        }
    }
}

impl SharedThreadPoolConfig {
    /// Check the direct scheduler boundary before queues or tasks are created.
    pub fn validate(&self) -> Result<(), SystemError> {
        use crate::thread::scheduler::configuration::{count, duration};
        use parrot_config::ThreadResolved;
        for (key, value, maximum) in [
            (
                "scheduler.shared.pool_size",
                self.pool_size,
                ThreadResolved::MAX_WORKERS,
            ),
            (
                "scheduler.shared.max_queue_capacity",
                self.max_queue_capacity,
                ThreadResolved::MAX_QUEUE_CAPACITY,
            ),
            (
                "scheduler.shared.max_messages_per_batch",
                self.max_messages_per_batch,
                super::super::MAX_MESSAGES_PER_BATCH,
            ),
        ] {
            count(key, value, 1, maximum)?;
        }
        if self
            .pool_size
            .checked_add(self.burst_workers_max)
            .is_none_or(|total| total > ThreadResolved::MAX_WORKERS)
        {
            return Err(SystemError::ConfigError(format!(
                "scheduler.shared.burst_workers_max: core and burst workers together must be <= {}",
                ThreadResolved::MAX_WORKERS,
            )));
        }
        duration(
            "scheduler.shared.idle_sleep_duration",
            self.idle_sleep_duration,
        )?;
        if self.burst_workers_max > 0 {
            duration(
                "scheduler.shared.burst_backlog_threshold patrol tick",
                (self.burst_backlog_threshold / 2).max(Duration::from_millis(1)),
            )?;
        }
        // burst_idle_timeout is only compared with elapsed time; no deadline
        // is constructed from it, so Duration::MAX is a valid nonexpiring idle budget.
        Ok(())
    }
}

/// Status codes for the scheduler
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerStatus {
    /// Scheduler is initializing
    Initializing = 0,

    /// Scheduler is running normally
    Running = 1,

    /// Scheduler is shutting down
    ShuttingDown = 2,

    /// Scheduler has completed shutdown
    Shutdown = 3,

    /// Scheduler has encountered an error
    Error = 4,
}

impl SchedulerStatus {
    pub fn from_usize(v: usize) -> Self {
        match v {
            0 => SchedulerStatus::Initializing,
            1 => SchedulerStatus::Running,
            2 => SchedulerStatus::ShuttingDown,
            3 => SchedulerStatus::Shutdown,
            _ => SchedulerStatus::Error,
        }
    }
}

/// Metrics about the scheduler state
#[derive(Debug, Clone)]
pub struct SchedulerMetrics {
    /// Configured core worker tasks, not an owned OS-thread count
    pub pool_size: usize,

    /// Number of currently-alive elastic burst workers
    pub burst_workers_alive: usize,

    /// Current length of the scheduling queue
    pub queue_length: usize,

    /// Configured resident ready-queue capacity.
    pub queue_capacity: usize,

    /// Registered actors waiting for ready-queue space (one marker per actor).
    pub deferred_ready: usize,
    /// Resident normal work in core-worker local queues (included in queue_length).
    pub local_ready: usize,
    /// Batches selected from the selecting worker's own local queue.
    pub local_selections: u64,
    /// Batches selected from another core worker's local queue.
    pub work_steals: u64,

    /// Whether the scheduler is shutting down
    pub is_shutting_down: bool,

    /// Current status of the scheduler
    pub status: SchedulerStatus,

    /// Number of active processors
    pub active_processors: usize,
}

/// Elastic burst-worker controller.
///
/// `probe()` is called from mailbox wake hooks (i.e. on message arrival).
/// It decides whether to spawn a temporary burst worker using the
/// algorithm documented on [`SharedThreadPool`].
pub struct ElasticController {
    /// Weak ref to the scheduling queue (probe reads its length)
    queue: std::sync::Weak<SchedulingQueue>,

    /// Weak ref to the worker manager (idle-count source)
    worker_manager: std::sync::Weak<WorkerManager>,

    /// Shared shutdown flag
    shutting_down: Arc<std::sync::atomic::AtomicBool>,

    /// Alive burst worker count (shared with pool)
    burst_alive: Arc<std::sync::atomic::AtomicUsize>,

    /// First-backlog timestamp in micros (shared with pool)
    backlog_since_us: Arc<std::sync::atomic::AtomicU64>,

    /// Pool creation instant (time base for backlog_since_us)
    pool_started: std::time::Instant,

    /// Spawn decision serializer
    spawn_gate: Arc<std::sync::Mutex<()>>,

    /// Pool config (burst parameters)
    config: SharedThreadPoolConfig,

    /// Runtime handle for spawning burst workers
    runtime: Handle,

    workers: std::sync::Weak<std::sync::Mutex<Vec<JoinHandle<()>>>>,
    retirement: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
    failures: Arc<Failures>,
    cleanups: Arc<IsolatedCleanupTracker>,
}

impl ElasticController {
    /// Probe scheduler pressure; maybe spawn one burst worker.
    pub fn probe(&self) {
        use std::sync::atomic::Ordering as O;

        if self.shutting_down.load(O::Relaxed) {
            return;
        }
        // Static budget disabled → feature off entirely.
        if self.config.burst_workers_max == 0 {
            return;
        }

        let Some(queue) = self.queue.upgrade() else {
            return;
        };
        let Some(wm) = self.worker_manager.upgrade() else {
            return;
        };

        // No pressure if the queue drained or a core worker is idle.
        if queue.is_empty() || wm.idle_worker_count() > 0 {
            self.backlog_since_us.store(0, O::Relaxed);
            return;
        }

        // Record (or read) the first-backlog instant.
        //
        // compare_exchange semantics: only the *first* observer of a new
        // backlog period stores its timestamp; later probes must NOT
        // overwrite it (a plain swap would restart the window on every
        // probe and the threshold would never be reached).
        let now_us = self.pool_started.elapsed().as_micros() as u64;
        let first_us =
            match self
                .backlog_since_us
                .compare_exchange(0, now_us, O::AcqRel, O::Relaxed)
            {
                Ok(_) => now_us,   // we are the first observer
                Err(prev) => prev, // keep the original timestamp
            };
        if u128::from(now_us.saturating_sub(first_us))
            < self.config.burst_backlog_threshold.as_micros()
        {
            // Backlog not persistent enough yet; wait for more probes.
            return;
        }

        // Backlog persisted: spawn at most one burst worker per probe,
        // bounded by the global budget.
        let alive = self.burst_alive.load(O::Relaxed);
        if alive >= self.config.burst_workers_max {
            return;
        }

        // Reset the timer so the next burst worker requires another full
        // threshold interval (prevents thundering spawns).
        self.backlog_since_us.store(0, O::Relaxed);

        // Serialize spawn decisions (cheap: only on the spawn path).
        let _gate = self.spawn_gate.lock().unwrap();
        let alive = self.burst_alive.load(O::Relaxed);
        if alive >= self.config.burst_workers_max || self.shutting_down.load(O::Acquire) {
            return;
        }
        let Some(workers) = self.workers.upgrade() else {
            return;
        };
        // A slow payload destructor must not allow unlimited finished handles
        // to accumulate behind its observer. Pause expansion until it retires.
        let mut retirement = self.retirement.lock().unwrap();
        if retirement.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        self.burst_alive.fetch_add(1, O::Relaxed);

        let queue = queue as Arc<SchedulingQueue>;
        let burst_alive = self.burst_alive.clone();
        let idle_timeout = self.config.burst_idle_timeout;
        let batch_size = self.config.max_messages_per_batch;
        let idle_sleep = self.config.idle_sleep_duration;

        let worker = Worker::from_validated_config(
            usize::MAX,
            self.runtime.clone(),
            queue,
            self.shutting_down.clone(),
            WorkerConfig {
                batch_size,
                idle_sleep_duration: idle_sleep,
                yield_after_each_message: self.config.yield_after_each_message,
                enable_detailed_logging: self.config.enable_detailed_logging,
            },
            self.cleanups.clone(),
        );
        // Burst workers share the pool shutdown flag and are joined/aborted
        // with core workers. Idle retirement must not cancel suspended actors.
        let handle = worker.spawn_burst(idle_timeout, burst_alive);
        let mut workers = workers.lock().unwrap();
        // A tracked observer consumes completed joins outside every pool lock.
        // It cannot be aborted by the grace deadline: it owns retirement receipts.
        {
            let mut finished = Vec::new();
            let mut index = 0;
            while index < workers.len() {
                if workers[index].is_finished() {
                    finished.push(workers.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            if let Some(previous) = retirement.take() {
                finished.push(previous);
            }
            if !finished.is_empty() {
                let failures = self.failures.clone();
                *retirement = Some(self.runtime.spawn(async move {
                    for handle in finished {
                        failures.join(handle.await, false);
                    }
                }));
            }
        }
        workers.push(handle);
    }
}

impl fmt::Debug for SharedThreadPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedThreadPool")
            .field("pool_size", &self.pool_size)
            .field(
                "workers",
                &self.workers.lock().map(|w| w.len()).unwrap_or(0),
            )
            .field("status", &self.status())
            .field(
                "is_shutting_down",
                &self.is_shutting_down.load(Ordering::Relaxed),
            )
            .finish()
    }
}

/// Scheduler implementation for managing a shared thread pool
///
/// SharedThreadPool maintains a group of Tokio worker tasks that pull ready
/// mailboxes from a bounded [`SchedulingQueue`]. With work_stealing enabled,
/// batches requeue locally and idle workers may steal them. Each mailbox carries its
/// type-erased processor (`Arc<dyn ProcessorInterface>`); workers execute it
/// without knowing the concrete actor type.
///
/// # Performance Characteristics
/// - Optional worker-local FIFO queues with stealing (no OS-thread affinity)
/// - Batch processing for efficiency
/// - Mailboxes re-queued while they still hold messages
pub struct SharedThreadPool {
    /// Size of thread pool
    pool_size: usize,

    /// Collection of worker task JoinHandles
    workers: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,

    /// Central scheduling queue for ready mailboxes
    scheduling_queue: Arc<SchedulingQueue>,

    /// System runtime handle
    runtime_handle: Handle,

    /// Shutdown flag
    is_shutting_down: Arc<AtomicBool>,

    /// Configuration
    config: SharedThreadPoolConfig,

    /// Worker manager
    worker_manager: Arc<WorkerManager>,

    /// Current status of the scheduler
    status: Arc<AtomicUsize>,

    /// Scheduled actor paths (registered by path, executed via queue)
    scheduled_paths: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<dyn Mailbox>>>>,

    shutdown_receipt: std::sync::Mutex<Option<Receipt>>,
    failures: Arc<Failures>,
    cleanups: Arc<IsolatedCleanupTracker>,
    retirement: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
    patrol: std::sync::Mutex<Option<JoinHandle<()>>>,

    // ----- Elastic burst-worker state -----
    /// Currently alive burst workers (bounded by burst_workers_max)
    burst_workers_alive: Arc<std::sync::atomic::AtomicUsize>,

    /// Monotonic micros when the queue was first observed backlogged with
    /// zero idle core workers (0 = not currently backlogged).
    backlog_since_us: Arc<std::sync::atomic::AtomicU64>,

    /// Instant source for backlog tracking (captured at pool creation).
    pool_started: std::time::Instant,

    /// Serialize burst-worker spawn decisions
    burst_spawn_gate: Arc<std::sync::Mutex<()>>,
}

impl SharedThreadPool {
    /// Create new SharedThreadPool
    ///
    /// # Arguments
    /// * `config` - Optional configuration for the thread pool
    /// * `runtime_handle` - Tokio runtime handle
    pub fn new(config: Option<SharedThreadPoolConfig>, runtime_handle: Handle) -> Self {
        Self::try_new(config, runtime_handle).expect("invalid shared scheduler configuration")
    }

    /// Return invalid input before allocating queue storage or spawning tasks.
    pub fn try_new(
        config: Option<SharedThreadPoolConfig>,
        runtime_handle: Handle,
    ) -> Result<Self, SystemError> {
        let config = config.unwrap_or_default();
        config.validate()?;
        Ok(Self::start(config, runtime_handle))
    }

    #[cfg(test)]
    fn new_unchecked_for_test(
        config: Option<SharedThreadPoolConfig>,
        runtime_handle: Handle,
    ) -> Self {
        Self::start(config.unwrap_or_default(), runtime_handle)
    }

    fn start(config: SharedThreadPoolConfig, runtime_handle: Handle) -> Self {
        let scheduling_queue = Arc::new(SchedulingQueue::with_workers(
            config.max_queue_capacity,
            if config.work_stealing {
                config.pool_size
            } else {
                0
            },
        ));
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        let status = Arc::new(AtomicUsize::new(SchedulerStatus::Initializing as usize));

        let worker_manager = Arc::new(WorkerManager::new(scheduling_queue.clone()));

        let mut pool = Self {
            pool_size: config.pool_size,
            workers: Arc::new(std::sync::Mutex::new(Vec::with_capacity(config.pool_size))),
            scheduling_queue,
            runtime_handle,
            is_shutting_down,
            config,
            worker_manager,
            status,
            scheduled_paths: Default::default(),
            shutdown_receipt: Default::default(),
            failures: Default::default(),
            cleanups: Default::default(),
            retirement: Default::default(),
            patrol: Default::default(),
            burst_workers_alive: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            backlog_since_us: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pool_started: std::time::Instant::now(),
            burst_spawn_gate: Arc::new(std::sync::Mutex::new(())),
        };

        pool.start_workers();
        pool.start_elastic_patrol();

        pool.status
            .store(SchedulerStatus::Running as usize, Ordering::SeqCst);

        pool
    }

    /// Periodic elastic patrol: probes scheduler pressure on a fixed tick
    /// so burst spawning does not depend on fresh message arrivals.
    ///
    /// This is essential when callers *await* their send (a saturated pool
    /// would otherwise never see another wake hook until a burst worker
    /// exists — a chicken-and-egg stall the stress suite exposed).
    fn start_elastic_patrol(&self) {
        if self.config.burst_workers_max == 0 {
            return;
        }
        let controller = self.elastic_controller();
        let shutting_down = self.is_shutting_down.clone();
        let tick = (self.config.burst_backlog_threshold / 2).max(Duration::from_millis(1));
        let handle = self.runtime_handle.spawn(async move {
            while !shutting_down.load(Ordering::Relaxed) {
                tokio::time::sleep(tick).await;
                controller.probe();
            }
        });
        *self.patrol.lock().unwrap() = Some(handle);
    }

    /// Start worker tasks
    fn start_workers(&mut self) {
        for worker_id in 0..self.pool_size {
            let worker_config = WorkerConfig {
                batch_size: self.config.max_messages_per_batch,
                idle_sleep_duration: self.config.idle_sleep_duration,
                yield_after_each_message: self.config.yield_after_each_message,
                enable_detailed_logging: self.config.enable_detailed_logging,
            };

            let worker = Worker::from_validated_config(
                worker_id,
                self.runtime_handle.clone(),
                self.scheduling_queue.clone(),
                self.is_shutting_down.clone(),
                worker_config,
                self.cleanups.clone(),
            );

            self.worker_manager.track_worker(worker.status());

            let handle = worker.spawn();
            self.workers.lock().unwrap().push(handle);
        }
    }

    /// Schedule an actor on the thread pool
    ///
    /// Registers the actor path, installs a wake hook on the mailbox, and
    /// pushes the (processor-carrying) mailbox into the scheduling queue.
    /// Workers process a batch and re-queue the mailbox while it still holds
    /// messages; the wake hook covers the race where a push lands after a
    /// worker decided not to re-queue an empty mailbox.
    /// Overrides are validated here; actor policy belongs to the already
    /// attached processor and mailbox, rather than being reconstructed here.
    pub async fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), SystemError> {
        self.schedule_now(path, mailbox, config)
    }

    pub(crate) fn schedule_now(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), SystemError> {
        if let Some(config) = &config {
            config
                .validate()
                .map_err(|error| SystemError::ConfigError(error.to_string()))?;
        }
        if self.is_shutting_down.load(Ordering::Relaxed) {
            return Err(SystemError::ShuttingDown);
        }

        if !mailbox.has_processor() {
            return Err(SystemError::Other(anyhow::anyhow!(
                "Mailbox for actor {} has no processor attached",
                path
            )));
        }

        {
            let mut paths = self.scheduled_paths.lock().unwrap();
            if paths.contains_key(path) {
                return Err(SystemError::RegistrationError(format!(
                    "actor {path} is already scheduled"
                )));
            }
            if !self.scheduling_queue.register(&mailbox) {
                return Err(SystemError::ShuttingDown);
            }
            paths.insert(path.to_string(), mailbox.clone());
        }

        // Every admission after this point can race with shutdown. Keep the
        // rollback in one place so a failed initial enqueue never leaves a
        // path that claims to be scheduled after the queue is closed.
        let rollback = || {
            self.scheduling_queue.unregister(&mailbox);
            let mut paths = self.scheduled_paths.lock().unwrap();
            if paths
                .get(path)
                .is_some_and(|registered| Arc::ptr_eq(registered, &mailbox))
            {
                paths.remove(path);
            }
        };

        // Install the wake hook: whenever a push succeeds after this actor is
        // scheduled, (re-)enqueue the mailbox so a worker picks it up even if
        // it had previously drained it to empty. The schedule slot ensures at
        // most one queue entry per mailbox.
        {
            let queue = self.scheduling_queue.clone();
            let weak_mailbox = Arc::downgrade(&mailbox);
            let shutting_down = self.is_shutting_down.clone();
            // Elastic-scaling probe: every wake (message arrival under load)
            // feeds the burst-worker controller.
            let elastic = self.elastic_controller();
            let hook = Arc::new(move || {
                if shutting_down.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(strong) = weak_mailbox.upgrade()
                    && strong.schedule_state().try_enqueue()
                {
                    // M2: wake path also honors the priority lane.
                    let lane = if strong.has_high_priority_messages() {
                        crate::thread::scheduler::queue::Lane::High
                    } else {
                        crate::thread::scheduler::queue::Lane::Normal
                    };
                    queue.enqueue_registered(strong, lane);
                    elastic.probe();
                }
            });
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                mailbox.set_wake_hook(hook);
            })) {
                rollback();
                return Err(SystemError::Other(anyhow::anyhow!(
                    "failed to install scheduler wake hook for {path}: {}",
                    crate::owned_payload::panic_message(payload)
                )));
            }
        }

        if mailbox.schedule_state().try_enqueue() {
            let admitted = self.scheduling_queue.enqueue_registered(
                mailbox.clone(),
                crate::thread::scheduler::queue::Lane::Normal,
            );
            if !admitted {
                rollback();
                return Err(SystemError::ShuttingDown);
            }
        }

        // Shutdown may have closed the queue immediately after the hook was
        // installed. Do not report success for an admission with no live
        // registration.
        if self.is_shutting_down.load(Ordering::Acquire) {
            rollback();
            return Err(SystemError::ShuttingDown);
        }

        Ok(())
    }

    /// Deschedule an actor from the thread pool
    pub async fn deschedule(&self, path: &str) -> Result<(), SystemError> {
        let removed = {
            let mut paths = self.scheduled_paths.lock().unwrap();
            paths.remove(path)
        };
        let mailbox = removed.ok_or_else(|| SystemError::ActorNotFound(path.to_string()))?;
        self.scheduling_queue.unregister(&mailbox);
        // Keep the hook: queue registration checks make stale wakes harmless.
        // Clearing it here could overwrite a concurrently installed hook when
        // the same mailbox is registered again.
        Ok(())
    }

    /// Check if an actor is scheduled
    pub fn is_scheduled(&self, path: &str) -> bool {
        self.scheduled_paths.lock().unwrap().contains_key(path)
    }

    /// Close admission and join every owned task. Concurrent/repeated calls share
    /// one result; cancelling a waiter does not cancel cleanup. The first caller's
    /// `timeout_ms` sets the grace period, after which pending workers are aborted
    /// and joined. Mailbox closure remains owned until actual cleanup completes,
    /// even past that period. The caller-owned Tokio runtime must remain alive.
    pub async fn shutdown(&self, timeout_ms: u64) -> Result<(), SystemError> {
        self.shutdown_on(timeout_ms, &self.runtime_handle).await
    }

    pub(crate) async fn shutdown_on(
        &self,
        timeout_ms: u64,
        runtime: &Handle,
    ) -> Result<(), SystemError> {
        let mut receipt = self.ensure_shutdown_on(timeout_ms, runtime);
        loop {
            if let Some(result) = receipt.borrow_and_update().clone() {
                return result.map_err(SystemError::ShutdownError);
            }
            receipt.changed().await.map_err(|_| {
                SystemError::ShutdownError(
                    "shared cleanup runtime stopped before completion".into(),
                )
            })?;
        }
    }

    fn ensure_shutdown(&self, timeout_ms: u64) -> Receipt {
        self.ensure_shutdown_on(timeout_ms, &self.runtime_handle)
    }

    fn ensure_shutdown_on(&self, timeout_ms: u64, runtime: &Handle) -> Receipt {
        let mut receipt = self.shutdown_receipt.lock().unwrap();
        if let Some(receipt) = &*receipt {
            return receipt.clone();
        }
        let _gate = self.burst_spawn_gate.lock().unwrap();
        self.is_shutting_down.store(true, Ordering::Release);
        self.status
            .store(SchedulerStatus::ShuttingDown as usize, Ordering::Release);
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        let retirement = self.retirement.lock().unwrap().take();
        let patrol = self.patrol.lock().unwrap().take();
        let (sender, receiver) = tokio::sync::watch::channel(None);
        *receipt = Some(receiver.clone());
        let queue = self.scheduling_queue.clone();
        let registrations = self.scheduled_paths.clone();
        let failures = self.failures.clone();
        let status = self.status.clone();
        let cleanups = self.cleanups.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        drop(_gate);
        drop(receipt);
        runtime.spawn(async move {
            let mut closing = FuturesUnordered::new();
            // Close the queue BEFORE taking the path index: a concurrent schedule
            // holding that index must either finish registration or see closed.
            for mailbox in queue.close_and_take() {
                failures.isolate("mailbox schedule release", || {
                    mailbox.schedule_state().force_release()
                });
                closing.push(MailboxClose::new(
                    mailbox,
                    failures.clone(),
                    "queued mailbox close",
                ));
            }
            let registrations = std::mem::take(&mut *registrations.lock().unwrap());
            for mailbox in registrations.into_values() {
                closing.push(MailboxClose::new(
                    mailbox,
                    failures.clone(),
                    "registered mailbox close",
                ));
            }
            // A grace expiry stops waiting here, not the individual close
            // operations. Join/abort workers before awaiting the remaining drain,
            // since a mailbox destructor can depend on worker retirement.
            while let Ok(Some(())) = tokio::time::timeout_at(deadline, closing.next()).await {}
            if let Some(patrol) = patrol {
                let already_finished = patrol.is_finished();
                patrol.abort();
                failures.join(patrol.await, !already_finished);
            }
            for mut worker in workers.into_iter().rev() {
                let result = tokio::time::timeout_at(deadline, &mut worker).await;
                match result {
                    Ok(result) => failures.join(result, false),
                    Err(_) => {
                        let already_finished = worker.is_finished();
                        worker.abort();
                        failures.join(worker.await, !already_finished);
                    }
                }
            }
            if let Some(retirement) = retirement {
                failures.join(retirement.await, false);
            }
            while closing.next().await.is_some() {}
            // Worker task joins do not imply that a cancelled user future's
            // destructor has run. Wait for this pool's actual poll/disposal
            // owners before publishing its terminal shutdown state.
            cleanups.wait().await;
            status.store(SchedulerStatus::Shutdown as usize, Ordering::Release);
            crate::owned_payload::publish_watch(&sender, failures.result());
        });
        receiver
    }

    /// Get the pool size
    pub fn pool_size(&self) -> usize {
        self.pool_size
    }

    // ------------------------------------------------------------------
    // Elastic burst workers (2026-10-02, per stress report M1 finding)
    // ------------------------------------------------------------------
    //
    // Motivation: when every core worker is stuck inside a long CPU-bound
    // handler (e.g. minute-level tasks occupying all 8 workers), short
    // tasks queued behind them starve for the entire duration. The elastic
    // controller spawns *temporary* burst workers to drain the backlog and
    // reaps them after `burst_idle_timeout` of continuous idleness.
    //
    // Worker-task bound: pool_size + burst_workers_max. The caller owns OS threads.
    //
    // Controller algorithm (probe() runs on every queue push under load):
    //   1. If a core worker is idle OR no burst budget remains → reset.
    //   2. If queue is empty → reset.
    //   3. Otherwise record the first-backlog instant (CAS once); once the
    //      backlog has persisted ≥ burst_backlog_threshold, spawn one
    //      burst worker and reset the timer (one worker per interval).

    /// Build a controller handle for injection into wake hooks.
    ///
    /// The controller holds only weak references to the queue so a parked
    /// spawn path can never keep the pool alive.
    fn elastic_controller(&self) -> Arc<ElasticController> {
        Arc::new(ElasticController {
            queue: Arc::downgrade(&self.scheduling_queue),
            worker_manager: Arc::downgrade(&self.worker_manager),
            shutting_down: self.is_shutting_down.clone(),
            burst_alive: self.burst_workers_alive.clone(),
            backlog_since_us: self.backlog_since_us.clone(),
            pool_started: self.pool_started,
            spawn_gate: self.burst_spawn_gate.clone(),
            config: self.config.clone(),
            runtime: self.runtime_handle.clone(),
            workers: Arc::downgrade(&self.workers),
            retirement: self.retirement.clone(),
            failures: self.failures.clone(),
            cleanups: self.cleanups.clone(),
        })
    }

    /// Get the current scheduler status
    pub fn status(&self) -> SchedulerStatus {
        SchedulerStatus::from_usize(self.status.load(Ordering::Relaxed))
    }

    /// Get metrics about the scheduler
    pub fn metrics(&self) -> SchedulerMetrics {
        SchedulerMetrics {
            pool_size: self.pool_size,
            burst_workers_alive: self.burst_workers_alive.load(Ordering::Relaxed),
            queue_length: self.scheduling_queue.len(),
            queue_capacity: self.scheduling_queue.capacity(),
            deferred_ready: self.scheduling_queue.deferred_len(),
            local_ready: self.scheduling_queue.local_len(),
            local_selections: self.scheduling_queue.local_pops(),
            work_steals: self.scheduling_queue.steals(),
            is_shutting_down: self.is_shutting_down.load(Ordering::Relaxed),
            status: self.status(),
            active_processors: self.worker_manager.tracked_worker_count(),
        }
    }
}

impl Drop for SharedThreadPool {
    fn drop(&mut self) {
        // The operation owns cleanup independently of this pool and its waiters.
        // Explicit shutdown is required to observe errors before runtime teardown.
        self.ensure_shutdown(0);
    }
}

impl ThreadScheduler for SharedThreadPool {
    fn metrics_thread_count(&self) -> usize {
        // These workers are tasks on a caller-owned Tokio runtime, not OS
        // threads owned by this pool. Configured slots remain in metrics().
        0
    }

    fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.runtime_handle
            .block_on(async { self.schedule(path, mailbox, config).await })
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    fn deschedule(&self, path: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.runtime_handle
            .block_on(async { self.deschedule(path).await })
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    fn is_scheduled(&self, path: &str) -> bool {
        self.is_scheduled(path)
    }

    fn shutdown(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.runtime_handle
            .block_on(async { self.shutdown(5000).await })
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::actor::ThreadActor;
    use crate::thread::config::ThreadActorConfig;
    use crate::thread::context::ThreadContext;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use crate::thread::processor::ActorProcessor;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::address::ActorPath;
    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
    use std::sync::Arc;

    /// Counting actor that records processed values.
    #[derive(Debug, Default)]
    struct CountingActor {
        count: u64,
    }

    impl Actor for CountingActor {
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
                    self.count += *v;
                }
                Ok(msg)
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    /// Build a mailbox with an attached processor-backed actor stack.
    fn build_mailbox(path: &str) -> Arc<MpscMailbox> {
        let actor_path = ActorPath::placeholder(path);
        let mailbox = Arc::new(MpscMailbox::new(64, actor_path));
        let context = ThreadContext::<CountingActor>::new_for_test(path);
        let processor = Arc::new(ActorProcessor::<CountingActor>::new(
            ThreadActor::new_for_test(CountingActor::default()),
            context,
            path.to_string(),
            ThreadActorConfig::default(),
        ));
        mailbox.set_processor(processor);
        mailbox
    }

    fn make_pool(pool_size: usize) -> SharedThreadPool {
        SharedThreadPool::new(
            Some(SharedThreadPoolConfig {
                pool_size,
                idle_sleep_duration: Duration::from_millis(2),
                ..Default::default()
            }),
            tokio::runtime::Handle::current(),
        )
    }

    #[tokio::test]
    async fn shutdown_joins_aborted_worker_before_reporting_completion() {
        struct Retired(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Retired {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let pool = make_pool(1);
        assert_eq!(pool.metrics_thread_count(), 0);
        assert_eq!(pool.metrics().pool_size, 1);
        let retired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = retired.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let pending = tokio::spawn(async move {
            let _owned = Retired(signal);
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        // shutdown pops from the end. Put the suspended task first so it is
        // aborted last; a current-thread executor cannot retire it until polled.
        pool.workers.lock().unwrap().insert(0, pending);
        pool.shutdown(0).await.unwrap();
        let disposed_at_completion = retired.load(Ordering::Acquire);
        tokio::task::yield_now().await;
        assert!(
            disposed_at_completion,
            "abort alone is not worker retirement"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_pool_starts_running_with_workers() {
        let pool = make_pool(2);
        assert_eq!(pool.pool_size(), 2);
        assert_eq!(pool.status(), SchedulerStatus::Running);

        let metrics = pool.metrics();
        assert_eq!(metrics.pool_size, 2);
        assert_eq!(metrics.status, SchedulerStatus::Running);
        assert!(!metrics.is_shutting_down);

        pool.shutdown(2000).await.unwrap();
        assert_eq!(pool.status(), SchedulerStatus::Shutdown);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_schedule_registers_path_and_processes_messages() {
        let pool = make_pool(2);
        let mailbox = build_mailbox("pool/process/a");

        pool.schedule("pool/process/a", mailbox.clone(), None)
            .await
            .unwrap();
        assert!(pool.is_scheduled("pool/process/a"));

        // Push messages; the shared pool must drain them.
        for v in 1..=10u64 {
            mailbox
                .push(
                    Box::new(v),
                    crate::thread::config::BackpressureStrategy::Block,
                )
                .await
                .unwrap();
        }

        for _ in 0..500 {
            if mailbox.is_empty().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(mailbox.is_empty().await, "pool must drain the mailbox");

        pool.shutdown(2000).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_schedule_rejects_mailbox_without_processor() {
        let pool = make_pool(1);
        let mailbox: Arc<dyn Mailbox> =
            Arc::new(MpscMailbox::new(8, ActorPath::placeholder("pool/no-proc")));

        let result = pool.schedule("pool/no-proc", mailbox, None).await;
        assert!(result.is_err());
        assert!(!pool.is_scheduled("pool/no-proc"));

        pool.shutdown(2000).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_deschedule_unknown_actor_errors() {
        let pool = make_pool(1);
        let result = pool.deschedule("does/not/exist").await;
        assert!(matches!(result, Err(SystemError::ActorNotFound(_))));
        pool.shutdown(2000).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_deschedule_removes_registration() {
        let pool = make_pool(1);
        let mailbox = build_mailbox("pool/desched/a");

        pool.schedule("pool/desched/a", mailbox, None)
            .await
            .unwrap();
        assert!(pool.is_scheduled("pool/desched/a"));

        pool.deschedule("pool/desched/a").await.unwrap();
        assert!(!pool.is_scheduled("pool/desched/a"));

        // Descheduling again fails: not registered anymore.
        let again = pool.deschedule("pool/desched/a").await;
        assert!(matches!(again, Err(SystemError::ActorNotFound(_))));

        pool.shutdown(2000).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_schedule_rejected_after_shutdown() {
        let pool = make_pool(1);
        pool.shutdown(2000).await.unwrap();

        let mailbox = build_mailbox("pool/late");
        let result = pool.schedule("pool/late", mailbox, None).await;
        assert!(matches!(result, Err(SystemError::ShuttingDown)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_actors_share_the_pool() {
        let pool = make_pool(2);
        let m1 = build_mailbox("pool/multi/a");
        let m2 = build_mailbox("pool/multi/b");

        pool.schedule("pool/multi/a", m1.clone(), None)
            .await
            .unwrap();
        pool.schedule("pool/multi/b", m2.clone(), None)
            .await
            .unwrap();

        for i in 0..5u64 {
            m1.push(
                Box::new(i),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
            m2.push(
                Box::new(i * 10),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        }

        // Both mailboxes must be drained by the pool workers.
        for _ in 0..500 {
            let e1 = m1.is_empty().await;
            let e2 = m2.is_empty().await;
            if e1 && e2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(m1.is_empty().await);
        assert!(m2.is_empty().await);

        pool.shutdown(2000).await.unwrap();
    }

    /// Regression test for the lost-wakeup race: a message pushed *after* a
    /// worker drained the mailbox to empty must still be processed, because
    /// the wake hook re-enqueues the mailbox.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_push_after_drain_is_still_processed_via_wake_hook() {
        let pool = make_pool(2);
        let mailbox = build_mailbox("pool/wake-hook/a");

        pool.schedule("pool/wake-hook/a", mailbox.clone(), None)
            .await
            .unwrap();

        // First wave: push and wait until fully drained (mailbox no longer
        // re-queued by the worker because it looked empty afterwards).
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        for _ in 0..500 {
            if mailbox.is_empty().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(mailbox.is_empty().await, "first wave must be drained");

        // Second wave arrives after the drain: must still be processed.
        mailbox
            .push(
                Box::new(2u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();

        for _ in 0..500 {
            if mailbox.is_empty().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            mailbox.is_empty().await,
            "second wave must be drained via the wake hook"
        );

        pool.shutdown(2000).await.unwrap();
    }

    #[test]
    fn test_scheduler_status_from_usize() {
        assert_eq!(
            SchedulerStatus::from_usize(0),
            SchedulerStatus::Initializing
        );
        assert_eq!(SchedulerStatus::from_usize(1), SchedulerStatus::Running);
        assert_eq!(
            SchedulerStatus::from_usize(2),
            SchedulerStatus::ShuttingDown
        );
        assert_eq!(SchedulerStatus::from_usize(3), SchedulerStatus::Shutdown);
        assert_eq!(SchedulerStatus::from_usize(99), SchedulerStatus::Error);
    }

    #[test]
    fn test_pool_debug_formatting() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let pool = SharedThreadPool::new(
            Some(SharedThreadPoolConfig {
                pool_size: 1,
                ..Default::default()
            }),
            rt.handle().clone(),
        );
        let repr = format!("{:?}", pool);
        assert!(repr.contains("SharedThreadPool"));
    }
    #[tokio::test]
    async fn scheduler_owns_deferred_actors_and_drop_breaks_ready_queue_cycles() {
        // No await before Drop: on this current-thread runtime the worker
        // cannot dequeue either registration, making saturation deterministic.
        let pool = SharedThreadPool::new(
            Some(SharedThreadPoolConfig {
                pool_size: 1,
                max_queue_capacity: 1,
                burst_workers_max: 0,
                ..Default::default()
            }),
            Handle::current(),
        );
        let resident = build_mailbox("owned/resident");
        let deferred = build_mailbox("owned/deferred");
        let weak_resident = Arc::downgrade(&resident);
        let weak_deferred = Arc::downgrade(&deferred);
        pool.schedule_now("owned/resident", resident, None).unwrap();
        pool.schedule_now("owned/deferred", deferred, None).unwrap();
        assert_eq!(pool.metrics().queue_length, 1);
        assert_eq!(pool.metrics().deferred_ready, 1);
        assert!(
            weak_deferred.upgrade().is_some(),
            "accepted actor lost its only owner while waiting for queue space"
        );
        let terminal = pool.status.clone();
        drop(pool);
        // Drop starts owned asynchronous retirement. Keep the runtime alive and
        // observe its actual completion before checking strong-reference cycles.
        tokio::time::timeout(Duration::from_secs(3), async {
            while SchedulerStatus::from_usize(terminal.load(Ordering::Acquire))
                != SchedulerStatus::Shutdown
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            weak_resident.upgrade().is_none(),
            "mailbox -> hook -> queue retained a resident actor"
        );
        assert!(
            weak_deferred.upgrade().is_none(),
            "deferred actor leaked after scheduler Drop"
        );
    }
    #[tokio::test]
    async fn descheduled_ready_mailbox_can_be_registered_again_without_losing_work() {
        let pool = SharedThreadPool::new(
            Some(SharedThreadPoolConfig {
                pool_size: 1,
                max_queue_capacity: 1,
                burst_workers_max: 0,
                ..Default::default()
            }),
            Handle::current(),
        );
        let first = build_mailbox("reuse/first");
        let waiting = build_mailbox("reuse/waiting");
        pool.schedule_now("reuse/first", first, None).unwrap();
        pool.schedule_now("reuse/waiting", waiting.clone(), None)
            .unwrap();
        waiting
            .push(
                Box::new(7_u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        pool.deschedule("reuse/waiting").await.unwrap();
        assert_eq!(pool.metrics().deferred_ready, 0);
        waiting
            .push(
                Box::new(8_u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        pool.schedule_now("reuse/waiting", waiting.clone(), None)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !waiting.is_empty().await {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("re-registration lost the earlier ready ownership");
        pool.shutdown(2000).await.unwrap();
    }
}
