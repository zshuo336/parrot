//! Callbacks are arbitrary user code. Exercise reentry in bounded child processes
//! so a lock regression fails instead of hanging the complete test harness.
use parrot_api::engine::EngineCatalog;
use parrot_api::engine::{self, BoxedActorRefErased, EngineRuntime, ErasedActor};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Runtime {
    key: &'static str,
    name_reentry: bool,
    drop_reentry: bool,
    name_panic: bool,
    drop_panic: bool,
}
impl Runtime {
    fn plain(key: &'static str) -> Self {
        Self {
            key,
            name_reentry: false,
            drop_reentry: false,
            name_panic: false,
            drop_panic: false,
        }
    }
}
impl EngineRuntime for Runtime {
    fn name(&self) -> &'static str {
        if self.name_reentry {
            let _ = engine::list_engines();
        }
        assert!(!self.name_panic, "name panic");
        self.key
    }
    fn spawn_erased(
        &self,
        _: Box<dyn ErasedActor>,
        _: &str,
    ) -> Result<BoxedActorRefErased, String> {
        Err("fixture has no actors".into())
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if self.drop_reentry {
            assert!(engine::lookup_engine(self.key).is_some());
        }
        assert!(!self.drop_panic, "runtime Drop panic");
    }
}

fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_CATALOG_CASE").ok().as_deref() == Some(name) {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_CATALOG_CASE", name)
        .spawn()
        .unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child contract failed: {name}: {status}");
            return;
        }
        if std::time::Instant::now() >= end {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("registry callback deadlocked: {name}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn runtime_name_can_query_the_global_catalog() {
    isolated("runtime_name_can_query_the_global_catalog", || {
        engine::register_engine(Arc::new(Runtime {
            name_reentry: true,
            ..Runtime::plain("reentrant-name")
        }));
        assert!(engine::lookup_engine("reentrant-name").is_some());
    });
}

#[test]
fn replaced_runtime_drop_can_query_the_global_catalog() {
    isolated("replaced_runtime_drop_can_query_the_global_catalog", || {
        engine::register_engine(Arc::new(Runtime {
            drop_reentry: true,
            ..Runtime::plain("reentrant-drop")
        }));
        engine::register_engine(Arc::new(Runtime::plain("reentrant-drop")));
        assert!(engine::lookup_engine("reentrant-drop").is_some());
    });
}

#[test]
fn runtime_name_panic_does_not_poison_the_catalog() {
    isolated("runtime_name_panic_does_not_poison_the_catalog", || {
        assert!(
            std::panic::catch_unwind(|| engine::register_engine(Arc::new(Runtime {
                name_panic: true,
                ..Runtime::plain("name-panic")
            })))
            .is_err()
        );
        engine::register_engine(Arc::new(Runtime::plain("healthy")));
        assert!(engine::lookup_engine("healthy").is_some());
        assert!(engine::lookup_engine("name-panic").is_none());
    });
}

#[test]
fn replaced_runtime_panic_does_not_poison_the_catalog() {
    isolated("replaced_runtime_panic_does_not_poison_the_catalog", || {
        engine::register_engine(Arc::new(Runtime {
            drop_panic: true,
            ..Runtime::plain("drop-panic")
        }));
        let result = std::panic::catch_unwind(|| {
            engine::register_engine(Arc::new(Runtime::plain("drop-panic")))
        });
        assert!(result.is_err());
        assert!(engine::lookup_engine("drop-panic").is_some());
        engine::register_engine(Arc::new(Runtime::plain("healthy")));
    });
}

struct CountedRuntime {
    key: &'static str,
    drops: Arc<AtomicUsize>,
    on_drop: Option<Box<dyn Fn() + Send + Sync>>,
}
impl EngineRuntime for CountedRuntime {
    fn name(&self) -> &'static str {
        self.key
    }
    fn spawn_erased(
        &self,
        _: Box<dyn ErasedActor>,
        _: &str,
    ) -> Result<BoxedActorRefErased, String> {
        Err("counted fixture".into())
    }
}
impl Drop for CountedRuntime {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(callback) = &self.on_drop {
            callback();
        }
    }
}

#[test]
fn catalog_ownership_isolated_sorted_and_lookup_handles_survive_removal() {
    let catalog = EngineCatalog::new();
    let other = EngineCatalog::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let token = catalog
        .register(Arc::new(CountedRuntime {
            key: "b",
            drops: drops.clone(),
            on_drop: None,
        }))
        .unwrap();
    assert_eq!(token.name(), "b");
    assert!(catalog.insert(Arc::new(Runtime::plain("a"))).is_none());
    assert!(catalog.insert(Arc::new(Runtime::plain("c"))).is_none());
    assert_eq!(catalog.clone().list(), ["a", "b", "c"]);
    assert!(other.list().is_empty());
    let captured = catalog.lookup("b").unwrap();
    let removed = token.unregister().unwrap();
    assert!(Arc::ptr_eq(&captured, &removed));
    assert!(token.unregister().is_none());
    assert!(catalog.lookup("b").is_none());
    drop(removed);
    drop(token);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(captured.name(), "b");
    drop(captured);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn stale_tokens_cannot_remove_replaced_or_reregistered_engines() {
    let catalog = EngineCatalog::new();
    let first = catalog.register(Arc::new(Runtime::plain("same"))).unwrap();
    let newer: Arc<dyn EngineRuntime> = Arc::new(Runtime::plain("same"));
    assert!(catalog.insert(newer.clone()).is_some());
    assert!(first.unregister().is_none());
    drop(first);
    assert!(Arc::ptr_eq(&catalog.lookup("same").unwrap(), &newer));
    catalog.remove("same");
    let second = catalog.register(newer.clone()).unwrap();
    catalog.remove("same");
    let third = catalog.register(newer.clone()).unwrap();
    drop(second); // Same Arc runtime, different registration identity.
    assert!(Arc::ptr_eq(&catalog.lookup("same").unwrap(), &newer));
    drop(third);
    assert!(catalog.list().is_empty());
}

#[test]
fn scoped_token_does_not_keep_catalog_or_runtime_alive() {
    let drops = Arc::new(AtomicUsize::new(0));
    let catalog = EngineCatalog::new();
    let token = catalog
        .register(Arc::new(CountedRuntime {
            key: "weak",
            drops: drops.clone(),
            on_drop: None,
        }))
        .unwrap();
    drop(catalog);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(token.unregister().is_none());
    drop(token);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_scoped_registration_has_exactly_one_owner() {
    let catalog = EngineCatalog::new();
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let catalog = catalog.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                catalog.register(Arc::new(Runtime::plain("race")))
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(results
        .iter()
        .filter_map(|r| r.as_ref().err())
        .all(|e| e.0 == "race"));
    assert_eq!(catalog.list(), ["race"]);
    drop(results);
    assert!(catalog.list().is_empty());
}

#[test]
fn rejected_and_scoped_runtime_destructors_can_reenter_catalog() {
    isolated(
        "rejected_and_scoped_runtime_destructors_can_reenter_catalog",
        || {
            let catalog = EngineCatalog::new();
            let drops = Arc::new(AtomicUsize::new(0));
            let check = catalog.clone();
            let token = catalog
                .register(Arc::new(CountedRuntime {
                    key: "owned",
                    drops: drops.clone(),
                    on_drop: Some(Box::new(move || assert!(check.lookup("owned").is_none()))),
                }))
                .unwrap();
            let check = catalog.clone();
            let rejected = catalog.register(Arc::new(CountedRuntime {
                key: "owned",
                drops: drops.clone(),
                on_drop: Some(Box::new(move || assert!(check.lookup("owned").is_some()))),
            }));
            assert!(rejected.is_err());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(token);
            assert_eq!(drops.load(Ordering::SeqCst), 2);
            assert!(catalog.list().is_empty());
        },
    );
}

#[test]
fn scoped_cleanup_is_safe_during_unwinding_and_drop_panic() {
    isolated(
        "scoped_cleanup_is_safe_during_unwinding_and_drop_panic",
        || {
            let catalog = EngineCatalog::new();
            let drops = Arc::new(AtomicUsize::new(0));
            let token = catalog
                .register(Arc::new(CountedRuntime {
                    key: "unwind",
                    drops: drops.clone(),
                    on_drop: Some(Box::new(|| panic!("user runtime destructor"))),
                }))
                .unwrap();
            let outer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _owned = token;
                panic!("outer failure");
            }));
            assert!(outer.is_err());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(catalog.list().is_empty());
            let healthy = catalog
                .register(Arc::new(Runtime::plain("healthy")))
                .unwrap();
            assert!(catalog.lookup("healthy").is_some());
            drop(healthy);
        },
    );
}

#[test]
fn global_scoped_and_persistent_registrations_share_identity_rules() {
    isolated(
        "global_scoped_and_persistent_registrations_share_identity_rules",
        || {
            let token = engine::register_engine_scoped(Arc::new(Runtime::plain("global"))).unwrap();
            assert!(engine::register_engine_scoped(Arc::new(Runtime::plain("global"))).is_err());
            engine::register_engine(Arc::new(Runtime::plain("global")));
            drop(token);
            assert!(engine::lookup_engine("global").is_some());
            assert!(engine::unregister_engine("global").is_some());
            assert!(engine::unregister_engine("global").is_none());
        },
    );
}

struct DummyActor(Arc<AtomicUsize>);
impl ErasedActor for DummyActor {
    fn receive(&mut self, message: parrot_api::types::BoxedMessage) -> engine::BoxedResult {
        Ok(message)
    }
}
impl Drop for DummyActor {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct SpawningRuntime {
    entered: std::sync::mpsc::SyncSender<String>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    drops: Arc<AtomicUsize>,
}
impl EngineRuntime for SpawningRuntime {
    fn name(&self) -> &'static str {
        "selected"
    }
    fn spawn_erased(
        &self,
        mut actor: Box<dyn ErasedActor>,
        path: &str,
    ) -> Result<BoxedActorRefErased, String> {
        self.entered.send(path.to_string()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        assert_eq!(
            *actor
                .receive(Box::new(42_u64))
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        Err("original runtime completed".into())
    }
}
impl Drop for SpawningRuntime {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
#[test]
fn spawn_captures_one_runtime_without_holding_catalog_lock() {
    isolated(
        "spawn_captures_one_runtime_without_holding_catalog_lock",
        || {
            let catalog = EngineCatalog::new();
            let actor_drops = Arc::new(AtomicUsize::new(0));
            let runtime_drops = Arc::new(AtomicUsize::new(0));
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
            let token = catalog
                .register(Arc::new(SpawningRuntime {
                    entered: entered_tx,
                    release: std::sync::Mutex::new(release_rx),
                    drops: runtime_drops.clone(),
                }))
                .unwrap();
            let spawn_catalog = catalog.clone();
            let actor_counter = actor_drops.clone();
            let thread = std::thread::spawn(move || {
                let result = spawn_catalog.spawn(
                    "selected",
                    Box::new(DummyActor(actor_counter)),
                    "/counter",
                );
                assert_eq!(result.err().as_deref(), Some("original runtime completed"));
            });
            assert_eq!(
                entered_rx
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap(),
                "/counter"
            );
            drop(token);
            assert!(catalog.lookup("selected").is_none());
            catalog.insert(Arc::new(Runtime::plain("selected")));
            assert_eq!(runtime_drops.load(Ordering::SeqCst), 0);
            release_tx.send(()).unwrap();
            thread.join().unwrap();
            assert_eq!(runtime_drops.load(Ordering::SeqCst), 1);
            assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
            assert!(catalog.lookup("selected").is_some());
            let absent = catalog.spawn(
                "missing",
                Box::new(DummyActor(actor_drops.clone())),
                "/absent",
            );
            assert_eq!(
                absent.err().as_deref(),
                Some("engine not registered: missing")
            );
            assert_eq!(actor_drops.load(Ordering::SeqCst), 2);
        },
    );
}
