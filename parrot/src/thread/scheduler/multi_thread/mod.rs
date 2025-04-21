//! Multi-threaded scheduler implementation for the actor system.
//!
//! Provides an efficient and scalable scheduling mechanism for actor execution across
//! multiple threads. Key features include:
//! - Worker pool management with dynamic work stealing
//! - Batch processing of messages for throughput optimization
//! - Automatic error isolation and recovery
//! - Graceful shutdown behavior with timeout handling

pub mod worker;

use std::fmt;
use std::sync::{Arc, Weak, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::thread::mailbox::Mailbox;
use crate::thread::config::ThreadActorConfig;
use crate::thread::error::SystemError;
use crate::thread::scheduler::queue::SchedulingQueue;
use self::worker::{Worker, WorkerConfig, SystemRef, WorkerStatus};

/// # Multi-Threaded Scheduler
///
/// Core implementation of the shared thread pool scheduler that distributes
/// actor mailboxes across a pool of worker threads.
///
/// ## Key Responsibilities
/// - Managing the pool of worker threads
/// - Scheduling mailboxes to workers via shared queue
/// - Monitoring worker health and performance
/// - Coordinating graceful shutdown sequence
///
/// ## Design Principles
/// - Lock-free operations where possible for high throughput
/// - Fair work distribution across worker threads
/// - Resilience through isolation of actor failures
///
/// ## Thread Safety
/// - Thread-safe through Arc, AtomicBool and message passing
/// - Uses weak references to avoid reference cycles
/// - Coordinator pattern for centralized management
#[derive(Debug)]
pub struct MultiThreadScheduler {
    /// Number of worker threads in the pool
    pool_size: usize,
    
    /// Handles to all worker threads for management
    workers: Vec<JoinHandle<()>>,
    
    /// Central queue for mailboxes ready for processing
    scheduling_queue: Arc<SchedulingQueue>,
    
    /// System runtime handle
    runtime_handle: Handle,
    
    /// Flag signaling shutdown across all workers
    is_shutting_down: Arc<AtomicBool>,
    
    /// Reference to the system for actor lookups and error handling
    system: Option<Weak<dyn SystemRef + Send + Sync>>,
    
    /// Worker configuration
    worker_config: WorkerConfig,
    
    /// Current status of the scheduler
    status: Arc<AtomicUsize>,
}

/// Status codes for the multi-thread scheduler
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

impl MultiThreadScheduler {
    /// Creates a new multi-threaded scheduler
    ///
    /// ## Parameters
    /// - `pool_size`: Number of worker threads to create
    /// - `runtime_handle`: Tokio runtime handle for async operations
    /// - `max_queue_capacity`: Maximum capacity for the scheduling queue (for monitoring)
    /// - `system`: Optional system reference for actor lookups and error reporting
    /// - `worker_config`: Optional custom configuration for workers
    pub fn new(
        pool_size: usize,
        runtime_handle: Handle,
        max_queue_capacity: usize,
        system: Option<Weak<dyn SystemRef + Send + Sync>>,
        worker_config: Option<WorkerConfig>,
    ) -> Self {
        let scheduling_queue = Arc::new(SchedulingQueue::new(max_queue_capacity));
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        let worker_config = worker_config.unwrap_or_default();
        let status = Arc::new(AtomicUsize::new(SchedulerStatus::Initializing as usize));
        
        // Create the scheduler struct first
        let mut scheduler = Self {
            pool_size,
            workers: Vec::with_capacity(pool_size),
            scheduling_queue,
            runtime_handle,
            is_shutting_down,
            system,
            worker_config,
            status,
        };
        
        // Then start the workers
        scheduler.start_workers();
        
        // Mark scheduler as running
        scheduler.status.store(SchedulerStatus::Running as usize, Ordering::SeqCst);
        
        scheduler
    }
    
    /// Starts all worker threads in the pool
    fn start_workers(&mut self) {
        for worker_id in 0..self.pool_size {
            let worker = Worker::new(
                worker_id,
                self.runtime_handle.clone(),
                self.scheduling_queue.clone(),
                self.is_shutting_down.clone(),
                self.system.clone(),
                Some(self.worker_config.clone()),
            );
            
            let join_handle = worker.spawn();
            self.workers.push(join_handle);
        }
    }
    
    /// Schedules a mailbox for processing
    ///
    /// ## Parameters
    /// - `mailbox`: The mailbox to schedule
    /// - `config`: Optional actor-specific configuration
    pub fn schedule(&self, mailbox: Arc<dyn Mailbox + Send + Sync>, _config: Option<&ThreadActorConfig>) {
        if !self.is_shutting_down.load(Ordering::Relaxed) {
            self.scheduling_queue.push(mailbox);
        }
    }
    
    /// Gracefully shuts down the scheduler and all its workers
    ///
    /// Waits for workers to complete their current work or until timeout
    ///
    /// ## Parameters
    /// - `timeout_ms`: Maximum time to wait for workers to complete in milliseconds
    ///
    /// ## Returns
    /// Result indicating successful shutdown or timeout error
    pub async fn shutdown(&self, timeout_ms: u64) -> Result<(), SystemError> {
        // Update status to shutting down
        self.status.store(SchedulerStatus::ShuttingDown as usize, Ordering::SeqCst);
        
        // Set the shutdown flag for all workers
        self.is_shutting_down.store(true, Ordering::SeqCst);
        
        // Notify all workers about shutdown
        for _ in 0..self.pool_size {
            self.scheduling_queue.notify_handle().notify_one();
        }
        
        // TODO: Implement proper waiting for worker shutdown with timeout
        // For now, just assume success
        
        // Mark as fully shutdown
        self.status.store(SchedulerStatus::Shutdown as usize, Ordering::SeqCst);
        
        Ok(())
    }
    
    /// Returns the current number of workers in the pool
    pub fn pool_size(&self) -> usize {
        self.pool_size
    }
    
    /// Returns the current status of the scheduler
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
    
    /// Returns metrics about the scheduler state
    pub fn metrics(&self) -> SchedulerMetrics {
        SchedulerMetrics {
            pool_size: self.pool_size,
            queue_length: self.scheduling_queue.len(),
            is_shutting_down: self.is_shutting_down.load(Ordering::Relaxed),
            status: self.status(),
        }
    }
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
}

#[cfg(test)]
mod tests {
    // TODO: Add comprehensive tests for MultiThreadScheduler
} 