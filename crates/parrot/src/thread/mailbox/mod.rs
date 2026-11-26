//! Mailbox abstraction for the thread-based actor system.

use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use async_trait::async_trait;

use parrot_api::address::ActorPath;
use parrot_api::types::BoxedMessage;

use crate::owned_payload::notification;
use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::processor::ProcessorInterface;

pub mod admission;
pub mod mpsc;
pub mod spsc;
pub mod spsc_ringbuf;

mod drain;

#[cfg(test)]
mod drain_budget_contracts;

type Input<T> = crate::owned_payload::OwnedPayload<T>;

// Final-owner Drop has no async disposal receipt or guaranteed live executor.
// Each detached value therefore needs its own synchronous unwind boundary.
fn retire_on_drop<T>(value: T, stage: &'static str) {
    if let Err(error) = crate::owned_payload::PublicationInput::new(value).retire() {
        crate::owned_payload::cleanup_diagnostic(|| {
            tracing::error!(stage, error = %error, "final mailbox owner disposal failed");
        });
    }
}

// Call only after releasing queue locks and signalling committed work. Rejection
// keeps its original category; successful replacement must report cleanup failure.
async fn finish_push<T: Send + 'static>(
    result: Result<(), MailboxError>,
    rejected: Option<Input<T>>,
    evicted: Option<Input<T>>,
) -> Result<(), MailboxError> {
    if let Some(rejected) = rejected
        && let Err(error) = rejected.retire().await
    {
        crate::owned_payload::cleanup_diagnostic(|| {
            tracing::error!(error = %error, rejection = ?result, "rejected mailbox input disposal failed");
        });
    }
    if let Some(evicted) = evicted
        && let Err(error) = evicted.retire().await
    {
        if result.is_ok() {
            return Err(MailboxError::PushError(format!(
                "replacement admitted; evicted mailbox input disposal failed: {}",
                crate::error_receipt::actor_summary(&error)
            )));
        }
        crate::owned_payload::cleanup_diagnostic(|| {
            tracing::error!(error = %error, rejection = ?result, "evicted mailbox input disposal failed after rejection");
        });
    }
    result
}

fn retire_detached_sync<T: Send + 'static>(values: impl IntoIterator<Item = T>) -> Option<String> {
    let mut errors = Vec::new();
    for value in values {
        if let Err(error) = crate::owned_payload::retire_synchronously(value) {
            errors.push(crate::error_receipt::actor_summary(&error));
        }
    }
    (!errors.is_empty()).then(|| errors.join("; "))
}

#[derive(Default)]
struct CloseStatus {
    claimed: bool,
    complete: bool,
    error: Option<String>,
}

enum CloseAttempt<T> {
    Reservation,
    Owned(T),
    Observing,
}

/// Shares the physical close completion across concurrent callers.
///
/// Sealing and queue extraction are synchronous with respect to the mailbox's
/// own bookkeeping locks, while user destructors run in an owned task. A
/// cancelled waiter therefore cannot abandon cleanup, and a later close cannot
/// report completion before the first drain has finished.
/// Admission waits happen before claiming or detaching the queue.
pub(crate) struct CloseState {
    status: Mutex<CloseStatus>,
    complete: tokio::sync::Notify,
}

impl CloseState {
    pub(crate) fn new() -> Self {
        Self {
            status: Mutex::new(CloseStatus::default()),
            complete: tokio::sync::Notify::new(),
        }
    }

    fn claim(&self, has_values: bool, reserved: bool) -> CloseAttempt<()> {
        let mut status = self.status.lock().unwrap();
        if status.claimed {
            CloseAttempt::Observing
        } else if has_values && !reserved {
            CloseAttempt::Reservation
        } else {
            status.claimed = true;
            CloseAttempt::Owned(())
        }
    }

    async fn reserve(&self) -> Option<drain::Reservation> {
        if self.status.lock().unwrap().claimed {
            return None;
        }
        tokio::select! {
            biased;
            _ = self.wait() => None,
            reservation = drain::Reservation::acquire() => Some(reservation),
        }
    }

    fn start_drain<T: Send + 'static>(
        self: &Arc<Self>,
        values: Vec<T>,
        reservation: Option<drain::Reservation>,
    ) {
        if values.is_empty() {
            self.finish(None);
            return;
        }
        let owner = self.clone();
        // Only raw external channel permits can publish after an empty-queue
        // claim. Such inputs inherit synchronous disposal, without a new thread.
        reservation
            .unwrap_or(drain::Reservation::Inline)
            .run(move || owner.finish_drain(values));
    }

    fn finish_drain<T: Send + 'static>(&self, values: Vec<T>) {
        let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            retire_detached_sync(values)
        }))
        .unwrap_or_else(|payload| Some(crate::owned_payload::panic_message(payload)));
        self.finish(error);
    }

    fn finish(&self, error: Option<String>) {
        {
            let mut status = self.status.lock().unwrap();
            status.error = error;
            status.complete = true;
        }
        notification::all(&self.complete);
    }

    pub(crate) async fn wait(&self) {
        loop {
            let notified = self.complete.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let completion = {
                let status = self.status.lock().unwrap();
                (status.complete, status.error.clone())
            };
            if completion.0 {
                if let Some(error) = completion.1 {
                    panic!("mailbox drain disposal failed: {error}");
                }
                return;
            }
            notification::observe(notified).await;
        }
    }
}

/// M5: mailbox 内部条目——ask 信封按值流动（零信封装箱）。
///
/// `Ask` 变体持有整个 `AskEnvelope`（含分层载荷与 inline oneshot）；
/// `Plain` 变体是历史普通消息（`BoxedMessage`，含控制/系统消息）。
/// 邮箱三实现的内部缓冲改为 `MailboxItem` 元素；`pop` 兼容路径把
/// `Ask` 装箱还原为 `BoxedMessage`（AskEnvelope），仅消费侧处理器
/// 的 `pop_item` 享受免装箱收益。
#[derive(Debug)]
pub enum MailboxItem {
    /// Ask 信封（按值，M5 免装箱路径）。
    Ask(crate::thread::envelope::AskEnvelope),
    /// 普通消息（历史 BoxedMessage 路径：tell / 控制 / 系统信号）。
    Plain(BoxedMessage),
}

impl MailboxItem {
    /// 兼容还原：把条目转回 `BoxedMessage`（AskEnvelope 装箱一次）。
    /// 供旧消费路径（`pop` + `downcast::<AskEnvelope>`）使用。
    pub fn into_boxed_message(self) -> BoxedMessage {
        match self {
            MailboxItem::Ask(env) => Box::new(env),
            MailboxItem::Plain(m) => m,
        }
    }
}

/// Weak reference to a mailbox.
pub type WeakMailboxRef = std::sync::Weak<dyn Mailbox + Send + Sync>;

/// Hook fired after a successful `push` so the scheduling layer can
/// (re-)enqueue the mailbox for processing.
///
/// This closes the lost-wakeup race where a worker drains a mailbox, decides
/// not to re-queue it (it looked empty), and a subsequent push only fires a
/// bare `Notify` that no worker associates with this mailbox.
pub type WakeHook = Arc<dyn Fn() + Send + Sync>;

fn invoke_wake_hook(hook: Option<WakeHook>) {
    let Some(hook) = hook else {
        return;
    };
    // A self-replacing hook can leave this snapshot as its last owner. Dispose
    // it after catching the callback, so two user panics cannot abort unwinding.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook()));
    let disposal = crate::owned_payload::PublicationInput::new(hook).retire();
    match (result, disposal) {
        (Ok(()), Ok(())) => {}
        (Err(payload), Ok(())) => std::panic::resume_unwind(payload),
        (Ok(()), Err(error)) => panic!(
            "mailbox wake hook disposal failed: {}",
            crate::error_receipt::actor_summary(&error)
        ),
        (Err(payload), Err(error)) => panic!(
            "mailbox wake hook failed: {}; disposal failed: {}",
            crate::owned_payload::panic_message(payload),
            crate::error_receipt::actor_summary(&error)
        ),
    }
}

/// Scheduling-slot state ensuring at most one queue entry (and at most one
/// owning worker) per mailbox.
///
/// The state machine is:
/// - `OWNED` bit clear: idle; anyone may acquire the slot and push
///   the mailbox into the scheduling queue.
/// - `OWNED` bit set: the mailbox is either sitting in the
///   scheduling queue or is being processed by a worker. Concurrent enqueue
///   attempts are rejected and recorded in the `WAKE` bit so the owning
///   worker takes over the re-queue on release instead of losing the wakeup.
///
/// This preserves actor semantics: a single actor's mailbox is processed by
/// exactly one worker at a time.
#[derive(Debug, Default)]
pub struct ScheduleState {
    // Ownership and pending wake must change atomically. Two separate bools
    // allow a releasing worker to reclaim a slot already taken by a producer.
    state: AtomicU8,
}

impl ScheduleState {
    const OWNED: u8 = 1;
    const WAKE: u8 = 2;

    /// Coalesce a producer wake without acquiring an idle slot. A free slot
    /// must be claimed by the mailbox's currently registered scheduler.
    pub(crate) fn wake_if_owned(&self) -> bool {
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & Self::OWNED != 0).then_some(state | Self::WAKE)
            })
            .is_ok()
    }

    /// Acquire a free slot, or record a wake for its current owner.
    pub fn try_enqueue(&self) -> bool {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let free = current & Self::OWNED == 0;
            let next = if free {
                Self::OWNED
            } else {
                current | Self::WAKE
            };
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return free,
                Err(observed) => current = observed,
            }
        }
    }

    /// Release ownership, or retain it for exactly one re-enqueue. The CAS
    /// makes producer takeover and worker re-enqueue mutually exclusive.
    pub fn release(&self, has_more: bool) -> bool {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let requeue = has_more || current & Self::WAKE != 0;
            let next = if requeue { Self::OWNED } else { 0 };
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return requeue,
                Err(observed) => current = observed,
            }
        }
    }

    /// Discard ownership and pending wakes after terminal processing failure.
    pub fn force_release(&self) {
        self.state.store(0, Ordering::Release);
    }
}

/// Trait defining a mailbox that buffers messages for an actor.
///
/// A mailbox is the single delivery point between senders (`ThreadActorRef`)
/// and the scheduling layer (workers). It also carries the (optional)
/// type-erased processor association used by workers to execute messages.
#[async_trait]
pub trait Mailbox: Debug + Send + Sync + 'static {
    /// Push a message into the mailbox using the given backpressure strategy.
    async fn push(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError>;

    /// Push a message into the mailbox with an explicit priority lane (M2).
    ///
    /// High-priority messages (`MessagePriority >= 70`) are stored in a
    /// separate internal lane that is drained before normal-priority
    /// messages, giving O(1) priority jumps over same-mailbox backlogs.
    ///
    /// Default implementation falls back to the normal lane (single-queue
    /// mailboxes keep their historical FIFO behavior).
    async fn push_with_priority(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
        _high_priority: bool,
    ) -> Result<bool, MailboxError> {
        // Return value: whether the message entered the high lane.
        self.push(msg, strategy).await?;
        Ok(false)
    }

    /// Whether explicit high-lane admission is supported.
    fn supports_high_priority(&self) -> bool {
        false
    }

    /// Whether this mailbox currently holds any high-priority messages (M2).
    ///
    /// Schedulers consult this when re-queueing a mailbox after a batch to
    /// decide which scheduling lane (High/Normal) the mailbox belongs in.
    /// Default: `false` (single-queue mailboxes have no high lane).
    fn has_high_priority_messages(&self) -> bool {
        false
    }

    /// Pop the next message from the mailbox, or `None` when empty.
    async fn pop(&self) -> Option<BoxedMessage>;

    /// M5: pop the next internal item (ask envelopes flow by value).
    ///
    /// Default implementation delegates to `pop` and re-wraps the boxed
    /// `AskEnvelope` (compat path; single-alloc consumers override).
    async fn pop_item(&self) -> Option<MailboxItem> {
        self.pop().await.map(|m| {
            // 旧路径装箱的信封还原（downcast 失败说明是普通消息）。
            match m.downcast::<crate::thread::envelope::AskEnvelope>() {
                Ok(env) => MailboxItem::Ask(*env),
                Err(m) => MailboxItem::Plain(m),
            }
        })
    }

    /// M5: push an ask envelope by value (single-block path).
    ///
    /// Default implementation boxes the envelope and falls back to `push`
    /// (compat); single-alloc mailboxes override.
    async fn push_ask(
        &self,
        envelope: crate::thread::envelope::AskEnvelope,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        self.push(Box::new(envelope) as BoxedMessage, strategy)
            .await
    }

    /// M5: push an internal item (shared backpressure plumbing).
    ///
    /// Default: `Plain` items delegate to `push`; `Ask` items delegate to
    /// `push_ask`. Mailboxes with native `MailboxItem` buffers override.
    async fn push_item(
        &self,
        item: MailboxItem,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        match item {
            MailboxItem::Ask(env) => self.push_ask(env, strategy).await,
            MailboxItem::Plain(m) => self.push(m, strategy).await,
        }
    }

    /// Whether the mailbox currently holds no messages.
    async fn is_empty(&self) -> bool;

    /// Wake any processor waiting for messages on this mailbox.
    async fn signal_ready(&self);

    /// The actor path this mailbox belongs to.
    fn path(&self) -> &ActorPath;

    /// Maximum number of messages the mailbox can hold.
    fn capacity(&self) -> usize;

    /// Current number of buffered messages.
    async fn len(&self) -> usize;

    /// Reject new mailbox delivery without running message destructors.
    /// This must be synchronous and must wake producers waiting for capacity.
    fn seal(&self);

    /// Seal and drain the mailbox. Dropping user messages can execute user code;
    /// lifecycle callers isolate this operation from their deadline timers.
    /// Built-in mailboxes share 32 native drain leases per process, held through
    /// OS join (including TLS disposal). Saturation waits before queue extraction;
    /// cancellation then leaves values in the sealed mailbox for retry or final
    /// owner disposal. After extraction, cleanup survives waiter cancellation and
    /// the original runtime's exit. Empty queues need no lease. Reentrant disposal
    /// and OS thread startup failure use synchronous cleanup outside queue locks.
    /// The close receipt covers message disposal; the helper's native join can
    /// finish later while still retaining its lease.
    async fn close(&self);

    /// Whether the mailbox is closed.
    async fn is_closed(&self) -> bool {
        false
    }

    /// Publish a processor, then release the old handle outside the slot lock.
    /// Reentrant replacements survive; this does not stop or retire processors.
    /// An old handle's destructor panic propagates after the new value is set.
    fn set_processor(&self, processor: Arc<dyn ProcessorInterface>);

    /// Get the associated processor, if any.
    fn get_processor(&self) -> Option<Arc<dyn ProcessorInterface>>;

    /// Whether a processor is associated with this mailbox.
    fn has_processor(&self) -> bool;

    /// Install the wake hook fired after each successful push.
    ///
    /// Schedulers use this to re-enqueue the mailbox when new messages arrive
    /// while no worker currently owns it. Publication precedes releasing the
    /// old hook outside the lock, including when its capture destructor panics.
    fn set_wake_hook(&self, hook: WakeHook);

    /// Invoke an owned hook snapshot outside the slot lock (no-op if absent).
    /// Reentrant/concurrent replacement does not cancel an in-flight callback.
    /// Independent calls may overlap; hooks must provide their own serialization
    /// if needed. Callback panics propagate without poisoning the slot. If both
    /// callback and snapshot disposal panic, their diagnostics are combined.
    fn fire_wake_hook(&self);

    /// The scheduling slot state guarding single-owner processing.
    fn schedule_state(&self) -> &ScheduleState;

    /// Whether the mailbox has more messages after a pop (scheduling hint).
    async fn has_more_messages(&self) -> bool {
        !self.is_empty().await
    }
}

#[cfg(test)]
mod schedule_state_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn concurrent_wake_and_release_have_exactly_one_enqueue_owner() {
        let state = ScheduleState::default();
        let barrier = std::sync::Barrier::new(2);
        let producer_owns = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..100_000 {
                    barrier.wait();
                    producer_owns.store(state.try_enqueue(), Ordering::Relaxed);
                    barrier.wait();
                }
            });
            let mut violations = 0;
            for _ in 0..100_000 {
                state.force_release();
                assert!(state.try_enqueue());
                barrier.wait();
                let owner_requeues = state.release(false);
                barrier.wait();
                // A wake races the final release. Exactly one side must enqueue;
                // both true creates duplicate concurrent mailbox owners.
                if owner_requeues == producer_owns.load(Ordering::Relaxed) {
                    violations += 1;
                }
            }
            assert_eq!(violations, 0, "lost or duplicated scheduling ownership");
        });
    }

    #[test]
    fn test_initial_state_allows_enqueue() {
        let state = ScheduleState::default();
        assert!(state.try_enqueue(), "first enqueue acquires the slot");
    }

    #[test]
    fn test_second_enqueue_is_rejected_and_records_wake() {
        let state = ScheduleState::default();
        assert!(state.try_enqueue());
        assert!(!state.try_enqueue(), "slot already held");
        // The rejected attempt is recorded so the owner re-queues on release.
        assert!(state.release(false), "pending wake forces re-queue");
    }

    #[test]
    fn test_release_with_more_work_requeues_without_freeing() {
        let state = ScheduleState::default();
        assert!(state.try_enqueue());
        // Worker still owns the slot; more messages present.
        assert!(state.release(true));
        // Slot was not freed: a concurrent enqueue attempt is still rejected.
        assert!(!state.try_enqueue());
    }

    #[test]
    fn test_release_without_work_frees_slot() {
        let state = ScheduleState::default();
        assert!(state.try_enqueue());
        assert!(!state.release(false), "idle release frees the slot");
        // Slot free again: the next enqueue succeeds.
        assert!(state.try_enqueue());
    }

    #[test]
    fn test_force_release_discards_everything() {
        let state = ScheduleState::default();
        assert!(state.try_enqueue());
        assert!(!state.try_enqueue()); // record a pending wake
        state.force_release();
        assert!(state.try_enqueue(), "slot must be free after force release");
        assert!(!state.release(false), "pending wake was discarded");
    }

    #[test]
    fn test_pending_wake_cleared_after_handover() {
        let state = ScheduleState::default();
        assert!(state.try_enqueue());
        assert!(!state.try_enqueue()); // pending wake recorded
        assert!(state.release(false)); // handover: re-queue required
        // The new queue entry keeps the slot held; releasing without work
        // now frees it cleanly because no further wake was recorded.
        assert!(!state.release(false));
    }

    #[test]
    fn test_concurrent_enqueue_exactly_one_wins() {
        let state = Arc::new(ScheduleState::default());
        let winners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let s = state.clone();
                let w = winners.clone();
                std::thread::spawn(move || {
                    if s.try_enqueue() {
                        w.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "exactly one enqueue wins"
        );
    }

    /// 修复回归守护：hook 在 owner 完全释放后必须能接管 slot。
    ///
    /// 旧实现在 owner `release` 的最终 `pending_wake` 检查与 slot 释放
    /// 之间设置 pending_wake 会丢失唤醒（hook 的 slot CAS 失败、owner
    /// 的二次检查也看不到 flag → 无入队者）。新实现在 store 后用 RMW
    /// 重试 slot，本测试验证该接管路径。
    #[test]
    fn test_hook_takes_over_fully_released_slot() {
        let state = ScheduleState::default();
        assert!(state.try_enqueue(), "owner acquires");
        // 模拟 owner 完全释放（无人 pending）
        assert!(!state.release(false), "slot freed");
        // hook 到来：必须成功接管（修复点）
        assert!(
            state.try_enqueue(),
            "hook must take over the freed slot after owner release"
        );
        // 接管后 pending_wake 被清除，下一次释放干净
        assert!(!state.release(false), "clean release after takeover");
    }

    /// 修复回归守护：owner 持有期间 hook 的 RMW 重试不得误抢。
    ///
    /// owner 仍在 batch 中（slot held）时，失败的 hook 重试必须继续
    /// 失败并把 pending_wake 留给 owner 的 release 捕获。
    #[test]
    fn test_hook_retry_does_not_steal_held_slot() {
        let state = ScheduleState::default();
        assert!(state.try_enqueue(), "owner acquires");
        // hook 在 owner 持有期间尝试两次（初次 + RMW 重试）都失败
        assert!(!state.try_enqueue(), "first attempt rejected");
        assert!(!state.try_enqueue(), "RMW retry must also fail while held");
        // owner 释放时必须看到 pending_wake（两次尝试留下的）并 re-queue
        assert!(
            state.release(false),
            "owner must observe pending_wake left by failed hook retries"
        );
    }
}
