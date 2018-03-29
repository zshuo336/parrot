use std::time::Duration;
use async_trait::async_trait;
use crate::errors::ActorError;

/// Runtime configuration
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Number of worker threads
    pub worker_threads: Option<usize>,
    /// Number of IO threads
    pub io_threads: Option<usize>,
    /// Scheduler configuration
    pub scheduler_config: SchedulerConfig,
}

/// Scheduler configuration
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Task queue capacity
    pub task_queue_capacity: usize,
    /// Task execution timeout
    pub task_timeout: Duration,
    /// Load balancing strategy
    pub load_balancing: LoadBalancingStrategy,
}

/// Load balancing strategy
#[derive(Debug, Clone)]
pub enum LoadBalancingStrategy {
    /// Round robin
    RoundRobin,
    /// Random
    Random,
    /// Least loaded
    LeastLoaded,
}

/// Actor runtime trait
#[async_trait]
pub trait ActorRuntime: Send + Sync + 'static {
    /// Start the runtime
    async fn start(config: RuntimeConfig) -> Result<Self, ActorError> where Self: Sized;
    
    /// Shutdown the runtime
    async fn shutdown(self) -> Result<(), ActorError>;
    
    /// Submit task to the runtime
    async fn spawn<F, T>(&self, future: F) -> Result<T, ActorError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static;
        
    /// Get runtime metrics
    fn metrics(&self) -> RuntimeMetrics;
}

/// Runtime metrics
#[derive(Debug, Clone)]
pub struct RuntimeMetrics {
    /// Number of active actors
    pub active_actors: usize,
    /// Number of pending messages
    pub pending_messages: usize,
    /// CPU usage
    pub cpu_usage: f64,
    /// Memory usage
    pub memory_usage: usize,
} 