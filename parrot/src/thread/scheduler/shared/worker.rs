use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;
use std::collections::HashMap;
use std::any::Any;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio::time;
use anyhow::anyhow;
use tracing::{debug, error, info, warn};

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::SchedulingQueue;
use crate::thread::error::{SystemError, MailboxError};
use crate::thread::config::ThreadActorConfig;
use crate::thread::processor::ActorProcessorManager;
use crate::thread::actor::ThreadActor;
use crate::thread::context::ThreadContext;
use parrot_api::actor::Actor;
use parrot_api::types::{BoxedMessage, ActorResult};
use crate::thread::system::ThreadActorSystem;

/// # Worker Thread Implementation
///
/// Represents an individual worker thread in the shared thread pool.
/// Each worker pulls mailboxes from a shared scheduling queue and processes their messages.
///
/// ## Key Responsibilities
/// - Processing mailboxes from the scheduling queue
/// - Handling actor message execution through ActorProcessor
/// - Error isolation through panic recovery
/// - Graceful shutdown behavior
///
/// ## Implementation Details
/// ### Core Algorithm
/// 1. Pull mailbox from shared queue
/// 2. Process a batch of messages from the mailbox using its associated processor
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
    system: Option<Weak<ThreadActorSystem>>,
    
    /// Worker configuration
    config: WorkerConfig,
    
    /// Map of mailbox paths to their processors
    processors: Arc<Mutex<HashMap<String, Box<dyn std::any::Any + Send + Sync>>>>,
    
    /// Processor factory for creating new processors
    processor_factory: Option<Arc<ActorProcessorManager>>,
}

/// Status codes for worker state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        system: Option<Weak<ThreadActorSystem>>,
        config: Option<WorkerConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        let batch_size = config.batch_size;
        let processor_factory = Some(Arc::new(ActorProcessorManager::new()));
        
        Self {
            id,
            runtime_handle,
            scheduling_queue,
            shutdown_flag,
            batch_size,
            status: Arc::new(AtomicUsize::new(WorkerStatus::Idle as usize)),
            system,
            config,
            processors: Arc::new(Mutex::new(HashMap::new())),
            processor_factory,
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
                    self.process_mailbox(mailbox, self.batch_size).await.unwrap();
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
                            // Periodic check for shutdown
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
    }
    
    /// Process messages from a mailbox using its associated processor
    async fn process_mailbox(&self, mailbox: Arc<dyn Mailbox>, max_messages: usize) -> anyhow::Result<(bool, usize)> {
        let actor_path = mailbox.path().to_string();
        let worker_name = format!("worker-{}", self.id);
        let enable_detailed_logging = self.config.enable_detailed_logging;
        let yield_after_each_message = self.config.yield_after_each_message;
        
        // Check if the mailbox has an associated processor
        if !mailbox.has_processor() {
            // If the mailbox has no associated processor, try to get or create one
            if let Some(processor) = self.get_or_create_processor_for_mailbox(&mailbox).await {
                // Set the processor for the mailbox
                mailbox.set_processor(processor);
            } else {
                // No processor available, log warning and skip processing
                warn!("Could not get or create processor for actor at {}", actor_path);
                return Ok((false, 0));
            }
        }
        
        let mut processed = 0;
        let mut should_requeue = false;
        
        // Process messages using the mailbox's associated processor
        // We'll handle a batch of messages here
        let processor_opt = mailbox.get_processor();
        
        if let Some(processor_any) = processor_opt {
            // Try to process a batch of messages using the processor
            let process_result = panic::catch_unwind(AssertUnwindSafe(|| {
                self.process_batch_with_processor(processor_any, &mailbox, max_messages, yield_after_each_message)
            }));
            
            match process_result {
                Ok(future) => {
                    // Execute the batch processing future
                    match future.await {
                        Ok((proc_count, had_error)) => {
                            processed = proc_count;
                            should_requeue = had_error;
                            
                            if enable_detailed_logging {
                                debug!("[{}] Processed {} messages for {}", worker_name, processed, actor_path);
                            }
                        },
                        Err(e) => {
                            // Error during batch processing
                            error!("[{}] Error processing batch for {}: {}", worker_name, actor_path, e);
                            should_requeue = true;
                        }
                    }
                },
                Err(panic_error) => {
                    // Processor panicked during batch processing
                    let error_msg = match panic_error.downcast::<String>() {
                        Ok(string) => format!("Panic in actor {}: {}", actor_path, string),
                        Err(e) => format!("Panic in actor {}: {:?}", actor_path, e),
                    };
                    
                    error!("[{}] {}", worker_name, error_msg);
                    
                    // Report error to system
                    self.report_error(error_msg, actor_path.clone());
                    
                    // Mark for requeue
                    should_requeue = true;
                }
            }
        } else {
            // Processor not available
            warn!("[{}] No processor available for actor {}", worker_name, actor_path);
            should_requeue = false;
        }
        
        // If the mailbox still has messages, it should be requeued
        should_requeue = should_requeue || (processed > 0 && mailbox.has_more_messages().await);
        
        Ok((should_requeue, processed))
    }
    
    /// Process a batch of messages using the provided processor
    fn process_batch_with_processor(
        &self, 
        processor_any: &Box<dyn Any + Send + Sync>,
        mailbox: &Arc<dyn Mailbox>,
        max_messages: usize,
        yield_after_each_message: bool
    ) -> Pin<Box<dyn Future<Output = Result<(usize, bool), anyhow::Error>> + Send + '_>> {
        Box::pin(async move {
            let mut processed = 0;
            let mut had_error = false;
            
            // First, try to use the system reference to process the batch
            if let Some(strong_system) = self.system.as_ref().and_then(|sys| sys.upgrade()) {
                return strong_system.process_batch(mailbox.path().to_string(), *mailbox.clone(), max_messages, yield_after_each_message).await
                    .map_err(|e| anyhow!("Error processing batch via system: {}", e));
            }
            
            // If we can't use the system, try to directly process using the processor
            // This is more complex because we need to handle type erasure
            
            // We'll try to process messages one by one from the mailbox
            for _ in 0..max_messages {
                match mailbox.pop().await {
                    Some(msg) => {
                        match self.process_message_with_processor(processor_any, msg, mailbox.path().to_string().as_str()).await {
                            Ok(_) => {
                                processed += 1;
                                if yield_after_each_message {
                                    tokio::task::yield_now().await;
                                }
                            },
                            Err(e) => {
                                error!("Error processing message: {}", e);
                                had_error = true;
                                processed += 1;
                            }
                        }
                    },
                    None => break, // No more messages
                }
            }
            
            Ok((processed, had_error))
        })
    }
    
    /// Try to get or create a processor for the given mailbox
    async fn get_or_create_processor_for_mailbox(&self, mailbox: &Arc<dyn Mailbox>) -> Option<Box<dyn Any + Send + Sync>> {
        let path = mailbox.path().to_string();
        
        // First check if we already have this processor
        {
            let processors = self.processors.lock().unwrap();
            if let Some(processor) = processors.get(&path) {
                return Some(processor.clone());
            }
        }
        
        // If not, try to create through the system
        if let Some(strong_system) = self.system.as_ref().and_then(|sys| sys.upgrade()) {
            if let Ok(processor) = strong_system.create_processor_for_mailbox(mailbox.clone()).await {
                // Store in our local cache
                let mut processors = self.processors.lock().unwrap();
                processors.insert(path, processor.clone());
                return Some(processor);
            }
        }
        
        None
    }
    
    /// Process a single message using the provided processor
    async fn process_message_with_processor(
        &self,
        processor_any: &Box<dyn Any + Send + Sync>,
        message: BoxedMessage,
        actor_path: &str
    ) -> Result<(), anyhow::Error> {
        // Try using the system to process the message
        if let Some(strong_system) = self.system.as_ref().and_then(|sys| sys.upgrade()) {
            return strong_system.handle_message(actor_path, message).await
                .map_err(|e| anyhow!("Error processing message via system: {}", e));
        }
        
        // If system approach fails, try direct message processing
        // This depends on the actual type of the processor, which we don't know here
        // For a real implementation, you would need a way to dispatch this properly
        
        Err(anyhow!("No suitable processor implementation found for actor {}", actor_path))
    }
    
    /// Reports an error to the system
    fn report_error(&self, error_msg: String, mailbox_path: String) {
        if let Some(system_ref) = &self.system {
            if let Some(system) = system_ref.upgrade() {
                system.handle_worker_panic(error_msg, mailbox_path);
            }
        }
    }
    
    /// Returns a reference to the worker's status
    pub fn status(&self) -> Arc<AtomicUsize> {
        self.status.clone()
    }
    
    /// Get the current status of the worker
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
    // TODO: Add comprehensive worker tests
} 