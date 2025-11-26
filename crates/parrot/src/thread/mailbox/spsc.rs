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

use crate::owned_payload::notification;
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
    receiver: Mutex<Option<Receiver<BoxedMessage>>>,
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
    close_state: Arc<crate::thread::mailbox::CloseState>,
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
            receiver: Mutex::new(Some(receiver)),
            path,
            capacity,
            notify,
            is_closed: Arc::new(AtomicBool::new(false)),
            closed_notify: Notify::new(),
            processor,
            wake_hook,
            schedule_state: crate::thread::mailbox::ScheduleState::default(),
            close_state: Arc::new(crate::thread::mailbox::CloseState::new()),
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

impl Drop for SpscMailbox {
    fn drop(&mut self) {
        self.is_closed.store(true, Ordering::SeqCst);
        let receiver = self.receiver.get_mut().take();
        let mut drained = Vec::new();
        if let Some(mut receiver) = receiver {
            notification::call(|| receiver.close());
            loop {
                let (result, wakes) = receiver.try_recv_deferred();
                super::retire_on_drop(wakes, "capacity notification");
                let Ok(message) = result else { break };
                drained.push(message);
            }
            super::retire_on_drop(receiver, "receiver");
        }
        for message in drained {
            super::retire_on_drop(message, "message");
        }
        let processor = self
            .processor
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let hook = self
            .wake_hook
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        super::retire_on_drop(processor, "processor");
        super::retire_on_drop(hook, "wake hook");
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
        let mut msg = Some(super::Input::unbound(msg));
        let mut evicted = None;
        let mut replacement_wakes = None;
        let result = async {
            let closed = self.closed_notify.notified();
            tokio::pin!(closed);
            closed.as_mut().enable();
            if self.closed() {
                return Err(MailboxError::Closed);
            }
            if matches!(strategy, BackpressureStrategy::Block) {
                let permit = match self.sender.try_reserve() {
                    Ok(permit) => permit,
                    Err(TrySendError::Closed(())) => return Err(MailboxError::Closed),
                    Err(TrySendError::Full(())) => notification::observe(async {
                        tokio::select! {
                            biased;
                            _ = closed => Err(MailboxError::Closed),
                            permit = self.sender.reserve() => permit.map_err(|_| MailboxError::Closed),
                        }
                    }).await?,
                };
                // Commit under the same receiver lock as close/drain. A pending
                // reservation must not publish after close has already returned.
                let receiver = notification::lock(&self.receiver).await;
                if self.closed() {
                    return Err(MailboxError::Closed);
                }
                permit.send(msg.take().unwrap().take());
                drop(receiver);
            } else {
                let mut receiver = notification::lock(&self.receiver).await;
                if self.closed() {
                    return Err(MailboxError::Closed);
                }
                let result = match self.sender.try_send(msg.take().unwrap().take()) {
                    Err(TrySendError::Full(msg))
                        if matches!(strategy, BackpressureStrategy::DropOldest) =>
                    {
                        if let Some(receiver) = receiver.as_mut() {
                            let (value, wakes) = receiver.try_recv_deferred();
                            if let Ok(value) = value {
                                evicted = Some(super::Input::unbound(value));
                                match wakes.replace(msg) {
                                    Ok(wakes) => {
                                        replacement_wakes = Some(wakes);
                                        Ok(())
                                    }
                                    Err(tokio::sync::mpsc::error::SendError(msg)) => Err(TrySendError::Closed(msg)),
                                }
                            } else {
                                Err(TrySendError::Full(msg))
                            }
                        } else {
                            Err(TrySendError::Closed(msg))
                        }
                    }
                    other => other,
                };
                drop(receiver);
                // Evicted and rejected values may run arbitrary destructors.
                // Release the receiver before disposing either kind of value.
                match result {
                    Ok(()) => {}
                    Err(TrySendError::Full(returned))
                        if matches!(strategy, BackpressureStrategy::DropNewest) =>
                    {
                        msg = Some(super::Input::unbound(returned));
                        return Err(MailboxError::Dropped {
                            capacity: self.capacity,
                        });
                    }
                    Err(TrySendError::Full(returned)) => {
                        msg = Some(super::Input::unbound(returned));
                        return Err(MailboxError::Full {
                            capacity: self.capacity,
                        });
                    }
                    Err(TrySendError::Closed(returned)) => {
                        msg = Some(super::Input::unbound(returned));
                        return Err(MailboxError::Closed);
                    }
                }
            }
            Ok(())
        }
        .await;
        super::retire_on_drop(replacement_wakes, "replacement notification");
        if result.is_ok() {
            notification::one(&self.notify);
            self.fire_wake_hook();
        }
        super::finish_push(result, msg, evicted).await
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        // Get exclusive access to the receiver
        let (result, wakes) = {
            let mut receiver = notification::lock(&self.receiver).await;
            let receiver = receiver.as_mut()?;
            receiver.try_recv_deferred()
        };
        super::retire_on_drop(wakes, "capacity notification");
        result.ok()
    }

    async fn is_empty(&self) -> bool {
        notification::lock(&self.receiver)
            .await
            .as_ref()
            .is_none_or(Receiver::is_empty)
    }

    async fn signal_ready(&self) {
        // Notify any waiting consumers
        notification::one(&self.notify);
    }

    fn path(&self) -> &ActorPath {
        &self.path
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    async fn len(&self) -> usize {
        notification::lock(&self.receiver)
            .await
            .as_ref()
            .map_or(0, Receiver::len)
    }

    fn seal(&self) {
        self.is_closed.store(true, Ordering::SeqCst);
        notification::all(&self.closed_notify);
        notification::all(&self.notify);
    }

    async fn close(&self) {
        notification::observe(async {
            self.seal();
            let mut reservation = None;
            let drained = loop {
                let attempt = {
                    let mut receiver = notification::lock(&self.receiver).await;
                    match self.close_state.claim(
                        receiver
                            .as_ref()
                            .is_some_and(|receiver| !receiver.is_empty()),
                        reservation.is_some(),
                    ) {
                        super::CloseAttempt::Owned(()) => {
                            super::CloseAttempt::Owned(receiver.take())
                        }
                        super::CloseAttempt::Reservation => super::CloseAttempt::Reservation,
                        super::CloseAttempt::Observing => super::CloseAttempt::Observing,
                    }
                };
                match attempt {
                    super::CloseAttempt::Owned(values) => break Some(values),
                    super::CloseAttempt::Observing => break None,
                    super::CloseAttempt::Reservation => {
                        reservation = self.close_state.reserve().await;
                        if reservation.is_none() {
                            break None;
                        }
                    }
                }
            };
            if let Some(receiver) = drained {
                let mut values = Vec::new();
                if let Some(mut receiver) = receiver {
                    notification::call(|| receiver.close());
                    loop {
                        let (result, wakes) = receiver.try_recv_deferred();
                        super::retire_on_drop(wakes, "capacity notification");
                        let Ok(message) = result else { break };
                        values.push(message);
                    }
                    super::retire_on_drop(receiver, "receiver");
                }
                self.close_state.start_drain(values, reservation);
            } else {
                drop(reservation);
            }
            self.close_state.wait().await;
        })
        .await;
    }

    /// associate a processor with this mailbox
    fn set_processor(&self, processor: Arc<dyn ProcessorInterface>) {
        let old = self.processor.lock().unwrap().replace(processor);
        drop(old);
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
        let old = self.wake_hook.lock().unwrap().replace(hook);
        drop(old);
    }

    fn fire_wake_hook(&self) {
        let hook = self.wake_hook.lock().unwrap().clone();
        super::invoke_wake_hook(hook);
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
    use std::sync::atomic::AtomicUsize;
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

        // The discarded message must report a rejected admission
        assert!(matches!(result, Err(MailboxError::Dropped { capacity: 1 })));

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

    struct LockedInput(Arc<AtomicUsize>);
    impl Drop for LockedInput {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("spsc lock-wait input");
        }
    }

    #[tokio::test]
    async fn cancelling_wait_for_receiver_lock_retires_input_after_unlock() {
        let mailbox = SpscMailbox::new(2, create_test_actor_path("lock-wait"));
        let drops = Arc::new(AtomicUsize::new(0));
        let _receiver = mailbox.receiver.lock().await;
        let mut pending = Box::pin(mailbox.push(
            Box::new(LockedInput(drops.clone())),
            BackpressureStrategy::Block,
        ));
        assert!(futures::poll!(&mut pending).is_pending());
        drop(pending);
        tokio::time::timeout(Duration::from_secs(2), async {
            while drops.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled lock waiter must retire its input");
        drop(_receiver);
        mailbox
            .push(Box::new(91_u64), BackpressureStrategy::Error)
            .await
            .unwrap();
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 91);
    }

    #[tokio::test]
    async fn cancelled_close_waiting_for_receiver_lock_can_be_retried() {
        struct CountDrop(Arc<AtomicUsize>);
        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mailbox = SpscMailbox::new(2, create_test_actor_path("close-lock-wait"));
        let drops = Arc::new(AtomicUsize::new(0));
        mailbox
            .push(
                Box::new(CountDrop(drops.clone())),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
        let receiver = mailbox.receiver.lock().await;
        for _ in 0..3 {
            let mut close = Box::pin(mailbox.close());
            assert!(futures::poll!(&mut close).is_pending());
            drop(close);
            assert!(mailbox.is_closed().await);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }
        let mut retry = Box::pin(mailbox.close());
        assert!(futures::poll!(&mut retry).is_pending());
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(2), retry)
            .await
            .expect("cancelled lock waiter must not strand close ownership");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(mailbox.len().await, 0);
        tokio::time::timeout(Duration::from_secs(2), mailbox.close())
            .await
            .expect("completed close must remain repeatable");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

// Direct users of sender() bypass Mailbox's commit lock and wake hook.
// Mailbox API producers serialize eviction/publication with close and pop.
