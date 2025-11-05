//! Admission before queueing, shared by every dynamic Thread delivery path.
mod managed;
use super::*;
use crate::thread::envelope::{AskEnvelope, AskPayload, ControlMessage};
pub(crate) use managed::ManagedAdmission;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Producer-side admission and normalization. Implementations must be short,
/// thread-safe and must not call actor methods or block on I/O. To account for
/// queued AND executing work, return an actor-understood value owning a permit;
/// the actor must retain it until processing ends. A rejection must be explicit.
/// Caller-owned memory and arbitrary user-code allocations are not bounded here.
pub trait MessageAdmission: Debug + Send + Sync + 'static {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError>;
}

#[derive(Debug)]
pub(crate) struct AdmissionMailbox {
    inner: Arc<dyn Mailbox>,
    policy: Arc<dyn MessageAdmission>,
}
impl AdmissionMailbox {
    pub(crate) fn new(inner: Arc<dyn Mailbox>, policy: Arc<dyn MessageAdmission>) -> Self {
        Self { inner, policy }
    }
    fn prepare_message(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
        if message.is::<crate::children::ChildTerminated>()
            || message.is::<crate::streams::StreamEvent>()
            || message.is::<crate::receive_timeout::IdleSignal>()
            || message.is::<parrot_api::context::ActorTerminated>()
            || message.is::<crate::thread::system::Terminated>()
            || message.is::<ControlMessage>()
            || message.is::<crate::thread::system::WatchRequest>()
            || message.is::<crate::thread::system::UnwatchRequest>()
        {
            return Ok(message);
        }
        catch_unwind(AssertUnwindSafe(|| self.policy.prepare(message))).unwrap_or_else(|payload| {
            Err(MailboxError::PushError(format!(
                "message admission panicked: {}",
                crate::owned_payload::panic_message(payload)
            )))
        })
    }
    async fn prepare_item(&self, item: MailboxItem) -> Result<MailboxItem, MailboxError> {
        if self.inner.is_closed().await {
            return Err(MailboxError::Closed);
        }
        let item = match item {
            MailboxItem::Plain(message) => {
                let (message, parent) = crate::thread::envelope::TraceEnvelope::unpack(message);
                match message.downcast::<AskEnvelope>() {
                    Ok(mut envelope) => {
                        // An explicit outer delivery owns the causal parent;
                        // raw legacy ASK keeps the parent's envelope metadata.
                        if parent.is_some() {
                            envelope.trace_context = parent;
                        }
                        MailboxItem::Ask(*envelope)
                    }
                    Err(message) => {
                        return self.prepare_message(message).map(|message| {
                            MailboxItem::Plain(crate::thread::envelope::TraceEnvelope::with_parent(
                                message, parent,
                            ))
                        });
                    }
                }
            }
            item => item,
        };
        match item {
            MailboxItem::Ask(mut envelope) => {
                let payload =
                    std::mem::replace(&mut envelope.payload, AskPayload::Boxed(Box::new(())));
                envelope.payload = AskPayload::Boxed(self.prepare_message(payload.into_boxed())?);
                Ok(MailboxItem::Ask(envelope))
            }
            MailboxItem::Plain(_) => unreachable!(),
        }
    }
}
#[async_trait]
impl Mailbox for AdmissionMailbox {
    async fn push(
        &self,
        message: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        self.push_item(MailboxItem::Plain(message), strategy).await
    }
    async fn push_ask(
        &self,
        envelope: AskEnvelope,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        self.push_item(MailboxItem::Ask(envelope), strategy).await
    }
    async fn push_item(
        &self,
        item: MailboxItem,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        self.inner
            .push_item(self.prepare_item(item).await?, strategy)
            .await
    }
    async fn push_with_priority(
        &self,
        message: BoxedMessage,
        strategy: BackpressureStrategy,
        high: bool,
    ) -> Result<bool, MailboxError> {
        let item = self.prepare_item(MailboxItem::Plain(message)).await?;
        self.inner
            .push_with_priority(item.into_boxed_message(), strategy, high)
            .await
    }
    fn supports_high_priority(&self) -> bool {
        self.inner.supports_high_priority()
    }
    fn has_high_priority_messages(&self) -> bool {
        self.inner.has_high_priority_messages()
    }
    async fn pop(&self) -> Option<BoxedMessage> {
        self.inner.pop().await
    }
    async fn pop_item(&self) -> Option<MailboxItem> {
        self.inner.pop_item().await
    }
    async fn is_empty(&self) -> bool {
        self.inner.is_empty().await
    }
    async fn signal_ready(&self) {
        self.inner.signal_ready().await
    }
    fn path(&self) -> &ActorPath {
        self.inner.path()
    }
    fn capacity(&self) -> usize {
        self.inner.capacity()
    }
    async fn len(&self) -> usize {
        self.inner.len().await
    }
    fn seal(&self) {
        self.inner.seal()
    }
    async fn close(&self) {
        self.inner.close().await
    }
    async fn is_closed(&self) -> bool {
        self.inner.is_closed().await
    }
    fn set_processor(&self, p: Arc<dyn ProcessorInterface>) {
        self.inner.set_processor(p)
    }
    fn get_processor(&self) -> Option<Arc<dyn ProcessorInterface>> {
        self.inner.get_processor()
    }
    fn has_processor(&self) -> bool {
        self.inner.has_processor()
    }
    fn set_wake_hook(&self, hook: WakeHook) {
        self.inner.set_wake_hook(hook)
    }
    fn fire_wake_hook(&self) {
        self.inner.fire_wake_hook()
    }
    fn schedule_state(&self) -> &ScheduleState {
        self.inner.schedule_state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Debug)]
    struct Charge(Arc<AtomicUsize>);
    impl Drop for Charge {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[derive(Debug)]
    struct Policy(Arc<AtomicUsize>);
    impl MessageAdmission for Policy {
        fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
            let n = *message
                .downcast_ref::<u64>()
                .expect("only business u64 reaches policy");
            if n == 13 {
                panic!("injected admission panic");
            }
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Charge(self.0.clone())))
        }
    }
    fn fixtures() -> Vec<Arc<dyn Mailbox>> {
        vec![
            Arc::new(super::super::mpsc::MpscMailbox::new(
                1,
                ActorPath::for_test("/mpsc"),
            )),
            Arc::new(super::super::spsc::SpscMailbox::new(
                1,
                ActorPath::for_test("/spsc"),
            )),
            Arc::new(super::super::spsc_ringbuf::SpscRingbufMailbox::new(
                1,
                ActorPath::for_test("/ring"),
            )),
        ]
    }
    #[tokio::test]
    async fn preparation_lifetime_covers_blocked_producers_popped_items_and_close() {
        for inner in fixtures() {
            let held = Arc::new(AtomicUsize::new(0));
            let m = AdmissionMailbox::new(inner, Arc::new(Policy(held.clone())));
            m.push(Box::new(1u64), BackpressureStrategy::Block)
                .await
                .unwrap();
            let mut blocked = Box::pin(m.push(Box::new(2u64), BackpressureStrategy::Block));
            assert!(futures::poll!(&mut blocked).is_pending());
            assert_eq!(held.load(Ordering::SeqCst), 2);
            drop(blocked);
            assert_eq!(held.load(Ordering::SeqCst), 1);
            let popped = m.pop().await.unwrap();
            assert_eq!(
                held.load(Ordering::SeqCst),
                1,
                "dequeue must not release processing charge"
            );
            drop(popped);
            assert_eq!(held.load(Ordering::SeqCst), 0);
            m.push_with_priority(Box::new(3u64), BackpressureStrategy::Block, true)
                .await
                .unwrap();
            assert_eq!(held.load(Ordering::SeqCst), 1);
            m.close().await;
            assert_eq!(held.load(Ordering::SeqCst), 0);
            assert!(matches!(
                m.push(Box::new(4u64), BackpressureStrategy::Block).await,
                Err(MailboxError::Closed)
            ));
            assert_eq!(held.load(Ordering::SeqCst), 0);
        }
    }
    #[tokio::test]
    async fn asks_inline_legacy_controls_and_panics_preserve_delivery_contracts() {
        for inner in fixtures() {
            let held = Arc::new(AtomicUsize::new(0));
            let m = AdmissionMailbox::new(inner, Arc::new(Policy(held.clone())));
            for legacy in [false, true] {
                let (ask, mut rx) = AskEnvelope::new_inline(7u64);
                if legacy {
                    m.push(Box::new(ask), BackpressureStrategy::Block)
                        .await
                        .unwrap();
                } else {
                    m.push_ask(ask, BackpressureStrategy::Block).await.unwrap();
                }
                assert_eq!(held.load(Ordering::SeqCst), 1);
                let MailboxItem::Ask(mut envelope) = m.pop_item().await.unwrap() else {
                    panic!("lost ask");
                };
                assert!(rx.try_recv().is_err());
                envelope
                    .take_reply()
                    .unwrap()
                    .send(Ok(Box::new(42u64)))
                    .unwrap();
                assert_eq!(*rx.await.unwrap().unwrap().downcast::<u64>().unwrap(), 42);
                drop(envelope);
                assert_eq!(held.load(Ordering::SeqCst), 0);
            }
            let (ask, rx) = AskEnvelope::new(Box::new(ControlMessage::HealthCheck));
            m.push_ask(ask, BackpressureStrategy::Block).await.unwrap();
            assert_eq!(held.load(Ordering::SeqCst), 0);
            drop(m.pop_item().await);
            assert!(rx.await.is_err());
            assert!(
                m.push(Box::new(13u64), BackpressureStrategy::Block)
                    .await
                    .is_err()
            );
            assert!(m.is_empty().await);
            m.push(Box::new(8u64), BackpressureStrategy::Block)
                .await
                .unwrap();
            m.close().await;
            assert_eq!(held.load(Ordering::SeqCst), 0);
        }
    }
}
