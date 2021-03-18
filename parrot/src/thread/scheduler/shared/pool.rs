use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock, Weak, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use anyhow::anyhow;

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::SchedulingQueue;
use crate::thread::config::ThreadActorConfig;
use crate::thread::error::SystemError;
use crate::thread::scheduler::ThreadScheduler;
use super::worker::{Worker, WorkerConfig, WorkerStatus};
use super::worker_manager::WorkerManager;

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

/// Metrics about the scheduler state
#[derive(Debug, Clone)]
pub struct SchedulerMetrics {
    /// Number of worker threads in the pool
    pub pool_size: usize,
    
    /// Current length of the scheduling queue
    pub queue_length: usize,
    
    /// Whether the scheduler is shutting down
    pub is_shutting_down: bool,
    
    /// Current status of the scheduler
    pub status: SchedulerStatus,
    
    /// Number of active processors
    pub active_processors: usize,
}

/// Scheduler implementation for managing a shared thread pool
/// 
/// SharedThreadPool maintains a group of worker threads that pull ready mailboxes from a central SchedulingQueue.
/// This approach is highly efficient when there are many actors but not all are active simultaneously.
/// 
/// # Thread Safety
/// - Uses Arc and weak references to manage shared state
/// - Uses atomic flags to control shutdown behavior
/// - Uses SchedulingQueue for inter-thread communication
/// 
/// # Performance Characteristics
/// - Supports work stealing pattern for load balancing
/// - Avoids thundering herd on thread wakeups
/// - Batch processes messages for efficiency
/// 
/// # Worker Thread Behavior
/// 1. Gets ready mailbox from SchedulingQueue
/// 2. Processes a batch of messages (limited by max_messages_per_batch)
/// 3. Re-queues mailbox if it still has messages
/// 4. Captures panics and notifies supervisor
#[derive(Debug)]
pub struct SharedThreadPool {
    /// Size of thread pool
    pool_size: usize,
    
    /// Collection of worker thread JoinHandles
    workers: Vec<JoinHandle<()>>,
    
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
    
    /// Worker configurations
    worker_configs: Vec<Arc<AtomicUsize>>,
}

impl SharedThreadPool {
    /// Create new SharedThreadPool
    /// 
    /// # Arguments
    /// * `config` - Optional configuration for the thread pool
    /// * `runtime_handle` - Tokio runtime handle
    /// * `system` - Optional system reference
    pub fn new(
        config: Option<SharedThreadPoolConfig>,
        runtime_handle: Handle,
    ) -> Self {
        let config = config.unwrap_or_default();
        let scheduling_queue = Arc::new(SchedulingQueue::new(config.max_queue_capacity));
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        let status = Arc::new(AtomicUsize::new(SchedulerStatus::Initializing as usize));
        
        // Create worker manager
        let worker_manager = Arc::new(WorkerManager::new(
            runtime_handle.clone(),
            scheduling_queue.clone(),
            config.max_messages_per_batch,
        ));
        
        let mut pool = Self {
            pool_size: config.pool_size,
            workers: Vec::with_capacity(config.pool_size),
            scheduling_queue,
            runtime_handle: runtime_handle.clone(),
            is_shutting_down,
            config,
            worker_manager,
            status,
            worker_configs: Vec::with_capacity(config.pool_size),
        };
        
        // Start worker threads
        pool.start_workers();
        
        // Mark as running
        pool.status.store(SchedulerStatus::Running as usize, Ordering::SeqCst);
        
        pool
    }
    
    /// Start worker threads
    fn start_workers(&mut self) {
        for worker_id in 0..self.pool_size {
            // Create worker configuration
            let worker_config = WorkerConfig {
                batch_size: self.config.max_messages_per_batch,
                idle_sleep_duration: self.config.idle_sleep_duration,
                yield_after_each_message: self.config.yield_after_each_message,
                enable_detailed_logging: self.config.enable_detailed_logging,
            };
            
            // Create and spawn the worker
            let worker = Worker::new(
                worker_id,
                self.runtime_handle.clone(),
                self.scheduling_queue.clone(),
                self.is_shutting_down.clone(),
                self.system.clone(),
                Some(worker_config.clone()),
            );
            
            // Track worker status
            self.worker_configs.push(worker.status().clone());
            
            let handle = worker.spawn();
            self.workers.push(handle);
        }
    }
    
    /// Schedule an actor on the thread pool
    /// 
    /// # Arguments
    /// * `path` - Actor path
    /// * `mailbox` - Actor mailbox
    /// * `config` - Actor configuration
    pub async fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), SystemError> {
        // Check if already shutting down
        if self.is_shutting_down.load(Ordering::Relaxed) {
            return Err(SystemError::ShuttingDown);
        }
        
        // Schedule mailbox with the worker manager
        self.worker_manager.schedule_mailbox(
            path.to_string(),
            mailbox.clone(),
            config,
        ).await?;
        
        Ok(())
    }
    
    /// Deschedule an actor from the thread pool
    /// 
    /// # Arguments
    /// * `path` - Actor path to deschedule
    pub async fn deschedule(&self, path: &str) -> Result<(), SystemError> {
        self.worker_manager.stop_processor(path).await
    }
    
    /// Check if an actor is scheduled
    /// 
    /// # Arguments
    /// * `path` - Actor path to check
    pub fn is_scheduled(&self, path: &str) -> bool {
        self.worker_manager.is_scheduled(path)
    }
    
    /// Shutdown the thread pool
    /// 
    /// # Arguments
    /// * `timeout_ms` - Timeout in milliseconds to wait for graceful shutdown
    pub async fn shutdown(&self, timeout_ms: u64) -> Result<(), SystemError> {
        // Set the shutting down flag
        self.status.store(SchedulerStatus::ShuttingDown as usize, Ordering::SeqCst);
        self.is_shutting_down.store(true, Ordering::SeqCst);
        
        // Notify all workers that may be waiting on the queue
        for _ in 0..self.pool_size {
            self.scheduling_queue.notify_handle().notify_one();
        }
        
        // TODO: Implement timeout waiting for worker threads to complete
        
        // Mark as fully shutdown
        self.status.store(SchedulerStatus::Shutdown as usize, Ordering::SeqCst);
        
        Ok(())
    }
    
    /// Get the pool size
    pub fn pool_size(&self) -> usize {
        self.pool_size
    }
    
    /// Get the current scheduler status
    pub fn status(&self) -> SchedulerStatus {
        match self.status.load(Ordering::Relaxed) {
            0 => SchedulerStatus::Initializing,
            1 => SchedulerStatus::Running,
            2 => SchedulerStatus::ShuttingDown,
            3 => SchedulerStatus::Shutdown,
            4 => SchedulerStatus::Error,
            _ => SchedulerStatus::Error, // Default to error for unknown status
        }
    }
    
    /// Get metrics about the scheduler
    pub fn metrics(&self) -> SchedulerMetrics {
        SchedulerMetrics {
            pool_size: self.pool_size,
            queue_length: self.scheduling_queue.len(),
            is_shutting_down: self.is_shutting_down.load(Ordering::Relaxed),
            status: self.status(),
            active_processors: self.worker_manager.processor_count(),
        }
    }
}

impl ThreadScheduler for SharedThreadPool {
    fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.schedule(path, mailbox, config).await
            })
        }).map_err(|e| e)
    }
    
    fn deschedule(
        &self,
        path: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.deschedule(path).await
            })
        }).map_err(|e| e)
    }
    
    fn is_scheduled(&self, path: &str) -> bool {
        self.is_scheduled(path)
    }
    
    fn shutdown(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        tokio::task::block_in_place(|| {
            // Default timeout of 5 seconds
            tokio::runtime::Handle::current().block_on(async {
                self.shutdown(5000).await
            })
        }).map_err(|e| e)
    }
} 
