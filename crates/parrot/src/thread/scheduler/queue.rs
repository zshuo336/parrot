use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;

use crate::thread::mailbox::Mailbox;

type ReadyMailbox = Arc<dyn Mailbox>;

/// High-priority ready work is selected before normal work. Admission order
/// within a lane is preserved, including registered actors waiting for space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    High,
    Normal,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Admission {
    lane: Lane,
    ticket: u128,
}
struct Entry {
    mailbox: ReadyMailbox,
    admission: Admission,
}
struct Registration {
    mailbox: Weak<dyn Mailbox>,
    // Fixed per-actor readiness state, never one allocation per wake/message.
    // ScheduleState retains ownership until a worker actually takes this work.
    deferred: Option<Admission>,
}
#[derive(Default)]
struct State {
    normal: VecDeque<Entry>,
    high: VecDeque<Entry>,
    registered: HashMap<usize, Registration>,
    deferred_count: usize,
    next_ticket: u128,
    closed: bool,
}
impl State {
    fn len(&self) -> usize {
        self.normal.len() + self.high.len()
    }
    fn admission(&mut self, lane: Lane) -> Admission {
        let admission = Admission {
            lane,
            ticket: self.next_ticket,
        };
        self.next_ticket += 1;
        admission
    }
    fn append(&mut self, entry: Entry) {
        match entry.admission.lane {
            Lane::High => self.high.push_back(entry),
            Lane::Normal => self.normal.push_back(entry),
        }
    }
    fn oldest_deferred(&self) -> Option<(usize, Admission)> {
        if self.deferred_count == 0 {
            return None;
        }
        self.registered
            .iter()
            .filter_map(|(&id, entry)| entry.deferred.map(|admission| (id, admission)))
            .min_by_key(|(_, admission)| *admission)
    }
    fn take_deferred(&mut self, id: usize) -> Option<Entry> {
        let registration = self.registered.get_mut(&id)?;
        let admission = registration.deferred.take()?;
        self.deferred_count -= 1;
        match registration.mailbox.upgrade() {
            Some(mailbox) => Some(Entry { mailbox, admission }),
            None => {
                self.registered.remove(&id);
                None
            }
        }
    }
    fn refill(&mut self, capacity: usize) {
        while self.len() < capacity {
            let Some((id, _)) = self.oldest_deferred() else {
                break;
            };
            if let Some(entry) = self.take_deferred(id) {
                self.append(entry);
            }
        }
    }
}
fn identity(mailbox: &ReadyMailbox) -> usize {
    Arc::as_ptr(mailbox) as *const () as usize
}

/// A bounded ready queue shared by high and normal priority lanes.
///
/// `push`/`push_with_lane` are nonblocking admission operations and return the
/// mailbox unchanged when full. They never allocate an overflow queue or task.
/// The shared scheduler registers actors separately: when ready admission is
/// full, a coalesced readiness marker stays in that actor's registration. Workers
/// select the oldest such marker when draining, preserving accepted wakeups and
/// allowing more actors than queue slots. Mailbox capacities continue to apply
/// backpressure to message producers. Deferred actor count is exposed separately.
///
/// An uncontended admission/pop is O(1). Saturation scans the actor registrations
/// for the oldest eligible deferred actor; this deliberately favors bounded
/// memory and correct progress over an unbounded overflow worklist.
/// All lane/capacity decisions share one lock. User mailbox operations and
/// mailbox destruction are performed outside it.
pub struct SchedulingQueue {
    state: Mutex<State>,
    notify: Arc<Notify>,
    max_capacity: usize,
}
impl fmt::Debug for SchedulingQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().unwrap();
        f.debug_struct("SchedulingQueue")
            .field("max_capacity", &self.max_capacity)
            .field("normal_len", &state.normal.len())
            .field("high_len", &state.high.len())
            .field("deferred", &state.deferred_count)
            .finish()
    }
}
impl SchedulingQueue {
    /// Construct a queue with a strictly positive capacity, shared by both lanes.
    /// Panics for zero, like bounded channel constructors. File configuration
    /// rejects zero during validation before constructing a scheduler.
    pub fn new(max_capacity: usize) -> Self {
        assert!(
            max_capacity > 0,
            "scheduling queue capacity must be positive"
        );
        Self {
            state: Mutex::new(State::default()),
            notify: Arc::new(Notify::new()),
            max_capacity,
        }
    }
    /// Nonblocking normal-lane admission. Err returns ownership when full/closed.
    pub fn push(&self, mailbox: ReadyMailbox) -> Result<(), ReadyMailbox> {
        self.push_with_lane(mailbox, Lane::Normal)
    }
    /// Both lanes share the same capacity. Failed admission leaves the caller
    /// responsible for the mailbox; no wakeup or scheduling ownership is lost.
    pub fn push_with_lane(&self, mailbox: ReadyMailbox, lane: Lane) -> Result<(), ReadyMailbox> {
        let mut state = self.state.lock().unwrap();
        state.refill(self.max_capacity);
        if state.closed || state.len() >= self.max_capacity {
            return Err(mailbox);
        }
        let admission = state.admission(lane);
        state.append(Entry { mailbox, admission });
        drop(state);
        self.notify.notify_one();
        Ok(())
    }
    pub(crate) fn register(&self, mailbox: &ReadyMailbox) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.registered.contains_key(&identity(mailbox)) {
            return false;
        }
        state.registered.insert(
            identity(mailbox),
            Registration {
                mailbox: Arc::downgrade(mailbox),
                deferred: None,
            },
        );
        true
    }
    /// Called only by a holder of the mailbox's scheduling ownership. Full
    /// queues retain that ownership in one fixed readiness marker per actor.
    pub(crate) fn enqueue_registered(&self, mailbox: ReadyMailbox, lane: Lane) -> bool {
        let schedule = mailbox.schedule_state();
        let mut state = self.state.lock().unwrap();
        let id = identity(&mailbox);
        if state.closed || !state.registered.contains_key(&id) {
            // Release before a concurrent re-registration can publish new work.
            // The trait access happened above, outside the queue lock.
            schedule.force_release();
            return false;
        }
        if let Some(deferred) = &mut state.registered.get_mut(&id).unwrap().deferred {
            deferred.lane = deferred.lane.min(lane);
        } else {
            state.refill(self.max_capacity);
            let admission = state.admission(lane);
            if state.len() < self.max_capacity {
                state.append(Entry {
                    mailbox: mailbox.clone(),
                    admission,
                });
            } else {
                state.registered.get_mut(&id).unwrap().deferred = Some(admission);
                state.deferred_count += 1;
            }
        }
        drop(state);
        self.notify.notify_one();
        true
    }
    pub(crate) fn unregister(&self, mailbox: &ReadyMailbox) {
        let id = identity(mailbox);
        let schedule = mailbox.schedule_state();
        let mut state = self.state.lock().unwrap();
        let was_deferred = state
            .registered
            .remove(&id)
            .is_some_and(|entry| entry.deferred.is_some());
        if was_deferred {
            state.deferred_count -= 1;
        }
        let mut removed = Vec::new();
        let State { normal, high, .. } = &mut *state;
        for lane in [normal, high] {
            let mut retained = VecDeque::new();
            for entry in lane.drain(..) {
                if identity(&entry.mailbox) == id {
                    removed.push(entry);
                } else {
                    retained.push_back(entry);
                }
            }
            *lane = retained;
        }
        if was_deferred || !removed.is_empty() {
            // No worker owns this ready admission. It may be registered again.
            // A running mailbox keeps ownership until its worker releases it.
            schedule.force_release();
        }
        state.refill(self.max_capacity);
        drop(state);
        drop(removed);
        self.notify.notify_one();
    }
    /// Atomically close admission and transfer ready ownership without invoking
    /// user mailbox methods or destructors under the queue lock.
    pub(crate) fn close_and_take(&self) -> Vec<ReadyMailbox> {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        let registrations = std::mem::take(&mut state.registered);
        state.deferred_count = 0;
        let normal = std::mem::take(&mut state.normal);
        let high = std::mem::take(&mut state.high);
        drop(state);
        let mut mailboxes: Vec<_> = normal.into_iter().chain(high).map(|e| e.mailbox).collect();
        for registration in registrations.into_values() {
            if registration.deferred.is_some()
                && let Some(mailbox) = registration.mailbox.upgrade()
            {
                mailboxes.push(mailbox);
            }
        }
        self.notify.notify_waiters();
        mailboxes
    }
    #[cfg(test)]
    pub(crate) fn close(&self) {
        for mailbox in self.close_and_take() {
            mailbox.schedule_state().force_release();
        }
    }
    /// Pop the oldest high-priority admission, otherwise the oldest normal one.
    /// Deferred high work may precede a full normal lane without exceeding capacity.
    pub fn try_pop(&self) -> Option<ReadyMailbox> {
        let mut state = self.state.lock().unwrap();
        loop {
            let front = state
                .high
                .front()
                .or_else(|| state.normal.front())
                .map(|entry| entry.admission);
            let deferred = state.oldest_deferred();
            let entry = if let Some((id, _)) =
                deferred.filter(|(_, admission)| front.is_none_or(|front| *admission < front))
            {
                let Some(entry) = state.take_deferred(id) else {
                    continue;
                };
                entry
            } else {
                state
                    .high
                    .pop_front()
                    .or_else(|| state.normal.pop_front())?
            };
            state.refill(self.max_capacity);
            return Some(entry.mailbox);
        }
    }
    pub async fn pop(&self) -> ReadyMailbox {
        loop {
            // Register before inspecting the queue, including with multiple
            // concurrent consumers (Notify stores only one unclaimed permit).
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(mailbox) = self.try_pop() {
                return mailbox;
            }
            notified.await;
        }
    }
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().len()
    }
    pub fn high_len(&self) -> usize {
        self.state.lock().unwrap().high.len()
    }
    pub fn normal_len(&self) -> usize {
        self.state.lock().unwrap().normal.len()
    }
    pub fn deferred_len(&self) -> usize {
        self.state.lock().unwrap().deferred_count
    }
    pub fn capacity(&self) -> usize {
        self.max_capacity
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn notify_handle(&self) -> Arc<Notify> {
        self.notify.clone()
    }
    /// Shared access uses the same bounded admission API; raw lane mutation
    /// would bypass capacity checks and worker notifications.
    pub fn queue_handle(self: &Arc<Self>) -> Arc<Self> {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;

    fn make_mailbox(name: &str) -> Arc<dyn Mailbox> {
        Arc::new(MpscMailbox::new(4, ActorPath::placeholder(name)))
    }

    #[test]
    fn test_new_queue_starts_empty() {
        let queue = SchedulingQueue::new(16);
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.high_len(), 0);
        assert_eq!(queue.normal_len(), 0);
        assert!(queue.try_pop().is_none());
    }

    #[test]
    fn test_push_then_try_pop_fifo() {
        let queue = SchedulingQueue::new(16);
        let m1 = make_mailbox("q/a");
        let m2 = make_mailbox("q/b");

        queue.push(m1).unwrap();
        queue.push(m2).unwrap();
        assert_eq!(queue.len(), 2);
        assert!(!queue.is_empty());

        // FIFO order (best-effort for concurrent queues, deterministic here
        // because pushes and pops are sequential).
        assert_eq!(queue.try_pop().unwrap().path().path, "q/a");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/b");
        assert!(queue.is_empty());
    }

    // ------------------------------------------------------------------
    // M2 dual-lane scheduling
    // ------------------------------------------------------------------

    #[test]
    fn test_high_lane_jumps_over_normal_backlog() {
        let queue = SchedulingQueue::new(16);
        // Normal backlog: n1..n3
        for i in 1..=3 {
            queue.push(make_mailbox(&format!("q/n{}", i))).unwrap();
        }
        // High arrival after the backlog exists
        queue
            .push_with_lane(make_mailbox("q/h1"), Lane::High)
            .unwrap();

        // High must be popped first despite arriving last.
        assert_eq!(queue.try_pop().unwrap().path().path, "q/h1");
        // Then the normal backlog in FIFO order.
        assert_eq!(queue.try_pop().unwrap().path().path, "q/n1");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/n2");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/n3");
        assert!(queue.is_empty());
    }

    #[test]
    fn test_high_lane_preserves_fifo_within_lane() {
        let queue = SchedulingQueue::new(16);
        queue
            .push_with_lane(make_mailbox("q/h2"), Lane::High)
            .unwrap();
        queue
            .push_with_lane(make_mailbox("q/h1"), Lane::High)
            .unwrap();
        queue.push(make_mailbox("q/n1")).unwrap();

        assert_eq!(queue.try_pop().unwrap().path().path, "q/h2");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/h1");
        assert_eq!(queue.try_pop().unwrap().path().path, "q/n1");
    }

    #[test]
    fn test_len_counts_both_lanes() {
        let queue = SchedulingQueue::new(16);
        queue.push(make_mailbox("q/n")).unwrap();
        queue
            .push_with_lane(make_mailbox("q/h"), Lane::High)
            .unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.normal_len(), 1);
        assert_eq!(queue.high_len(), 1);
    }

    #[tokio::test]
    async fn test_async_pop_waits_until_push() {
        let queue = Arc::new(SchedulingQueue::new(16));

        // Spawn a consumer that blocks until an item appears.
        let consumer_queue = queue.clone();
        let consumer = tokio::spawn(async move {
            let mailbox = consumer_queue.pop().await;
            mailbox.path().path.clone()
        });

        // Give the consumer a moment to park, then push.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        queue.push(make_mailbox("q/late")).unwrap();

        let path = tokio::time::timeout(std::time::Duration::from_secs(2), consumer)
            .await
            .expect("consumer must wake up")
            .expect("join must succeed");
        assert_eq!(path, "q/late");
    }

    #[tokio::test]
    async fn test_async_pop_prefers_high_lane() {
        let queue = Arc::new(SchedulingQueue::new(16));
        queue.push(make_mailbox("q/n0")).unwrap(); // normal backlog exists

        let got = queue.pop().await;
        // With no high entry, normal is popped.
        assert_eq!(got.path().path, "q/n0");

        queue
            .push_with_lane(make_mailbox("q/h0"), Lane::High)
            .unwrap();
        let got = queue.pop().await;
        assert_eq!(got.path().path, "q/h0");
    }

    #[test]
    fn test_notify_handle_is_shareable() {
        let queue = SchedulingQueue::new(8);
        let notify1 = queue.notify_handle();
        let notify2 = queue.notify_handle();
        // Both handles reference the same Notify instance; use a small
        // current-thread runtime to await the notified future.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async move {
            notify1.notify_one();
            let notified = notify2.notified();
            tokio::pin!(notified);
            // The permit from notify_one is available: awaiting returns immediately.
            notified.await;
        });
    }

    #[test]
    fn test_queue_handle_shares_state() {
        let queue = Arc::new(SchedulingQueue::new(8));
        let handle = queue.queue_handle();
        handle.push(make_mailbox("q/shared")).unwrap();
        // The handle observes the same underlying queue.
        assert_eq!(queue.len(), 1);
        assert!(queue.try_pop().is_some());
    }

    #[test]
    fn test_concurrent_push_and_pop() {
        let queue = Arc::new(SchedulingQueue::new(1024));
        let producers: Vec<_> = (0..4)
            .map(|p| {
                let q = queue.clone();
                std::thread::spawn(move || {
                    for i in 0..100 {
                        q.push(make_mailbox(&format!("q/p{}/{}", p, i))).unwrap();
                    }
                })
            })
            .collect();

        for p in producers {
            p.join().unwrap();
        }
        assert_eq!(queue.len(), 400);

        let mut popped = 0;
        while queue.try_pop().is_some() {
            popped += 1;
        }
        assert_eq!(popped, 400);
    }

    #[test]
    fn test_debug_formatting() {
        let queue = SchedulingQueue::new(8);
        let repr = format!("{:?}", queue);
        assert!(repr.contains("SchedulingQueue"));
    }
    #[test]
    fn saturated_registrations_preserve_priority_fifo_and_coalesce_wakes() {
        let queue = SchedulingQueue::new(1);
        let mailboxes: Vec<_> = ["normal-1", "normal-2", "high-1", "high-2"]
            .into_iter()
            .map(make_mailbox)
            .collect();
        for (index, mailbox) in mailboxes.iter().enumerate() {
            assert!(queue.register(mailbox));
            assert!(mailbox.schedule_state().try_enqueue());
            assert!(queue.enqueue_registered(
                mailbox.clone(),
                if index < 2 { Lane::Normal } else { Lane::High }
            ));
        }
        for _ in 0..1000 {
            assert!(!mailboxes[1].schedule_state().try_enqueue());
        }
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.deferred_len(),
            3,
            "wake storms cannot allocate more ready entries"
        );
        for expected in ["high-1", "high-2", "normal-1", "normal-2"] {
            assert_eq!(queue.try_pop().unwrap().path().path, expected);
            assert!(queue.len() <= 1);
        }
        assert!(queue.try_pop().is_none());
        assert_eq!(queue.deferred_len(), 0);
    }

    #[test]
    fn rejected_admission_returns_the_same_mailbox_and_shared_handle_cannot_bypass_capacity() {
        let queue = Arc::new(SchedulingQueue::new(1));
        queue.push(make_mailbox("occupied")).unwrap();
        let rejected = make_mailbox("rejected");
        let returned = queue
            .queue_handle()
            .push_with_lane(rejected.clone(), Lane::High)
            .unwrap_err();
        assert!(Arc::ptr_eq(&rejected, &returned));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.try_pop().unwrap().path().path, "occupied");
        queue.push(returned).unwrap();
        assert_eq!(queue.try_pop().unwrap().path().path, "rejected");
    }

    #[test]
    fn unregister_removes_only_that_generation_and_promotes_waiting_work() {
        let queue = SchedulingQueue::new(1);
        let old = make_mailbox("same-path");
        let next = make_mailbox("same-path");
        let other = make_mailbox("other");
        for mailbox in [&old, &next, &other] {
            assert!(queue.register(mailbox));
            assert!(queue.enqueue_registered(mailbox.clone(), Lane::Normal));
        }
        queue.unregister(&old);
        assert!(!queue.enqueue_registered(old, Lane::High));
        assert!(Arc::ptr_eq(&queue.try_pop().unwrap(), &next));
        assert!(Arc::ptr_eq(&queue.try_pop().unwrap(), &other));
        assert!(queue.is_empty());
        assert_eq!(queue.deferred_len(), 0);
        queue.close();
        assert!(!queue.register(&next));
        assert!(queue.push(other).is_err());
    }

    #[test]
    fn a_stale_weak_registration_does_not_block_live_work() {
        let queue = SchedulingQueue::new(1);
        let first = make_mailbox("first");
        let stale = make_mailbox("stale");
        let last = make_mailbox("last");
        for mailbox in [&first, &stale, &last] {
            assert!(queue.register(mailbox));
            assert!(queue.enqueue_registered(mailbox.clone(), Lane::Normal));
        }
        drop(stale);
        assert_eq!(queue.try_pop().unwrap().path().path, "first");
        assert_eq!(queue.try_pop().unwrap().path().path, "last");
        assert_eq!(queue.deferred_len(), 0);
    }

    #[test]
    fn closing_releases_ready_ownership_but_not_a_running_worker() {
        let queue = SchedulingQueue::new(1);
        let running = make_mailbox("running");
        let resident = make_mailbox("resident");
        let deferred = make_mailbox("deferred");
        assert!(queue.register(&running));
        assert!(running.schedule_state().try_enqueue());
        assert!(queue.enqueue_registered(running.clone(), Lane::Normal));
        let _processing = queue.try_pop().unwrap();
        for mailbox in [&resident, &deferred] {
            assert!(queue.register(mailbox));
            assert!(mailbox.schedule_state().try_enqueue());
            assert!(queue.enqueue_registered(mailbox.clone(), Lane::Normal));
        }
        queue.close();
        assert!(resident.schedule_state().try_enqueue());
        assert!(deferred.schedule_state().try_enqueue());
        assert!(!running.schedule_state().try_enqueue());
        assert!(queue.is_empty());
        assert_eq!(queue.deferred_len(), 0);
    }

    #[test]
    fn concurrent_saturation_never_exceeds_capacity_or_loses_admitted_entries() {
        let queue = Arc::new(SchedulingQueue::new(3));
        let consumer_queue = queue.clone();
        let consumer = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut seen = std::collections::HashSet::new();
            while seen.len() < 400 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "lost an admitted entry"
                );
                if let Some(mailbox) = consumer_queue.try_pop() {
                    assert!(seen.insert(mailbox.path().path.clone()), "duplicate entry");
                } else {
                    std::thread::yield_now();
                }
                assert!(consumer_queue.len() <= 3);
            }
        });
        let producers: Vec<_> = (0..4)
            .map(|producer| {
                let queue = queue.clone();
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    for n in 0..100 {
                        let mut mailbox = make_mailbox(&format!("{producer}/{n}"));
                        loop {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "queue failed to release admission capacity"
                            );
                            match queue.push_with_lane(
                                mailbox,
                                if n % 2 == 0 { Lane::High } else { Lane::Normal },
                            ) {
                                Ok(()) => break,
                                Err(returned) => {
                                    mailbox = returned;
                                    std::thread::yield_now();
                                }
                            }
                        }
                        assert!(queue.len() <= 3);
                    }
                })
            })
            .collect();
        for producer in producers {
            producer.join().unwrap();
        }
        consumer.join().unwrap();
        assert!(queue.is_empty());
    }
}
