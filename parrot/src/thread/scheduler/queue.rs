use std::fmt;
use std::sync::Arc;
use tokio::sync::Notify;
use crossbeam_queue::SegQueue;

use crate::thread::mailbox::Mailbox;

/// A queue that holds references to mailboxes that have messages ready for processing.
///
/// The SchedulingQueue is a central component of the SharedThreadPool scheduler.
/// It stores Arc<dyn Mailbox> instances that have signaled they have messages
/// ready for processing. Worker threads pull mailboxes from this queue to process
/// their messages.
///
/// # Thread Safety
/// - Uses a lock-free queue internally (SegQueue)
/// - Safe for concurrent producers and consumers
/// - Uses Notify for efficient worker wakeup
///
/// # Performance Characteristics
/// - O(1) push and pop operations
/// - Lock-free implementation for high throughput
/// - Work-stealing design to maximize thread utilization
pub struct SchedulingQueue {
    /// Lock-free queue holding ready mailboxes
    queue: Arc<SegQueue<Arc<dyn Mailbox + Send + Sync>>>,
    
    /// Notification mechanism to wake up workers when queue has items
    notify: Arc<Notify>,
    
    /// Maximum capacity tracker (for metrics and monitoring)
    #[allow(dead_code)]
    max_capacity: usize,
}

impl fmt::Debug for SchedulingQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulingQueue")
            .field("max_capacity", &self.max_capacity)
            .finish()
    }
}

impl SchedulingQueue {
    /// Creates a new SchedulingQueue with the specified initial capacity
    pub fn new(max_capacity: usize) -> Self {
        Self {
            queue: Arc::new(SegQueue::new()),
            notify: Arc::new(Notify::new()),
            max_capacity,
        }
    }
    
    /// Pushes a mailbox into the queue.
    ///
    /// If the queue was empty, this will notify any waiting workers.
    ///
    /// # Parameters
    /// * `mailbox` - Strong reference to a Mailbox that has messages ready to process
    pub fn push(&self, mailbox: Arc<dyn Mailbox + Send + Sync>) {
        // SegQueue doesn't have a capacity limit, so we just push
        self.queue.push(mailbox);
        
        // Notify one worker that a mailbox is available
        self.notify.notify_one();
    }
    
    /// Tries to pop a mailbox from the queue.
    ///
    /// # Returns
    /// * `Some(mailbox)` - A mailbox that has messages ready to process
    /// * `None` - The queue is empty
    pub fn try_pop(&self) -> Option<Arc<dyn Mailbox + Send + Sync>> {
        self.queue.pop()
    }
    
    /// Asynchronously waits for a mailbox to become available.
    ///
    /// This method will:
    /// 1. First try to pop a mailbox immediately
    /// 2. If none is available, wait for a notification
    /// 3. After notification, try to pop again
    ///
    /// This approach ensures that workers don't miss notifications and
    /// don't spin unnecessarily when the queue is empty.
    ///
    /// # Returns
    /// Future resolving to Arc<dyn Mailbox>
    pub async fn pop(&self) -> Arc<dyn Mailbox + Send + Sync> {
        loop {
            // First try to pop immediately
            if let Some(mailbox) = self.try_pop() {
                return mailbox;
            }
            
            // If nothing is available, wait for notification
            self.notify.notified().await;
            
            // Try again after notification (might still fail if another worker got it first)
            if let Some(mailbox) = self.try_pop() {
                return mailbox;
            }
            
            // If we get here, another worker took the mailbox, so we loop and wait again
        }
    }
    
    /// Gets the number of mailboxes currently in the queue.
    ///
    /// This is a snapshot and may change by the time the value is used.
    ///
    /// # Returns
    /// Current queue length
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    
    /// Checks if the queue is empty.
    ///
    /// This is a snapshot and may change by the time the value is used.
    ///
    /// # Returns
    /// true if the queue is empty, false otherwise
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
    
    /// Gets a clone of the notification mechanism for workers to wait on.
    ///
    /// # Returns
    /// Arc<Notify> that workers can await on
    pub fn notify_handle(&self) -> Arc<Notify> {
        self.notify.clone()
    }
    
    /// Gets a clone of the underlying queue for direct access.
    ///
    /// # Returns
    /// Arc<SegQueue<Arc<dyn Mailbox>>>
    pub fn queue_handle(&self) -> Arc<SegQueue<Arc<dyn Mailbox + Send + Sync>>> {
        self.queue.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;

    fn make_mailbox(name: &str) -> Arc<dyn Mailbox + Send + Sync> {
        Arc::new(MpscMailbox::new(4, ActorPath::placeholder(name)))
    }

    #[test]
    fn test_new_queue_starts_empty() {
        let queue = SchedulingQueue::new(16);
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert!(queue.try_pop().is_none());
    }

    #[test]
    fn test_push_then_try_pop_fifo() {
        let queue = SchedulingQueue::new(16);
        let m1 = make_mailbox("q/a");
        let m2 = make_mailbox("q/b");

        queue.push(m1);
        queue.push(m2);
        assert_eq!(queue.len(), 2);
        assert!(!queue.is_empty());

        // FIFO order (best-effort for concurrent queues, deterministic here
        // because pushes and pops are sequential).
        assert_eq!(queue.try_pop().unwrap().path().path, "q/a");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/b");
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn test_async_pop_waits_until_push() {
        let queue = Arc::new(SchedulingQueue::new(16));

        // Spawn a consumer that blocks until an item appears.
        let consumer_queue = queue.clone();
        let consumer = tokio::spawn(async move {
            let mailbox = consumer_queue.pop().await;
            mailbox.path().path.clone()
        });

        // Give the consumer a moment to park, then push.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        queue.push(make_mailbox("q/late"));

        let path = tokio::time::timeout(std::time::Duration::from_secs(2), consumer)
            .await
            .expect("consumer must wake up")
            .expect("join must succeed");
        assert_eq!(path, "q/late");
    }

    #[test]
    fn test_notify_handle_is_shareable() {
        let queue = SchedulingQueue::new(8);
        let notify1 = queue.notify_handle();
        let notify2 = queue.notify_handle();
        // Both handles reference the same Notify instance; use a small
        // current-thread runtime to await the notified future.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async move {
            notify1.notify_one();
            let notified = notify2.notified();
            tokio::pin!(notified);
            // The permit from notify_one is available: awaiting returns immediately.
            notified.await;
        });
    }

    #[test]
    fn test_queue_handle_shares_state() {
        let queue = SchedulingQueue::new(8);
        let handle = queue.queue_handle();
        handle.push(make_mailbox("q/shared"));
        // The handle observes the same underlying queue.
        assert_eq!(queue.len(), 1);
        assert!(queue.try_pop().is_some());
    }

    #[test]
    fn test_concurrent_push_and_pop() {
        let queue = Arc::new(SchedulingQueue::new(1024));
        let producers: Vec<_> = (0..4)
            .map(|p| {
                let q = queue.clone();
                std::thread::spawn(move || {
                    for i in 0..100 {
                        q.push(make_mailbox(&format!("q/p{}/{}", p, i)));
                    }
                })
            })
            .collect();

        for p in producers {
            p.join().unwrap();
        }
        assert_eq!(queue.len(), 400);

        let mut popped = 0;
        while queue.try_pop().is_some() {
            popped += 1;
        }
        assert_eq!(popped, 400);
    }

    #[test]
    fn test_debug_formatting() {
        let queue = SchedulingQueue::new(8);
        let repr = format!("{:?}", queue);
        assert!(repr.contains("SchedulingQueue"));
    }
} 