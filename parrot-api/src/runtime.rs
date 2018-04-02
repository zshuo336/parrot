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
//!
//! let runtime = ActorRuntime::start(config).await?;
//! ```

use std::time::Duration;
use async_trait::async_trait;
use crate::errors::ActorError;

/// Configuration for the actor system runtime.
///
/// This structure defines the resource allocation and execution
/// parameters for the entire actor system.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub enum LoadBalancingStrategy {
    /// Distribute tasks evenly in circular order.
    ///
    /// Best for uniform workloads with similar task costs.
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
    async fn start(config: RuntimeConfig) -> Result<Self, ActorError> where Self: Sized;
    
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