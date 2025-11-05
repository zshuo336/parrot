//! The same derived actor executes on real dedicated and shared-worker backends.
use api_neutral::{
    ActorBehaviour, BindingRegistry, BoxedMessage, BoxedResult, EngineRuntime, Lifecycle,
};
use core_engines::{DedicatedEngine, SharedEngine};
use derive_decouple::ParrotActorNeutral;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
#[derive(ParrotActorNeutral)]
#[neutral(crate = "api_neutral", lifecycle)]
struct Counter {
    n: u64,
    events: Arc<Mutex<Vec<&'static str>>>,
}
impl ActorBehaviour for Counter {
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult {
        self.events.lock().unwrap().push("receive");
        self.n += *msg.downcast::<u64>().map_err(|_| "wrong message")?;
        Ok(Box::new(self.n))
    }
}
impl Lifecycle for Counter {
    fn pre_start(&mut self) -> Result<(), String> {
        self.events.lock().unwrap().push("start");
        Ok(())
    }
    fn post_stop(&mut self) {
        self.events.lock().unwrap().push("stop");
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.events.lock().unwrap().push("drop");
    }
}
#[test]
fn derived_dispatch_and_lifecycle_work_through_real_dyn_engines() {
    let engines: Vec<Box<dyn EngineRuntime>> = vec![
        Box::new(DedicatedEngine::new()),
        Box::new(SharedEngine::new(1).unwrap()),
    ];
    for engine in engines {
        let events = Arc::new(Mutex::new(vec![]));
        let actor = Counter {
            n: 40,
            events: events.clone(),
        }
        .spawn_on(engine.as_ref(), "counter")
        .unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["start"]);
        actor.tell(Box::new(1u64)).unwrap();
        assert_eq!(
            *actor
                .ask(Box::new(1u64))
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        actor.stop();
        actor.wait_stopped(Duration::from_secs(3)).unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec!["start", "receive", "receive", "stop", "drop"]
        );
        engine.shutdown().unwrap();
        assert_eq!(engine.worker_counts(), (1, 1));
    }
}
static CONSTRUCTED: AtomicUsize = AtomicUsize::new(0);
#[derive(ParrotActorNeutral)]
#[neutral(crate = "api_neutral", binding = "counter")]
struct Registered(u64);
impl Default for Registered {
    fn default() -> Self {
        CONSTRUCTED.fetch_add(1, Ordering::SeqCst);
        Self(42)
    }
}
impl ActorBehaviour for Registered {
    fn receive(&mut self, _: BoxedMessage) -> BoxedResult {
        Ok(Box::new(self.0))
    }
}
#[test]
fn generated_binding_is_lazy_owned_and_actually_instantiates_on_selected_runtime() {
    let registry = BindingRegistry::new();
    let before = CONSTRUCTED.load(Ordering::SeqCst);
    let token = Registered::register_binding(&registry).unwrap();
    assert_eq!(CONSTRUCTED.load(Ordering::SeqCst), before);
    assert!(Registered::register_binding(&registry).is_err());
    let engine = SharedEngine::new(1).unwrap();
    let actor = registry.spawn("counter", &engine, "registered").unwrap();
    assert_eq!(CONSTRUCTED.load(Ordering::SeqCst), before + 1);
    assert_eq!(
        *actor.ask(Box::new(())).unwrap().downcast::<u64>().unwrap(),
        42
    );
    drop(token);
    assert!(registry.list().is_empty());
    assert!(registry.spawn("counter", &engine, "missing").is_err());
    assert!(actor.ask(Box::new(())).is_ok());
    engine.shutdown().unwrap();
}
