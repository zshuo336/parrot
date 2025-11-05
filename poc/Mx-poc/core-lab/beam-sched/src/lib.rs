//! Reduction-based actor scheduling experiment.
//!
//! Each worker exclusively owns one actor for a bounded reduction turn. Actor
//! state and user callbacks never run under the global scheduling lock. Resumable
//! handlers mutate their message state, consume a budget and return Yield; the
//! scheduler can then run other actors or higher-priority messages. An arbitrary
//! blocking synchronous Rust callback cannot be instruction-preempted.

use std::any::Any;
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

pub type BoxedMessage = Box<dyn Any + Send>;
type Immediate = Box<dyn FnMut(&mut Heap, BoxedMessage) -> bool + Send>;
type Resumable =
    Box<dyn FnMut(&mut Heap, &mut (dyn Any + Send), &mut ReductionBudget) -> Step + Send>;
const DEFAULT_BUDGET: u64 = 4;
const MAILBOX_CAPACITY: usize = 1024;
thread_local! { static WORKER_OF: Cell<usize> = const { Cell::new(0) }; }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Complete,
    Yield,
    Stop,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerError {
    InvalidWorkers,
    InvalidBudget,
    ThreadSpawn(String),
    Stopped,
    UnknownActor,
    MailboxFull,
    ActorIdExhausted,
    CalledFromWorker,
    WorkerPanicked,
}
impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SchedulerError {}

/// A reduction represents one bounded unit of user work. A step that forgets to
/// charge still costs one reduction, so returning Yield cannot monopolize a turn.
pub struct ReductionBudget {
    remaining: u64,
}
impl ReductionBudget {
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
    pub fn consume(&mut self, amount: u64) -> bool {
        if amount == 0 || amount > self.remaining {
            return false;
        }
        self.remaining -= amount;
        true
    }
}
fn safe_drop<T>(value: T) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
            std::mem::forget(secondary);
        }
    }
}

pub struct Heap {
    slots: Vec<Option<BoxedMessage>>,
}
impl Heap {
    pub fn new(cap: usize) -> Self {
        Self {
            slots: (0..cap).map(|_| None).collect(),
        }
    }
    pub fn put(&mut self, index: usize, value: BoxedMessage) {
        self.slots[index] = Some(value);
    }
    pub fn take(&mut self, index: usize) -> Option<BoxedMessage> {
        self.slots[index].take()
    }
    pub fn used(&self) -> usize {
        self.slots.iter().filter(|v| v.is_some()).count()
    }
}
impl Drop for Heap {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Some(value) = slot.take() {
                safe_drop(value);
            }
        }
    }
}

enum Behaviour {
    Immediate(Immediate),
    Resumable(Resumable),
}
struct Process {
    behaviour: Behaviour,
    heap: Heap,
}
struct Work {
    message: Option<BoxedMessage>,
    priority: Priority,
}
impl Drop for Work {
    fn drop(&mut self) {
        if let Some(message) = self.message.take() {
            safe_drop(message);
        }
    }
}

/// Prefer high priority, but after eight high steps service a waiting normal or
/// low message; after four normal steps a waiting low message gets a turn.
#[derive(Default)]
struct Fairness {
    high: usize,
    normal: usize,
}
impl Fairness {
    fn choose(&mut self, ready: [bool; 3]) -> Option<usize> {
        let chosen = if ready[0] && self.normal >= 4 {
            0
        } else if ready[2] && (self.high < 8 || (!ready[0] && !ready[1])) {
            2
        } else if ready[1] {
            1
        } else if ready[0] {
            0
        } else if ready[2] {
            2
        } else {
            return None;
        };
        match chosen {
            2 => self.high = self.high.saturating_add(1).min(8),
            1 => {
                self.high = 0;
                self.normal = self.normal.saturating_add(1).min(4);
            }
            _ => {
                self.high = 0;
                self.normal = 0;
            }
        }
        Some(chosen)
    }
}
struct Slot {
    process: Option<Process>,
    mailbox: [VecDeque<Work>; 3],
    queued: Option<usize>,
    running: bool,
    stopping: bool,
    outstanding: usize,
    fairness: Fairness,
}
impl Slot {
    fn priority(&self) -> Option<usize> {
        (0..3).rev().find(|&p| !self.mailbox[p].is_empty())
    }
    fn pop(&mut self) -> Option<Work> {
        let p = self
            .fairness
            .choose(std::array::from_fn(|p| !self.mailbox[p].is_empty()))?;
        self.mailbox[p].pop_front()
    }
}
#[derive(Default)]
struct State {
    slots: BTreeMap<usize, Slot>,
    next_id: usize,
    ready: [VecDeque<usize>; 3],
    fairness: Fairness,
    closing: bool,
    retired: Vec<Slot>,
}
impl State {
    fn enqueue(&mut self, id: usize) {
        let slot = self.slots.get_mut(&id).unwrap();
        if slot.running || slot.stopping {
            return;
        }
        let desired = slot.priority();
        if desired == slot.queued {
            return;
        }
        if let Some(old) = slot.queued.take() {
            self.ready[old].retain(|&entry| entry != id);
        }
        if let Some(priority) = desired {
            self.ready[priority].push_back(id);
            slot.queued = Some(priority);
        }
    }
    fn claim(&mut self) -> Option<(usize, Process)> {
        let priority = self
            .fairness
            .choose(std::array::from_fn(|p| !self.ready[p].is_empty()))?;
        let id = self.ready[priority].pop_front().unwrap();
        let slot = self.slots.get_mut(&id).unwrap();
        slot.queued = None;
        slot.running = true;
        Some((id, slot.process.take().unwrap()))
    }
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    budget: u64,
    preemptions: Arc<AtomicU64>,
    high_dispatches: Arc<AtomicU64>,
    panics: AtomicUsize,
    started: AtomicUsize,
    exited: AtomicUsize,
}
struct WorkerExit(Arc<Shared>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.exited.fetch_add(1, Ordering::SeqCst);
        WORKER_OF.set(0);
    }
}

pub struct ReductionScheduler {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    pub preemptions: Arc<AtomicU64>,
    pub high_dispatches: Arc<AtomicU64>,
}
impl ReductionScheduler {
    pub fn new(workers: usize) -> Result<Arc<Self>, SchedulerError> {
        Self::with_budget(workers, DEFAULT_BUDGET)
    }
    pub fn with_budget(workers: usize, budget: u64) -> Result<Arc<Self>, SchedulerError> {
        if workers == 0 {
            return Err(SchedulerError::InvalidWorkers);
        }
        if budget == 0 {
            return Err(SchedulerError::InvalidBudget);
        }
        let preemptions = Arc::new(AtomicU64::new(0));
        let high_dispatches = Arc::new(AtomicU64::new(0));
        let scheduler = Arc::new(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
                budget,
                preemptions: preemptions.clone(),
                high_dispatches: high_dispatches.clone(),
                panics: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
                exited: AtomicUsize::new(0),
            }),
            workers: Mutex::new(vec![]),
            preemptions,
            high_dispatches,
        });
        for index in 0..workers {
            let shared = scheduler.shared.clone();
            match std::thread::Builder::new()
                .name(format!("poc-reduction-{index}"))
                .spawn(move || worker(shared))
            {
                Ok(worker) => scheduler.workers.lock().unwrap().push(worker),
                Err(error) => {
                    let _ = scheduler.shutdown();
                    return Err(SchedulerError::ThreadSpawn(error.to_string()));
                }
            }
        }
        Ok(scheduler)
    }
    pub fn spawn(&self, behaviour: Immediate, heap_cap: usize) -> Result<usize, SchedulerError> {
        self.insert(Behaviour::Immediate(behaviour), heap_cap)
    }
    pub fn spawn_resumable(
        &self,
        behaviour: Resumable,
        heap_cap: usize,
    ) -> Result<usize, SchedulerError> {
        self.insert(Behaviour::Resumable(behaviour), heap_cap)
    }
    fn insert(&self, behaviour: Behaviour, heap_cap: usize) -> Result<usize, SchedulerError> {
        let process = Process {
            behaviour,
            heap: Heap::new(heap_cap),
        };
        let mut state = self.shared.state.lock().unwrap();
        if state.closing {
            return Err(SchedulerError::Stopped);
        }
        let id = state.next_id;
        state.next_id = id.checked_add(1).ok_or(SchedulerError::ActorIdExhausted)?;
        state.slots.insert(
            id,
            Slot {
                process: Some(process),
                mailbox: std::array::from_fn(|_| VecDeque::new()),
                queued: None,
                running: false,
                stopping: false,
                outstanding: 0,
                fairness: Fairness::default(),
            },
        );
        Ok(id)
    }
    /// Admission is bounded; errors return the original message to its caller.
    pub fn send(
        &self,
        actor: usize,
        message: BoxedMessage,
        priority: Priority,
    ) -> Result<(), (SchedulerError, BoxedMessage)> {
        let mut state = self.shared.state.lock().unwrap();
        if state.closing {
            return Err((SchedulerError::Stopped, message));
        }
        let Some(slot) = state.slots.get_mut(&actor) else {
            return Err((SchedulerError::UnknownActor, message));
        };
        if slot.stopping {
            return Err((SchedulerError::Stopped, message));
        }
        if slot.outstanding >= MAILBOX_CAPACITY {
            return Err((SchedulerError::MailboxFull, message));
        }
        slot.outstanding += 1;
        slot.mailbox[priority as usize].push_back(Work {
            message: Some(message),
            priority,
        });
        state.enqueue(actor);
        drop(state);
        self.shared.wake.notify_one();
        Ok(())
    }
    /// Stop one actor without consuming mailbox capacity. A running callback
    /// may finish; queued messages are retired without executing their handlers.
    pub fn stop(&self, actor: usize) -> Result<(), SchedulerError> {
        let mut state = self.shared.state.lock().unwrap();
        let slot = state
            .slots
            .get_mut(&actor)
            .ok_or(SchedulerError::UnknownActor)?;
        slot.stopping = true;
        if !slot.running {
            let slot = state.slots.remove(&actor).unwrap();
            for queue in &mut state.ready {
                queue.retain(|&id| id != actor);
            }
            state.retired.push(slot);
        }
        drop(state);
        self.shared.wake.notify_all();
        Ok(())
    }
    pub fn alive_count(&self) -> usize {
        self.shared.state.lock().unwrap().slots.len()
    }
    pub fn worker_counts(&self) -> (usize, usize) {
        (
            self.shared.started.load(Ordering::SeqCst),
            self.shared.exited.load(Ordering::SeqCst),
        )
    }
    pub fn handler_panics(&self) -> usize {
        self.shared.panics.load(Ordering::SeqCst)
    }
    pub fn request_shutdown(&self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.closing = true;
            for queue in &mut state.ready {
                queue.clear();
            }
            let idle: Vec<_> = state
                .slots
                .iter()
                .filter(|(_, s)| !s.running)
                .map(|(&id, _)| id)
                .collect();
            for id in idle {
                let slot = state.slots.remove(&id).unwrap();
                state.retired.push(slot);
            }
        }
        // Workers retire user objects. Concurrent shutdown callers all join
        // those workers, so none can return while another caller still drops
        // an idle actor; destructors can also reenter without a global lock.
        self.shared.wake.notify_all();
    }

    /// Waits for active callbacks to return. Calling from a worker requests stop
    /// but returns an error instead of trying to join itself.
    pub fn shutdown(&self) -> Result<(), SchedulerError> {
        self.request_shutdown();
        if WORKER_OF.get() == Arc::as_ptr(&self.shared) as usize {
            return Err(SchedulerError::CalledFromWorker);
        }
        let mut workers = self.workers.lock().unwrap();
        let mut failed = false;
        for worker in workers.drain(..) {
            failed |= worker.join().is_err();
        }
        if failed {
            Err(SchedulerError::WorkerPanicked)
        } else {
            Ok(())
        }
    }
}
impl Drop for ReductionScheduler {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn worker(shared: Arc<Shared>) {
    WORKER_OF.set(Arc::as_ptr(&shared) as usize);
    shared.started.fetch_add(1, Ordering::SeqCst);
    let _exit = WorkerExit(shared.clone());
    loop {
        let (id, mut process) = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if !state.retired.is_empty() {
                    let retired = std::mem::take(&mut state.retired);
                    drop(state);
                    for slot in retired {
                        safe_drop(slot);
                    }
                    state = shared.state.lock().unwrap();
                    continue;
                }
                if state.closing {
                    return;
                }
                if let Some(claim) = state.claim() {
                    break claim;
                }
                state = shared.wake.wait(state).unwrap();
            }
        };
        let mut budget = ReductionBudget {
            remaining: shared.budget,
        };
        let mut stop = false;
        while budget.remaining() > 0 {
            let next = {
                let mut state = shared.state.lock().unwrap();
                if state.closing || state.slots.get(&id).unwrap().stopping {
                    None
                } else {
                    state.slots.get_mut(&id).unwrap().pop()
                }
            };
            let Some(mut work) = next else { break };
            if work.priority == Priority::High {
                shared.high_dispatches.fetch_add(1, Ordering::SeqCst);
            }
            let before = budget.remaining();
            let outcome = catch_unwind(AssertUnwindSafe(|| match &mut process.behaviour {
                Behaviour::Immediate(handler) => {
                    if handler(&mut process.heap, work.message.take().unwrap()) {
                        Step::Complete
                    } else {
                        Step::Stop
                    }
                }
                Behaviour::Resumable(handler) => handler(
                    &mut process.heap,
                    work.message.as_mut().unwrap().as_mut(),
                    &mut budget,
                ),
            }));
            if budget.remaining() == before {
                budget.consume(1);
            }
            let step = match outcome {
                Ok(step) => step,
                Err(payload) => {
                    shared.panics.fetch_add(1, Ordering::SeqCst);
                    safe_drop(payload);
                    Step::Stop
                }
            };
            if step == Step::Stop {
                stop = true;
                drop(work);
                break;
            }
            let mut state = shared.state.lock().unwrap();
            let slot = state.slots.get_mut(&id).unwrap();
            if step == Step::Yield {
                slot.mailbox[work.priority as usize].push_front(work);
                drop(state);
            } else {
                slot.outstanding -= 1;
                drop(state);
                drop(work);
            }
        }
        let retired = {
            let mut state = shared.state.lock().unwrap();
            if stop || state.closing || state.slots.get(&id).unwrap().stopping {
                let slot = state.slots.remove(&id).unwrap();
                Some((slot, process))
            } else {
                let slot = state.slots.get_mut(&id).unwrap();
                slot.running = false;
                slot.process = Some(process);
                if slot.outstanding > 0 && budget.remaining() == 0 {
                    shared.preemptions.fetch_add(1, Ordering::SeqCst);
                }
                state.enqueue(id);
                None
            }
        };
        if let Some((slot, process)) = retired {
            safe_drop(slot);
            safe_drop(process);
        }
        shared.wake.notify_all();
    }
}
