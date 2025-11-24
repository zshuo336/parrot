//! # Actor System Runtime
//!
//! This module defines the runtime environment and execution configuration
//! for the Parrot actor system. It provides control over thread allocation,
//! scheduling, and resource management.
//!
//! ## Design Philosophy
//!
//! The runtime system is designed with these principles:
//! - Configurability: Fine-grained control over system resources
//! - Performance: Efficient task scheduling and execution
//! - Monitoring: Comprehensive metrics and diagnostics
//! - Adaptability: Multiple scheduling and load balancing strategies
//!
//! ## Core Components
//!
//! - `RuntimeConfig`: System-wide runtime settings
//! - `SchedulerConfig`: Task scheduling parameters
//! - `ActorRuntime`: Runtime management interface
//! - `RuntimeMetrics`: Performance monitoring
//!
//! ## Usage Example
//!
//! ```rust
//! use parrot_api::runtime::{RuntimeConfig, SchedulerConfig, LoadBalancingStrategy};
//! use std::time::Duration;
//!
//! let config = RuntimeConfig {
//!     worker_threads: None,
//!     io_threads: None,
//!     scheduler_config: SchedulerConfig {
//!         task_queue_capacity: 1000,
//!         task_timeout: Duration::from_secs(30),
//!         load_balancing: LoadBalancingStrategy::RoundRobin,
//!     },
//! };
//! // Pass `config` to your runtime implementation of choice, e.g.
//! // `ActorRuntime::start(config).await?` inside an async context.
//! // The API ships a Tokio-backed implementation:
//! // `let runtime = TokioActorRuntime::start(config).await?;`
//! # let _ = config;
//! ```

use crate::errors::ActorError;
use async_trait::async_trait;
use futures::FutureExt;
use std::time::Duration;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::sync::{Notify, Semaphore};

#[cfg(test)]
#[path = "runtime/lifecycle_contracts.rs"]
mod lifecycle_contracts;

/// Configuration for the actor system runtime.
///
/// Each runtime applies the settings it owns. The default Tokio adapter borrows
/// the caller's executor; specifying a worker or I/O count gives it an owned
/// Tokio runtime so those settings have real execution semantics.
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// Number of threads for processing actor messages.
    ///
    /// Native Thread maps this to core worker tasks, Actix to owned arbiters.
    /// None preserves the native settings. TokioActorRuntime borrows the caller
    /// executor when both counts are `None`; an explicit value makes it own a
    /// multi-thread Tokio runtime. Native counts are limited to 1..=4096.
    pub worker_threads: Option<usize>,

    /// Number of worker threads in the owned Tokio I/O runtime.
    ///
    /// None keeps [`ActorRuntime::spawn_io`] on the actor runtime (or the
    /// caller's runtime when the actor runtime is borrowed). Tokio's readiness
    /// driver and I/O futures then have an independent owned runtime whenever a
    /// value is supplied. Native Thread and Actix engines reject this field
    /// because they do not expose an equivalent I/O runtime boundary.
    pub io_threads: Option<usize>,

    /// Configuration for the task scheduler.
    pub scheduler_config: SchedulerConfig,
}

/// Configuration for the task scheduler.
///
/// Defines how tasks are queued, executed, and distributed
/// across worker threads.
#[derive(Debug, Clone, Default)]
pub struct SchedulerConfig {
    /// Implementation-specific task admission budget. Zero preserves a native
    /// engine's default; TokioActorRuntime instead uses unbounded admission.
    /// Thread maps positive values to resident ready entries (not mailbox size
    /// or actor count), coalescing readiness when full. Actix rejects positive
    /// values because its scheduler does not expose an equivalent ready queue.
    /// TokioActorRuntime bounds active tasks with a semaphore per execution
    /// lane (actor and explicitly configured I/O); waiting callers remain
    /// pending and are not a bounded resident queue. Separate lane permits let
    /// an actor await its I/O submission without consuming all I/O capacity.
    pub task_queue_capacity: usize,

    /// Maximum time allowed for task execution.
    ///
    /// Tasks exceeding this timeout will be cancelled and
    /// may trigger supervision.
    pub task_timeout: Duration,

    /// Requested worker distribution policy. Native Actix places actor executions
    /// on its owned arbiters with all three policies. LeastLoaded counts reserved
    /// executions, including idle actors through future disposal, not message
    /// backlog or CPU. Tokio uses an owned single-thread worker pool for
    /// nondefault policies; the borrowed caller executor remains RoundRobin-only.
    pub load_balancing: LoadBalancingStrategy,
}

/// Strategies for distributing tasks across worker threads.
///
/// Different strategies optimize for different workload
/// patterns and performance characteristics.
#[derive(Debug, Clone, Default)]
pub enum LoadBalancingStrategy {
    /// Distribute tasks evenly in circular order.
    ///
    /// Best for uniform workloads with similar task costs.
    #[default]
    RoundRobin,

    /// Distribute tasks randomly across workers.
    ///
    /// Good for varying workloads to prevent hotspots.
    Random,

    /// Assign tasks to workers with least pending work.
    ///
    /// Best for non-uniform workloads with varying task costs.
    LeastLoaded,
}

/// Interface for managing the actor system runtime.
///
/// This trait provides control over:
/// - Runtime lifecycle
/// - Task execution
/// - Performance monitoring
#[async_trait]
pub trait ActorRuntime: Send + Sync + 'static {
    /// Initializes and starts the runtime with given configuration.
    ///
    /// # Parameters
    /// * `config` - Runtime configuration
    ///
    /// # Returns
    /// * `Ok(Self)` - Successfully initialized runtime
    /// * `Err(ActorError)` - Initialization failed
    async fn start(config: RuntimeConfig) -> Result<Self, ActorError>
    where
        Self: Sized;

    /// Performs graceful shutdown of the runtime.
    ///
    /// This process:
    /// 1. Stops accepting new tasks
    /// 2. Resolves pending submissions and waits for accepted task cleanup
    /// 3. Releases system resources
    ///
    /// # Returns
    /// Result indicating success or failure of shutdown
    async fn shutdown(self) -> Result<(), ActorError>;

    /// Submits a task for execution on the runtime.
    ///
    /// # Type Parameters
    /// * `F` - Future type to execute
    /// * `T` - Result type of the future
    ///
    /// # Parameters
    /// * `future` - The task to execute
    ///
    /// # Returns
    /// Result containing the task's output or error
    async fn spawn<F, T>(&self, future: F) -> Result<T, ActorError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static;

    /// Submits an I/O future to the runtime's I/O execution boundary.
    ///
    /// Implementations without a separate I/O runtime may use the normal
    /// execution path. TokioActorRuntime routes this method to the owned I/O
    /// runtime when `RuntimeConfig::io_threads` is set, so readiness state and
    /// I/O work do not share actor worker threads.
    async fn spawn_io<F, T>(&self, future: F) -> Result<T, ActorError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn(future).await
    }

    /// Retrieves current runtime performance metrics.
    ///
    /// Use this method to monitor:
    /// - System load
    /// - Resource usage
    /// - Task throughput
    fn metrics(&self) -> RuntimeMetrics;
}

/// Performance metrics for the runtime system.
///
/// These metrics provide insight into the system's current
/// operational status and resource utilization.
#[derive(Debug, Clone)]
pub struct RuntimeMetrics {
    /// Number of actors currently executing.
    pub active_actors: usize,

    /// Number of messages waiting to be processed.
    pub pending_messages: usize,

    /// Percentage of CPU utilization (0.0 - 100.0).
    ///
    /// This portable runtime implementation reports `0.0` when the platform
    /// sampler is unavailable. Consumers that need to distinguish a measured
    /// idle process from an unavailable sample must use the remote measured
    /// metrics API, which carries explicit availability bits.
    pub cpu_usage: f64,

    /// Bytes of memory currently in use.
    ///
    /// A value of zero is an unavailable sample at this API layer; it is not a
    /// claim that the process consumes no memory.
    pub memory_usage: usize,
}

/// Tokio-backed implementation of [`ActorRuntime`].
///
/// The default RoundRobin implementation runs on the caller's Tokio runtime.
/// Explicit counts and nondefault placement create owned worker runtimes. It
/// does not claim to measure process CPU/memory; those fields remain zero
/// because portable, dependency-free measurement is not available at this
/// layer. `task_queue_capacity == 0` means unbounded,
/// while a positive value bounds tasks holding an execution permit. Callers
/// waiting for permits remain separately counted pending inputs, with no
/// resident queue bound at this layer. A task timeout of zero disables the timeout.
///
/// Admission starts when `spawn` is polled. Shutdown rejects queued work that
/// has not started, and waits for accepted futures to be disposed, including
/// cancelled submissions. Cancelling a shutdown waiter keeps admission closed;
/// another clone can join the remaining cleanup. User code runs outside the
/// accounting lock. Synchronous polling and disposal remain cooperative.
/// Poll panics become `ActorError::Panic` with at most 1024 diagnostic characters
/// plus truncation/cleanup annotations. Their payload is disposed before the
/// task is retired; a payload destructor panic is retained in that receipt.
///
/// With default RoundRobin settings this adapter borrows the caller's executor.
/// Supplying either thread count creates an owned actor runtime. Supplying
/// `io_threads` additionally creates an independent owned I/O runtime used by
/// [`ActorRuntime::spawn_io`]. A nondefault load-balancing strategy creates an
/// owned pool of single-worker runtimes so each actor task has an actual
/// worker-thread placement; omitted `worker_threads` uses two placement
/// workers. Queue capacities exceeding [`Semaphore::MAX_PERMITS`] and
/// unrepresentable task deadlines are rejected.
#[derive(Clone)]
pub struct TokioActorRuntime {
    inner: Arc<TokioRuntimeInner>,
}

struct TokioRuntimeInner {
    permits: Option<Arc<Semaphore>>,
    io_permits: Option<Arc<Semaphore>>,
    timeout: Duration,
    owned_runtime: Option<Arc<OwnedRuntime>>,
    io_runtime: Option<Arc<OwnedRuntime>>,
    strategy: LoadBalancingStrategy,
    state: Mutex<TaskState>,
    changed: Notify,
}

/// Tokio forbids dropping a multi-thread runtime from an async context because
/// its destructor joins worker threads. Keep that blocking join off the host
/// executor while retaining normal ownership and shutdown semantics.
struct OwnedRuntime {
    runtimes: Option<Vec<tokio::runtime::Runtime>>,
    handles: Vec<tokio::runtime::Handle>,
}

impl OwnedRuntime {
    fn new(
        worker_threads: Option<usize>,
        io_threads: Option<usize>,
        strategy: &LoadBalancingStrategy,
    ) -> Result<Self, String> {
        const DEFAULT_PLACEMENT_WORKERS: usize = 2;
        let runtimes = if matches!(strategy, LoadBalancingStrategy::RoundRobin) {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.enable_all();
            if let Some(worker_threads) = worker_threads {
                builder.worker_threads(worker_threads);
            }
            if let Some(io_threads) = io_threads {
                builder.max_blocking_threads(io_threads);
            }
            vec![builder
                .build()
                .map_err(|error| format!("failed to build owned Tokio runtime: {error}"))?]
        } else {
            let placement_workers = worker_threads.unwrap_or(DEFAULT_PLACEMENT_WORKERS);
            let mut runtimes = Vec::with_capacity(placement_workers);
            for _ in 0..placement_workers {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.worker_threads(1).enable_all();
                if let Some(io_threads) = io_threads {
                    builder.max_blocking_threads(io_threads);
                }
                match builder.build() {
                    Ok(runtime) => runtimes.push(runtime),
                    Err(error) => {
                        retire_runtimes(runtimes);
                        return Err(format!("failed to build owned Tokio runtime: {error}"));
                    }
                }
            }
            runtimes
        };
        let handles = runtimes
            .iter()
            .map(tokio::runtime::Runtime::handle)
            .cloned()
            .collect();
        Ok(Self {
            runtimes: Some(runtimes),
            handles,
        })
    }

    fn new_io(io_threads: usize) -> Result<Self, String> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder
            .worker_threads(io_threads)
            .max_blocking_threads(io_threads)
            .enable_all();
        let runtime = builder
            .build()
            .map_err(|error| format!("failed to build owned Tokio I/O runtime: {error}"))?;
        let handle = runtime.handle().clone();
        Ok(Self {
            runtimes: Some(vec![runtime]),
            handles: vec![handle],
        })
    }

    fn worker_count(&self) -> usize {
        self.handles.len()
    }

    fn spawn_on<F, T>(&self, worker: usize, future: F) -> tokio::task::JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.handles[worker % self.handles.len()].spawn(future)
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(runtimes) = self.runtimes.take() {
            retire_runtimes(runtimes);
        }
    }
}

fn retire_runtimes(runtimes: Vec<tokio::runtime::Runtime>) {
    let holder = Arc::new(Mutex::new(Some(runtimes)));
    let worker_holder = holder.clone();
    let spawn = std::thread::Builder::new()
        .name("parrot-tokio-runtime-drop".into())
        .spawn(move || {
            if let Some(runtimes) = worker_holder.lock().unwrap().take() {
                drop(runtimes);
            }
        });
    if spawn.is_err() {
        if let Some(runtimes) = holder.lock().unwrap().take() {
            // A destructor cannot report a thread-creation failure. Leaking
            // the runtimes is preferable to joining them on an async executor
            // thread.
            std::mem::forget(runtimes);
        }
    }
}

#[derive(Default)]
struct TaskState {
    shutting_down: bool,
    active: usize,
    pending: usize,
    next_worker: usize,
    workers: Vec<WorkerLoad>,
}

#[derive(Default)]
struct WorkerLoad {
    active: usize,
    pending: usize,
}

impl TaskState {
    fn choose_worker(&mut self, strategy: &LoadBalancingStrategy, worker_count: usize) -> usize {
        if worker_count <= 1 {
            return 0;
        }
        match strategy {
            LoadBalancingStrategy::RoundRobin => {
                let worker = self.next_worker % worker_count;
                self.next_worker = self.next_worker.wrapping_add(1);
                worker
            }
            LoadBalancingStrategy::Random => fastrand::usize(..worker_count),
            LoadBalancingStrategy::LeastLoaded => self
                .workers
                .iter()
                .enumerate()
                .min_by_key(|(worker, load)| (load.active + load.pending, *worker))
                .map(|(worker, _)| worker)
                .unwrap_or(0),
        }
    }
}

// Field order keeps the accepted input alive in accounting until its actual
// destructor returns, including cancellation while waiting for a permit.
struct PendingTask<F> {
    future: F,
    registration: PendingGuard,
}

struct PendingGuard {
    inner: Arc<TokioRuntimeInner>,
    worker: Option<usize>,
    armed: bool,
}

impl PendingGuard {
    fn activate(&mut self) -> Result<ActiveGuard, ActorError> {
        let inner = self.inner.clone();
        let mut state = inner.state.lock().unwrap();
        if state.shutting_down {
            return Err(ActorError::Stopped);
        }
        state.pending -= 1;
        state.active += 1;
        if let Some(worker) = self.worker {
            state.workers[worker].pending -= 1;
            state.workers[worker].active += 1;
        }
        self.armed = false;
        drop(state);
        Ok(ActiveGuard {
            inner,
            worker: self.worker,
        })
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self.inner.state.lock().unwrap();
            state.pending -= 1;
            if let Some(worker) = self.worker {
                state.workers[worker].pending -= 1;
            }
            self.inner.changed.notify_waiters();
        }
    }
}

struct ActiveGuard {
    inner: Arc<TokioRuntimeInner>,
    worker: Option<usize>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.active -= 1;
        if let Some(worker) = self.worker {
            state.workers[worker].active -= 1;
        }
        self.inner.changed.notify_waiters();
    }
}

/// A detached Tokio task must be aborted when the caller drops the public
/// `spawn` future. Plain `JoinHandle` drop detaches the task, which would leak
/// active accounting and let user work outlive a cancelled submission.
struct AbortOnDrop<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // JoinHandle is safe to poll through a pinned mutable reference; its
        // own future implementation does not move the handle's allocation.
        Pin::new(&mut self.handle).poll(cx)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl std::fmt::Debug for TokioActorRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioActorRuntime")
            .field("metrics", &self.metrics())
            .finish_non_exhaustive()
    }
}

impl TokioActorRuntime {
    async fn wait_for_idle(inner: &TokioRuntimeInner) {
        loop {
            let notified = inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let idle = {
                let state = inner.state.lock().unwrap();
                state.active == 0 && state.pending == 0
            };
            if idle {
                return;
            }
            notified.await;
        }
    }
}

async fn run_task<F, T>(
    inner: Arc<TokioRuntimeInner>,
    mut task: PendingTask<F>,
) -> Result<T, ActorError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let permits = if task.registration.worker.is_some() {
        &inner.permits
    } else {
        &inner.io_permits
    };
    let permit = match permits {
        Some(permits) => match permits.clone().acquire_owned().await {
            Ok(permit) => Some(permit),
            Err(_) => return Err(ActorError::Stopped),
        },
        None => None,
    };
    let deadline = if inner.timeout.is_zero() {
        None
    } else {
        Some(
            tokio::time::Instant::now()
                .checked_add(inner.timeout)
                .ok_or_else(|| {
                    ActorError::InitializationError(
                        "runtime task_timeout cannot form a monotonic deadline".into(),
                    )
                })?,
        )
    };
    let active_guard = task.registration.activate()?;
    let result = std::panic::AssertUnwindSafe(task.future).catch_unwind();
    let result = match deadline {
        None => Ok(result.await),
        Some(deadline) => tokio::time::timeout_at(deadline, result)
            .await
            .map_err(|_| ActorError::Timeout),
    };
    let result = match result {
        Err(error) => Err(error),
        Ok(Ok(value)) => Ok(value),
        Ok(Err(payload)) => Err(crate::owned_input::panic_error(payload)),
    };
    drop(active_guard);
    drop(permit);
    result
}

#[async_trait]
impl ActorRuntime for TokioActorRuntime {
    async fn start(config: RuntimeConfig) -> Result<Self, ActorError> {
        const MAX_RUNTIME_THREADS: usize = 4096;
        for (name, count) in [
            ("worker_threads", config.worker_threads),
            ("io_threads", config.io_threads),
        ] {
            if count.is_some_and(|count| count == 0 || count > MAX_RUNTIME_THREADS) {
                return Err(ActorError::InitializationError(format!(
                    "runtime {name} must be in 1..={MAX_RUNTIME_THREADS}"
                )));
            }
        }
        let strategy = config.scheduler_config.load_balancing.clone();
        let capacity = config.scheduler_config.task_queue_capacity;
        if capacity > Semaphore::MAX_PERMITS {
            return Err(ActorError::InitializationError(format!(
                "runtime task_queue_capacity exceeds {}",
                Semaphore::MAX_PERMITS
            )));
        }
        if std::time::Instant::now()
            .checked_add(config.scheduler_config.task_timeout)
            .is_none()
        {
            return Err(ActorError::InitializationError(
                "runtime task_timeout cannot form a monotonic deadline".into(),
            ));
        }
        let needs_owned_runtime = config.worker_threads.is_some()
            || config.io_threads.is_some()
            || !matches!(strategy, LoadBalancingStrategy::RoundRobin);
        let owned_runtime = if needs_owned_runtime {
            Some(Arc::new(
                OwnedRuntime::new(config.worker_threads, config.io_threads, &strategy)
                    .map_err(ActorError::InitializationError)?,
            ))
        } else {
            None
        };
        let io_runtime = config
            .io_threads
            .map(|io_threads| {
                OwnedRuntime::new_io(io_threads)
                    .map(Arc::new)
                    .map_err(ActorError::InitializationError)
            })
            .transpose()?;
        let permits = (capacity > 0).then(|| Arc::new(Semaphore::new(capacity)));
        let io_permits =
            (capacity > 0 && io_runtime.is_some()).then(|| Arc::new(Semaphore::new(capacity)));
        Ok(Self {
            inner: Arc::new(TokioRuntimeInner {
                permits,
                io_permits,
                timeout: config.scheduler_config.task_timeout,
                owned_runtime,
                io_runtime,
                strategy,
                state: Mutex::new(TaskState::default()),
                changed: Notify::new(),
            }),
        })
    }

    async fn shutdown(self) -> Result<(), ActorError> {
        let inner = self.inner.clone();
        inner.state.lock().unwrap().shutting_down = true;
        inner.changed.notify_waiters();
        Self::wait_for_idle(&inner).await;
        Ok(())
    }

    async fn spawn<F, T>(&self, future: F) -> Result<T, ActorError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let task = {
            let mut state = self.inner.state.lock().unwrap();
            if state.shutting_down {
                return Err(ActorError::Stopped);
            }
            let worker_count = self
                .inner
                .owned_runtime
                .as_ref()
                .map_or(1, |runtime| runtime.worker_count());
            if state.workers.len() != worker_count {
                state.workers.resize_with(worker_count, WorkerLoad::default);
            }
            let worker = state.choose_worker(&self.inner.strategy, worker_count);
            state.pending += 1;
            state.workers[worker].pending += 1;
            PendingTask {
                future,
                registration: PendingGuard {
                    inner: self.inner.clone(),
                    worker: Some(worker),
                    armed: true,
                },
            }
        };
        if let Some(runtime) = self.inner.owned_runtime.clone() {
            let worker = task.registration.worker.unwrap_or(0);
            let handle = runtime.spawn_on(worker, run_task(self.inner.clone(), task));
            let mut handle = AbortOnDrop { handle };
            match (&mut handle).await {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => Err(ActorError::Stopped),
                Err(error) => Err(ActorError::Panic(format!(
                    "owned Tokio runtime task panicked: {error}"
                ))),
            }
        } else {
            run_task(self.inner.clone(), task).await
        }
    }

    async fn spawn_io<F, T>(&self, future: F) -> Result<T, ActorError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let Some(runtime) = self.inner.io_runtime.clone() else {
            return self.spawn(future).await;
        };
        let task = {
            let mut state = self.inner.state.lock().unwrap();
            if state.shutting_down {
                return Err(ActorError::Stopped);
            }
            state.pending += 1;
            PendingTask {
                future,
                registration: PendingGuard {
                    inner: self.inner.clone(),
                    worker: None,
                    armed: true,
                },
            }
        };
        let handle = runtime.spawn_on(0, run_task(self.inner.clone(), task));
        let mut handle = AbortOnDrop { handle };
        match (&mut handle).await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Err(ActorError::Stopped),
            Err(error) => Err(ActorError::Panic(format!(
                "owned Tokio I/O runtime task panicked: {error}"
            ))),
        }
    }

    fn metrics(&self) -> RuntimeMetrics {
        let state = self.inner.state.lock().unwrap();
        RuntimeMetrics {
            active_actors: state.active,
            pending_messages: state.pending,
            cpu_usage: 0.0,
            memory_usage: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn runtime_config_defaults_are_none() {
        let c = RuntimeConfig::default();
        assert!(c.worker_threads.is_none());
        assert!(c.io_threads.is_none());
        assert_eq!(c.scheduler_config.task_queue_capacity, 0);
        assert_eq!(c.scheduler_config.task_timeout, Duration::ZERO);
        assert!(matches!(
            c.scheduler_config.load_balancing,
            LoadBalancingStrategy::RoundRobin
        ));
    }

    #[test]
    fn runtime_config_full_construction() {
        let c = RuntimeConfig {
            worker_threads: Some(4),
            io_threads: Some(2),
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 1000,
                task_timeout: Duration::from_secs(30),
                load_balancing: LoadBalancingStrategy::LeastLoaded,
            },
        };
        assert_eq!(c.worker_threads, Some(4));
        assert_eq!(c.scheduler_config.task_queue_capacity, 1000);
        assert!(matches!(
            c.scheduler_config.load_balancing,
            LoadBalancingStrategy::LeastLoaded
        ));
        // Clone 保留
        let c2 = c.clone();
        assert_eq!(c2.io_threads, Some(2));
    }

    #[test]
    fn load_balancing_all_variants_and_default() {
        assert!(matches!(
            LoadBalancingStrategy::default(),
            LoadBalancingStrategy::RoundRobin
        ));
        let v = [
            LoadBalancingStrategy::RoundRobin,
            LoadBalancingStrategy::Random,
            LoadBalancingStrategy::LeastLoaded,
        ];
        // Clone + Debug 可用，互不等（用 matches 区分）
        for x in &v {
            let _ = x.clone();
            let _ = format!("{:?}", x);
        }
    }

    #[test]
    fn scheduler_config_zero_capacity_and_timeout() {
        // 边界：0 容量 / 0 超时仍可构造（实现层负责语义）
        let s = SchedulerConfig {
            task_queue_capacity: 0,
            task_timeout: Duration::ZERO,
            load_balancing: LoadBalancingStrategy::Random,
        };
        assert_eq!(s.task_queue_capacity, 0);
    }

    // ---------------- ActorRuntime trait 对象可用性 ----------------

    struct NopRuntime;

    #[async_trait]
    impl ActorRuntime for NopRuntime {
        async fn start(_config: RuntimeConfig) -> Result<Self, ActorError>
        where
            Self: Sized,
        {
            Ok(NopRuntime)
        }
        async fn shutdown(self) -> Result<(), ActorError> {
            Ok(())
        }
        async fn spawn<F, T>(&self, _future: F) -> Result<T, ActorError>
        where
            F: std::future::Future<Output = T> + Send + 'static,
            T: Send + 'static,
        {
            Err(ActorError::InternalError("nop".into()))
        }
        fn metrics(&self) -> RuntimeMetrics {
            RuntimeMetrics {
                active_actors: 0,
                pending_messages: 0,
                cpu_usage: 0.0,
                memory_usage: 0,
            }
        }
    }

    #[tokio::test]
    async fn runtime_trait_lifecycle_contract() {
        let rt = NopRuntime::start(RuntimeConfig::default()).await.unwrap();
        // spawn 透传错误
        let r = rt.spawn(async { 1u32 }).await;
        assert!(r.is_err());
        // metrics 返回
        assert_eq!(rt.metrics().active_actors, 0);
        // shutdown 成功
        rt.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tokio_runtime_enforces_capacity_timeout_and_panic_boundaries() {
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 1,
                task_timeout: Duration::from_millis(20),
                ..SchedulerConfig::default()
            },
            ..RuntimeConfig::default()
        })
        .await
        .unwrap();
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let mut first = Box::pin(runtime.spawn(async move {
            entered.send(()).unwrap();
            let _ = release_rx.await;
            7_u32
        }));
        assert!(futures::poll!(&mut first).is_pending());
        entered_rx.await.unwrap();
        // The first task is occupying the only slot, so the second remains
        // pending until the first is released.
        let (_ready, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let mut second = Box::pin(runtime.spawn(async move {
            let _ = ready_rx.await;
            8_u32
        }));
        assert!(futures::poll!(&mut second).is_pending());
        // Release the first task. The task returns its value and the permit
        // becomes free.
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap(), 7);
        drop(second);

        let timeout = runtime.spawn(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            1_u8
        });
        assert!(matches!(timeout.await, Err(ActorError::Timeout)));
        let panic = runtime.spawn(async { panic!("runtime panic") });
        assert!(matches!(
            panic.await,
            Err(ActorError::Panic(message)) if message == "runtime panic"
        ));
        runtime.clone().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tokio_runtime_shutdown_waits_for_active_tasks() {
        let runtime = TokioActorRuntime::start(RuntimeConfig::default())
            .await
            .unwrap();
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut task = Box::pin(runtime.spawn(async move {
            entered.send(()).unwrap();
            let _ = release_rx.await;
        }));
        assert!(futures::poll!(&mut task).is_pending());
        entered_rx.await.unwrap();
        assert_eq!(runtime.metrics().active_actors, 1);
        let shutdown = runtime.clone().shutdown();
        tokio::pin!(shutdown);
        assert!(futures::poll!(&mut shutdown).is_pending());
        release_tx.send(()).unwrap();
        task.await.unwrap();
        shutdown.await.unwrap();
        assert_eq!(runtime.metrics().active_actors, 0);
    }

    #[tokio::test]
    async fn tokio_runtime_cancelling_active_task_releases_metrics_and_shutdown() {
        let runtime = TokioActorRuntime::start(RuntimeConfig::default())
            .await
            .unwrap();
        let mut task = Box::pin(runtime.spawn(async {
            futures::future::pending::<()>().await;
        }));
        assert!(futures::poll!(&mut task).is_pending());
        assert_eq!(runtime.metrics().active_actors, 1);
        drop(task);
        tokio::task::yield_now().await;
        assert_eq!(runtime.metrics().active_actors, 0);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tokio_runtime_rejects_zero_thread_counts() {
        for config in [
            RuntimeConfig {
                worker_threads: Some(0),
                ..RuntimeConfig::default()
            },
            RuntimeConfig {
                io_threads: Some(0),
                ..RuntimeConfig::default()
            },
        ] {
            assert!(matches!(
                TokioActorRuntime::start(config).await,
                Err(ActorError::InitializationError(_))
            ));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_runtime_explicit_counts_use_an_owned_executor() {
        let host = std::thread::current().id();
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(1),
            io_threads: Some(2),
            ..RuntimeConfig::default()
        })
        .await
        .unwrap();
        let worker = runtime
            .spawn(async { std::thread::current().id() })
            .await
            .unwrap();
        assert_ne!(
            worker, host,
            "explicit worker_threads still used the host runtime"
        );
        assert!(runtime.inner.owned_runtime.is_some());
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_runtime_io_tasks_use_an_independent_owned_executor() {
        let host = std::thread::current().id();
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(1),
            io_threads: Some(1),
            ..RuntimeConfig::default()
        })
        .await
        .unwrap();
        let actor_worker = runtime
            .spawn(async { std::thread::current().id() })
            .await
            .unwrap();
        let io_worker = runtime
            .spawn_io(async { std::thread::current().id() })
            .await
            .unwrap();
        assert_ne!(actor_worker, host, "actor task ran on the host executor");
        assert_ne!(io_worker, host, "I/O task ran on the host executor");
        assert_ne!(
            actor_worker, io_worker,
            "I/O task shared the actor runtime worker"
        );
        assert_eq!(runtime.inner.io_runtime.as_ref().unwrap().worker_count(), 1);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_runtime_least_loaded_uses_distinct_owned_worker_threads() {
        let host = std::thread::current().id();
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(2),
            scheduler_config: SchedulerConfig {
                load_balancing: LoadBalancingStrategy::LeastLoaded,
                ..Default::default()
            },
            ..RuntimeConfig::default()
        })
        .await
        .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first_runtime = runtime.clone();
        let first = {
            let barrier = barrier.clone();
            first_runtime.spawn(async move {
                let id = std::thread::current().id();
                barrier.wait().await;
                id
            })
        };
        let second_runtime = runtime.clone();
        let second = {
            let barrier = barrier.clone();
            second_runtime.spawn(async move {
                let id = std::thread::current().id();
                barrier.wait().await;
                id
            })
        };
        let (first, second) = tokio::join!(first, second);
        let ids = HashSet::from([first.unwrap(), second.unwrap()]);
        assert_eq!(ids.len(), 2, "LeastLoaded did not spread concurrent work");
        assert!(!ids.contains(&host), "owned task ran on the host executor");
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_runtime_random_uses_owned_worker_pool() {
        let host = std::thread::current().id();
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(2),
            scheduler_config: SchedulerConfig {
                load_balancing: LoadBalancingStrategy::Random,
                ..Default::default()
            },
            ..RuntimeConfig::default()
        })
        .await
        .unwrap();
        assert_eq!(
            runtime.inner.owned_runtime.as_ref().unwrap().worker_count(),
            2
        );
        let worker = runtime
            .spawn(async { std::thread::current().id() })
            .await
            .unwrap();
        assert_ne!(worker, host, "random placement ran on the host executor");
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_runtime_cancelling_owned_task_aborts_user_work_and_retires_it() {
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(1),
            io_threads: Some(1),
            ..RuntimeConfig::default()
        })
        .await
        .unwrap();
        let mut task = Box::pin(runtime.spawn(async {
            std::future::pending::<()>().await;
        }));
        assert!(futures::poll!(&mut task).is_pending());
        drop(task);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.metrics().active_actors == 0 && runtime.metrics().pending_messages == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned task abort did not retire accounting");
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_runtime_cancelling_io_task_retires_accounting() {
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            io_threads: Some(1),
            ..RuntimeConfig::default()
        })
        .await
        .unwrap();
        let mut task = Box::pin(runtime.spawn_io(async {
            std::future::pending::<()>().await;
        }));
        assert!(futures::poll!(&mut task).is_pending());
        drop(task);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.metrics().active_actors == 0 && runtime.metrics().pending_messages == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned I/O task abort did not retire accounting");
        runtime.shutdown().await.unwrap();
    }

    #[test]
    fn runtime_metrics_fields_constructible() {
        let m = RuntimeMetrics {
            active_actors: 3,
            pending_messages: 42,
            cpu_usage: 55.5,
            memory_usage: 1024,
        };
        let c = m.clone();
        assert_eq!(c.active_actors, 3);
        assert!((c.cpu_usage - 55.5).abs() < f64::EPSILON);
        assert!(format!("{:?}", c).contains("RuntimeMetrics"));
    }
}
