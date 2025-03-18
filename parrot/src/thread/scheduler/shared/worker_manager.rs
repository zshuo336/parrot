use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::anyhow;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::SchedulingQueue;
use crate::thread::config::ThreadActorConfig;
use crate::thread::error::SystemError;
use crate::thread::processor::ActorProcessorManager;

/// Manager for worker threads in the shared thread pool
///
/// WorkerManager is responsible for:
/// - Tracking active mailboxes and their processors
/// - Starting and stopping actor processors
/// - Scheduling mailboxes to be processed
#[derive(Debug)]
pub struct WorkerManager {
    /// Map of active actor paths to their processors
    processors: Mutex<HashMap<String, Arc<ActorProcessorManager>>>,
    
    /// Queue for scheduling mailboxes
    scheduling_queue: Arc<SchedulingQueue>,
    
    /// Runtime handle for async operations
    runtime_handle: Handle,
    
    /// Maximum messages to process per batch
    max_messages_per_batch: usize,
}

impl WorkerManager {
    /// Create a new worker manager
    ///
    /// # Arguments
    /// * `runtime_handle` - Tokio runtime handle for async operations
    /// * `scheduling_queue` - Shared queue for mailboxes
    /// * `max_messages_per_batch` - Maximum messages to process per batch
    pub fn new(
        runtime_handle: Handle,
        scheduling_queue: Arc<SchedulingQueue>,
        max_messages_per_batch: usize,
    ) -> Self {
        Self {
            processors: Mutex::new(HashMap::new()),
            scheduling_queue,
            runtime_handle,
            max_messages_per_batch,
        }
    }
    
    /// Schedule a mailbox for processing
    ///
    /// # Arguments
    /// * `path` - Actor path
    /// * `mailbox` - Actor mailbox
    /// * `config` - Optional actor configuration
    pub async fn schedule_mailbox(
        &self,
        path: String,
        mailbox: Arc<dyn Mailbox>,
        _config: Option<ThreadActorConfig>,
    ) -> Result<(), SystemError> {
        // Schedule for processing
        self.scheduling_queue.push(mailbox);
        
        Ok(())
    }
    
    /// Stop an actor processor
    ///
    /// # Arguments
    /// * `path` - Actor path to stop
    pub async fn stop_processor(&self, path: &str) -> Result<(), SystemError> {
        let mut processors = self.processors.lock().unwrap();
        if processors.remove(path).is_none() {
            return Err(SystemError::ActorNotFound(path.to_string()));
        }
        
        Ok(())
    }
    
    /// Check if an actor is scheduled
    ///
    /// # Arguments
    /// * `path` - Actor path to check
    pub fn is_scheduled(&self, path: &str) -> bool {
        let processors = self.processors.lock().unwrap();
        processors.contains_key(path)
    }
    
    /// Get the current number of active processors
    pub fn processor_count(&self) -> usize {
        let processors = self.processors.lock().unwrap();
        processors.len()
    }
} 