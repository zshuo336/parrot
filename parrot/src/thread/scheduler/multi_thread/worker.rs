use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio::time;
use anyhow::anyhow;

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::SchedulingQueue;
use crate::thread::error::{SystemError, MailboxError};
use crate::thread::config::ThreadActorConfig;

/// # Worker Thread Implementation
///
/// Represents an individual worker thread in the multi-threaded scheduling system.
/// Each worker pulls mailboxes from a shared scheduling queue and processes their messages.
///
/// ## Key Responsibilities
/// - Processing mailboxes from the scheduling queue
/// - Handling actor message execution
/// - Error isolation through panic recovery
/// - Graceful shutdown behavior
///
/// ## Implementation Details
/// ### Core Algorithm
/// 1. Pull mailbox from shared queue
/// 2. Process a batch of messages from the mailbox
/// 3. Return mailbox to queue if not empty
/// 4. Repeat until shutdown signal received
///
/// ### Performance Characteristics
/// - Optimized for throughput with batch processing
/// - Designed for low contention between workers
/// - Backpressure handled through mailbox behavior
///
/// ### Safety Considerations
/// - Panic recovery to isolate actor failures
/// - Structured shutdown sequence
/// - Resource cleanup on termination
pub struct Worker {
    /// Unique identifier for this worker
    id: usize,
    
    /// Tokio runtime handle for async operations
    runtime_handle: Handle,
    
    /// Queue providing mailboxes with messages to process
    scheduling_queue: Arc<SchedulingQueue>,
    
    /// Signal for worker shutdown
    shutdown_flag: Arc<AtomicBool>,
    
    /// Maximum number of messages to process per mailbox in a single run
    batch_size: usize,
    
    /// Current worker status
    status: Arc<AtomicUsize>,
    
    /// System reference for error reporting and actor lookup
    /// Weak reference to avoid circular dependencies
    system: Option<Weak<dyn SystemRef + Send + Sync>>,
    
    /// Optional worker configuration
    config: WorkerConfig,
}

/// Status codes for worker state
pub enum WorkerStatus {
    /// Worker is idle, waiting for work
    Idle = 0,
    
    /// Worker is actively processing messages
    Processing = 1,
    
    /// Worker is shutting down
    ShuttingDown = 2,
    
    /// Worker has encountered an error
    Error = 3,
}

/// Configuration options for worker behavior
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Maximum number of messages to process from a mailbox in one batch
    pub batch_size: usize,
    
    /// Duration to sleep when idle before checking for work again
    pub idle_sleep_duration: Duration,
    
    /// Whether to yield to the scheduler after processing each message
    pub yield_after_each_message: bool,
    
    /// Whether to log detailed processing metrics
    pub enable_detailed_logging: bool,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            batch_size: 10,
            idle_sleep_duration: Duration::from_millis(10),
            yield_after_each_message: false,
            enable_detailed_logging: false,
        }
    }
}

/// Interface for system-level operations needed by workers
pub trait SystemRef {
    /// Find an actor by its path
    fn find_actor(&self, path: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>>;
    
    /// Handle worker panic, providing error details and affected mailbox path
    fn handle_worker_panic(&self, error: String, mailbox_path: String);
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("id", &self.id)
            .field("batch_size", &self.batch_size)
            .field("status", &self.status.load(Ordering::Relaxed))
            .finish()
    }
}

impl Worker {
    /// Creates a new worker with the specified parameters
    ///
    /// ## Parameters
    /// - `id`: Unique identifier for this worker
    /// - `runtime_handle`: Tokio runtime handle for async operations
    /// - `scheduling_queue`: Queue providing mailboxes with messages to process
    /// - `shutdown_flag`: Signal for worker shutdown
    /// - `system`: Optional system reference for error reporting
    /// - `config`: Optional worker configuration
    pub fn new(
        id: usize,
        runtime_handle: Handle,
        scheduling_queue: Arc<SchedulingQueue>,
        shutdown_flag: Arc<AtomicBool>,
        system: Option<Weak<dyn SystemRef + Send + Sync>>,
        config: Option<WorkerConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        let batch_size = config.batch_size;
        
        Self {
            id,
            runtime_handle,
            scheduling_queue,
            shutdown_flag,
            batch_size,
            status: Arc::new(AtomicUsize::new(WorkerStatus::Idle as usize)),
            system,
            config,
        }
    }
    
    /// Launches the worker's main loop as a Tokio task
    ///
    /// ## Returns
    /// A JoinHandle for the worker task
    pub fn spawn(self) -> JoinHandle<()> {
        self.runtime_handle.spawn(async move {
            self.run_loop().await;
        })
    }
    
    /// Main worker loop
    ///
    /// Continuously processes mailboxes until shutdown is requested
    async fn run_loop(&self) {
        // Worker identification for logging
        let worker_name = format!("worker-{}", self.id);
        
        while !self.shutdown_flag.load(Ordering::Relaxed) {
            // Try to get a mailbox from the queue
            match self.scheduling_queue.try_pop() {
                Some(mailbox) => {
                    // We have a mailbox to process
                    self.status.store(WorkerStatus::Processing as usize, Ordering::Relaxed);
                    self.process_mailbox(mailbox, &worker_name).await;
                    self.status.store(WorkerStatus::Idle as usize, Ordering::Relaxed);
                }
                None => {
                    // No mailbox available, wait for notification or timeout
                    self.status.store(WorkerStatus::Idle as usize, Ordering::Relaxed);
                    
                    // Wait for notification or periodic check
                    let notified = self.scheduling_queue.notify_handle().notified();
                    tokio::select! {
                        _ = notified => {
                            // Notified that work is available
                        },
                        _ = time::sleep(self.config.idle_sleep_duration) => {
                            // Periodic check for shutdown condition
                            if self.shutdown_flag.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                    }
                }
            }
        }
        
        // Worker is shutting down
        self.status.store(WorkerStatus::ShuttingDown as usize, Ordering::Relaxed);
        if self.config.enable_detailed_logging {
            println!("Worker {} shutting down", worker_name);
        }
    }
    
    /// Processes a single mailbox, handling a batch of messages
    ///
    /// ## Parameters
    /// - `mailbox`: The mailbox to process
    /// - `worker_name`: Worker identification for logging
    async fn process_mailbox(&self, mailbox: Arc<dyn Mailbox + Send + Sync>, worker_name: &str) {
        // Get actor path for error reporting
        let actor_path = mailbox.path().path.clone();
        
        // Execute in a panic-catching boundary
        let process_result = panic::catch_unwind(AssertUnwindSafe(|| -> Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + Send>> {
            let mailbox = mailbox.clone();
            let shutdown_flag = self.shutdown_flag.clone();
            let batch_size = self.batch_size;
            let yield_after_each_message = self.config.yield_after_each_message;
            
            Box::pin(async move {
                // Process up to batch_size messages
                let mut processed = 0;
                
                while processed < batch_size {
                    // Check shutdown flag frequently
                    if shutdown_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    
                    // Try to get a message
                    if let Some(msg) = mailbox.pop().await {
                        // TODO: Process message with the actual actor
                        // For now, we just count it as processed
                        processed += 1;
                        
                        // Optionally yield to scheduler after each message
                        if yield_after_each_message {
                            tokio::task::yield_now().await;
                        }
                    } else {
                        // No more messages in this mailbox
                        break;
                    }
                }
                
                // If the mailbox still has messages, put it back in the queue
                if !mailbox.is_empty().await {
                    self.scheduling_queue.push(mailbox);
                }
                
                Ok(())
            })
        }));
        
        // Handle the result of message processing
        match process_result {
            Ok(future) => {
                // Execute the future in the runtime
                match self.runtime_handle.block_on(future) {
                    Ok(_) => {
                        // Successfully processed the mailbox
                        if self.config.enable_detailed_logging {
                            println!("Worker {} successfully processed mailbox {}", worker_name, actor_path);
                        }
                    }
                    Err(e) => {
                        // Error during async processing
                        let error_msg = format!(
                            "Runtime error in worker {}, actor {}: {:?}",
                            worker_name, actor_path, e
                        );
                        
                        // Report error to system
                        self.report_error(error_msg, actor_path.clone());
                        
                        // Put mailbox back in the queue for retry
                        self.scheduling_queue.push(mailbox);
                    }
                }
            }
            Err(panic_error) => {
                // Actor panicked during message processing
                let error_msg = match panic_error.downcast::<String>() {
                    Ok(string) => format!("Panic in actor {}: {}", actor_path, string),
                    Err(e) => format!("Panic in actor {}: {:?}", actor_path, e),
                };
                
                // Report panic to system
                self.report_error(error_msg, actor_path.clone());
                
                // Put mailbox back in the queue for retry
                // Note: In a real implementation, this might depend on supervision strategy
                self.scheduling_queue.push(mailbox);
                
                // Mark worker as having encountered an error
                self.status.store(WorkerStatus::Error as usize, Ordering::Relaxed);
            }
        }
    }
    
    /// Reports an error to the system
    ///
    /// ## Parameters
    /// - `error_msg`: Detailed error message
    /// - `mailbox_path`: Path of the affected actor's mailbox
    fn report_error(&self, error_msg: String, mailbox_path: String) {
        if let Some(sys_ref) = &self.system {
            if let Some(sys) = sys_ref.upgrade() {
                sys.handle_worker_panic(error_msg, mailbox_path);
            }
        }
    }
    
    /// Returns the current status of the worker
    pub fn get_status(&self) -> WorkerStatus {
        match self.status.load(Ordering::Relaxed) {
            0 => WorkerStatus::Idle,
            1 => WorkerStatus::Processing,
            2 => WorkerStatus::ShuttingDown,
            3 => WorkerStatus::Error,
            _ => WorkerStatus::Error, // Default to error for unknown status
        }
    }
}

#[cfg(test)]
mod tests {
    // TODO: Add comprehensive tests for Worker functionality
} 