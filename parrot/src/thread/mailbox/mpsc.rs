use async_trait::async_trait;
use flume::{Receiver, Sender};
use parrot_api::address::ActorPath;
use parrot_api::types::BoxedMessage;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::mailbox::Mailbox;
use crate::thread::mailbox::MailboxItem;
use crate::thread::processor::ProcessorInterface;

/// A multi-producer, single-consumer mailbox implementation using flume.
///
/// This mailbox allows multiple senders to send messages to a single consumer,
/// which is typically an actor. It provides FIFO ordering guarantees.
///
/// M5: the internal channel carries [`MailboxItem`] values — ask envelopes
/// flow **by value** (no envelope boxing); `pop` re-boxes for compat and
/// `pop_item` is the single-block consumer path.
pub struct MpscMailbox {
    /// The sending half of the channel
    sender: Sender<MailboxItem>,
    /// The receiving half of the channel
    receiver: Receiver<MailboxItem>,
    /// Path of the actor this mailbox belongs to
    path: ActorPath,
    /// Capacity of the mailbox
    capacity: usize,
    /// Flag to indicate if the mailbox has messages and is ready for processing
    is_ready: Arc<AtomicBool>,
    /// Notify mechanism to wake up processors when new messages arrive
    notify: Arc<Notify>,
    /// Flag indicating if this mailbox has been closed
    is_closed: Arc<AtomicBool>,
    /// Associated processor (interior mutability so `&self` suffices)
    processor: Mutex<Option<Arc<dyn ProcessorInterface>>>,
    /// Wake hook fired after a successful push (re-enqueue scheduling).
    wake_hook: Mutex<Option<crate::thread::mailbox::WakeHook>>,
    /// Scheduling slot state (single-owner processing guard).
    schedule_state: crate::thread::mailbox::ScheduleState,
    /// M2: high-priority lane (drained before the normal flume queue).
    ///
    /// Mutex<VecDeque> keeps the hot path (push/pop under contention) simple
    /// and correct; priority messages are rare (system/death/control), so the
    /// lock is effectively uncontended in practice.
    high_lane: std::sync::Mutex<std::collections::VecDeque<MailboxItem>>,
    /// M2: capacity already reserved from the normal queue by high-lane
    /// messages (shared capacity accounting across both lanes).
    high_reserved: AtomicUsize,
}

impl Debug for MpscMailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpscMailbox")
            .field("path", &self.path)
            .field("capacity", &self.capacity)
            .field("is_ready", &self.is_ready)
            .field("is_closed", &self.is_closed)
            .field(
                "processor",
                &self.processor.lock().map(|p| p.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl MpscMailbox {
    /// Creates a new MpscMailbox with the specified capacity and actor path.
    pub fn new(capacity: usize, path: ActorPath) -> Self {
        let (sender, receiver) = flume::bounded(capacity);
        let is_ready = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let is_closed = Arc::new(AtomicBool::new(false));
        let processor = Mutex::new(None);
        let wake_hook = Mutex::new(None);

        Self {
            sender,
            receiver,
            path,
            capacity,
            is_ready,
            notify,
            is_closed,
            processor,
            wake_hook,
            schedule_state: crate::thread::mailbox::ScheduleState::default(),
            high_lane: std::sync::Mutex::new(std::collections::VecDeque::new()),
            high_reserved: AtomicUsize::new(0),
        }
    }

    /// Creates a clone of the sender that can be used to send messages to this mailbox.
    pub fn sender(&self) -> Sender<MailboxItem> {
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

    /// Signal work availability + wake hook (shared by all push paths).
    fn signal_ready_and_wake(&self) {
        self.is_ready.store(true, Ordering::SeqCst);
        self.notify.notify_one();
        self.fire_wake_hook();
    }
}

#[async_trait]
impl Mailbox for MpscMailbox {
    async fn push(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        self.push_item(MailboxItem::Plain(msg), strategy).await
    }

    /// M5 single-block path: push an ask envelope **by value**.
    ///
    /// flume's bounded ring buffer stores the item inline — zero envelope
    /// boxing on this path.
    ///
    /// `parrot_envelope_legacy` feature：回退历史双装箱路径（信封先
    /// `Box::new` 再 `push`），供 M5 灰度期回退（首个稳定版移除）。
    #[cfg(not(feature = "parrot_envelope_legacy"))]
    async fn push_ask(
        &self,
        envelope: crate::thread::envelope::AskEnvelope,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        self.push_item(MailboxItem::Ask(envelope), strategy).await
    }

    /// Legacy escape hatch（见上）。
    #[cfg(feature = "parrot_envelope_legacy")]
    async fn push_ask(
        &self,
        envelope: crate::thread::envelope::AskEnvelope,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        self.push_item(
            MailboxItem::Plain(Box::new(envelope) as BoxedMessage),
            strategy,
        )
        .await
    }

    async fn push_item(
        &self,
        item: MailboxItem,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        // Check if mailbox is already closed
        if self.closed() {
            return Err(MailboxError::Closed);
        }

        match strategy {
            BackpressureStrategy::DropNewest => {
                // Try to send without waiting. If the mailbox is full, drop the message.
                match self.sender.try_send(item) {
                    Ok(_) => {
                        self.signal_ready_and_wake();
                        Ok(())
                    }
                    Err(flume::TrySendError::Full(_)) => {
                        // Mailbox is full, drop the message as per strategy
                        Ok(())
                    }
                    Err(flume::TrySendError::Disconnected(_)) => Err(MailboxError::Closed),
                }
            }
            BackpressureStrategy::Block => {
                // Block until the message can be sent
                match self.sender.send_async(item).await {
                    Ok(_) => {
                        self.signal_ready_and_wake();
                        Ok(())
                    }
                    Err(_) => Err(MailboxError::Closed),
                }
            }
            BackpressureStrategy::Error => {
                // Try to send without waiting. If the mailbox is full, return an error.
                match self.sender.try_send(item) {
                    Ok(_) => {
                        self.signal_ready_and_wake();
                        Ok(())
                    }
                    Err(flume::TrySendError::Full(_)) => Err(MailboxError::Full {
                        capacity: self.capacity,
                    }),
                    Err(flume::TrySendError::Disconnected(_)) => Err(MailboxError::Closed),
                }
            }
            BackpressureStrategy::DropOldest => {
                // If the mailbox is full, try to pop the oldest message first
                if self.len().await >= self.capacity {
                    // Try to remove an item from the queue
                    let _ = self.receiver.try_recv();
                }

                // Then try to send the new message
                match self.sender.try_send(item) {
                    Ok(_) => {
                        self.signal_ready_and_wake();
                        Ok(())
                    }
                    Err(flume::TrySendError::Full(_)) => {
                        // This shouldn't happen as we just made room, but handle just in case
                        Err(MailboxError::PushError(
                            "Failed to push message after dropping oldest".to_string(),
                        ))
                    }
                    Err(flume::TrySendError::Disconnected(_)) => Err(MailboxError::Closed),
                }
            }
        }
    }

    /// M2: push with explicit priority lane selection.
    ///
    /// High-priority messages are buffered in the high lane which `pop`
    /// drains first. Capacity accounting spans both lanes:
    /// `flume.len() + high_lane.len() <= capacity`.
    ///
    /// Strategies:
    /// - Block: wait (bounded poll) until capacity is available — system and
    ///   death messages MUST NOT drop
    /// - Error: fail fast when full
    /// - DropNewest: drop the message when full (caller opted into lossy)
    /// - DropOldest: drop the oldest message when full
    ///
    /// Returns `true` when the message entered the high lane.
    async fn push_with_priority(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
        high_priority: bool,
    ) -> Result<bool, MailboxError> {
        if !high_priority {
            self.push(msg, strategy).await?;
            return Ok(false);
        }

        if self.closed() {
            return Err(MailboxError::Closed);
        }

        // Shared-capacity check across both lanes.
        let total_len = self.len().await;

        match strategy {
            BackpressureStrategy::Block => {
                // High-priority messages must not be lost. Poll with short
                // sleeps until a consumer frees capacity. High-lane traffic
                // is rare (system/death/control), so this is effectively
                // uncontended; the bound (capacity × 10ms) prevents an
                // unbreakable stall when the actor is wedged.
                let mut waited = 0u64;
                loop {
                    if self.closed() {
                        return Err(MailboxError::Closed);
                    }
                    if self.len().await < self.capacity {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    waited += 1;
                    if waited > 10_000 {
                        // ~10s bound: treat as a wedged consumer.
                        return Err(MailboxError::Full {
                            capacity: self.capacity,
                        });
                    }
                }
                self.high_reserved.fetch_add(1, Ordering::SeqCst);
                self.high_lane
                    .lock()
                    .unwrap()
                    .push_back(MailboxItem::Plain(msg));
            }
            BackpressureStrategy::Error => {
                if total_len >= self.capacity {
                    return Err(MailboxError::Full {
                        capacity: self.capacity,
                    });
                }
                self.high_reserved.fetch_add(1, Ordering::SeqCst);
                self.high_lane
                    .lock()
                    .unwrap()
                    .push_back(MailboxItem::Plain(msg));
            }
            BackpressureStrategy::DropNewest => {
                if total_len >= self.capacity {
                    return Ok(true); // dropped by caller's explicit choice
                }
                self.high_reserved.fetch_add(1, Ordering::SeqCst);
                self.high_lane
                    .lock()
                    .unwrap()
                    .push_back(MailboxItem::Plain(msg));
            }
            BackpressureStrategy::DropOldest => {
                if total_len >= self.capacity {
                    // Drop the oldest message (high lane first — it was the
                    // oldest entry by arrival order at the front).
                    let _ = self.pop().await;
                }
                self.high_reserved.fetch_add(1, Ordering::SeqCst);
                self.high_lane
                    .lock()
                    .unwrap()
                    .push_back(MailboxItem::Plain(msg));
            }
        }

        self.signal_ready_and_wake();

        Ok(true)
    }

    fn has_high_priority_messages(&self) -> bool {
        !self.high_lane.lock().unwrap().is_empty()
    }

    /// M5: pop the internal item (ask envelopes by value).
    async fn pop_item(&self) -> Option<MailboxItem> {
        // M2: drain the high-priority lane first (O(1) jump over the
        // normal backlog within this mailbox). Lock guard is dropped
        // before any await point.
        let high_msg = {
            let mut lane = self.high_lane.lock().unwrap();
            lane.pop_front()
        };
        if let Some(item) = high_msg {
            self.high_reserved.fetch_sub(1, Ordering::SeqCst);
            // If there are more messages, set ready flag again.
            // Optimistic: flume len is atomic; high lane uses try_lock
            // (uncontended in practice — only this actor's pushes touch it).
            let more = !self.receiver.is_empty()
                || self
                    .high_lane
                    .try_lock()
                    .map(|l| !l.is_empty())
                    .unwrap_or(true);
            if more {
                self.is_ready.store(true, Ordering::SeqCst);
                self.notify.notify_one();
            }
            return Some(item);
        }

        // Reset ready flag before attempting to receive
        self.is_ready.store(false, Ordering::SeqCst);

        // Try to receive a message without blocking: the `Mailbox` contract
        // requires `pop` to return `None` immediately when empty. Waiting for
        // new messages is the scheduling queue's responsibility (`Notify`).
        match self.receiver.try_recv() {
            Ok(item) => {
                // If there are more messages, set ready flag again.
                let more = !self.receiver.is_empty()
                    || self
                        .high_lane
                        .try_lock()
                        .map(|l| !l.is_empty())
                        .unwrap_or(true);
                if more {
                    self.is_ready.store(true, Ordering::SeqCst);
                    self.notify.notify_one();
                }
                Some(item)
            }
            Err(_) => None, // Channel is empty or disconnected
        }
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        self.pop_item().await.map(|item| item.into_boxed_message())
    }

    async fn is_empty(&self) -> bool {
        self.receiver.is_empty() && self.high_lane.lock().unwrap().is_empty()
    }

    async fn signal_ready(&self) {
        // Set the ready flag and notify any waiting processors
        self.is_ready.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn path(&self) -> &ActorPath {
        &self.path
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    async fn len(&self) -> usize {
        self.receiver.len() + self.high_lane.lock().unwrap().len()
    }

    async fn close(&self) {
        // Mark the mailbox as closed
        self.is_closed.store(true, Ordering::SeqCst);

        // Drop our sender so the channel disconnects once other senders are gone
        drop(self.sender.clone());

        // Drain any remaining messages to ensure proper cleanup
        while self.receiver.try_recv().is_ok() {
            // Nothing to do, just drain
        }
        // M2: drain the high-priority lane as well
        self.high_lane.lock().unwrap().clear();
        self.high_reserved.store(0, Ordering::SeqCst);

        // Notify anyone waiting on this mailbox that it's now closed
        self.notify.notify_waiters();
    }

    async fn is_closed(&self) -> bool {
        self.closed()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::address::{ActorPath, ActorRef};
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
            Box::pin(async { Ok(()) })
        }

        fn path(&self) -> String {
            self.path_value.clone()
        }

        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
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

    #[tokio::test]
    async fn test_push_and_pop() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(10, path);

        // Push a message
        let message: BoxedMessage = Box::new("test message");
        mailbox
            .push(message, BackpressureStrategy::Block)
            .await
            .unwrap();

        // Pop the message
        let received = mailbox.pop().await;
        assert!(received.is_some());

        if let Some(msg) = received {
            let msg_str = msg.downcast::<&str>().unwrap();
            assert_eq!(*msg_str, "test message");
        }
    }

    #[tokio::test]
    async fn test_backpressure_drop_newest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(1, path);

        // Fill the mailbox
        let message1: BoxedMessage = Box::new("message 1");
        mailbox
            .push(message1, BackpressureStrategy::Block)
            .await
            .unwrap();

        // Try to push with drop strategy
        let message2: BoxedMessage = Box::new("message 2");
        let result = mailbox
            .push(message2, BackpressureStrategy::DropNewest)
            .await;

        // Should succeed but the message is dropped
        assert!(result.is_ok());

        // Pop should only return the first message
        let received1 = mailbox.pop().await;
        assert!(received1.is_some());

        // Mailbox should be empty now
        let received2 = mailbox.pop().await;
        // flume recv_async on empty channel waits forever; use is_empty instead
        assert!(mailbox.is_empty().await);
        assert!(received2.is_none() || !mailbox.is_empty().await);
    }

    #[tokio::test]
    async fn test_backpressure_drop_oldest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(1, path);

        // Fill the mailbox with first message
        let message1: BoxedMessage = Box::new("message 1");
        mailbox
            .push(message1, BackpressureStrategy::Block)
            .await
            .unwrap();

        // Try to push with DropOldest strategy
        let message2: BoxedMessage = Box::new("message 2");
        let result = mailbox
            .push(message2, BackpressureStrategy::DropOldest)
            .await;

        // Should succeed
        assert!(result.is_ok());

        // Pop should return the second message, as the first was dropped
        let received = mailbox.pop().await;
        assert!(received.is_some());

        if let Some(msg) = received {
            let msg_str = msg.downcast::<&str>().unwrap();
            assert_eq!(*msg_str, "message 2");
        }
    }

    #[tokio::test]
    async fn test_backpressure_error() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(1, path);

        // Fill the mailbox
        let message1: BoxedMessage = Box::new("message 1");
        mailbox
            .push(message1, BackpressureStrategy::Block)
            .await
            .unwrap();

        // Try to push with error strategy
        let message2: BoxedMessage = Box::new("message 2");
        let result = mailbox.push(message2, BackpressureStrategy::Error).await;

        // Should fail with Full error
        assert!(matches!(result, Err(MailboxError::Full { .. })));
    }

    #[tokio::test]
    async fn test_signal_ready() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(10, path);

        // Check that it's initially not ready
        assert!(!mailbox.is_ready.load(Ordering::SeqCst));

        // Signal ready
        mailbox.signal_ready().await;

        // Verify the flag is set
        assert!(mailbox.is_ready.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_close() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(10, path);

        // Push some messages
        for i in 0..5 {
            let msg = Box::new(format!("Message {}", i)) as BoxedMessage;
            mailbox
                .push(msg, BackpressureStrategy::Block)
                .await
                .unwrap();
        }

        // Close the mailbox
        mailbox.close().await;

        // Verify the mailbox is closed
        assert!(mailbox.is_closed().await);

        // Try to push to a closed mailbox
        let msg = Box::new("This should fail") as BoxedMessage;
        let result = mailbox.push(msg, BackpressureStrategy::Block).await;
        assert!(matches!(result, Err(MailboxError::Closed)));
    }

    #[tokio::test]
    async fn test_processor_association() {
        use crate::thread::actor::ThreadActor;
        use crate::thread::config::ThreadActorConfig;
        use crate::thread::context::ThreadContext;
        use crate::thread::processor::ActorProcessor;
        use parrot_api::actor::{Actor, ActorState, EmptyConfig};

        #[derive(Debug)]
        struct TestActor;

        impl Actor for TestActor {
            type Config = EmptyConfig;
            type Context = ThreadContext<Self>;

            fn init<'a>(
                &'a mut self,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }

            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(msg) })
            }

            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(10, path);

        assert!(!mailbox.has_processor());
        assert!(mailbox.get_processor().is_none());

        let context = ThreadContext::new_for_test("test-actor");
        let processor = Arc::new(ActorProcessor::<TestActor>::new(
            ThreadActor::new_for_test(TestActor),
            context,
            "test-actor".to_string(),
            ThreadActorConfig::default(),
        ));

        mailbox.set_processor(processor);
        assert!(mailbox.has_processor());
        assert!(mailbox.get_processor().is_some());
    }

    // ============ M5 单块信封路径 ============

    #[tokio::test]
    async fn test_push_ask_envelope_by_value_roundtrip() {
        use crate::thread::envelope::AskEnvelope;

        let path = create_test_actor_path("m5-actor");
        let mailbox = MpscMailbox::new(10, path);

        // inline ask：载荷零独立分配，信封按值入队
        let (envelope, reply_rx) = AskEnvelope::new_inline(42u64);
        mailbox
            .push_ask(envelope, BackpressureStrategy::Block)
            .await
            .unwrap();

        // pop_item：按值取出（零装箱）
        let item = mailbox.pop_item().await.expect("item queued");
        match item {
            MailboxItem::Ask(env) => {
                let (payload, reply) = env.into_parts_layered();
                let boxed = payload.into_boxed();
                assert_eq!(*boxed.downcast::<u64>().unwrap(), 42);
                let _ = reply.send(Ok(Box::new("done") as BoxedMessage));
            }
            other => panic!("expected Ask item, got {:?}", other.into_boxed_message()),
        }

        let reply = reply_rx.await.unwrap().unwrap();
        assert_eq!(*reply.downcast::<&str>().unwrap(), "done");
    }

    #[tokio::test]
    async fn test_pop_compat_reboxes_ask_envelope() {
        use crate::thread::envelope::AskEnvelope;

        let path = create_test_actor_path("m5-compat");
        let mailbox = MpscMailbox::new(10, path);

        let (envelope, _rx) = AskEnvelope::new_typed("big".to_string());
        mailbox
            .push_ask(envelope, BackpressureStrategy::Block)
            .await
            .unwrap();

        // 兼容路径 pop：AskEnvelope 装箱还原（旧消费方仍可 downcast）
        let boxed = mailbox.pop().await.expect("message queued");
        let env = boxed
            .downcast::<AskEnvelope>()
            .expect("compat pop must re-box ask envelopes");
        let (payload, _reply) = env.into_parts_layered();
        let boxed = payload.into_boxed();
        assert_eq!(*boxed.downcast::<String>().unwrap(), "big");
    }
}
