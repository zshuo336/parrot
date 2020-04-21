use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Weak, atomic::{AtomicBool, Ordering}};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use anyhow::anyhow;

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::SchedulingQueue;
use crate::thread::config::ThreadActorConfig;
use crate::thread::error::SystemError;

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
/// 2. Processes a batch of messages (limited by max_messages_per_run)
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
    
    /// System reference (weak to avoid cycles)
    #[allow(dead_code)]
    system: Option<Weak<dyn SystemRef + Send + Sync>>,
}

/// Interface for system functionality
pub trait SystemRef {
    /// Find an Actor instance
    fn find_actor(&self, path: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>>;
    
    /// Handle panic caught in worker thread
    fn handle_worker_panic(&self, error: String, mailbox_path: String);
}

impl SharedThreadPool {
    /// Create new SharedThreadPool
    /// 
    /// # Arguments
    /// * `pool_size` - Number of worker threads
    /// * `runtime_handle` - Tokio runtime handle
    /// * `max_queue_capacity` - Max scheduling queue capacity (for monitoring)
    /// * `system` - Optional system reference
    pub fn new(
        pool_size: usize,
        runtime_handle: Handle,
        max_queue_capacity: usize,
        system: Option<Weak<dyn SystemRef + Send + Sync>>,
    ) -> Self {
        let scheduling_queue = Arc::new(SchedulingQueue::new(max_queue_capacity));
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        
        let mut workers = Vec::with_capacity(pool_size);
        
        for worker_id in 0..pool_size {
            let queue = scheduling_queue.clone();
            let shutdown_flag = is_shutting_down.clone();
            let rt_handle = runtime_handle.clone();
            let sys = system.clone();
            
            // Start worker thread
            let worker = runtime_handle.spawn(async move {
                SharedThreadPool::worker_loop(worker_id, queue, shutdown_flag, rt_handle, sys).await;
            });
            
            workers.push(worker);
        }
        
        Self {
            pool_size,
            workers,
            scheduling_queue,
            runtime_handle,
            is_shutting_down,
            system,
        }
    }
    
    /// Add mailbox to scheduling queue
    /// 
    /// # Arguments
    /// * `mailbox` - Mailbox to schedule
    /// * `_config` - Actor config (for future priority features)
    pub fn schedule(&self, mailbox: Arc<dyn Mailbox + Send + Sync>, _config: Option<&ThreadActorConfig>) {
        if !self.is_shutting_down.load(Ordering::Relaxed) {
            self.scheduling_queue.push(mailbox);
        }
    }
    
    /// Gracefully shutdown thread pool
    /// 
    /// Waits for all worker threads to complete (up to timeout_ms milliseconds)
    /// 
    /// # Arguments
    /// * `timeout_ms` - Maximum milliseconds to wait for threads
    /// 
    /// # Returns
    /// Success or timeout error
    pub async fn shutdown(&self, timeout_ms: u64) -> Result<(), SystemError> {
        // Set shutdown flag
        self.is_shutting_down.store(true, Ordering::Relaxed);
        
        // Notify all worker threads
        for _ in 0..self.pool_size {
            self.scheduling_queue.notify_handle().notify_one();
        }
        
        // Wait for all worker threads to terminate with timeout
        let workers = self.workers.iter();
        // This is just an example implementation, actually need to wait for all workers
        // In real code, would collect worker JoinHandles into an interruptible future
        
        // For simplicity, just return success
        // Real impl should wait for workers to finish or timeout
        Ok(())
    }
    
    /// Get number of worker threads
    pub fn pool_size(&self) -> usize {
        self.pool_size
    }
    
    /// Main loop for worker threads
    /// 
    /// Responsible for getting mailboxes from queue and processing messages
    /// 
    /// # Arguments
    /// * `worker_id` - Unique worker thread ID (for logging/debugging)
    /// * `queue` - Scheduling queue reference
    /// * `is_shutting_down` - Shutdown flag
    /// * `runtime_handle` - Tokio runtime handle
    /// * `system` - Optional system reference
    async fn worker_loop(
        worker_id: usize,
        queue: Arc<SchedulingQueue>,
        is_shutting_down: Arc<AtomicBool>,
        runtime_handle: Handle,
        system: Option<Weak<dyn SystemRef + Send + Sync>>,
    ) {
        // Worker thread identifier (for logging)
        let worker_name = format!("thread-pool-worker-{}", worker_id);
        
        // Main loop
        while !is_shutting_down.load(Ordering::Relaxed) {
            // Get next ready mailbox from queue
            let mailbox = match queue.try_pop() {
                Some(mb) => mb,
                None => {
                    // No mailbox available, wait for notification
                    let notified = queue.notify_handle().notified();
                    tokio::select! {
                        _ = notified => {},
                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                            // Periodically check shutdown to avoid permanent blocking
                            if is_shutting_down.load(Ordering::Relaxed) {
                                break;
                            }
                            continue;
                        }
                    }
                    continue;
                }
            };
            
            // Get actor path (for error reporting)
            let actor_path = mailbox.path().path.clone();
            
            // Safely process messages, catch panics
            let process_result = panic::catch_unwind(AssertUnwindSafe(|| async {
                // Default process 10 messages
                const DEFAULT_BATCH_SIZE: usize = 10;
                
                // Process a batch of messages
                let mut processed = 0;
                while processed < DEFAULT_BATCH_SIZE {
                    // Try to get message
                    if let Some(msg) = mailbox.pop().await {
                        // Find corresponding actor and process message
                        // TODO: This needs to be implemented in system
                        // Currently placeholder code
                        processed += 1;
                    } else {
                        // No more messages
                        break;
                    }
                    
                    // Check shutdown flag
                    if is_shutting_down.load(Ordering::Relaxed) {
                        break;
                    }
                }
                
                // If mailbox not empty, requeue it
                if !mailbox.is_empty().await {
                    queue.push(mailbox);
                }
            }));
            
            // Execute async processing in Tokio runtime
            match process_result {
                Ok(future) => {
                    // Run async task in worker thread
                    match runtime_handle.block_on(future) {
                        Ok(_) => {
                            // Successfully processed
                        }
                        Err(e) => {
                            // Async runtime error
                            let error_msg = format!(
                                "Runtime error in worker {}, actor {}: {:?}",
                                worker_name, actor_path, e
                            );
                            
                            // Notify system to handle error
                            if let Some(sys_ref) = &system {
                                if let Some(sys) = sys_ref.upgrade() {
                                    sys.handle_worker_panic(error_msg, actor_path);
                                }
                            }
                        }
                    }
                }
                Err(panic_error) => {
                    // Panic occurred in actor processing
                    let error_msg = match panic_error.downcast::<String>() {
                        Ok(string) => format!("Panic in actor {}: {}", actor_path, string),
                        Err(e) => format!("Panic in actor {}: {:?}", actor_path, e),
                    };
                    
                    // Notify system to handle panic
                    if let Some(sys_ref) = &system {
                        if let Some(sys) = sys_ref.upgrade() {
                            sys.handle_worker_panic(error_msg, actor_path);
                        }
                    }
                    
                    // Requeue mailbox for other workers to try
                    // Note: Real system may need more complex recovery logic
                    queue.push(mailbox);
                }
            }
        }
        
        // Cleanup when thread exits
        println!("Worker {} shutting down", worker_name);
    }
}

#[cfg(test)]
mod tests {
    // TODO: Add tests for SharedThreadPool
} 