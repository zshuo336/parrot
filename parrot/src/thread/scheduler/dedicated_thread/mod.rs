//! # Dedicated Thread Module
//!
//! Provides the implementation of dedicated thread schedulers for actor execution.
//! Each actor gets its own dedicated thread for execution, providing isolation
//! and predictable performance at the cost of higher resource usage.
//!
//! ## Key Concepts
//! - Thread isolation: Each actor runs on its own dedicated thread
//! - Predictable performance: No contention with other actors
//! - Priority management: Support for actor priority levels
//!
//! ## Design Principles
//! - Isolation: Performance and errors are isolated to individual threads
//! - Determinism: Consistent execution characteristics
//! - Resource efficiency: Thread lifecycle management to limit overhead

mod worker;

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex, Weak, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use parrot_api::actor::Actor;

use crate::thread::mailbox::Mailbox;
use crate::thread::config::ThreadActorConfig;
use crate::thread::error::SystemError;
use crate::thread::scheduler::{ThreadScheduler, TypedThreadScheduler};
use crate::thread::context::ThreadContext;
use worker::Worker;

/// Status codes for the dedicated thread scheduler
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

/// Configuration for dedicated thread scheduler
#[derive(Debug, Clone)]
pub struct DedicatedThreadConfig {
    /// Whether to use a priority queue for message processing
    pub use_priority_queue: bool,
    
    /// Maximum number of messages to process in one batch
    pub max_messages_per_batch: usize,
    
    /// Duration to sleep when idle before checking for work again
    pub idle_sleep_duration: Duration,
    
    /// Whether to yield to the scheduler after processing each message
    pub yield_after_each_message: bool,
    
    /// Whether to log detailed processing metrics
    pub enable_detailed_logging: bool,
}

impl Default for DedicatedThreadConfig {
    fn default() -> Self {
        Self {
            use_priority_queue: false,
            max_messages_per_batch: 10,
            idle_sleep_duration: Duration::from_millis(10),
            yield_after_each_message: false,
            enable_detailed_logging: false,
        }
    }
}

/// Implementation of a dedicated thread scheduler
///
/// Each actor is assigned a dedicated thread for its message processing.
#[derive(Debug, Clone)]
pub struct DedicatedThreadScheduler {
    /// Map of actor paths to their dedicated worker threads
    workers: Arc<Mutex<HashMap<String, Worker>>>,
    
    /// Scheduler configuration
    config: DedicatedThreadConfig,
    
    /// Shutdown flag
    is_shutting_down: Arc<AtomicBool>,
    
    /// Scheduler status
    status: Arc<AtomicUsize>,
}

impl DedicatedThreadScheduler {
    /// Create a new dedicated thread scheduler
    pub fn new(
        config: Option<DedicatedThreadConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        let status = Arc::new(AtomicUsize::new(SchedulerStatus::Initializing as usize));
        
        let scheduler = Self {
            workers: Arc::new(Mutex::new(HashMap::new())),
            config,
            is_shutting_down,
            status,
        };
        
        // Mark as running
        scheduler.status.store(SchedulerStatus::Running as usize, Ordering::SeqCst);
        
        scheduler
    }
    
    /// Schedule an actor on a dedicated thread
    pub async fn schedule<A>(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), SystemError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        // Check if already shutting down
        if self.is_shutting_down.load(Ordering::Relaxed) {
            return Err(SystemError::ShuttingDown);
        }
        
        let mut workers = self.workers.lock().unwrap();
        
        // dedicated thread scheduler is olny one scedule for each actor
        if workers.contains_key(path) {
            return Ok(());
        }
        
        // Create a new worker for this actor
        let worker = Worker::new(
            path.to_string(),
            mailbox,
            ThreadActorConfig {
                yield_after_each_message: Some(self.config.yield_after_each_message),
                idle_sleep_duration: Some(self.config.idle_sleep_duration),
                thread_stack_size: Some(3 * 1024 * 1024), // default 3MB
                ..Default::default()
            },
            self.config.max_messages_per_batch,
        );
        
        // Start the worker
        worker.start::<A>();
        
        // Add to workers map
        workers.insert(path.to_string(), worker);
        
        Ok(())
    }
    
    /// Deschedule an actor from its dedicated thread
    pub async fn deschedule(&self, path: &str) -> Result<(), SystemError> {
        let mut workers = self.workers.lock().unwrap();
        
        // Get the worker for this actor
        let worker = workers.remove(path).ok_or_else(|| {
            SystemError::ActorNotFound(path.to_string())
        })?;
        
        // Stop the worker
        worker.stop().await?;
        
        Ok(())
    }
    
    /// Check if an actor is scheduled
    pub fn is_scheduled(&self, path: &str) -> bool {
        let workers = self.workers.lock().unwrap();
        workers.contains_key(path)
    }
    
    /// Shutdown the scheduler and all dedicated threads
    pub async fn shutdown(&self) -> Result<(), SystemError> {
        // Set the shutting down flag
        self.status.store(SchedulerStatus::ShuttingDown as usize, Ordering::SeqCst);
        self.is_shutting_down.store(true, Ordering::SeqCst);
        
        // Stop all workers
        let workers = {
            let mut workers_guard = self.workers.lock().unwrap();
            std::mem::take(&mut *workers_guard)
        };
        
        // Stop each worker
        for (_, worker) in workers {
            let _ = worker.stop().await;
        }
        
        // Mark as fully shutdown
        self.status.store(SchedulerStatus::Shutdown as usize, Ordering::SeqCst);
        
        Ok(())
    }
    
    /// Get the current status of the scheduler
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
    
    /// Get the number of dedicated workers
    pub fn worker_count(&self) -> usize {
        let workers = self.workers.lock().unwrap();
        workers.len()
    }
}

// Implement the ThreadScheduler trait
impl ThreadScheduler for DedicatedThreadScheduler {
    fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        Err("Cannot schedule without knowing the actor type. Use schedule_typed instead.".into())
    }
    
    fn deschedule(
        &self,
        path: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.deschedule(path).await
            })
        }).map_err(|e| e.into())
    }
    
    fn is_scheduled(&self, path: &str) -> bool {
        self.is_scheduled(path)
    }
    
    fn shutdown(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.shutdown().await
            })
        }).map_err(|e| e.into())
    }
}

impl TypedThreadScheduler for DedicatedThreadScheduler {
    fn schedule_typed<A>(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.schedule::<A>(path, mailbox, config).await
            })
        }).map_err(|e| e.into())
    }
} 