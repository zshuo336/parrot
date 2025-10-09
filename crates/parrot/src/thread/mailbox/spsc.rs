use async_trait::async_trait;
#[allow(unused_imports)] // 测试模块需要 ActorRef/BoxedActorRef/WeakActorTarget
use parrot_api::address::{ActorPath, ActorRef};
#[allow(unused_imports)] // 测试模块需要全部类型形状
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage, WeakActorTarget};
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{Mutex, Notify};

use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::mailbox::Mailbox;
use crate::thread::processor::ProcessorInterface;
use std::sync::Mutex as StdMutex;

/// A single-producer, single-consumer mailbox implementation using tokio channels.
///
/// This mailbox is optimized for the single-producer case, which is the common case
/// for dedicated actor mailboxes where only the scheduler sends messages.
/// It provides FIFO ordering guarantees.
pub struct SpscMailbox {
    /// The sending half of the channel
    sender: Sender<BoxedMessage>,
    /// The receiving half of the channel, wrapped in a Mutex for interior mutability
    receiver: Arc<Mutex<Receiver<BoxedMessage>>>,
    /// Path of the actor this mailbox belongs to
    path: ActorPath,
    /// Capacity of the mailbox
    capacity: usize,
    /// Notify mechanism to wake up processors when new messages arrive
    notify: Arc<Notify>,
    /// Flag indicating if this mailbox has been closed
    is_closed: Arc<AtomicBool>,
    closed_notify: Notify,
    /// Associated processor
    processor: StdMutex<Option<Arc<dyn ProcessorInterface>>>,
    /// Wake hook fired after a successful push (re-enqueue scheduling).
    wake_hook: StdMutex<Option<crate::thread::mailbox::WakeHook>>,
    /// Scheduling slot state (single-owner processing guard).
    schedule_state: crate::thread::mailbox::ScheduleState,
}

impl Debug for SpscMailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpscMailbox")
            .field("path", &self.path)
            .field("capacity", &self.capacity)
            .field("processor", &"processor")
            .finish()
    }
}

impl SpscMailbox {
    /// Creates a new SpscMailbox with the specified capacity and actor path.
    pub fn new(capacity: usize, path: ActorPath) -> Self {
        let capacity = capacity.max(1);
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        let notify = Arc::new(Notify::new());
        let processor = StdMutex::new(None);
        let wake_hook = StdMutex::new(None);

        Self {
            sender,
            receiver: Arc::new(Mutex::new(receiver)),
            path,
            capacity,
            notify,
            is_closed: Arc::new(AtomicBool::new(false)),
            closed_notify: Notify::new(),
            processor,
            wake_hook,
            schedule_state: crate::thread::mailbox::ScheduleState::default(),
        }
    }

    /// Low-level sender for adapters managing their own scheduler wake hooks.
    /// Actor delivery should use the `Mailbox` methods. Close also closes this
    /// channel; callers using raw reserved permits inherit Tokio's semantics.
    pub fn sender(&self) -> Sender<BoxedMessage> {
        self.sender.clone()
    }

    /// Returns a reference to the notify mechanism
    pub fn notify_ref(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Check if this mailbox is closed
    fn closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Mailbox for SpscMailbox {
    async fn push(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        use tokio::sync::mpsc::error::TrySendError;
        let closed = self.closed_notify.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if self.closed() {
            return Err(MailboxError::Closed);
        }
        if matches!(strategy, BackpressureStrategy::Block) {
            let permit = tokio::select! {
                biased;
                _ = closed => return Err(MailboxError::Closed),
                permit = self.sender.reserve() => permit.map_err(|_| MailboxError::Closed)?,
            };
            // Commit under the same receiver lock as close/drain. A pending
            // reservation must not publish after close has already returned.
            let receiver = self.receiver.lock().await;
            if self.closed() {
                return Err(MailboxError::Closed);
            }
            permit.send(msg);
            drop(receiver);
        } else {
            let mut evicted = None;
            let mut receiver = self.receiver.lock().await;
            if self.closed() {
                return Err(MailboxError::Closed);
            }
            let result = match self.sender.try_send(msg) {
                Err(TrySendError::Full(msg))
                    if matches!(strategy, BackpressureStrategy::DropOldest) =>
                {
                    evicted = receiver.try_recv().ok();
                    self.sender.try_send(msg)
                }
                other => other,
            };
            drop(receiver);
            // Evicted and rejected values may run arbitrary destructors.
            // Release the receiver before disposing either kind of value.
            drop(evicted);
            match result {
                Ok(()) => {}
                Err(TrySendError::Full(_))
                    if matches!(strategy, BackpressureStrategy::DropNewest) =>
                {
                    return Ok(());
                }
                Err(TrySendError::Full(_)) => {
                    return Err(MailboxError::Full {
                        capacity: self.capacity,
                    });
                }
                Err(TrySendError::Closed(_)) => return Err(MailboxError::Closed),
            }
        }
        self.notify.notify_one();
        self.fire_wake_hook();
        Ok(())
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        // Get exclusive access to the receiver
        let mut receiver = self.receiver.lock().await;

        // Try to receive a message without blocking: the `Mailbox` contract
        // requires `pop` to return `None` immediately when empty. Waiting for
        // new messages is the scheduling queue's responsibility (`Notify`).
        receiver.try_recv().ok()
    }

    async fn is_empty(&self) -> bool {
        self.receiver.lock().await.is_empty()
    }

    async fn signal_ready(&self) {
        // Notify any waiting consumers
        self.notify.notify_one();
    }

    fn path(&self) -> &ActorPath {
        &self.path
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    async fn len(&self) -> usize {
        self.receiver.lock().await.len()
    }

    fn seal(&self) {
        self.is_closed.store(true, Ordering::SeqCst);
        self.closed_notify.notify_waiters();
        self.notify.notify_waiters();
    }

    async fn close(&self) {
        self.seal();
        let drained = {
            let mut receiver = self.receiver.lock().await;
            receiver.close();
            let mut drained = Vec::new();
            while let Ok(message) = receiver.try_recv() {
                drained.push(message);
            }
            drained
        };
        drop(drained);
    }

    /// associate a processor with this mailbox
    fn set_processor(&self, processor: Arc<dyn ProcessorInterface>) {
        *self.processor.lock().unwrap() = Some(processor);
    }

    /// get the associated processor
    fn get_processor(&self) -> Option<Arc<dyn ProcessorInterface>> {
        self.processor.lock().unwrap().clone()
    }

    /// check if there is a processor associated with this mailbox
    fn has_processor(&self) -> bool {
        self.processor.lock().map(|p| p.is_some()).unwrap_or(false)
    }

    fn set_wake_hook(&self, hook: crate::thread::mailbox::WakeHook) {
        *self.wake_hook.lock().unwrap() = Some(hook);
    }

    fn fire_wake_hook(&self) {
        if let Some(hook) = self.wake_hook.lock().unwrap().as_ref() {
            hook();
        }
    }

    fn schedule_state(&self) -> &crate::thread::mailbox::ScheduleState {
        &self.schedule_state
    }

    async fn is_closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;
    use std::time::Duration;
    use tokio::test;

    /// Mock implementation of ActorRef for testing
    #[derive(Debug)]
    struct MockActorRef {
        path_value: String,
    }

    impl MockActorRef {
        fn new(path: &str) -> Self {
            Self {
                path_value: path.to_string(),
            }
        }
    }

    #[async_trait]
    impl ActorRef for MockActorRef {
        fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }

        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _timeout_duration: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }

        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move { Ok(()) })
        }

        fn path(&self) -> String {
            self.path_value.clone()
        }

        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async move { true })
        }

        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(Self {
                path_value: self.path_value.clone(),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    // Helper to create a test ActorPath
    fn create_test_actor_path(path_str: &str) -> ActorPath {
        // Create a mock actor reference for the target
        let mock_ref = MockActorRef::new(path_str);

        ActorPath {
            target: Arc::new(mock_ref) as WeakActorTarget,
            path: path_str.to_string(),
        }
    }

    #[test]
    async fn test_push_and_pop() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscMailbox::new(10, path);

        // Test basic push and pop functionality
        let msg = Box::new("Hello, World!") as BoxedMessage;
        mailbox
            .push(msg, BackpressureStrategy::Block)
            .await
            .unwrap();

        // Verify message count
        assert_eq!(mailbox.len().await, 1);

        // Pop the message
        let received = mailbox.pop().await;
        assert!(received.is_some());

        let str_msg = received.unwrap().downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "Hello, World!");

        // Verify mailbox is now empty
        assert!(mailbox.is_empty().await);
        assert_eq!(mailbox.len().await, 0);
    }

    #[test]
    async fn test_backpressure_drop_newest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscMailbox::new(1, path);

        // Fill the mailbox
        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox
            .push(msg1, BackpressureStrategy::Block)
            .await
            .unwrap();

        // Try to push when mailbox is full with DropNewest strategy
        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::DropNewest).await;

        // The push should succeed (message was dropped)
        assert!(result.is_ok());

        // Verify only one message is in the mailbox
        assert_eq!(mailbox.len().await, 1);

        // Pop the message and verify it's the first one
        let received = mailbox.pop().await.unwrap();
        let str_msg = received.downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "First message");
    }

    #[test]
    async fn test_backpressure_error() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscMailbox::new(1, path);

        // Fill the mailbox
        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox
            .push(msg1, BackpressureStrategy::Block)
            .await
            .unwrap();

        // Try to push when mailbox is full with Error strategy
        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::Error).await;

        // The push should fail with Full error
        assert!(matches!(result, Err(MailboxError::Full { .. })));
    }

    #[test]
    async fn test_backpressure_drop_oldest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscMailbox::new(1, path);

        // Fill the mailbox
        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox
            .push(msg1, BackpressureStrategy::Block)
            .await
            .unwrap();

        // Try to push when mailbox is full with DropOldest strategy
        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::DropOldest).await;

        // The push should succeed
        assert!(result.is_ok());

        // Pop the message and verify it's the second one
        let received = mailbox.pop().await.unwrap();
        let str_msg = received.downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "Second message");
    }
}

// Direct users of sender() bypass Mailbox's commit lock and wake hook.
// Mailbox API producers serialize eviction/publication with close and pop.
