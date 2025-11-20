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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

#[cfg(test)]
#[path = "runtime/lifecycle_contracts.rs"]
mod lifecycle_contracts;

/// Configuration for the actor system runtime.
///
/// Each runtime applies the settings it owns. Built-in native adapters reject
/// unsupported explicit settings; they cannot resize a caller-owned executor.
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// Number of threads for processing actor messages.
    ///
    /// Native Thread maps this to core worker tasks, Actix to owned arbiters.
    /// None preserves the native settings. TokioActorRuntime borrows its executor
    /// and rejects explicit counts; native counts are limited to 1..=4096.
    pub worker_threads: Option<usize>,

    /// Number of threads for handling I/O operations.
    ///
    /// None uses the host's I/O driver. Built-in runtimes currently reject an
    /// explicit value because they do not own a separate configurable I/O pool.
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
    /// TokioActorRuntime bounds active tasks with a semaphore; waiting callers
    /// remain pending and are not a bounded resident queue.
    pub task_queue_capacity: usize,

    /// Maximum time allowed for task execution.
    ///
    /// Tasks exceeding this timeout will be cancelled and
    /// may trigger supervision.
    pub task_timeout: Duration,

    /// Requested worker distribution policy. Native Actix places actor executions
    /// on its owned arbiters with all three policies. LeastLoaded counts reserved
    /// executions, including idle actors through future disposal, not message
    /// backlog or CPU. Native Thread and the borrowed Tokio adapter currently
    /// reject nondefault policies.
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
/// The implementation deliberately runs on the caller's Tokio runtime. It
/// does not create a second executor or claim to measure process CPU/memory;
/// those fields remain zero because portable, dependency-free measurement is
/// not available at this layer. `task_queue_capacity == 0` means unbounded,
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
/// This adapter borrows the caller's executor. Explicit `worker_threads`,
/// `io_threads`, and nondefault `load_balancing` are rejected because they
/// cannot reconfigure that executor. Queue capacities exceeding
/// [`Semaphore::MAX_PERMITS`] and unrepresentable task deadlines are rejected.
#[derive(Clone)]
pub struct TokioActorRuntime {
    inner: Arc<TokioRuntimeInner>,
}

struct TokioRuntimeInner {
    permits: Option<Arc<Semaphore>>,
    timeout: Duration,
    state: Mutex<TaskState>,
    changed: Notify,
}

#[derive(Default)]
struct TaskState {
    shutting_down: bool,
    active: usize,
    pending: usize,
}

// Field order keeps the accepted input alive in accounting until its actual
// destructor returns, including cancellation while waiting for a permit.
struct PendingTask<'a, F> {
    future: F,
    registration: PendingGuard<'a>,
}

struct PendingGuard<'a> {
    inner: &'a TokioRuntimeInner,
    armed: bool,
}

impl<'a> PendingGuard<'a> {
    fn activate(&mut self) -> Result<ActiveGuard<'a>, ActorError> {
        let mut state = self.inner.state.lock().unwrap();
        if state.shutting_down {
            return Err(ActorError::Stopped);
        }
        state.pending -= 1;
        state.active += 1;
        self.armed = false;
        Ok(ActiveGuard { inner: self.inner })
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.inner.state.lock().unwrap().pending -= 1;
            self.inner.changed.notify_waiters();
        }
    }
}

struct ActiveGuard<'a> {
    inner: &'a TokioRuntimeInner,
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.inner.state.lock().unwrap().active -= 1;
        self.inner.changed.notify_waiters();
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

#[async_trait]
impl ActorRuntime for TokioActorRuntime {
    async fn start(config: RuntimeConfig) -> Result<Self, ActorError> {
        if config.worker_threads.is_some() {
            return Err(ActorError::InitializationError(
                "runtime worker_threads cannot reconfigure the caller-owned Tokio executor".into(),
            ));
        }
        if config.io_threads.is_some() {
            return Err(ActorError::InitializationError(
                "runtime io_threads cannot reconfigure the caller-owned Tokio I/O driver".into(),
            ));
        }
        if !matches!(
            config.scheduler_config.load_balancing,
            LoadBalancingStrategy::RoundRobin
        ) {
            return Err(ActorError::InitializationError(
                "runtime load_balancing cannot reconfigure the caller-owned Tokio scheduler".into(),
            ));
        }
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
        let permits = (capacity > 0).then(|| Arc::new(Semaphore::new(capacity)));
        Ok(Self {
            inner: Arc::new(TokioRuntimeInner {
                permits,
                timeout: config.scheduler_config.task_timeout,
                state: Mutex::new(TaskState::default()),
                changed: Notify::new(),
            }),
        })
    }

    async fn shutdown(self) -> Result<(), ActorError> {
        self.inner.state.lock().unwrap().shutting_down = true;
        self.inner.changed.notify_waiters();
        Self::wait_for_idle(&self.inner).await;
        Ok(())
    }

    async fn spawn<F, T>(&self, future: F) -> Result<T, ActorError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let mut task = {
            let mut state = self.inner.state.lock().unwrap();
            if state.shutting_down {
                return Err(ActorError::Stopped);
            }
            state.pending += 1;
            PendingTask {
                future,
                registration: PendingGuard {
                    inner: &self.inner,
                    armed: true,
                },
            }
        };
        let permit = match self.inner.permits.as_ref() {
            Some(permits) => match permits.clone().acquire_owned().await {
                Ok(permit) => Some(permit),
                Err(_) => {
                    return Err(ActorError::Stopped);
                }
            },
            None => None,
        };
        let deadline = if self.inner.timeout.is_zero() {
            None
        } else {
            Some(
                tokio::time::Instant::now()
                    .checked_add(self.inner.timeout)
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
