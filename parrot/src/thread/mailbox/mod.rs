//! Mailbox abstraction for the thread-based actor system.

use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_trait::async_trait;

use parrot_api::address::ActorPath;
use parrot_api::types::BoxedMessage;

use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::processor::ProcessorInterface;

pub mod mpsc;
pub mod spsc;
pub mod spsc_ringbuf;

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
    /// 供旧消费路径（`pop` + downcast::<AskEnvelope>）使用。
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

/// Scheduling-slot state ensuring at most one queue entry (and at most one
/// owning worker) per mailbox.
///
/// The state machine is:
/// - `queued_or_owned == false`: idle; anyone may acquire the slot and push
///   the mailbox into the scheduling queue.
/// - `queued_or_owned == true`: the mailbox is either sitting in the
///   scheduling queue or is being processed by a worker. Concurrent enqueue
///   attempts are rejected and recorded in `pending_wake` so the owning
///   worker takes over the re-queue on release instead of losing the wakeup.
///
/// This preserves actor semantics: a single actor's mailbox is processed by
/// exactly one worker at a time.
#[derive(Debug, Default)]
pub struct ScheduleState {
    queued_or_owned: AtomicBool,
    pending_wake: AtomicBool,
}

impl ScheduleState {
    /// Attempt to acquire the scheduling slot.
    ///
    /// Returns `true` when the caller should push the mailbox into the
    /// scheduling queue (the slot was acquired). Returns `false` when the
    /// mailbox is already queued or owned; the attempt is recorded in
    /// `pending_wake` so the current owner re-queues on release.
    pub fn try_enqueue(&self) -> bool {
        if !self.queued_or_owned.swap(true, Ordering::AcqRel) {
            return true;
        }
        self.pending_wake.store(true, Ordering::Release);
        // Close the lost-wakeup window: the owner's `release` may have
        // completed its final `pending_wake` check *before* the store above
        // became visible and freed the slot (`queued_or_owned == false`),
        // in which case nobody re-queues this mailbox. Re-check the slot
        // with an RMW: if the owner fully released, this caller takes over
        // the enqueue responsibility itself. If the slot is still held
        // (owner mid-batch or a concurrent re-queue in flight), the flag
        // stays set and the owner's release path observes it.
        if !self.queued_or_owned.swap(true, Ordering::AcqRel) {
            self.pending_wake.store(false, Ordering::Release);
            return true;
        }
        false
    }

    /// Worker-side release after finishing a batch.
    ///
    /// Returns `true` when the mailbox must be re-queued (more messages are
    /// pending, or a push raced with this release). Returns `false` when the
    /// slot was freed and no re-queue is needed.
    pub fn release(&self, has_more: bool) -> bool {
        if has_more {
            return true; // still owns the slot; caller re-queues directly.
        }
        if self.pending_wake.swap(false, Ordering::AcqRel) {
            return true; // a push raced us; keep the slot and re-queue.
        }
        self.queued_or_owned.store(false, Ordering::Release);
        // A hook may have landed between the pending_wake swap above and the
        // store: its slot CAS failed (we still owned it) and it set
        // pending_wake. Take the slot back and re-queue on its behalf.
        if self.pending_wake.swap(false, Ordering::AcqRel) {
            self.queued_or_owned.store(true, Ordering::Release);
            return true;
        }
        false
    }

    /// Drop the slot unconditionally (error paths). Any pending wakeup is
    /// discarded; the next successful push re-enqueues the mailbox.
    pub fn force_release(&self) {
        self.pending_wake.store(false, Ordering::Release);
        self.queued_or_owned.store(false, Ordering::Release);
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

    /// Close the mailbox: further pushes fail, buffered messages are drained.
    async fn close(&self);

    /// Whether the mailbox is closed.
    async fn is_closed(&self) -> bool {
        false
    }

    /// Associate a processor with this mailbox (interior mutability).
    fn set_processor(&self, processor: Arc<dyn ProcessorInterface>);

    /// Get the associated processor, if any.
    fn get_processor(&self) -> Option<Arc<dyn ProcessorInterface>>;

    /// Whether a processor is associated with this mailbox.
    fn has_processor(&self) -> bool;

    /// Install the wake hook fired after each successful push.
    ///
    /// Schedulers use this to re-enqueue the mailbox when new messages arrive
    /// while no worker currently owns it.
    fn set_wake_hook(&self, hook: WakeHook);

    /// Fire the installed wake hook (no-op when none is installed).
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
