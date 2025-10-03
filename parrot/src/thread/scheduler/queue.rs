use crossbeam_queue::SegQueue;
use std::fmt;
use std::sync::Arc;
use tokio::sync::Notify;

use crate::thread::mailbox::Mailbox;

/// Scheduling lane for a ready mailbox (M2 priority scheduling).
///
/// `High` is drained strictly before `Normal`: workers pop from the high
/// lane first, giving O(1) priority jumps for system messages and
/// user messages declared with `MessagePriority >= 70`.
///
/// Within a lane, FIFO order is preserved (two `SegQueue`s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Default lane for normal-priority work (FIFO among Normal).
    Normal,
    /// Priority lane drained before Normal (FIFO among High).
    High,
}

/// A queue that holds references to mailboxes that have messages ready for processing.
///
/// The SchedulingQueue is a central component of the SharedThreadPool scheduler.
/// It stores Arc<dyn Mailbox> instances that have signaled they have messages
/// ready for processing. Worker threads pull mailboxes from this queue to process
/// their messages.
///
/// M2 priority scheduling: the queue internally maintains **two lanes**
/// (High/Normal), each a lock-free SegQueue. `try_pop` drains the High lane
/// first, so a mailbox containing high-priority messages O(1)-jumps over any
/// backlog of normal-priority mailboxes. Lane selection happens at push time
/// (see `push_with_lane`); plain `push` keeps the historical Normal-lane
/// behavior, which preserves FIFO semantics for all existing callers.
///
/// # Thread Safety
/// - Uses lock-free queues internally (SegQueue per lane)
/// - Safe for concurrent producers and consumers
/// - Uses Notify for efficient worker wakeup
///
/// # Performance Characteristics
/// - O(1) push and pop operations in both lanes
/// - Lock-free implementation for high throughput
/// - High-lane preemption is a single extra empty-queue check on pop
pub struct SchedulingQueue {
    /// Lock-free queue holding ready mailboxes (normal lane)
    normal: Arc<SegQueue<Arc<dyn Mailbox + Send + Sync>>>,

    /// Lock-free queue holding ready mailboxes (high-priority lane)
    high: Arc<SegQueue<Arc<dyn Mailbox + Send + Sync>>>,

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
            .field("normal_len", &self.normal.len())
            .field("high_len", &self.high.len())
            .finish()
    }
}

impl SchedulingQueue {
    /// Creates a new SchedulingQueue with the specified initial capacity
    pub fn new(max_capacity: usize) -> Self {
        Self {
            normal: Arc::new(SegQueue::new()),
            high: Arc::new(SegQueue::new()),
            notify: Arc::new(Notify::new()),
            max_capacity,
        }
    }

    /// Pushes a mailbox into the queue (Normal lane).
    ///
    /// If the queue was empty, this will notify any waiting workers.
    ///
    /// # Parameters
    /// * `mailbox` - Strong reference to a Mailbox that has messages ready to process
    pub fn push(&self, mailbox: Arc<dyn Mailbox + Send + Sync>) {
        self.push_with_lane(mailbox, Lane::Normal);
    }

    /// Pushes a mailbox into a specific lane.
    ///
    /// High-lane entries are popped before any Normal-lane entry. Use for
    /// system messages (Terminated / control) and user messages with
    /// `MessagePriority >= HIGH (70)`.
    pub fn push_with_lane(&self, mailbox: Arc<dyn Mailbox + Send + Sync>, lane: Lane) {
        match lane {
            Lane::Normal => self.normal.push(mailbox),
            Lane::High => self.high.push(mailbox),
        }
        // Notify one worker that a mailbox is available
        self.notify.notify_one();
    }

    /// Tries to pop a mailbox from the queue, High lane first.
    ///
    /// # Returns
    /// * `Some(mailbox)` - A mailbox that has messages ready to process
    /// * `None` - The queue is empty
    pub fn try_pop(&self) -> Option<Arc<dyn Mailbox + Send + Sync>> {
        // Priority drain: high lane strictly before normal lane.
        if let Some(mailbox) = self.high.pop() {
            return Some(mailbox);
        }
        self.normal.pop()
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

    /// Gets the number of mailboxes currently in the queue (both lanes).
    ///
    /// This is a snapshot and may change by the time the value is used.
    ///
    /// # Returns
    /// Current queue length
    pub fn len(&self) -> usize {
        self.normal.len() + self.high.len()
    }

    /// Current length of the high-priority lane.
    pub fn high_len(&self) -> usize {
        self.high.len()
    }

    /// Current length of the normal lane.
    pub fn normal_len(&self) -> usize {
        self.normal.len()
    }

    /// Checks if the queue is empty.
    ///
    /// This is a snapshot and may change by the time the value is used.
    ///
    /// # Returns
    /// true if the queue is empty, false otherwise
    pub fn is_empty(&self) -> bool {
        self.normal.is_empty() && self.high.is_empty()
    }

    /// Gets a clone of the notification mechanism for workers to wait on.
    ///
    /// # Returns
    /// Arc<Notify> that workers can await on
    pub fn notify_handle(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Gets a clone of the underlying normal-lane queue for direct access.
    ///
    /// # Returns
    /// Arc<SegQueue<Arc<dyn Mailbox>>>
    pub fn queue_handle(&self) -> Arc<SegQueue<Arc<dyn Mailbox + Send + Sync>>> {
        self.normal.clone()
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
        assert_eq!(queue.high_len(), 0);
        assert_eq!(queue.normal_len(), 0);
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

    // ------------------------------------------------------------------
    // M2 dual-lane scheduling
    // ------------------------------------------------------------------

    #[test]
    fn test_high_lane_jumps_over_normal_backlog() {
        let queue = SchedulingQueue::new(16);
        // Normal backlog: n1..n3
        for i in 1..=3 {
            queue.push(make_mailbox(&format!("q/n{}", i)));
        }
        // High arrival after the backlog exists
        queue.push_with_lane(make_mailbox("q/h1"), Lane::High);

        // High must be popped first despite arriving last.
        assert_eq!(queue.try_pop().unwrap().path().path, "q/h1");
        // Then the normal backlog in FIFO order.
        assert_eq!(queue.try_pop().unwrap().path().path, "q/n1");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/n2");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/n3");
        assert!(queue.is_empty());
    }

    #[test]
    fn test_high_lane_preserves_fifo_within_lane() {
        let queue = SchedulingQueue::new(16);
        queue.push_with_lane(make_mailbox("q/h2"), Lane::High);
        queue.push_with_lane(make_mailbox("q/h1"), Lane::High);
        queue.push(make_mailbox("q/n1"));

        assert_eq!(queue.try_pop().unwrap().path().path, "q/h2");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/h1");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/n1");
    }

    #[test]
    fn test_len_counts_both_lanes() {
        let queue = SchedulingQueue::new(16);
        queue.push(make_mailbox("q/n"));
        queue.push_with_lane(make_mailbox("q/h"), Lane::High);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.normal_len(), 1);
        assert_eq!(queue.high_len(), 1);
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

    #[tokio::test]
    async fn test_async_pop_prefers_high_lane() {
        let queue = Arc::new(SchedulingQueue::new(16));
        queue.push(make_mailbox("q/n0")); // normal backlog exists

        let got = queue.pop().await;
        // With no high entry, normal is popped.
        assert_eq!(got.path().path, "q/n0");

        queue.push_with_lane(make_mailbox("q/h0"), Lane::High);
        let got = queue.pop().await;
        assert_eq!(got.path().path, "q/h0");
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
