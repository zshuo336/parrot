//! An owned, flat supervision tree with bounded mailboxes and terminal DeathWatch.
//!
//! Each admitted child has one worker that exclusively owns its factory and
//! actor, including across restarts. A restart retains the child identity;
//! DeathWatch fires once, after final retirement, not on each recovered panic.
//! Escalation stops this supervisor's entire child set and closes admission.
//! Synchronous callbacks must return before stop/shutdown can finish.
use std::any::Any;
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub type BoxedMessage = Box<dyn Any + Send>;
pub type BoxedResult = Result<BoxedMessage, String>;
pub type Actor = Box<dyn FnMut(BoxedMessage) -> BoxedResult + Send>;
/// Owned by one child worker; no internal factory lock or unsafe closure clone.
pub type ChildFactory = Box<dyn FnMut() -> Actor + Send>;
const MAILBOX_CAPACITY: usize = 1024;
const MAX_WATCHERS: usize = 4096;
thread_local! { static WORKER_OF: Cell<usize> = const { Cell::new(0) }; }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeathReason {
    Normal,
    Panic(String),
    Killed,
    Escalated(String),
    NotFound,
}
impl std::fmt::Display for DeathReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    Terminated {
        path: String,
        /// None only for a watch of a path that was not registered.
        generation: Option<u64>,
        reason: DeathReason,
    },
}
#[derive(Debug, Clone)]
pub enum SupervisionStrategy {
    /// Window-limited restarts, with interruptible 10..250 ms exponential delay.
    OneForOne {
        max_restarts: usize,
        window: Duration,
    },
    Stop,
    Escalate,
}
fn safe_drop<T>(value: T) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        // Ordinary panic payloads are released too. Only a secondary panic
        // payload whose destructor may itself panic is deliberately abandoned,
        // avoiding an unbounded recursive panic during retirement.
        if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
            std::mem::forget(secondary);
        }
    }
}
struct Owned<T>(Option<T>);
impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            safe_drop(value);
        }
    }
}
struct Work {
    message: Option<BoxedMessage>,
    reply: Option<Sender<BoxedResult>>,
    deadline: Option<Instant>,
}
impl Work {
    fn complete(&mut self, result: BoxedResult) {
        if let Some(reply) = self.reply.take() {
            safe_drop(reply.send(result));
        } else {
            safe_drop(result);
        }
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        if let Some(message) = self.message.take() {
            safe_drop(message);
        }
        if let Some(reply) = self.reply.take() {
            safe_drop(reply.send(Err("actor stopped before processing message".into())));
        }
    }
}
struct Slot {
    generation: u64,
    inbox: VecDeque<Work>,
    outstanding: usize,
    stop: Option<DeathReason>,
}
struct Watch {
    generation: u64,
    tx: Sender<WatchEvent>,
}
#[derive(Default)]
struct State {
    children: BTreeMap<String, Slot>,
    watches: BTreeMap<(String, String), Watch>,
    next_generation: u64,
    closing: bool,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    started: AtomicUsize,
    exited: AtomicUsize,
    failed_workers: AtomicUsize,
}
struct WorkerExit(Arc<Shared>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.exited.fetch_add(1, Ordering::SeqCst);
        WORKER_OF.set(0);
    }
}
pub struct Supervisor {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}
impl Supervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
                started: AtomicUsize::new(0),
                exited: AtomicUsize::new(0),
                failed_workers: AtomicUsize::new(0),
            }),
            workers: Mutex::new(vec![]),
        })
    }
    /// Admit a unique child path and own its worker. Factory initialization runs
    /// asynchronously on that worker; failures obey the same supervision policy
    /// as handler panics. An ask can be used to observe successful initialization.
    pub fn spawn_supervised(
        &self,
        factory: ChildFactory,
        path: &str,
        strategy: SupervisionStrategy,
    ) -> Result<String, String> {
        // Retain a separate owner until all locks are released, including when
        // OS thread creation fails and drops the closure given to spawn.
        let startup = Arc::new(Mutex::new(Owned(Some(factory))));
        if path.is_empty() {
            return Err("empty actor path".into());
        }
        if matches!(&strategy, SupervisionStrategy::OneForOne { window, .. } if window.is_zero()) {
            return Err("restart window must be positive".into());
        }
        let mut state = self.shared.state.lock().unwrap();
        if state.closing {
            return Err("supervisor stopped".into());
        }
        if state.children.contains_key(path) {
            return Err("actor path already registered".into());
        }
        let generation = state.next_generation;
        state.next_generation = generation.checked_add(1).ok_or("generation exhausted")?;
        // Lock order: state -> workers. Shutdown releases state before joining;
        // worker reentry sees closing and never waits for the join lock.
        let mut workers = self.workers.lock().unwrap();
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                if workers.swap_remove(index).join().is_err() {
                    self.shared.failed_workers.fetch_add(1, Ordering::SeqCst);
                }
            } else {
                index += 1;
            }
        }
        state.children.insert(
            path.into(),
            Slot {
                generation,
                inbox: VecDeque::new(),
                outstanding: 0,
                stop: None,
            },
        );
        let shared = self.shared.clone();
        let path_owned = path.to_string();
        let child_startup = startup.clone();
        let result = std::thread::Builder::new()
            .name(format!("poc-supervised-{generation}"))
            .spawn(move || worker(shared, path_owned, generation, child_startup, strategy));
        match result {
            Ok(handle) => workers.push(handle),
            Err(error) => {
                state.children.remove(path);
                drop(workers);
                drop(state);
                return Err(format!("thread spawn: {error}"));
            }
        }
        drop(workers);
        drop(state);
        Ok(path.into())
    }
    /// One terminal event for this target's current generation. Repeating the
    /// same (watcher,target) pair replaces that subscription; other targets stay.
    /// A missing target immediately yields NotFound. At most 4096 subscriptions.
    pub fn watch(&self, watcher: &str, target: &str) -> Result<Receiver<WatchEvent>, String> {
        let (tx, rx) = channel();
        let mut state = self.shared.state.lock().unwrap();
        let Some(generation) = state.children.get(target).map(|s| s.generation) else {
            drop(state);
            let _ = tx.send(WatchEvent::Terminated {
                path: target.into(),
                generation: None,
                reason: DeathReason::NotFound,
            });
            return Ok(rx);
        };
        let key = (watcher.into(), target.into());
        if state.watches.len() >= MAX_WATCHERS && !state.watches.contains_key(&key) {
            return Err("watch capacity exceeded".into());
        }
        state.watches.insert(key, Watch { generation, tx });
        Ok(rx)
    }
    pub fn unwatch(&self, watcher: &str) {
        self.shared
            .state
            .lock()
            .unwrap()
            .watches
            .retain(|(w, _), _| w != watcher);
    }
    pub fn unwatch_target(&self, watcher: &str, target: &str) {
        self.shared
            .state
            .lock()
            .unwrap()
            .watches
            .remove(&(watcher.into(), target.into()));
    }
    fn enqueue(&self, path: &str, work: Work) -> Result<(), (String, Work)> {
        let mut state = self.shared.state.lock().unwrap();
        if state.closing {
            return Err(("supervisor stopped".into(), work));
        }
        let Some(slot) = state.children.get_mut(path) else {
            return Err(("actor not found".into(), work));
        };
        if slot.stop.is_some() {
            return Err(("actor stopping".into(), work));
        }
        if slot.outstanding >= MAILBOX_CAPACITY {
            return Err(("mailbox full".into(), work));
        }
        slot.outstanding += 1;
        slot.inbox.push_back(work);
        drop(state);
        self.shared.wake.notify_all();
        Ok(())
    }
    /// Admission only. Rejection returns the original message to the caller.
    pub fn tell(&self, path: &str, msg: BoxedMessage) -> Result<(), (String, BoxedMessage)> {
        self.enqueue(
            path,
            Work {
                message: Some(msg),
                reply: None,
                deadline: None,
            },
        )
        .map_err(|(error, mut work)| (error, work.message.take().unwrap()))
    }
    pub fn ask(&self, path: &str, msg: BoxedMessage) -> BoxedResult {
        self.ask_with_timeout(path, msg, Duration::from_secs(5))
    }
    /// The timeout includes queue residence. Expired queued work is never
    /// started; a callback already running cannot be forcibly cancelled.
    pub fn ask_with_timeout(
        &self,
        path: &str,
        msg: BoxedMessage,
        timeout: Duration,
    ) -> BoxedResult {
        if timeout.is_zero() {
            safe_drop(msg);
            return Err("ask timeout must be positive".into());
        }
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            safe_drop(msg);
            return Err("ask timeout is too large".into());
        };
        let (tx, rx) = channel();
        if let Err((error, work)) = self.enqueue(
            path,
            Work {
                message: Some(msg),
                reply: Some(tx),
                deadline: Some(deadline),
            },
        ) {
            drop(work);
            return Err(error);
        }
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| e.to_string())?
    }
    /// Request normal termination, preserving the path until all user resources
    /// retire. A previously registered watch observes completion.
    pub fn stop_child(&self, path: &str) -> Result<(), String> {
        let mut state = self.shared.state.lock().unwrap();
        let slot = state.children.get_mut(path).ok_or("actor not found")?;
        if slot.stop.is_none() {
            slot.stop = Some(DeathReason::Normal);
        }
        drop(state);
        self.shared.wake.notify_all();
        Ok(())
    }
    pub fn alive_count(&self) -> usize {
        self.shared.state.lock().unwrap().children.len()
    }
    pub fn worker_counts(&self) -> (usize, usize) {
        (
            self.shared.started.load(Ordering::SeqCst),
            self.shared.exited.load(Ordering::SeqCst),
        )
    }
    pub fn request_shutdown(&self) {
        close_tree(&self.shared);
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.request_shutdown();
        if WORKER_OF.get() == Arc::as_ptr(&self.shared) as usize {
            return Err("shutdown requested from worker; cannot join itself".into());
        }
        let mut workers = self.workers.lock().unwrap();
        for handle in workers.drain(..) {
            if handle.join().is_err() {
                self.shared.failed_workers.fetch_add(1, Ordering::SeqCst);
            }
        }
        if self.shared.failed_workers.load(Ordering::SeqCst) != 0 {
            Err("worker panicked outside supervision boundary".into())
        } else {
            Ok(())
        }
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
fn close_tree(shared: &Shared) {
    let mut state = shared.state.lock().unwrap();
    state.closing = true;
    for slot in state.children.values_mut() {
        if slot.stop.is_none() {
            slot.stop = Some(DeathReason::Killed);
        }
    }
    drop(state);
    shared.wake.notify_all();
}
fn stop_reason(shared: &Shared, path: &str) -> Option<DeathReason> {
    shared
        .state
        .lock()
        .unwrap()
        .children
        .get(path)
        .unwrap()
        .stop
        .clone()
}
fn panic_reason(payload: BoxedMessage) -> DeathReason {
    let message = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "opaque panic payload".into()
    };
    safe_drop(payload);
    DeathReason::Panic(message)
}
/// None means restart; Some means this child must retire.
fn after_failure(
    shared: &Shared,
    path: &str,
    strategy: &SupervisionStrategy,
    restarts: &mut VecDeque<Instant>,
    reason: DeathReason,
) -> Option<DeathReason> {
    if let Some(stop) = stop_reason(shared, path) {
        return Some(stop);
    }
    match strategy {
        SupervisionStrategy::Stop => Some(reason),
        SupervisionStrategy::Escalate => {
            close_tree(shared);
            Some(DeathReason::Escalated(reason.to_string()))
        }
        SupervisionStrategy::OneForOne {
            max_restarts,
            window,
        } => {
            let now = Instant::now();
            restarts.retain(|t| now.duration_since(*t) < *window);
            if restarts.len() >= *max_restarts {
                close_tree(shared);
                return Some(DeathReason::Escalated(format!(
                    "restart budget exhausted: {path}: {reason}"
                )));
            }
            restarts.push_back(now);
            let delay = Duration::from_millis((10u64 << (restarts.len() - 1).min(5)).min(250));
            let state = shared.state.lock().unwrap();
            let (state, _) = shared
                .wake
                .wait_timeout_while(state, delay, |s| {
                    s.children.get(path).unwrap().stop.is_none()
                })
                .unwrap();
            state.children.get(path).unwrap().stop.clone()
        }
    }
}
fn worker(
    shared: Arc<Shared>,
    path: String,
    generation: u64,
    startup: Arc<Mutex<Owned<ChildFactory>>>,
    strategy: SupervisionStrategy,
) {
    WORKER_OF.set(Arc::as_ptr(&shared) as usize);
    shared.started.fetch_add(1, Ordering::SeqCst);
    let _exit = WorkerExit(shared.clone());
    let mut factory = startup.lock().unwrap().0.take().unwrap();
    drop(startup);
    let mut actor: Option<Actor> = None;
    let mut restarts = VecDeque::new();
    let reason = loop {
        if let Some(stop) = stop_reason(&shared, &path) {
            break stop;
        }
        if actor.is_none() {
            match catch_unwind(AssertUnwindSafe(&mut factory)) {
                Ok(instance) => actor = Some(instance),
                Err(payload) => {
                    if let Some(reason) = after_failure(
                        &shared,
                        &path,
                        &strategy,
                        &mut restarts,
                        panic_reason(payload),
                    ) {
                        break reason;
                    }
                    continue;
                }
            }
        }
        let next = {
            let mut state = shared.state.lock().unwrap();
            loop {
                let slot = state.children.get_mut(&path).unwrap();
                if slot.stop.is_some() {
                    break None;
                }
                if let Some(work) = slot.inbox.pop_front() {
                    break Some(work);
                }
                state = shared.wake.wait(state).unwrap();
            }
        };
        let Some(mut work) = next else {
            continue;
        };
        let failure = if work.deadline.is_some_and(|d| Instant::now() >= d) {
            work.complete(Err("ask expired before execution".into()));
            None
        } else {
            match catch_unwind(AssertUnwindSafe(|| {
                actor.as_mut().unwrap()(work.message.take().unwrap())
            })) {
                Ok(reply) => {
                    work.complete(reply);
                    None
                }
                Err(payload) => {
                    let reason = panic_reason(payload);
                    work.complete(Err(reason.to_string()));
                    Some(reason)
                }
            }
        };
        {
            let mut state = shared.state.lock().unwrap();
            state.children.get_mut(&path).unwrap().outstanding -= 1;
        }
        drop(work);
        if let Some(reason) = failure {
            safe_drop(actor.take());
            if let Some(reason) = after_failure(&shared, &path, &strategy, &mut restarts, reason) {
                break reason;
            }
        }
    };
    // Close this identity before user destructors reenter. Preserve it until
    // actor, factory and all pending messages have actually retired.
    let pending = {
        let mut state = shared.state.lock().unwrap();
        let slot = state.children.get_mut(&path).unwrap();
        slot.stop = Some(reason.clone());
        std::mem::take(&mut slot.inbox)
    };
    safe_drop(actor);
    safe_drop(factory);
    drop(pending); // each Work contains its own destructor boundary
    let watches = {
        let mut state = shared.state.lock().unwrap();
        assert_eq!(state.children.get(&path).unwrap().generation, generation);
        state.children.remove(&path);
        let keys: Vec<_> = state
            .watches
            .iter()
            .filter(|((_, target), watch)| target == &path && watch.generation == generation)
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .map(|key| state.watches.remove(&key).unwrap())
            .collect::<Vec<_>>()
    };
    for watch in watches {
        let _ = watch.tx.send(WatchEvent::Terminated {
            path: path.clone(),
            generation: Some(generation),
            reason: reason.clone(),
        });
    }
}
