use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use typed_static::{SimpleEngine, TypedAskRef, TypedReceive};
struct Counter(u64);
impl TypedReceive<u64> for Counter {
    type Reply = u64;
    fn receive_typed(&mut self, n: u64) -> Result<u64, String> {
        self.0 += n;
        Ok(self.0)
    }
}
#[test]
fn typed_spawn_rejects_an_occupied_path() {
    let engine = SimpleEngine::new();
    let _first = engine.actor(Counter(0), "a").unwrap();
    assert!(engine.actor(Counter(100), "a").is_err());
}
#[test]
fn dynamic_and_typed_spawns_share_one_path_namespace() {
    let engine = SimpleEngine::new();
    let _first = engine.actor(Counter(0), "a").unwrap();
    assert!(engine.spawn_dyn(Box::new(Ok), "a").is_err());
}
#[test]
fn dropping_engine_closes_surviving_references() {
    let engine = SimpleEngine::new();
    let actor = engine.actor(Counter(0), "a").unwrap();
    actor.ask(1).unwrap();
    drop(engine);
    assert!(
        actor.ask(1).is_err(),
        "an engine-owned worker must retire even while addresses survive"
    );
}
#[test]
fn dropping_engine_releases_actor_state_before_returning() {
    struct Tracked(Arc<AtomicUsize>);
    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl TypedReceive<u64> for Tracked {
        type Reply = u64;
        fn receive_typed(&mut self, n: u64) -> Result<u64, String> {
            Ok(n)
        }
    }
    let engine = SimpleEngine::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = engine.actor(Tracked(drops.clone()), "a").unwrap();
    actor.ask(1).unwrap();
    drop(engine);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "retirement must drop actual actor state"
    );
    drop(actor);
}

use std::sync::{mpsc, Mutex, Weak};
use std::time::Duration;
const WAIT: Duration = Duration::from_secs(3);
struct Blocker {
    entered: mpsc::Sender<()>,
    gate: mpsc::Receiver<()>,
    seen: Arc<Mutex<Vec<u64>>>,
}
impl TypedReceive<u64> for Blocker {
    type Reply = u64;
    fn receive_typed(&mut self, n: u64) -> Result<u64, String> {
        self.seen.lock().unwrap().push(n);
        if n == 0 {
            self.entered.send(()).unwrap();
            self.gate.recv().unwrap();
        }
        Ok(n)
    }
}
#[test]
fn queued_ask_expiry_prevents_late_business_side_effects() {
    let engine = SimpleEngine::new();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let seen = Arc::new(Mutex::new(vec![]));
    let actor = engine
        .actor(
            Blocker {
                entered,
                gate,
                seen: seen.clone(),
            },
            "a",
        )
        .unwrap();
    actor.tell(0).unwrap();
    ready.recv_timeout(WAIT).unwrap();
    assert!(actor
        .ask_with_timeout(1, Duration::from_millis(10))
        .is_err());
    release.send(()).unwrap();
    assert_eq!(actor.ask(2).unwrap(), 2);
    assert_eq!(*seen.lock().unwrap(), vec![0, 2]);
    engine.shutdown().unwrap();
}
#[test]
fn capacity_counts_running_work_and_rejection_returns_original_message() {
    let engine = SimpleEngine::with_capacity(2).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let actor = engine
        .actor(
            Blocker {
                entered,
                gate,
                seen: Arc::new(Mutex::new(vec![])),
            },
            "a",
        )
        .unwrap();
    actor.tell(0).unwrap();
    ready.recv_timeout(WAIT).unwrap();
    actor.tell(1).unwrap();
    let error = actor.tell(77).unwrap_err();
    assert_eq!(error.reason, "mailbox full");
    assert_eq!(error.message, 77);
    actor.stop();
    release.send(()).unwrap();
    actor.wait_stopped(WAIT).unwrap();
    engine.shutdown().unwrap();
    let error = actor.tell(88).unwrap_err();
    assert_eq!(error.message, 88);
    assert_eq!(engine.worker_counts(), (1, 1));
}
struct CountDrop(Arc<AtomicUsize>);
impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
enum Task {
    Crash,
    Pending(CountDrop),
}
struct CrashActor {
    pending_calls: Arc<AtomicUsize>,
    entered: mpsc::Sender<()>,
    gate: mpsc::Receiver<()>,
    _drop: CountDrop,
}
impl TypedReceive<Task> for CrashActor {
    type Reply = ();
    fn receive_typed(&mut self, msg: Task) -> Result<(), String> {
        match msg {
            Task::Crash => {
                self.entered.send(()).unwrap();
                self.gate.recv().unwrap();
                panic!("handler panic");
            }
            Task::Pending(probe) => {
                self.pending_calls.fetch_add(1, Ordering::SeqCst);
                drop(probe);
                panic!("pending message must never execute");
            }
        }
    }
}
#[test]
fn panic_retires_state_and_queued_messages_and_old_refs_cannot_target_reused_path() {
    let engine = SimpleEngine::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let pending_calls = Arc::new(AtomicUsize::new(0));
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let old = engine
        .actor(
            CrashActor {
                pending_calls: pending_calls.clone(),
                entered,
                gate,
                _drop: CountDrop(drops.clone()),
            },
            "a",
        )
        .unwrap();
    old.tell(Task::Crash).unwrap();
    ready.recv_timeout(WAIT).unwrap();
    old.tell(Task::Pending(CountDrop(drops.clone()))).unwrap();
    release.send(()).unwrap();
    old.wait_stopped(WAIT).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(pending_calls.load(Ordering::SeqCst), 0);
    assert_eq!(engine.alive_count(), 0);
    let newer = engine.actor(Counter(100), "a").unwrap();
    assert!(newer.generation() > old.generation());
    old.stop();
    assert!(old.tell(Task::Crash).is_err());
    assert_eq!(newer.ask(1).unwrap(), 101);
    engine.shutdown().unwrap();
    assert_eq!(engine.worker_counts(), (2, 2));
}
struct DropSignal(mpsc::Sender<()>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.send(()).unwrap();
    }
}
impl TypedReceive<()> for DropSignal {
    type Reply = ();
    fn receive_typed(&mut self, _: ()) -> Result<(), String> {
        Ok(())
    }
}
#[test]
fn dropping_last_reference_retires_actor_while_engine_is_still_alive() {
    let engine = SimpleEngine::new();
    let (dropped, done) = mpsc::channel();
    let actor = engine.actor(DropSignal(dropped), "a").unwrap();
    actor.ask(()).unwrap();
    drop(actor);
    done.recv_timeout(WAIT).unwrap();
    engine.shutdown().unwrap();
    assert_eq!(engine.alive_count(), 0);
    assert_eq!(engine.worker_counts(), (1, 1));
}
struct BlockDrop {
    entered: mpsc::Sender<()>,
    gate: mpsc::Receiver<()>,
}
impl Drop for BlockDrop {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.gate.recv().unwrap();
    }
}
impl TypedReceive<u64> for BlockDrop {
    type Reply = u64;
    fn receive_typed(&mut self, n: u64) -> Result<u64, String> {
        Ok(n)
    }
}
#[test]
fn path_stays_reserved_until_actual_actor_drop_finishes() {
    let engine = SimpleEngine::new();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let actor = engine.actor(BlockDrop { entered, gate }, "a").unwrap();
    actor.ask(1).unwrap();
    actor.stop();
    ready.recv_timeout(WAIT).unwrap();
    assert!(engine.actor(Counter(0), "a").is_err());
    assert!(actor.wait_stopped(Duration::from_millis(10)).is_err());
    release.send(()).unwrap();
    actor.wait_stopped(WAIT).unwrap();
    let fresh = engine.actor(Counter(0), "a").unwrap();
    assert_eq!(fresh.ask(42).unwrap(), 42);
    engine.shutdown().unwrap();
}
#[test]
fn concurrent_shutdown_waits_for_destructors_and_joins_workers() {
    let engine = SimpleEngine::new();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let actor = engine.actor(BlockDrop { entered, gate }, "a").unwrap();
    actor.ask(1).unwrap();
    let (started, starts) = mpsc::channel();
    let (finished, finishes) = mpsc::channel();
    let mut callers = vec![];
    for _ in 0..2 {
        let e = engine.clone();
        let started = started.clone();
        let finished = finished.clone();
        callers.push(std::thread::spawn(move || {
            started.send(()).unwrap();
            e.shutdown().unwrap();
            finished.send(()).unwrap();
        }));
    }
    starts.recv_timeout(WAIT).unwrap();
    starts.recv_timeout(WAIT).unwrap();
    ready.recv_timeout(WAIT).unwrap();
    assert!(matches!(
        finishes.recv_timeout(Duration::from_millis(30)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    release.send(()).unwrap();
    for caller in callers {
        caller.join().unwrap();
    }
    assert_eq!(engine.worker_counts(), (1, 1));
    assert_eq!(engine.alive_count(), 0);
}
struct ShutdownActor(Weak<SimpleEngine>);
impl TypedReceive<()> for ShutdownActor {
    type Reply = bool;
    fn receive_typed(&mut self, _: ()) -> Result<bool, String> {
        Ok(self.0.upgrade().unwrap().shutdown().is_err())
    }
}
#[test]
fn worker_shutdown_requests_stop_instead_of_joining_itself() {
    let engine = SimpleEngine::new();
    let actor = engine
        .actor(ShutdownActor(Arc::downgrade(&engine)), "a")
        .unwrap();
    assert!(actor.ask(()).unwrap());
    engine.shutdown().unwrap();
    assert_eq!(engine.worker_counts(), (1, 1));
}
struct ReentrantDrop(Weak<SimpleEngine>, Arc<AtomicUsize>);
impl Drop for ReentrantDrop {
    fn drop(&mut self) {
        if let Some(engine) = self.0.upgrade() {
            assert!(engine.alive_count() > 0);
            assert!(engine.shutdown().is_err());
        }
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}
impl TypedReceive<u64> for ReentrantDrop {
    type Reply = u64;
    fn receive_typed(&mut self, n: u64) -> Result<u64, String> {
        Ok(n)
    }
}
#[test]
fn actor_drop_can_reenter_engine_without_global_lock_or_join_deadlock() {
    let engine = SimpleEngine::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = engine
        .actor(ReentrantDrop(Arc::downgrade(&engine), drops.clone()), "a")
        .unwrap();
    actor.ask(42).unwrap();
    engine.shutdown().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(engine.worker_counts(), (1, 1));
}
#[test]
fn bridge_encoding_time_is_part_of_the_same_admission_deadline() {
    let engine = SimpleEngine::new();
    let actor = engine.actor(Counter(0), "a").unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(gate);
    let bridge = actor.clone().into_dyn(
        move |m| {
            entered.send(()).unwrap();
            gate.lock().unwrap().recv().unwrap();
            m.downcast::<u64>()
                .map(|n| *n)
                .map_err(|_| "wrong type".into())
        },
        |n| Box::new(n),
    );
    let (finished, done) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        let result = bridge.send_dyn_with_timeout(Box::new(9u64), Duration::from_millis(10));
        finished.send(result.is_err()).unwrap();
    });
    ready.recv_timeout(WAIT).unwrap();
    assert!(matches!(
        done.recv_timeout(Duration::from_millis(30)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    release.send(()).unwrap();
    assert!(done.recv_timeout(WAIT).unwrap());
    caller.join().unwrap();
    assert_eq!(actor.ask(0).unwrap(), 0);
    engine.shutdown().unwrap();
}
#[test]
fn adapter_panics_are_errors_and_do_not_retire_or_repeat_actor_work() {
    let engine = SimpleEngine::new();
    let actor = engine.actor(Counter(0), "a").unwrap();
    let bridge = actor.clone().into_dyn(
        |m| {
            let n = *m.downcast::<u64>().map_err(|_| "wrong type")?;
            if n == 9 {
                panic!("encoder");
            }
            Ok(n)
        },
        |n| {
            if n == 2 {
                panic!("decoder");
            }
            Box::new(n)
        },
    );
    assert_eq!(
        *bridge
            .send_dyn(Box::new(1u64))
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        1
    );
    assert!(bridge.send_dyn(Box::new(1u64)).is_err());
    assert_eq!(actor.ask(0).unwrap(), 2);
    assert!(bridge.send_dyn(Box::new(9u64)).is_err());
    assert_eq!(actor.ask(0).unwrap(), 2);
    engine.shutdown().unwrap();
}
#[test]
fn send_but_non_sync_messages_and_replies_remain_supported() {
    use std::cell::Cell;
    struct Echo;
    impl TypedReceive<Cell<u64>> for Echo {
        type Reply = Cell<u64>;
        fn receive_typed(&mut self, msg: Cell<u64>) -> Result<Cell<u64>, String> {
            msg.set(msg.get() + 1);
            Ok(msg)
        }
    }
    let engine = SimpleEngine::new();
    let actor = engine.actor(Echo, "a").unwrap();
    let clone = actor.clone();
    assert_eq!(
        std::thread::spawn(move || clone.ask(Cell::new(41)).unwrap().into_inner())
            .join()
            .unwrap(),
        42
    );
    engine.shutdown().unwrap();
}
#[test]
fn invalid_capacity_and_zero_timeout_do_not_start_business() {
    assert!(SimpleEngine::with_capacity(0).is_err());
    let engine = SimpleEngine::new();
    let actor = engine.actor(Counter(0), "a").unwrap();
    assert!(actor.ask_with_timeout(9, Duration::ZERO).is_err());
    assert_eq!(actor.ask(0).unwrap(), 0);
    engine.shutdown().unwrap();
    assert!(engine.actor(Counter(0), "b").is_err());
}

#[test]
fn discarded_reply_panic_does_not_kill_worker_or_lose_following_work() {
    struct Panics(Arc<AtomicUsize>);
    impl Drop for Panics {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("discarded reply");
        }
    }
    enum Reply {
        Panics(Panics),
        Value(u64),
    }
    struct Replies(Arc<AtomicUsize>);
    impl TypedReceive<u64> for Replies {
        type Reply = Reply;
        fn receive_typed(&mut self, n: u64) -> Result<Reply, String> {
            if n == 1 {
                Ok(Reply::Panics(Panics(self.0.clone())))
            } else {
                Ok(Reply::Value(n))
            }
        }
    }
    let engine = SimpleEngine::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = engine.actor(Replies(drops.clone()), "a").unwrap();
    actor.tell(1).unwrap();
    match actor.ask(42).unwrap() {
        Reply::Value(n) => assert_eq!(n, 42),
        Reply::Panics(p) => {
            std::mem::forget(p);
            panic!("wrong response");
        }
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    engine.shutdown().unwrap();
    assert_eq!(engine.worker_counts(), (1, 1));
}
