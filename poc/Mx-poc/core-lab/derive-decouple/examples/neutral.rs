//! One derived behaviour, owned registration and lifecycle, two real schedulers.
use api_neutral::{
    ActorBehaviour, BindingRegistry, BoxedMessage, BoxedResult, EngineRuntime, Lifecycle,
};
use core_engines::{DedicatedEngine, SharedEngine};
use derive_decouple::ParrotActorNeutral;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread::ThreadId;
enum Message {
    Add(u64),
    Identity,
}
#[derive(ParrotActorNeutral)]
#[neutral(crate = "api_neutral", lifecycle)]
struct Counter {
    n: u64,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}
impl ActorBehaviour for Counter {
    fn receive(&mut self, msg: BoxedMessage) -> BoxedResult {
        match *msg.downcast::<Message>().map_err(|_| "wrong type")? {
            Message::Add(n) => {
                self.n += n;
                Ok(Box::new(self.n))
            }
            Message::Identity => Ok(Box::new(std::thread::current().id())),
        }
    }
}
impl Lifecycle for Counter {
    fn pre_start(&mut self) -> Result<(), String> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn post_stop(&mut self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
}
fn main() {
    let engines: Vec<Box<dyn EngineRuntime>> = vec![
        Box::new(DedicatedEngine::new()),
        Box::new(SharedEngine::new(1).unwrap()),
    ];
    for engine in engines {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let a = Counter {
            n: 0,
            starts: starts.clone(),
            stops: stops.clone(),
        }
        .spawn_on(engine.as_ref(), "a")
        .unwrap();
        let registry = BindingRegistry::new();
        let factory_starts = starts.clone();
        let factory_stops = stops.clone();
        let token = registry
            .register("counter", move || {
                Ok(Box::new(Counter {
                    n: 100,
                    starts: factory_starts.clone(),
                    stops: factory_stops.clone(),
                }))
            })
            .unwrap();
        let b = registry.spawn("counter", engine.as_ref(), "b").unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(
            *a.ask(Box::new(Message::Add(42)))
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        assert_eq!(
            *b.ask(Box::new(Message::Add(1)))
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            101
        );
        let a_thread = *a
            .ask(Box::new(Message::Identity))
            .unwrap()
            .downcast::<ThreadId>()
            .unwrap();
        let b_thread = *b
            .ask(Box::new(Message::Identity))
            .unwrap()
            .downcast::<ThreadId>()
            .unwrap();
        let expected = if engine.name() == "dedicated-thread" {
            assert_ne!(a_thread, b_thread);
            2
        } else {
            assert_eq!(a_thread, b_thread);
            1
        };
        drop(token);
        assert!(registry.list().is_empty());
        engine.shutdown().unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), 2);
        assert_eq!(engine.worker_counts(), (expected, expected));
        println!("{}: counter values 42/101; threads {:?}/{:?}; both lifecycle hooks completed; workers {:?}",engine.name(),a_thread,b_thread,engine.worker_counts());
    }
}
