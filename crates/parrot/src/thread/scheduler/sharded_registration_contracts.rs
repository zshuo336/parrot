use super::*;
use crate::thread::{
    config::BackpressureStrategy,
    mailbox::mpsc::MpscMailbox,
    processor::{ProcessorInterface, ProcessorStats, ProcessorStatsTrait},
};
use parrot_api::{address::ActorPath, types::BoxedFuture};
use std::{
    any::Any,
    sync::{Condvar, atomic::AtomicUsize},
    thread::ThreadId,
    time::Duration,
};

#[derive(Clone, Debug, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);
impl Gate {
    fn wait(&self) {
        let (lock, changed) = &*self.0;
        let opened = lock.lock().unwrap();
        let (opened, _) = changed
            .wait_timeout_while(opened, Duration::from_secs(5), |opened| !*opened)
            .unwrap();
        assert!(*opened, "test gate was not released");
    }
    fn open(&self) {
        let (lock, changed) = &*self.0;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }
}
struct ReleaseGate(Gate);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.open();
    }
}

struct Probe {
    initialized: AtomicUsize,
    batches: AtomicUsize,
    live: AtomicUsize,
    max_live: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    empty: tokio::sync::Notify,
    suspend_first: bool,
    replies: tokio::sync::mpsc::UnboundedSender<(u64, ThreadId)>,
}
struct Batch(Arc<Probe>);
impl Drop for Batch {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}
impl ProcessorStatsTrait for Probe {
    fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
        None
    }
}
impl ProcessorInterface for Probe {
    fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::SeqCst) != 0
    }
    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async move {
            self.initialized.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn process_batch_erased(
        self: Arc<Self>,
        mailbox: Arc<dyn Mailbox>,
        _: usize,
        _: bool,
    ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
        Box::pin(async move {
            let active = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_live.fetch_max(active, Ordering::SeqCst);
            let _batch = Batch(self.clone());
            let first = self.batches.fetch_add(1, Ordering::SeqCst) == 0;
            let message = mailbox.pop().await;
            if first && self.suspend_first {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if let Some(message) = message {
                self.replies
                    .send((
                        *message.downcast::<u64>().unwrap(),
                        std::thread::current().id(),
                    ))
                    .unwrap();
                Ok((1, 0))
            } else {
                self.empty.notify_one();
                Ok((0, 0))
            }
        })
    }
    fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

type Replies = tokio::sync::mpsc::UnboundedReceiver<(u64, ThreadId)>;
fn mailbox(suspend_first: bool) -> (Arc<MpscMailbox>, Arc<Probe>, Replies) {
    mailbox_at("/handoff", suspend_first)
}
fn mailbox_at(path: &str, suspend_first: bool) -> (Arc<MpscMailbox>, Arc<Probe>, Replies) {
    let (replies, receiver) = tokio::sync::mpsc::unbounded_channel();
    let processor = Arc::new(Probe {
        initialized: AtomicUsize::new(0),
        batches: AtomicUsize::new(0),
        live: AtomicUsize::new(0),
        max_live: AtomicUsize::new(0),
        entered: Default::default(),
        release: Default::default(),
        empty: Default::default(),
        suspend_first,
        replies,
    });
    let mailbox = Arc::new(MpscMailbox::new(8, ActorPath::placeholder(path)));
    mailbox.set_processor(processor.clone());
    (mailbox, processor, receiver)
}
async fn receive(replies: &mut Replies) -> Result<(u64, ThreadId), tokio::time::error::Elapsed> {
    tokio::time::timeout(Duration::from_secs(2), async {
        replies.recv().await.unwrap()
    })
    .await
}

#[test]
fn closed_shard_wake_releases_the_acquired_schedule_slot() {
    let (mailbox, _, _) = mailbox(false);
    let (tx, rx) = flume::unbounded();
    drop(rx);
    let (stop_tx, _stop_rx) = flume::unbounded();
    let mailbox_trait: Arc<dyn Mailbox> = mailbox.clone();
    let registration = ShardRegistration {
        active: Arc::new(AtomicBool::new(true)),
        mailbox: Arc::downgrade(&mailbox_trait),
        shard: Arc::new(Shard { tx, stop_tx }),
    };

    assert!(!registration.enqueue());
    assert!(
        mailbox.schedule_state().try_enqueue(),
        "a failed shard send must return scheduling ownership"
    );
    mailbox.schedule_state().force_release();
}

#[tokio::test]
async fn queued_registration_hands_its_slot_to_a_new_registration_without_another_send() {
    let gate = Gate::default();
    let release = ReleaseGate(gate.clone());
    let scheduler = ShardedScheduler::start_with_startup(1, 1, move || gate.wait());
    let (mailbox, processor, mut replies) = mailbox(false);
    mailbox
        .push(Box::new(17u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    scheduler
        .schedule_affinity("/handoff", mailbox.clone(), "old")
        .unwrap();
    scheduler.deschedule("/handoff").unwrap();
    scheduler
        .schedule_affinity("/handoff", mailbox.clone(), "new")
        .unwrap();
    drop(release);
    let reply = receive(&mut replies).await;
    let shutdown = scheduler.shutdown_result();
    shutdown.unwrap();
    assert_eq!(
        reply.expect("new registration lost the old queued slot").0,
        17
    );
    assert_eq!(processor.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(processor.max_live.load(Ordering::SeqCst), 1);
    assert!(mailbox.is_empty().await);
    assert!(replies.try_recv().is_err());
}

#[tokio::test]
async fn shutdown_drains_queued_slot_for_mailbox_reuse_by_a_new_scheduler() {
    let gate = Gate::default();
    let release = ReleaseGate(gate.clone());
    let old = ShardedScheduler::start_with_startup(1, 1, move || gate.wait());
    let (mailbox, processor, mut replies) = mailbox(false);
    mailbox
        .push(Box::new(23u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    old.schedule_affinity("/reuse", mailbox.clone(), "old")
        .unwrap();
    assert_eq!(old.shard_lengths(), vec![1], "item must remain queued");
    old.deschedule("/reuse").unwrap();

    // Stop the old worker while its queue is still blocked at startup. The
    // item must be retired by the worker's shutdown drain, not by processing.
    old.request_shutdown();
    drop(release);
    old.shutdown_result().unwrap();

    let new = ShardedScheduler::start(1, 1);
    new.schedule_affinity("/reuse", mailbox, "new").unwrap();
    let reply = receive(&mut replies).await.unwrap();
    new.shutdown_result().unwrap();

    assert_eq!(reply.0, 23);
    assert_eq!(processor.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(processor.max_live.load(Ordering::SeqCst), 1);
    assert_eq!(processor.live.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn running_registration_hands_pending_work_to_the_new_owner_without_overlap() {
    let scheduler = ShardedScheduler::start(2, 1);
    let old_key = "old";
    let new_key = (0..256)
        .map(|i| format!("new-{i}"))
        .find(|key| {
            scheduler.shard_for(ShardedScheduler::hash_key(key))
                != scheduler.shard_for(ShardedScheduler::hash_key(old_key))
        })
        .unwrap();
    let (mailbox, processor, mut replies) = mailbox(true);
    mailbox
        .push(Box::new(1u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    scheduler
        .schedule_affinity("/handoff", mailbox.clone(), old_key)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), processor.entered.notified())
        .await
        .unwrap();
    scheduler.deschedule("/handoff").unwrap();
    scheduler
        .schedule_affinity("/handoff", mailbox.clone(), &new_key)
        .unwrap();
    mailbox
        .push(Box::new(2u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    processor.release.notify_one();
    let first = receive(&mut replies).await;
    let second = receive(&mut replies).await;
    let shutdown = scheduler.shutdown_result();
    shutdown.unwrap();
    let first = first.expect("old work never completed");
    let second = second.expect("new registration lost its pending work");
    assert_eq!((first.0, second.0), (1, 2));
    assert_ne!(
        first.1, second.1,
        "new key must choose the other real shard owner"
    );
    assert_eq!(processor.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(processor.max_live.load(Ordering::SeqCst), 1);
    assert_eq!(processor.live.load(Ordering::SeqCst), 0);
    assert!(mailbox.is_empty().await);
    assert!(replies.try_recv().is_err());
}

#[tokio::test]
async fn descheduled_queue_and_later_sends_stay_unprocessed_without_a_new_registration() {
    let gate = Gate::default();
    let release = ReleaseGate(gate.clone());
    let scheduler = ShardedScheduler::start_with_startup(1, 1, move || gate.wait());
    let (mailbox, processor, mut replies) = mailbox(false);
    mailbox
        .push(Box::new(1u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    scheduler
        .schedule_affinity("/handoff", mailbox.clone(), "old")
        .unwrap();
    scheduler.deschedule("/handoff").unwrap();
    mailbox
        .push(Box::new(2u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    let (sentinel, _, mut observed) = mailbox_at("/sentinel", false);
    sentinel
        .push(Box::new(99u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    scheduler
        .schedule_affinity("/sentinel", sentinel, "same-shard")
        .unwrap();
    drop(release);
    let observed = receive(&mut observed).await;
    scheduler.shutdown_result().unwrap();
    assert_eq!(observed.unwrap().0, 99);
    assert_eq!(processor.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(processor.batches.load(Ordering::SeqCst), 0);
    assert!(replies.try_recv().is_err());
    for value in [1, 2] {
        assert_eq!(
            *mailbox.pop().await.unwrap().downcast::<u64>().unwrap(),
            value
        );
    }
    assert!(mailbox.is_empty().await);
}

#[tokio::test]
async fn rejected_duplicate_registration_preserves_the_original_wake_and_owner() {
    let scheduler = ShardedScheduler::start(2, 1);
    let (mailbox, processor, mut replies) = mailbox(false);
    scheduler
        .schedule_affinity("/handoff", mailbox.clone(), "original")
        .unwrap();
    assert!(
        scheduler
            .schedule_affinity("/handoff", mailbox.clone(), "replacement")
            .is_err()
    );
    mailbox
        .push(Box::new(1u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    let first = receive(&mut replies).await;
    mailbox
        .push(Box::new(2u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    let second = receive(&mut replies).await;
    scheduler.shutdown_result().unwrap();
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!((first.0, second.0), (1, 2));
    assert_eq!(first.1, second.1);
    assert_eq!(processor.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(processor.max_live.load(Ordering::SeqCst), 1);
    assert_eq!(processor.live.load(Ordering::SeqCst), 0);
    assert!(mailbox.is_empty().await);
    assert!(replies.try_recv().is_err());
}

#[tokio::test]
async fn concurrent_registration_changes_preserve_live_wakes_and_serial_execution() {
    for round in 0..8 {
        let scheduler = ShardedScheduler::start(2, 1);
        let (mailbox, processor, mut replies) = mailbox(false);
        let start = Arc::new(std::sync::Barrier::new(3));
        let writers: Vec<_> = (0..2)
            .map(|_| {
                let scheduler = scheduler.clone();
                let mailbox = mailbox.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..64 {
                        scheduler.deschedule("/handoff").unwrap();
                        if let Err(error) =
                            scheduler.schedule_affinity("/handoff", mailbox.clone(), "owner")
                        {
                            assert!(error.contains("already scheduled"), "{error}");
                        }
                    }
                })
            })
            .collect();
        start.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        assert!(scheduler.assigned.lock().unwrap().contains_key("/handoff"));
        let (sentinel, _, mut observed) = mailbox_at("/sentinel", false);
        scheduler
            .schedule_affinity("/sentinel", sentinel.clone(), "owner")
            .unwrap();
        let mut results = Vec::new();
        for value in 1..=3u64 {
            mailbox
                .push(Box::new(value), BackpressureStrategy::Block)
                .await
                .unwrap();
            results.push(receive(&mut replies).await);
            sentinel
                .push(Box::new(99u64), BackpressureStrategy::Block)
                .await
                .unwrap();
            receive(&mut observed)
                .await
                .expect("unrelated owner progress");
        }
        scheduler.shutdown_result().unwrap();
        let replies: Vec<_> = results
            .into_iter()
            .map(|reply| {
                reply.unwrap_or_else(|error| panic!("live wake lost in round {round}: {error}"))
            })
            .collect();
        assert_eq!(
            replies.iter().map(|reply| reply.0).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(replies.iter().all(|reply| reply.1 == replies[0].1));
        assert_eq!(processor.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(processor.max_live.load(Ordering::SeqCst), 1);
        assert_eq!(processor.live.load(Ordering::SeqCst), 0);
        assert!(mailbox.is_empty().await);
    }
}

#[derive(Debug)]
struct DelayedHook {
    inner: Arc<MpscMailbox>,
    first: AtomicBool,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Gate,
}
#[async_trait::async_trait]
impl Mailbox for DelayedHook {
    async fn push(
        &self,
        message: parrot_api::types::BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> Result<(), crate::thread::error::MailboxError> {
        self.inner.push(message, strategy).await
    }
    async fn pop(&self) -> Option<parrot_api::types::BoxedMessage> {
        self.inner.pop().await
    }
    async fn is_empty(&self) -> bool {
        self.inner.is_empty().await
    }
    async fn signal_ready(&self) {
        self.inner.signal_ready().await;
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
        self.inner.seal();
    }
    async fn close(&self) {
        self.inner.close().await;
    }
    async fn is_closed(&self) -> bool {
        self.inner.is_closed().await
    }
    fn set_processor(&self, processor: Arc<dyn ProcessorInterface>) {
        self.inner.set_processor(processor);
    }
    fn get_processor(&self) -> Option<Arc<dyn ProcessorInterface>> {
        self.inner.get_processor()
    }
    fn has_processor(&self) -> bool {
        self.inner.has_processor()
    }
    fn set_wake_hook(&self, hook: crate::thread::mailbox::WakeHook) {
        if self.first.swap(false, Ordering::AcqRel) {
            self.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.release.wait();
        }
        self.inner.set_wake_hook(hook);
    }
    fn fire_wake_hook(&self) {
        self.inner.fire_wake_hook();
    }
    fn schedule_state(&self) -> &crate::thread::mailbox::ScheduleState {
        self.inner.schedule_state()
    }
}

#[tokio::test]
async fn late_old_hook_publication_cannot_hide_the_current_registration() {
    let scheduler = ShardedScheduler::start(1, 1);
    let (inner, processor, mut replies) = mailbox(false);
    let gate = Gate::default();
    let release = ReleaseGate(gate.clone());
    let (entered, first_hook) = tokio::sync::oneshot::channel();
    let mailbox = Arc::new(DelayedHook {
        inner,
        first: AtomicBool::new(true),
        entered: Mutex::new(Some(entered)),
        release: gate,
    });
    let initial = {
        let scheduler = scheduler.clone();
        let mailbox = mailbox.clone();
        std::thread::spawn(move || scheduler.schedule_affinity("/handoff", mailbox, "owner"))
    };
    tokio::time::timeout(Duration::from_secs(2), first_hook)
        .await
        .unwrap()
        .unwrap();
    let (finished, completion) = std::sync::mpsc::channel();
    let replacement = {
        let scheduler = scheduler.clone();
        let mailbox = mailbox.clone();
        std::thread::spawn(move || {
            scheduler.deschedule("/handoff").unwrap();
            scheduler
                .schedule_affinity("/handoff", mailbox, "owner")
                .unwrap();
            finished.send(()).unwrap();
        })
    };
    let early = completion.recv_timeout(Duration::from_secs(1)).is_ok();
    if early {
        tokio::time::timeout(Duration::from_secs(2), processor.empty.notified())
            .await
            .unwrap();
    }
    drop(release);
    initial.join().unwrap().unwrap();
    replacement.join().unwrap();
    let (sentinel, _, mut observed) = mailbox_at("/sentinel", false);
    sentinel
        .push(Box::new(99u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    scheduler
        .schedule_affinity("/sentinel", sentinel, "owner")
        .unwrap();
    receive(&mut observed).await.unwrap();
    mailbox
        .push(Box::new(7u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    let reply = receive(&mut replies).await;
    scheduler.shutdown_result().unwrap();
    println!("replacement_completed_before_old_hook={early}");
    assert_eq!(reply.expect("late old hook hid the live registration").0, 7);
    assert_eq!(processor.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(processor.max_live.load(Ordering::SeqCst), 1);
    assert!(mailbox.is_empty().await);
    assert!(replies.try_recv().is_err());
}
