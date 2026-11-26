use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::{Lane, SchedulingQueue, WakeAdmission};

// The same mailbox and path can be registered again before an older call
// finishes publishing its hook. Each admission needs its own rollback identity.
#[derive(Debug)]
pub(super) struct ScheduledMailbox(pub(super) Arc<dyn Mailbox>);
pub(super) type ScheduledPaths = Mutex<HashMap<String, Arc<ScheduledMailbox>>>;

impl ScheduledMailbox {
    pub(super) fn rollback(
        self: &Arc<Self>,
        paths: &ScheduledPaths,
        path: &str,
        queue: &SchedulingQueue,
    ) {
        let removed = {
            let mut paths = paths.lock().unwrap();
            if paths
                .get(path)
                .is_some_and(|current| Arc::ptr_eq(current, self))
            {
                paths.remove(path)
            } else {
                None
            }
        };
        if removed.is_some() {
            queue.unregister(&self.0);
        }
    }
}

/// Manager for worker threads in the shared thread pool
///
/// WorkerManager is responsible for:
/// - Tracking scheduled actor paths
/// - Tracking worker status slots for metrics
/// - Pushing ready mailboxes into the scheduling queue
#[derive(Debug, Default)]
pub struct WorkerManager {
    /// Map of scheduled actor paths
    scheduled: ScheduledPaths,

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
        self.schedule_mailbox_after_register(path, mailbox, || {})
    }

    fn schedule_mailbox_after_register(
        &self,
        path: String,
        mailbox: Arc<dyn Mailbox>,
        after_register: impl FnOnce(),
    ) -> Result<(), crate::thread::error::SystemError> {
        let queue = self
            .scheduling_queue
            .as_ref()
            .expect("WorkerManager built without scheduling queue");
        let registration = {
            let mut scheduled = self.scheduled.lock().unwrap();
            if scheduled.contains_key(&path) {
                return Err(crate::thread::error::SystemError::RegistrationError(
                    format!("actor {path} is already scheduled"),
                ));
            }
            if !queue.register(&mailbox) {
                return Err(crate::thread::error::SystemError::ShuttingDown);
            }
            let registration = Arc::new(ScheduledMailbox(mailbox.clone()));
            scheduled.insert(path.clone(), registration.clone());
            registration
        };
        after_register();
        if queue.wake_registered(&mailbox, Lane::Normal) == WakeAdmission::Detached {
            registration.rollback(&self.scheduled, &path, queue);
            return Err(crate::thread::error::SystemError::ShuttingDown);
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
            queue.unregister(&mailbox.0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;

    #[test]
    fn schedule_rolls_back_when_queue_closes_after_registration() {
        let queue = Arc::new(SchedulingQueue::new(4));
        let manager = WorkerManager::new(queue.clone());
        let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
            4,
            ActorPath::placeholder("worker-manager"),
        ));

        let result = manager.schedule_mailbox_after_register(
            "worker-manager".into(),
            mailbox.clone(),
            || queue.close(),
        );

        assert!(matches!(
            result,
            Err(crate::thread::error::SystemError::ShuttingDown)
        ));
        assert_eq!(manager.processor_count(), 0);
        assert!(!manager.is_scheduled("worker-manager"));
        assert!(mailbox.schedule_state().try_enqueue());
        mailbox.schedule_state().force_release();
    }
}
