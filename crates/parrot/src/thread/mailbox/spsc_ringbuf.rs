use async_trait::async_trait;
use parrot_api::address::ActorPath;
use parrot_api::types::BoxedMessage;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Notify};

use crate::owned_payload::notification;
use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::mailbox::Mailbox;
use crate::thread::processor::ProcessorInterface;

/// A single-producer, single-consumer mailbox implementation using a ring buffer.
///
/// Optimized for latency in the dedicated-thread scenario. `push` and `pop`
/// take async locks over the producer/consumer halves because the `Mailbox`
/// trait only hands out `&self`.
pub struct SpscRingbufMailbox {
    /// Producer half
    producer: Mutex<HeapProd>,
    /// Consumer half
    consumer: Mutex<HeapCons>,
    /// Path of the actor this mailbox belongs to
    path: ActorPath,
    /// Capacity of the mailbox
    capacity: usize,
    /// Notify mechanism to wake up processors when new messages arrive
    notify: Arc<Notify>,
    /// Wakes producers when a pop or close changes available capacity.
    capacity_notify: Notify,
    /// Flag indicating if this mailbox has been closed
    is_closed: Arc<AtomicBool>,
    /// Associated processor (interior mutability)
    processor: StdMutex<Option<Arc<dyn ProcessorInterface>>>,
    /// Wake hook fired after a successful push (re-enqueue scheduling).
    wake_hook: StdMutex<Option<crate::thread::mailbox::WakeHook>>,
    /// Scheduling slot state (single-owner processing guard).
    schedule_state: crate::thread::mailbox::ScheduleState,
    close_state: Arc<crate::thread::mailbox::CloseState>,
}

type HeapProd = <HeapRb<BoxedMessage> as Split>::Prod;
type HeapCons = <HeapRb<BoxedMessage> as Split>::Cons;

impl Debug for SpscRingbufMailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpscRingbufMailbox")
            .field("path", &self.path)
            .field("capacity", &self.capacity)
            .field("is_closed", &self.is_closed.load(Ordering::SeqCst))
            .finish()
    }
}

impl SpscRingbufMailbox {
    /// Create a new SPSC ring buffer mailbox.
    pub fn new(capacity: usize, path: ActorPath) -> Self {
        let capacity = capacity.max(1);
        let rb = HeapRb::<BoxedMessage>::new(capacity);
        let (prod, cons) = rb.split();

        Self {
            producer: Mutex::new(prod),
            consumer: Mutex::new(cons),
            path,
            capacity,
            notify: Arc::new(Notify::new()),
            capacity_notify: Notify::new(),
            is_closed: Arc::new(AtomicBool::new(false)),
            processor: StdMutex::new(None),
            wake_hook: StdMutex::new(None),
            schedule_state: crate::thread::mailbox::ScheduleState::default(),
            close_state: Arc::new(crate::thread::mailbox::CloseState::new()),
        }
    }

    /// Returns a reference to the notify mechanism
    pub fn notify_ref(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Check if this mailbox is closed
    fn closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }

    fn notify_ready(&self) {
        notification::one(&self.notify);
        self.fire_wake_hook();
    }
}

impl Drop for SpscRingbufMailbox {
    fn drop(&mut self) {
        self.is_closed.store(true, Ordering::SeqCst);
        let consumer = self.consumer.get_mut();
        let mut drained = Vec::new();
        while let Some(message) = consumer.try_pop() {
            drained.push(message);
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
impl Mailbox for SpscRingbufMailbox {
    async fn push(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        let mut msg = Some(super::Input::unbound(msg));
        let mut evicted = None;
        let result = async {
            loop {
                let available = self.capacity_notify.notified();
                tokio::pin!(available);
                available.as_mut().enable();
                if self.closed() {
                    return Err(MailboxError::Closed);
                }
                let mut producer = notification::lock(&self.producer).await;
                // Close also owns this lock, so no producer can publish after drain.
                if self.closed() {
                    return Err(MailboxError::Closed);
                }
                if producer.is_full() {
                    match strategy {
                        BackpressureStrategy::DropNewest => {
                            return Err(MailboxError::Dropped {
                                capacity: self.capacity,
                            });
                        }
                        BackpressureStrategy::Error => {
                            return Err(MailboxError::Full {
                                capacity: self.capacity,
                            });
                        }
                        BackpressureStrategy::DropOldest => {
                            evicted = notification::lock(&self.consumer)
                                .await
                                .try_pop()
                                .map(super::Input::unbound);
                        }
                        BackpressureStrategy::Block => {
                            drop(producer);
                            notification::observe(available).await;
                            continue;
                        }
                    }
                }
                if let Err(returned) = producer.try_push(msg.take().unwrap().take()) {
                    msg = Some(super::Input::unbound(returned));
                    return Err(MailboxError::PushError("ring buffer push failed".into()));
                }
                drop(producer);
                return Ok(());
            }
        }
        .await;
        if result.is_ok() {
            self.notify_ready();
        }
        super::finish_push(result, msg, evicted).await
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        let item = notification::lock(&self.consumer).await.try_pop();
        if item.is_some() {
            notification::all(&self.capacity_notify);
        }
        item
    }

    async fn is_empty(&self) -> bool {
        notification::lock(&self.consumer).await.is_empty()
    }

    async fn signal_ready(&self) {
        self.notify_ready();
    }

    fn path(&self) -> &ActorPath {
        &self.path
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    async fn len(&self) -> usize {
        notification::lock(&self.consumer).await.occupied_len()
    }

    fn seal(&self) {
        self.is_closed.store(true, Ordering::SeqCst);
        notification::all(&self.capacity_notify);
        notification::all(&self.notify);
    }

    async fn close(&self) {
        notification::observe(async {
            self.seal();
            let mut reservation = None;
            let drained = loop {
                let attempt = {
                    let _producer = notification::lock(&self.producer).await;
                    let mut consumer = notification::lock(&self.consumer).await;
                    match self
                        .close_state
                        .claim(!consumer.is_empty(), reservation.is_some())
                    {
                        super::CloseAttempt::Owned(()) => {
                            let mut drained = Vec::new();
                            while let Some(message) = consumer.try_pop() {
                                drained.push(message);
                            }
                            super::CloseAttempt::Owned(drained)
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
            if let Some(drained) = drained {
                self.close_state.start_drain(drained, reservation);
            } else {
                drop(reservation);
            }
            self.close_state.wait().await;
        })
        .await;
    }

    async fn is_closed(&self) -> bool {
        self.closed()
    }

    fn set_processor(&self, processor: Arc<dyn ProcessorInterface>) {
        let old = self.processor.lock().unwrap().replace(processor);
        drop(old);
    }

    fn get_processor(&self) -> Option<Arc<dyn ProcessorInterface>> {
        self.processor.lock().unwrap().clone()
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::address::ActorRef;
    use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, WeakActorTarget};
    use std::any::Any;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

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

    fn create_test_actor_path(path_str: &str) -> ActorPath {
        let mock_ref = MockActorRef::new(path_str);
        ActorPath {
            target: Arc::new(mock_ref) as WeakActorTarget,
            path: path_str.to_string(),
        }
    }

    #[tokio::test]
    async fn test_push_and_pop() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscRingbufMailbox::new(10, path);

        let msg = Box::new("Hello, World!") as BoxedMessage;
        mailbox
            .push(msg, BackpressureStrategy::Block)
            .await
            .unwrap();

        assert_eq!(mailbox.len().await, 1);

        let received = mailbox.pop().await;
        assert!(received.is_some());

        let str_msg = received.unwrap().downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "Hello, World!");

        assert!(mailbox.is_empty().await);
        assert_eq!(mailbox.len().await, 0);
    }

    #[tokio::test]
    async fn test_backpressure_drop_newest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscRingbufMailbox::new(1, path);

        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox
            .push(msg1, BackpressureStrategy::Block)
            .await
            .unwrap();

        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::DropNewest).await;

        assert!(matches!(result, Err(MailboxError::Dropped { capacity: 1 })));
        assert_eq!(mailbox.len().await, 1);

        let received = mailbox.pop().await.unwrap();
        let str_msg = received.downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "First message");
    }

    #[tokio::test]
    async fn test_backpressure_error() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscRingbufMailbox::new(1, path);

        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox
            .push(msg1, BackpressureStrategy::Block)
            .await
            .unwrap();

        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::Error).await;

        assert!(matches!(result, Err(MailboxError::Full { .. })));
    }

    #[tokio::test]
    async fn test_backpressure_drop_oldest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscRingbufMailbox::new(1, path);

        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox
            .push(msg1, BackpressureStrategy::Block)
            .await
            .unwrap();

        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::DropOldest).await;

        assert!(result.is_ok());

        let received = mailbox.pop().await.unwrap();
        let str_msg = received.downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "Second message");
    }

    #[tokio::test]
    async fn test_close() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscRingbufMailbox::new(10, path);

        for i in 0..5 {
            let msg = Box::new(format!("Message {}", i)) as BoxedMessage;
            mailbox
                .push(msg, BackpressureStrategy::Block)
                .await
                .unwrap();
        }

        mailbox.close().await;

        assert!(mailbox.is_closed().await);

        let msg = Box::new("This should fail") as BoxedMessage;
        let result = mailbox.push(msg, BackpressureStrategy::Block).await;
        assert!(matches!(result, Err(MailboxError::Closed)));

        assert_eq!(mailbox.len().await, 0);
    }

    struct LockedInput(Arc<AtomicUsize>);
    impl Drop for LockedInput {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("ring lock-wait input");
        }
    }

    #[tokio::test]
    async fn cancelling_wait_for_producer_lock_retires_input_after_unlock() {
        let mailbox = SpscRingbufMailbox::new(2, create_test_actor_path("lock-wait"));
        let drops = Arc::new(AtomicUsize::new(0));
        let _producer = mailbox.producer.lock().await;
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
        drop(_producer);
        mailbox
            .push(Box::new(91_u64), BackpressureStrategy::Error)
            .await
            .unwrap();
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 91);
    }

    async fn retry_cancelled_close_lock_wait(producer: bool) {
        struct CountDrop(Arc<AtomicUsize>);
        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mailbox = SpscRingbufMailbox::new(2, create_test_actor_path("close-lock-wait"));
        let drops = Arc::new(AtomicUsize::new(0));
        mailbox
            .push(
                Box::new(CountDrop(drops.clone())),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
        let producer_guard = if producer {
            Some(mailbox.producer.lock().await)
        } else {
            None
        };
        let consumer_guard = if producer {
            None
        } else {
            Some(mailbox.consumer.lock().await)
        };
        for _ in 0..3 {
            let mut close = Box::pin(mailbox.close());
            assert!(futures::poll!(&mut close).is_pending());
            drop(close);
            assert!(mailbox.is_closed().await);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }
        let mut retry = Box::pin(mailbox.close());
        assert!(futures::poll!(&mut retry).is_pending());
        drop(consumer_guard);
        drop(producer_guard);
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

    #[tokio::test]
    async fn cancelled_close_waiting_for_producer_lock_can_be_retried() {
        retry_cancelled_close_lock_wait(true).await;
    }

    #[tokio::test]
    async fn cancelled_close_waiting_for_consumer_lock_can_be_retried() {
        retry_cancelled_close_lock_wait(false).await;
    }
}
