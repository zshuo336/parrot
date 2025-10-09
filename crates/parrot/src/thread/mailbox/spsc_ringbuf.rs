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

    /// Push helper: returns Err(()) when the ring rejected the item.
    fn try_push_msg(producer: &mut HeapProd, msg: BoxedMessage) -> Result<(), ()> {
        producer.try_push(msg).map_err(|_| ())
    }

    fn notify_ready(&self) {
        self.notify.notify_one();
        self.fire_wake_hook();
    }
}

#[async_trait]
impl Mailbox for SpscRingbufMailbox {
    async fn push(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        loop {
            let mut evicted = None;
            let available = self.capacity_notify.notified();
            tokio::pin!(available);
            available.as_mut().enable();
            if self.closed() {
                return Err(MailboxError::Closed);
            }
            let mut producer = self.producer.lock().await;
            // Close also owns this lock, so no producer can publish after drain.
            if self.closed() {
                return Err(MailboxError::Closed);
            }
            if producer.is_full() {
                match strategy {
                    BackpressureStrategy::DropNewest => return Ok(()),
                    BackpressureStrategy::Error => {
                        return Err(MailboxError::Full {
                            capacity: self.capacity,
                        });
                    }
                    BackpressureStrategy::DropOldest => {
                        evicted = self.consumer.lock().await.try_pop();
                    }
                    BackpressureStrategy::Block => {
                        drop(producer);
                        available.await;
                        continue;
                    }
                }
            }
            Self::try_push_msg(&mut producer, msg)
                .map_err(|()| MailboxError::PushError("ring buffer push failed".into()))?;
            drop(producer);
            drop(evicted);
            self.notify_ready();
            return Ok(());
        }
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        let item = self.consumer.lock().await.try_pop();
        if item.is_some() {
            self.capacity_notify.notify_waiters();
        }
        item
    }

    async fn is_empty(&self) -> bool {
        self.consumer.lock().await.is_empty()
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
        self.consumer.lock().await.occupied_len()
    }

    fn seal(&self) {
        self.is_closed.store(true, Ordering::SeqCst);
        self.capacity_notify.notify_waiters();
        self.notify.notify_waiters();
    }

    async fn close(&self) {
        self.seal();
        let drained = {
            let _producer = self.producer.lock().await;
            let mut consumer = self.consumer.lock().await;
            let mut drained = Vec::new();
            while let Some(message) = consumer.try_pop() {
                drained.push(message);
            }
            drained
        };
        drop(drained);
    }

    async fn is_closed(&self) -> bool {
        self.closed()
    }

    fn set_processor(&self, processor: Arc<dyn ProcessorInterface>) {
        *self.processor.lock().unwrap() = Some(processor);
    }

    fn get_processor(&self) -> Option<Arc<dyn ProcessorInterface>> {
        self.processor.lock().unwrap().clone()
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::address::ActorRef;
    use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, WeakActorTarget};
    use std::any::Any;
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

        assert!(result.is_ok());
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
}
