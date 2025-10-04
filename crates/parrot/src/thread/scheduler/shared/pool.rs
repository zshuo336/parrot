use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use super::worker::{Worker, WorkerConfig};
use super::worker_manager::WorkerManager;
use crate::thread::config::ThreadActorConfig;
use crate::thread::error::SystemError;
use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::ThreadScheduler;
use crate::thread::scheduler::queue::SchedulingQueue;

/// Configuration for the shared thread pool
#[derive(Debug, Clone)]
pub struct SharedThreadPoolConfig {
    /// Number of worker threads
    pub pool_size: usize,

    /// Maximum capacity of the scheduling queue for metrics
    pub max_queue_capacity: usize,

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
    /// continuous idleness. The global thread count is bounded by
    /// `pool_size + burst_workers_max`, preventing thread explosions.
    pub burst_workers_max: usize,

    /// Elastic-scaling: how long the queue must stay backlogged (with no
    /// idle core worker) before a burst worker is spawned.
    pub burst_backlog_threshold: Duration,

    /// Elastic-scaling: how long a burst worker idles before exiting.
    pub burst_idle_timeout: Duration,
}

impl Default for SharedThreadPoolConfig {
    fn default() -> Self {
        Self {
            pool_size: num_cpus::get(),
            max_queue_capacity: 10000,
            max_messages_per_batch: 10,
            idle_sleep_duration: Duration::from_millis(10),
            yield_after_each_message: false,
            enable_detailed_logging: false,
            // Elastic defaults: up to `num_cpus` extra threads, requiring
            // the queue to stay backlogged ≥100ms before scaling out, and
            // reaping burst workers after 5s of idleness.
            burst_workers_max: num_cpus::get(),
            burst_backlog_threshold: Duration::from_millis(100),
            burst_idle_timeout: Duration::from_secs(5),
        }
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
    /// Number of worker threads in the pool
    pub pool_size: usize,

    /// Number of currently-alive elastic burst workers
    pub burst_workers_alive: usize,

    /// Current length of the scheduling queue
    pub queue_length: usize,

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
        if now_us.saturating_sub(first_us) < self.config.burst_backlog_threshold.as_micros() as u64
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
        if alive >= self.config.burst_workers_max {
            return;
        }
        self.burst_alive.fetch_add(1, O::Relaxed);

        let queue = queue as Arc<SchedulingQueue>;
        let burst_alive = self.burst_alive.clone();
        let idle_timeout = self.config.burst_idle_timeout;
        let batch_size = self.config.max_messages_per_batch;
        let idle_sleep = self.config.idle_sleep_duration;

        // Burst worker = Worker with a *dedicated* shutdown flag the reaper
        // controls. Setting it makes the worker exit at the next loop check
        // (after finishing its current batch) — no mid-batch abort.
        let worker_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = crate::thread::scheduler::shared::worker::Worker::new(
            usize::MAX, // marker id: burst worker
            self.runtime.clone(),
            queue.clone(),
            worker_shutdown.clone(),
            crate::thread::scheduler::shared::worker::WorkerConfig {
                batch_size,
                idle_sleep_duration: idle_sleep,
                yield_after_each_message: false,
                enable_detailed_logging: false,
            },
        );
        let status = worker.status();
        let handle = worker.spawn();

        // Reaper: set the dedicated flag after idle_timeout of continuous
        // idleness, then await the worker's natural exit.
        self.runtime.spawn(async move {
            let mut idle_since: Option<tokio::time::Instant> = None;
            loop {
                let is_processing = status.load(std::sync::atomic::Ordering::Relaxed) == 1;
                if is_processing {
                    idle_since = None;
                } else {
                    let started = *idle_since.get_or_insert_with(tokio::time::Instant::now);
                    if started.elapsed() >= idle_timeout {
                        break; // reap
                    }
                }
                tokio::time::sleep((idle_timeout / 10).max(Duration::from_millis(10))).await;
            }
            // Graceful: worker finishes its current batch, then observes the
            // flag at the top of the loop and exits.
            worker_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = handle.await;
            burst_alive.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
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
/// SharedThreadPool maintains a group of worker threads that pull ready
/// mailboxes from a central [`SchedulingQueue`]. Each mailbox carries its
/// type-erased processor (`Arc<dyn ProcessorInterface>`); workers execute it
/// without knowing the concrete actor type.
///
/// # Performance Characteristics
/// - Work stealing pattern for load balancing
/// - Batch processing for efficiency
/// - Mailboxes re-queued while they still hold messages
pub struct SharedThreadPool {
    /// Size of thread pool
    pool_size: usize,

    /// Collection of worker task JoinHandles
    workers: std::sync::Mutex<Vec<JoinHandle<()>>>,

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
    scheduled_paths: std::sync::Mutex<std::collections::HashSet<String>>,

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
        let config = config.unwrap_or_default();
        let scheduling_queue = Arc::new(SchedulingQueue::new(config.max_queue_capacity));
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        let status = Arc::new(AtomicUsize::new(SchedulerStatus::Initializing as usize));

        let worker_manager = Arc::new(WorkerManager::new(scheduling_queue.clone()));

        let mut pool = Self {
            pool_size: config.pool_size,
            workers: std::sync::Mutex::new(Vec::with_capacity(config.pool_size)),
            scheduling_queue,
            runtime_handle,
            is_shutting_down,
            config,
            worker_manager,
            status,
            scheduled_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
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
        let tick = self.config.burst_backlog_threshold / 2;
        self.runtime_handle.spawn(async move {
            while !shutting_down.load(Ordering::Relaxed) {
                tokio::time::sleep(tick).await;
                controller.probe();
            }
        });
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

            let worker = Worker::new(
                worker_id,
                self.runtime_handle.clone(),
                self.scheduling_queue.clone(),
                self.is_shutting_down.clone(),
                worker_config,
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
    pub async fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        _config: Option<ThreadActorConfig>,
    ) -> Result<(), SystemError> {
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
            paths.insert(path.to_string());
        }

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
            mailbox.set_wake_hook(Arc::new(move || {
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
                    queue.push_with_lane(strong, lane);
                    elastic.probe();
                }
            }));
        }

        mailbox.schedule_state().try_enqueue();
        self.scheduling_queue.push(mailbox.clone());

        Ok(())
    }

    /// Deschedule an actor from the thread pool
    pub async fn deschedule(&self, path: &str) -> Result<(), SystemError> {
        let removed = {
            let mut paths = self.scheduled_paths.lock().unwrap();
            paths.remove(path)
        };
        if !removed {
            return Err(SystemError::ActorNotFound(path.to_string()));
        }
        // Mailbox re-queueing is guarded by has_more_messages checks; the
        // mailbox may still be drained one last time but no new batches run.
        Ok(())
    }

    /// Check if an actor is scheduled
    pub fn is_scheduled(&self, path: &str) -> bool {
        self.scheduled_paths.lock().unwrap().contains(path)
    }

    /// Shutdown the thread pool
    ///
    /// # Arguments
    /// * `timeout_ms` - Timeout in milliseconds to wait for graceful shutdown
    pub async fn shutdown(&self, timeout_ms: u64) -> Result<(), SystemError> {
        self.status
            .store(SchedulerStatus::ShuttingDown as usize, Ordering::SeqCst);
        self.is_shutting_down.store(true, Ordering::SeqCst);

        // Wake all idle workers so they observe the shutdown flag.
        for _ in 0..self.pool_size {
            self.scheduling_queue.notify_handle().notify_one();
        }

        // Give workers a bounded chance to exit.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            // Take one handle out of the lock scope before awaiting so the
            // MutexGuard is not held across the await point.
            let next = { self.workers.lock().unwrap().pop() };
            let Some(handle) = next else { break };
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let _ = tokio::time::timeout(remaining, handle).await;
        }
        self.status
            .store(SchedulerStatus::Shutdown as usize, Ordering::SeqCst);
        Ok(())
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
    // Global thread bound: pool_size + burst_workers_max — no explosion.
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
            is_shutting_down: self.is_shutting_down.load(Ordering::Relaxed),
            status: self.status(),
            active_processors: self.worker_manager.tracked_worker_count(),
        }
    }
}

impl ThreadScheduler for SharedThreadPool {
    fn metrics_thread_count(&self) -> usize {
        self.pool_size
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
}
