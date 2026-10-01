use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::SchedulingQueue;

/// Manager for worker threads in the shared thread pool
///
/// WorkerManager is responsible for:
/// - Tracking scheduled actor paths
/// - Tracking worker status slots for metrics
/// - Pushing ready mailboxes into the scheduling queue
#[derive(Debug, Default)]
pub struct WorkerManager {
    /// Map of scheduled actor paths
    scheduled: Mutex<HashMap<String, ()>>,

    /// Worker status slots (one per worker, for metrics)
    worker_statuses: Mutex<Vec<Arc<AtomicUsize>>>,

    /// Queue for scheduling mailboxes
    scheduling_queue: Option<Arc<SchedulingQueue>>,
}

impl WorkerManager {
    /// Create a new worker manager
    ///
    /// # Arguments
    /// * `scheduling_queue` - Shared queue for mailboxes
    pub fn new(scheduling_queue: Arc<SchedulingQueue>) -> Self {
        Self {
            scheduled: Mutex::new(HashMap::new()),
            worker_statuses: Mutex::new(Vec::new()),
            scheduling_queue: Some(scheduling_queue),
        }
    }

    /// Schedule a mailbox for processing
    ///
    /// # Arguments
    /// * `path` - Actor path
    /// * `mailbox` - Actor mailbox
    pub fn schedule_mailbox(
        &self,
        path: String,
        mailbox: Arc<dyn Mailbox>,
    ) -> Result<(), crate::thread::error::SystemError> {
        let queue = self
            .scheduling_queue
            .as_ref()
            .expect("WorkerManager built without scheduling queue");
        {
            let mut scheduled = self.scheduled.lock().unwrap();
            scheduled.insert(path, ());
        }
        queue.push(mailbox);
        Ok(())
    }

    /// Stop tracking an actor
    pub fn stop_processor(&self, path: &str) -> Result<(), crate::thread::error::SystemError> {
        let mut scheduled = self.scheduled.lock().unwrap();
        if scheduled.remove(path).is_none() {
            return Err(crate::thread::error::SystemError::ActorNotFound(path.to_string()));
        }
        Ok(())
    }

    /// Check if an actor is scheduled
    pub fn is_scheduled(&self, path: &str) -> bool {
        self.scheduled.lock().unwrap().contains_key(path)
    }

    /// Get the current number of tracked actors
    pub fn processor_count(&self) -> usize {
        self.scheduled.lock().unwrap().len()
    }

    /// Register a worker status slot for metrics
    pub fn track_worker(&self, status: Arc<AtomicUsize>) {
        self.worker_statuses.lock().unwrap().push(status);
    }

    /// Number of tracked worker slots
    pub fn tracked_worker_count(&self) -> usize {
        self.worker_statuses.lock().unwrap().len()
    }

    /// Number of workers currently idle (by status code 0)
    pub fn idle_worker_count(&self) -> usize {
        self.worker_statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.load(Ordering::Relaxed) == 0)
            .count()
    }
}
