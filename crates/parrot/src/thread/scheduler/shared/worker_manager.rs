use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
    scheduled: Mutex<HashMap<String, Arc<dyn Mailbox>>>,

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
            if scheduled.contains_key(&path) {
                return Err(crate::thread::error::SystemError::RegistrationError(
                    format!("actor {path} is already scheduled"),
                ));
            }
            if !queue.register(&mailbox) {
                return Err(crate::thread::error::SystemError::ShuttingDown);
            }
            scheduled.insert(path, mailbox.clone());
        }
        if mailbox.schedule_state().try_enqueue() {
            queue.enqueue_registered(mailbox, crate::thread::scheduler::queue::Lane::Normal);
        }
        Ok(())
    }

    /// Stop tracking an actor
    pub fn stop_processor(&self, path: &str) -> Result<(), crate::thread::error::SystemError> {
        let mailbox =
            self.scheduled.lock().unwrap().remove(path).ok_or_else(|| {
                crate::thread::error::SystemError::ActorNotFound(path.to_string())
            })?;
        if let Some(queue) = &self.scheduling_queue {
            queue.unregister(&mailbox);
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

    /// Workers with poll capacity (idle or owning only suspended futures).
    pub fn idle_worker_count(&self) -> usize {
        self.worker_statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|s| matches!(s.load(Ordering::Relaxed), 0 | 4))
            .count()
    }
}
