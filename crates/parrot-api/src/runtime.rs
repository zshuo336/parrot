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
//!     worker_threads: Some(4),
//!     io_threads: Some(2),
//!     scheduler_config: SchedulerConfig {
//!         task_queue_capacity: 1000,
//!         task_timeout: Duration::from_secs(30),
//!         load_balancing: LoadBalancingStrategy::LeastLoaded,
//!     },
//! };
//! // Pass `config` to your runtime implementation of choice, e.g.
//! // `ActorRuntime::start(config).await?` inside an async context.
//! # let _ = config;
//! ```

use crate::errors::ActorError;
use async_trait::async_trait;
use std::time::Duration;

/// Configuration for the actor system runtime.
///
/// This structure defines the resource allocation and execution
/// parameters for the entire actor system.
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// Number of threads for processing actor messages.
    ///
    /// If None, the system will use the number of available CPU cores.
    pub worker_threads: Option<usize>,

    /// Number of threads for handling I/O operations.
    ///
    /// If None, the system will use a default based on workload.
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
    /// Maximum number of tasks that can be queued.
    ///
    /// When this limit is reached, task submission will be
    /// backpressured.
    pub task_queue_capacity: usize,

    /// Maximum time allowed for task execution.
    ///
    /// Tasks exceeding this timeout will be cancelled and
    /// may trigger supervision.
    pub task_timeout: Duration,

    /// Strategy for distributing tasks across workers.
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
    /// 2. Completes pending tasks
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
    pub cpu_usage: f64,

    /// Bytes of memory currently in use.
    pub memory_usage: usize,
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
