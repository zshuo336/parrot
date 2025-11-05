use api_neutral::{
    ActorBehaviour, BindingRegistry, BoxedMessage, BoxedResult, EngineRuntime, ErasedActor,
    Lifecycle, Spawner,
};
use core_engines::{DedicatedEngine, SharedEngine};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::ThreadId;
use std::time::Duration;
const WAIT: Duration = Duration::from_secs(3);
fn engines() -> Vec<Arc<dyn EngineRuntime>> {
    vec![
        Arc::new(DedicatedEngine::new()),
        Arc::new(SharedEngine::new(1).unwrap()),
    ]
}
type Events = Arc<Mutex<Vec<(&'static str, ThreadId)>>>;
struct Probe {
    events: Events,
    fail_start: bool,
    panic_start: bool,
    panic_stop: bool,
    panic_drop: bool,
}
impl Probe {
    fn new(events: Events) -> Self {
        Self {
            events,
            fail_start: false,
            panic_start: false,
            panic_stop: false,
            panic_drop: false,
        }
    }
    fn record(&self, event: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((event, std::thread::current().id()));
    }
}
impl ErasedActor for Probe {
    fn pre_start(&mut self) -> Result<(), String> {
        self.record("start");
        if self.panic_start {
            panic!("pre_start failure");
        }
        if self.fail_start {
            return Err("initialization refused".into());
        }
        Ok(())
    }
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult {
        self.record("receive");
        if msg.downcast_ref::<u64>() == Some(&999) {
            panic!("business failure");
        }
        Ok(Box::new(std::thread::current().id()))
    }
    fn post_stop(&mut self) {
        self.record("stop");
        if self.panic_stop {
            panic!("post_stop failure");
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.record("drop");
        if self.panic_drop {
            panic!("Drop failure");
        }
    }
}
fn labels(events: &Events) -> Vec<&'static str> {
    events
        .lock()
        .unwrap()
        .iter()
        .map(|(label, _)| *label)
        .collect()
}
#[test]
fn backends_have_real_distinct_scheduling_and_worker_affine_lifecycle() {
    for engine in engines() {
        let first = Arc::new(Mutex::new(vec![]));
        let second = Arc::new(Mutex::new(vec![]));
        let a = engine
            .spawn_erased(Box::new(Probe::new(first.clone())), "a")
            .unwrap();
        let b = engine
            .spawn_erased(Box::new(Probe::new(second.clone())), "b")
            .unwrap();
        assert_eq!(labels(&first), vec!["start"]);
        assert_eq!(labels(&second), vec!["start"]);
        let a_thread = *a.ask(Box::new(())).unwrap().downcast::<ThreadId>().unwrap();
        let b_thread = *b.ask(Box::new(())).unwrap().downcast::<ThreadId>().unwrap();
        assert_ne!(a_thread, std::thread::current().id());
        let workers = if engine.name() == "dedicated-thread" {
            assert_ne!(a_thread, b_thread);
            2
        } else {
            assert_eq!(a_thread, b_thread);
            1
        };
        engine.shutdown().unwrap();
        assert_eq!(engine.worker_counts(), (workers, workers));
        for (events, thread) in [(first, a_thread), (second, b_thread)] {
            assert_eq!(labels(&events), vec!["start", "receive", "stop", "drop"]);
            assert!(events.lock().unwrap().iter().all(|(_, id)| *id == thread));
        }
        assert!(a.ask(Box::new(())).is_err());
        assert!(b.tell(Box::new(())).is_err());
    }
}
struct Behaviour(Arc<Mutex<Vec<&'static str>>>);
impl ActorBehaviour for Behaviour {
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult {
        self.0.lock().unwrap().push("receive");
        Ok(msg)
    }
}
impl Lifecycle for Behaviour {
    fn pre_start(&mut self) -> Result<(), String> {
        self.0.lock().unwrap().push("start");
        Ok(())
    }
    fn post_stop(&mut self) {
        self.0.lock().unwrap().push("stop");
    }
}
#[test]
fn spawner_blanket_adapter_connects_optional_lifecycle() {
    for engine in engines() {
        let events = Arc::new(Mutex::new(vec![]));
        let actor = engine
            .as_ref()
            .spawn(Behaviour(events.clone()), "a")
            .unwrap();
        assert_eq!(
            *actor
                .ask(Box::new(42u64))
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        actor.stop();
        actor.wait_stopped(WAIT).unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["start", "receive", "stop"]);
        engine.shutdown().unwrap();
    }
}
#[test]
fn failed_or_panicking_initialization_cleans_up_before_returning_and_releases_path() {
    for engine in engines() {
        for panic_start in [false, true] {
            let events = Arc::new(Mutex::new(vec![]));
            let mut actor = Probe::new(events.clone());
            actor.fail_start = true;
            actor.panic_start = panic_start;
            assert!(engine.spawn_erased(Box::new(actor), "a").is_err());
            assert_eq!(labels(&events), vec!["start", "stop", "drop"]);
            let healthy = engine
                .spawn_erased(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))), "a")
                .unwrap();
            healthy.stop();
            healthy.wait_stopped(WAIT).unwrap();
        }
        engine.shutdown().unwrap();
    }
}
#[test]
fn duplicate_rejection_drops_unstarted_actor_without_lifecycle_hooks() {
    for engine in engines() {
        let a = engine
            .spawn_erased(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))), "a")
            .unwrap();
        let rejected = Arc::new(Mutex::new(vec![]));
        assert!(engine
            .spawn_erased(Box::new(Probe::new(rejected.clone())), "a")
            .is_err());
        assert_eq!(labels(&rejected), vec!["drop"]);
        a.ask(Box::new(())).unwrap();
        engine.shutdown().unwrap();
    }
}
#[test]
fn business_panic_retires_then_path_reuse_cannot_be_affected_by_old_address() {
    for engine in engines() {
        let events = Arc::new(Mutex::new(vec![]));
        let old = engine
            .spawn_erased(Box::new(Probe::new(events.clone())), "a")
            .unwrap();
        assert!(old.ask(Box::new(999u64)).is_err());
        old.wait_stopped(WAIT).unwrap();
        assert_eq!(labels(&events), vec!["start", "receive", "stop", "drop"]);
        let fresh = engine
            .spawn_erased(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))), "a")
            .unwrap();
        assert!(fresh.generation() > old.generation());
        old.stop();
        assert!(old.tell(Box::new(7u64)).is_err());
        assert!(fresh.ask(Box::new(())).is_ok());
        engine.shutdown().unwrap();
    }
}
struct Block {
    entered: mpsc::Sender<()>,
    gate: mpsc::Receiver<()>,
    calls: Arc<Mutex<Vec<u64>>>,
}
impl ErasedActor for Block {
    fn receive(&mut self, message: BoxedMessage) -> BoxedResult {
        let n = *message
            .downcast::<u64>()
            .map_err(|_| "unexpected payload")?;
        self.calls.lock().unwrap().push(n);
        if n == 0 {
            self.entered.send(()).unwrap();
            self.gate.recv().unwrap();
        }
        Ok(Box::new(n))
    }
}
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[test]
fn stop_is_out_of_band_even_when_mailbox_is_full_and_reclaims_pending_messages() {
    for engine in engines() {
        let (entered, ready) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let calls = Arc::new(Mutex::new(vec![]));
        let drops = Arc::new(AtomicUsize::new(0));
        let a = engine
            .spawn_erased(
                Box::new(Block {
                    entered,
                    gate,
                    calls: calls.clone(),
                }),
                "a",
            )
            .unwrap();
        a.tell(Box::new(0u64)).unwrap();
        ready.recv_timeout(WAIT).unwrap();
        for _ in 0..1023 {
            a.tell(Box::new(Dropped(drops.clone()))).unwrap();
        }
        let rejected = a.tell(Box::new(77u64)).unwrap_err();
        assert_eq!(*rejected.message.downcast::<u64>().unwrap(), 77);
        a.stop();
        release.send(()).unwrap();
        a.wait_stopped(WAIT).unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1023);
        assert_eq!(*calls.lock().unwrap(), vec![0]);
        engine.shutdown().unwrap();
    }
}
#[test]
fn expired_queued_requests_are_not_executed_on_either_runtime() {
    for engine in engines() {
        let (entered, ready) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let calls = Arc::new(Mutex::new(vec![]));
        let a = engine
            .spawn_erased(
                Box::new(Block {
                    entered,
                    gate,
                    calls: calls.clone(),
                }),
                "a",
            )
            .unwrap();
        a.tell(Box::new(0u64)).unwrap();
        ready.recv_timeout(WAIT).unwrap();
        assert!(a
            .ask_with_timeout(Box::new(1u64), Duration::from_millis(10))
            .is_err());
        release.send(()).unwrap();
        a.ask(Box::new(2u64)).unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![0, 2]);
        engine.shutdown().unwrap();
    }
}
#[test]
fn cleanup_panics_are_reported_without_skipping_other_cleanup_or_business() {
    for engine in engines() {
        let events = Arc::new(Mutex::new(vec![]));
        let mut bad = Probe::new(events.clone());
        bad.panic_stop = true;
        bad.panic_drop = true;
        let a = engine.spawn_erased(Box::new(bad), "a").unwrap();
        let healthy = engine
            .spawn_erased(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))), "b")
            .unwrap();
        a.stop();
        a.wait_stopped(WAIT).unwrap();
        assert_eq!(labels(&events), vec!["start", "stop", "drop"]);
        assert!(healthy.ask(Box::new(())).is_ok());
        let error = engine.shutdown().unwrap_err();
        assert!(error.contains("post_stop panicked") && error.contains("actor Drop panicked"));
        let (started, exited) = engine.worker_counts();
        assert_eq!(started, exited);
    }
}
#[test]
fn engine_owner_drop_stops_surviving_refs_and_last_ref_drop_stops_its_actor() {
    for engine in engines() {
        let events = Arc::new(Mutex::new(vec![]));
        let actor = engine
            .spawn_erased(Box::new(Probe::new(events.clone())), "a")
            .unwrap();
        drop(engine);
        assert_eq!(labels(&events), vec!["start", "stop", "drop"]);
        assert!(actor.ask(Box::new(())).is_err());
    }
    struct Signal(mpsc::Sender<()>);
    impl ErasedActor for Signal {
        fn receive(&mut self, m: BoxedMessage) -> BoxedResult {
            Ok(m)
        }
        fn post_stop(&mut self) {
            self.0.send(()).unwrap();
        }
    }
    for engine in engines() {
        let (sent, received) = mpsc::channel();
        let actor = engine.spawn_erased(Box::new(Signal(sent)), "a").unwrap();
        drop(actor);
        received.recv_timeout(WAIT).unwrap();
        engine.shutdown().unwrap();
    }
}
#[test]
fn binding_registry_instantiates_without_holding_lock_and_stale_tokens_do_not_remove_replacements()
{
    for engine in engines() {
        let registry = BindingRegistry::new();
        let callback = registry.clone();
        let token = registry
            .register("self-removing", move || {
                assert_eq!(callback.list(), vec!["self-removing"]);
                assert!(callback.unregister("self-removing"));
                Ok(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))))
            })
            .unwrap();
        let actor = registry
            .spawn("self-removing", engine.as_ref(), "a")
            .unwrap();
        assert!(registry.list().is_empty());
        let replacement = registry
            .register("self-removing", || {
                Ok(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))))
            })
            .unwrap();
        drop(token);
        assert_eq!(registry.list(), vec!["self-removing"]);
        actor.ask(Box::new(())).unwrap();
        drop(replacement);
        assert!(registry.list().is_empty());
        engine.shutdown().unwrap();
    }
}
#[test]
fn binding_factory_panic_is_an_error_and_does_not_poison_registry() {
    for engine in engines() {
        let registry = BindingRegistry::new();
        let bad = registry
            .register("bad", || panic!("factory failed"))
            .unwrap();
        assert!(registry.spawn("bad", engine.as_ref(), "a").is_err());
        assert_eq!(registry.list(), vec!["bad"]);
        drop(bad);
        let good = registry
            .register("good", || {
                Ok(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))))
            })
            .unwrap();
        let actor = registry.spawn("good", engine.as_ref(), "a").unwrap();
        actor.ask(Box::new(())).unwrap();
        drop(good);
        assert!(registry.spawn("good", engine.as_ref(), "b").is_err());
        engine.shutdown().unwrap();
    }
}

#[test]
fn startup_deadline_reports_pending_cleanup_then_owned_shutdown_finishes_it() {
    struct SlowStart {
        entered: mpsc::Sender<()>,
        gate: mpsc::Receiver<()>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }
    impl ErasedActor for SlowStart {
        fn pre_start(&mut self) -> Result<(), String> {
            self.events.lock().unwrap().push("start");
            self.entered.send(()).unwrap();
            self.gate.recv().unwrap();
            Ok(())
        }
        fn receive(&mut self, _: BoxedMessage) -> BoxedResult {
            self.events.lock().unwrap().push("receive");
            Ok(Box::new(()))
        }
        fn post_stop(&mut self) {
            self.events.lock().unwrap().push("stop");
        }
    }
    impl Drop for SlowStart {
        fn drop(&mut self) {
            self.events.lock().unwrap().push("drop");
        }
    }
    let timeout = Duration::from_millis(100);
    let engines: Vec<Arc<dyn EngineRuntime>> = vec![
        Arc::new(DedicatedEngine::with_start_timeout(timeout).unwrap()),
        Arc::new(SharedEngine::with_start_timeout(1, timeout).unwrap()),
    ];
    for engine in engines {
        let (entered, ready) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let events = Arc::new(Mutex::new(vec![]));
        let actor = SlowStart {
            entered,
            gate,
            events: events.clone(),
        };
        let caller_engine = engine.clone();
        let (finished, done) = mpsc::channel();
        let caller = std::thread::spawn(move || {
            let result = caller_engine.spawn_erased(Box::new(actor), "a");
            finished
                .send(result.err().expect("blocked initialization cannot succeed"))
                .unwrap();
        });
        ready.recv_timeout(WAIT).unwrap();
        let error = done.recv_timeout(WAIT).unwrap();
        assert!(error.contains("cleanup pending"));
        assert_eq!(*events.lock().unwrap(), vec!["start"]);
        assert!(engine
            .spawn_erased(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))), "a")
            .is_err());
        release.send(()).unwrap();
        caller.join().unwrap();
        engine.shutdown().unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["start", "stop", "drop"]);
        let (started, exited) = engine.worker_counts();
        assert_eq!(started, exited);
    }
}
#[test]
fn stopping_a_queued_shared_actor_removes_ready_entry_without_running_its_messages() {
    let engine = SharedEngine::new(1).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let blocker = engine
        .spawn_erased(
            Box::new(Block {
                entered,
                gate,
                calls: Arc::new(Mutex::new(vec![])),
            }),
            "blocker",
        )
        .unwrap();
    let events = Arc::new(Mutex::new(vec![]));
    let idle = engine
        .spawn_erased(Box::new(Probe::new(events.clone())), "idle")
        .unwrap();
    blocker.tell(Box::new(0u64)).unwrap();
    ready.recv_timeout(WAIT).unwrap();
    idle.tell(Box::new(())).unwrap();
    idle.stop();
    assert!(idle.tell(Box::new(())).is_err());
    assert!(idle.wait_stopped(Duration::from_millis(10)).is_err());
    release.send(()).unwrap();
    idle.wait_stopped(WAIT).unwrap();
    assert_eq!(labels(&events), vec!["start", "stop", "drop"]);
    assert!(blocker.ask(Box::new(2u64)).is_ok());
    engine.shutdown().unwrap();
    assert_eq!(engine.worker_counts(), (1, 1));
}

#[test]
fn panicking_panic_payload_does_not_kill_the_runtime_worker() {
    struct Payload;
    impl Drop for Payload {
        fn drop(&mut self) {
            panic!("panic payload destructor");
        }
    }
    struct Crashes;
    impl ErasedActor for Crashes {
        fn receive(&mut self, _: BoxedMessage) -> BoxedResult {
            std::panic::panic_any(Payload)
        }
    }
    let engines: Vec<Arc<dyn EngineRuntime>> = vec![
        Arc::new(DedicatedEngine::with_start_timeout(Duration::from_millis(100)).unwrap()),
        Arc::new(SharedEngine::with_start_timeout(1, Duration::from_millis(100)).unwrap()),
    ];
    for engine in engines {
        let actor = engine.spawn_erased(Box::new(Crashes), "a").unwrap();
        assert!(actor.ask(Box::new(())).is_err());
        actor.wait_stopped(WAIT).unwrap();
        let healthy = engine
            .spawn_erased(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))), "b")
            .expect("a caught payload destructor panic must not kill the worker");
        assert!(healthy.ask(Box::new(())).is_ok());
        engine.shutdown().unwrap();
    }
}

#[test]
fn global_binding_helpers_have_actual_owned_spawn_behavior() {
    let name = "batch230-global-counter";
    let token = api_neutral::register_engine_binding(name, || {
        Ok(Box::new(Probe::new(Arc::new(Mutex::new(vec![])))))
    })
    .unwrap();
    assert!(api_neutral::list_bindings().contains(&name.to_string()));
    let engine = DedicatedEngine::new();
    let actor = api_neutral::spawn_binding(name, &engine, "a").unwrap();
    actor.ask(Box::new(())).unwrap();
    drop(token);
    assert!(!api_neutral::list_bindings().contains(&name.to_string()));
    assert!(api_neutral::spawn_binding(name, &engine, "b").is_err());
    engine.shutdown().unwrap();
}
