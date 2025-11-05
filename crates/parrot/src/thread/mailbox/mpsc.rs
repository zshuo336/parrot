use async_trait::async_trait;
use flume::{Receiver, Sender};
use parrot_api::address::ActorPath;
use parrot_api::types::BoxedMessage;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// By default, the internal [`MailboxItem`] channel carries ask envelopes by
/// value; `pop` re-boxes them for compatibility. With `parrot_envelope_legacy`,
/// `push_ask` instead uses the boxed compatibility representation. Both paths
/// preserve the payload and original reply channel.
pub struct MpscMailbox {
    /// The sending half of the channel
    sender: Sender<MailboxItem>,
    /// The receiving half of the channel
    receiver: Mutex<Option<Receiver<MailboxItem>>>,
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
    /// `Mutex<VecDeque>` keeps the hot path (push/pop under contention) simple
    /// and correct; priority messages are rare (system/death/control), so the
    /// lock is effectively uncontended in practice.
    high_lane: std::sync::Mutex<std::collections::VecDeque<(u64, MailboxItem)>>,
    /// Normal-lane removals, used to compare arrival order across lanes.
    normal_removed: AtomicU64,
    /// Serializes enqueue/close and capacity decisions across both lanes.
    enqueue_gate: Mutex<()>,
    capacity_notify: Notify,
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
        let capacity = capacity.max(1);
        let (sender, receiver) = flume::bounded(capacity);
        let is_ready = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let is_closed = Arc::new(AtomicBool::new(false));
        let processor = Mutex::new(None);
        let wake_hook = Mutex::new(None);

        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
            path,
            capacity,
            is_ready,
            notify,
            is_closed,
            processor,
            wake_hook,
            schedule_state: crate::thread::mailbox::ScheduleState::default(),
            high_lane: std::sync::Mutex::new(std::collections::VecDeque::new()),
            normal_removed: AtomicU64::new(0),
            enqueue_gate: Mutex::new(()),
            capacity_notify: Notify::new(),
        }
    }

    /// Low-level channel access for adapters that manage their own scheduling.
    /// Direct sends bypass priority capacity accounting and scheduler wake hooks;
    /// actor delivery should use the `Mailbox` methods instead. Close disconnects
    /// these senders too.
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
    fn normal_len(&self) -> usize {
        self.receiver
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, Receiver::len)
    }

    fn pop_normal(&self) -> Option<MailboxItem> {
        let item = self.receiver.lock().unwrap().as_ref()?.try_recv().ok()?;
        self.normal_removed.fetch_add(1, Ordering::Relaxed);
        Some(item)
    }

    async fn enqueue(
        &self,
        mut item: MailboxItem,
        strategy: BackpressureStrategy,
        high_priority: bool,
    ) -> Result<(), MailboxError> {
        loop {
            // Keep removed values outside the lock scope: Drop is user code
            // and may inspect or send to this mailbox again.
            let mut evicted = None;
            // Register before checking capacity, so a concurrent pop/close
            // cannot be lost between observing Full and beginning the wait.
            let available = self.capacity_notify.notified();
            tokio::pin!(available);
            available.as_mut().enable();
            {
                let _gate = self.enqueue_gate.lock().unwrap();
                if self.closed() {
                    return Err(MailboxError::Closed);
                }
                let mut high = self.high_lane.lock().unwrap();
                let full = self.normal_len() + high.len() >= self.capacity;
                if full {
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
                            // Delivery priority must not redefine "oldest".
                            // A high entry records how many normal items had
                            // arrived before it; removals advance that frontier.
                            let removed = self.normal_removed.load(Ordering::Relaxed);
                            let high_is_oldest = high
                                .front()
                                .is_some_and(|(frontier, _)| *frontier == removed);
                            if high_is_oldest {
                                evicted = high.pop_front().map(|(_, item)| item);
                            } else {
                                evicted = self.pop_normal();
                            }
                        }
                        BackpressureStrategy::Block => {}
                    }
                }
                if !full || matches!(strategy, BackpressureStrategy::DropOldest) {
                    if high_priority {
                        let frontier = self
                            .normal_removed
                            .load(Ordering::Relaxed)
                            .wrapping_add(self.normal_len() as u64);
                        high.push_back((frontier, item));
                        drop(high);
                        drop(_gate);
                        self.signal_ready_and_wake();
                        return Ok(());
                    }
                    match self.sender.try_send(item) {
                        Ok(()) => {
                            drop(high);
                            drop(_gate);
                            self.signal_ready_and_wake();
                            return Ok(());
                        }
                        Err(flume::TrySendError::Disconnected(_)) => {
                            return Err(MailboxError::Closed);
                        }
                        Err(flume::TrySendError::Full(returned)) => {
                            item = returned;
                            if !matches!(strategy, BackpressureStrategy::Block) {
                                return Err(MailboxError::Full {
                                    capacity: self.capacity,
                                });
                            }
                        }
                    }
                }
            }
            drop(evicted);
            available.await;
        }
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
        let high = match &item {
            MailboxItem::Ask(envelope) => envelope.options.priority.value() >= 70,
            MailboxItem::Plain(message) => message
                .downcast_ref::<crate::thread::envelope::AskEnvelope>()
                .is_some_and(|envelope| envelope.options.priority.value() >= 70),
        };
        self.enqueue(item, strategy, high).await
    }

    /// Both priority lanes share one capacity and one close boundary.
    async fn push_with_priority(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
        high_priority: bool,
    ) -> Result<bool, MailboxError> {
        self.enqueue(MailboxItem::Plain(msg), strategy, high_priority)
            .await?;
        Ok(high_priority)
    }

    fn supports_high_priority(&self) -> bool {
        true
    }

    fn has_high_priority_messages(&self) -> bool {
        !self.high_lane.lock().unwrap().is_empty()
    }

    /// M5: pop the internal item (ask envelopes by value).
    async fn pop_item(&self) -> Option<MailboxItem> {
        let item = {
            let _gate = self.enqueue_gate.lock().unwrap();
            self.high_lane
                .lock()
                .unwrap()
                .pop_front()
                .map(|(_, item)| item)
                .or_else(|| self.pop_normal())
        };
        if item.is_some() {
            self.capacity_notify.notify_waiters();
        }
        item
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        self.pop_item().await.map(|item| item.into_boxed_message())
    }

    async fn is_empty(&self) -> bool {
        self.normal_len() == 0 && self.high_lane.lock().unwrap().is_empty()
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
        self.normal_len() + self.high_lane.lock().unwrap().len()
    }

    fn seal(&self) {
        self.is_closed.store(true, Ordering::SeqCst);
        self.capacity_notify.notify_waiters();
        self.notify.notify_waiters();
    }

    async fn close(&self) {
        self.seal();
        let (receiver, high) = {
            let _gate = self.enqueue_gate.lock().unwrap();
            let receiver = self.receiver.lock().unwrap().take();
            let high = std::mem::take(&mut *self.high_lane.lock().unwrap());
            (receiver, high)
        };
        // Flume keeps queued values in the shared channel even after the last
        // receiver disconnects. Explicitly take them so they are disposed by
        // this cleanup operation, rather than by a later sender/mailbox drop.
        let normal = receiver
            .as_ref()
            .map(|receiver| receiver.drain().collect::<Vec<_>>());
        drop(receiver);
        // User destructors must never run while holding mailbox bookkeeping locks.
        drop(normal);
        drop(high);
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

        // Discarding the message must report a rejected admission
        assert!(matches!(result, Err(MailboxError::Dropped { capacity: 1 })));

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
    async fn test_push_ask_roundtrip_preserves_feature_representation_and_reply() {
        use crate::thread::envelope::AskEnvelope;

        let path = create_test_actor_path("m5-actor");
        let mailbox = MpscMailbox::new(10, path);

        let (envelope, reply_rx) = AskEnvelope::new_inline(42u64);
        mailbox
            .push_ask(envelope, BackpressureStrategy::Block)
            .await
            .unwrap();

        // The feature intentionally changes the mailbox representation, not
        // the payload or the original reply channel. Check both contracts.
        let item = mailbox.pop_item().await.expect("item queued");
        assert_eq!(
            matches!(&item, MailboxItem::Plain(_)),
            cfg!(feature = "parrot_envelope_legacy"),
            "only legacy asks use the boxed compatibility representation"
        );
        let envelope = match item {
            MailboxItem::Ask(envelope) => envelope,
            MailboxItem::Plain(boxed) => *boxed
                .downcast::<AskEnvelope>()
                .expect("legacy item contains the original ask envelope"),
        };
        let (payload, reply) = envelope.into_parts_layered();
        assert_eq!(*payload.into_boxed().downcast::<u64>().unwrap(), 42);
        reply.send(Ok(Box::new("done") as BoxedMessage)).unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), reply_rx)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(*reply.downcast::<&str>().unwrap(), "done");
        assert!(mailbox.pop_item().await.is_none());
    }

    #[tokio::test]
    async fn test_pop_compat_preserves_ask_payload_and_reply() {
        use crate::thread::envelope::AskEnvelope;

        let path = create_test_actor_path("m5-compat");
        let mailbox = MpscMailbox::new(10, path);

        let (envelope, reply_rx) = AskEnvelope::new_typed("big".to_string());
        mailbox
            .push_ask(envelope, BackpressureStrategy::Block)
            .await
            .unwrap();

        // 兼容路径 pop：AskEnvelope 装箱还原（旧消费方仍可 downcast）
        let boxed = mailbox.pop().await.expect("message queued");
        let env = boxed
            .downcast::<AskEnvelope>()
            .expect("compat pop must re-box ask envelopes");
        let (payload, reply) = env.into_parts_layered();
        let boxed = payload.into_boxed();
        assert_eq!(*boxed.downcast::<String>().unwrap(), "big");
        reply.send(Ok(Box::new(3usize) as BoxedMessage)).unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), reply_rx)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(*reply.downcast::<usize>().unwrap(), 3);
        assert!(mailbox.pop().await.is_none());
    }
}
