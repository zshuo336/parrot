//! Directory destruction owns each remaining runtime reference independently.
use parrot_api::engine::{
    AsyncEngineCatalog, AsyncEngineRegistration, AsyncEngineRuntime, BoxedActorRefErased,
    EngineCatalog, EngineRegistration, EngineRuntime, ErasedActor, InstanceConfig, RuntimeActor,
    SpawnOptions,
};
use parrot_api::system::{SystemError, SystemStatus};
use parrot_api::types::{BoxedActorRef, BoxedFuture};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum Failure {
    None,
    Plain,
    Payload,
}
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("runtime panic payload destructor");
    }
}
struct Runtime {
    name: &'static str,
    drops: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    payload_drops: Arc<AtomicUsize>,
    failure: Failure,
    on_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(callback) = self.on_drop.take() {
            callback();
        }
        match self.failure {
            Failure::None => (),
            Failure::Plain => panic!("runtime destructor {}", self.name),
            Failure::Payload => std::panic::panic_any(Payload(self.payload_drops.clone())),
        }
    }
}
impl EngineRuntime for Runtime {
    fn name(&self) -> &'static str {
        self.name
    }
    fn spawn_erased(
        &self,
        _: Box<dyn ErasedActor>,
        _: &str,
    ) -> Result<BoxedActorRefErased, String> {
        panic!("retirement must not spawn actors")
    }
}
impl AsyncEngineRuntime for Runtime {
    fn spawn<'a>(
        &'a self,
        _: RuntimeActor,
        _: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        panic!("retirement must not spawn actors")
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        panic!("retirement must not query actors")
    }
    fn status(&self) -> SystemStatus {
        panic!("retirement must not query status")
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
#[derive(Clone)]
enum Catalog {
    Sync(EngineCatalog),
    Async(AsyncEngineCatalog),
}
enum Token {
    Sync(EngineRegistration),
    Async(AsyncEngineRegistration),
}
impl Token {
    fn is_removed(&self) -> bool {
        match self {
            Self::Sync(token) => token.unregister().is_none(),
            Self::Async(token) => token.unregister().is_none(),
        }
    }
}
impl Catalog {
    fn new(asynchronous: bool) -> Self {
        if asynchronous {
            Self::Async(AsyncEngineCatalog::new())
        } else {
            Self::Sync(EngineCatalog::new())
        }
    }
    fn register(&self, runtime: Arc<Runtime>) -> Token {
        match self {
            Self::Sync(catalog) => Token::Sync(catalog.register(runtime).unwrap()),
            Self::Async(catalog) => Token::Async(catalog.register(runtime.name, runtime).unwrap()),
        }
    }
}
struct Observed {
    drops: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    payload_drops: Arc<AtomicUsize>,
}
impl Observed {
    fn runtime(name: &'static str, failure: Failure) -> (Arc<Runtime>, Self) {
        let observed = Self {
            drops: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            payload_drops: Arc::new(AtomicUsize::new(0)),
        };
        let runtime = Arc::new(Runtime {
            name,
            drops: observed.drops.clone(),
            shutdowns: observed.shutdowns.clone(),
            payload_drops: observed.payload_drops.clone(),
            failure,
            on_drop: None,
        });
        (runtime, observed)
    }
    fn assert_retired(&self, payloads: usize) {
        assert_eq!(
            self.drops.load(Ordering::SeqCst),
            1,
            "runtime leaked or double dropped"
        );
        assert_eq!(
            self.shutdowns.load(Ordering::SeqCst),
            0,
            "catalog must not initiate engine shutdown"
        );
        assert_eq!(self.payload_drops.load(Ordering::SeqCst), payloads);
    }
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_CATALOG_RETIREMENT_CASE")
        .ok()
        .as_deref()
        == Some(name)
    {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_CATALOG_RETIREMENT_CASE", name)
        .spawn()
        .unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "catalog retirement child failed: {name}: {status}"
            );
            return;
        }
        if std::time::Instant::now() >= end {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("catalog retirement deadlocked: {name}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
fn throwing_members(asynchronous: bool, unwinding: bool, payload: bool) {
    let catalog = Catalog::new(asynchronous);
    let mut tokens = Vec::new();
    let mut observations = Vec::new();
    for (name, failure) in [
        (
            "a",
            if payload {
                Failure::Payload
            } else {
                Failure::Plain
            },
        ),
        ("b", Failure::Plain),
        ("c", Failure::None),
    ] {
        let (runtime, stats) = Observed::runtime(name, failure);
        tokens.push(catalog.register(runtime));
        observations.push(stats);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _owned_catalog = catalog;
        if unwinding {
            panic!("original caller panic");
        }
    }));
    if unwinding {
        let error = result.expect_err("caller panic must survive catalog cleanup");
        assert_eq!(error.downcast_ref::<&str>(), Some(&"original caller panic"));
    } else {
        assert!(
            result.is_ok(),
            "implicit directory cleanup must isolate each destructor"
        );
    }
    for (index, observation) in observations.iter().enumerate() {
        observation.assert_retired(usize::from(payload && index == 0));
    }
    assert!(tokens.iter().all(Token::is_removed));
}
fn cloned_directory(asynchronous: bool) {
    let catalog = Catalog::new(asynchronous);
    let clone = catalog.clone();
    let (runtime, stats) = Observed::runtime("one", Failure::None);
    let token = catalog.register(runtime);
    drop(catalog);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 0);
    drop(clone);
    stats.assert_retired(0);
    assert!(token.is_removed());
}
fn captured_reference(asynchronous: bool) {
    let catalog = Catalog::new(asynchronous);
    let (runtime, stats) = Observed::runtime("one", Failure::Plain);
    let token = catalog.register(runtime.clone());
    drop(catalog);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 0);
    assert!(token.is_removed());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(runtime)));
    assert!(
        result.is_err(),
        "caller-owned reference must retain its Drop behavior"
    );
    stats.assert_retired(0);
}
fn weak_token_reentry(asynchronous: bool) {
    let catalog = Catalog::new(asynchronous);
    let token = Arc::new(Mutex::new(None::<Token>));
    let reentries = Arc::new(AtomicUsize::new(0));
    let (mut runtime, stats) = Observed::runtime("one", Failure::None);
    let callback_token = token.clone();
    let callback_count = reentries.clone();
    Arc::get_mut(&mut runtime).unwrap().on_drop = Some(Box::new(move || {
        assert!(callback_token
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_removed());
        callback_count.fetch_add(1, Ordering::SeqCst);
    }));
    *token.lock().unwrap() = Some(catalog.register(runtime));
    drop(catalog);
    stats.assert_retired(0);
    assert_eq!(reentries.load(Ordering::SeqCst), 1);
}
macro_rules! cases {
    ($module:ident, $asynchronous:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn every_runtime_is_retired_despite_independent_panics() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::every_runtime_is_retired_despite_independent_panics"
                    ),
                    || throwing_members($asynchronous, false, false),
                );
            }
            #[test]
            fn caller_unwind_survives_directory_cleanup() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::caller_unwind_survives_directory_cleanup"
                    ),
                    || throwing_members($asynchronous, true, false),
                );
            }
            #[test]
            fn panic_payload_is_retired_independently() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::panic_payload_is_retired_independently"
                    ),
                    || throwing_members($asynchronous, false, true),
                );
            }
            #[test]
            fn last_directory_clone_owns_retirement() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::last_directory_clone_owns_retirement"
                    ),
                    || cloned_directory($asynchronous),
                );
            }
            #[test]
            fn captured_runtime_keeps_caller_owned_drop_behavior() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::captured_runtime_keeps_caller_owned_drop_behavior"
                    ),
                    || captured_reference($asynchronous),
                );
            }
            #[test]
            fn retiring_runtime_can_consult_weak_registration_token() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::retiring_runtime_can_consult_weak_registration_token"
                    ),
                    || weak_token_reentry($asynchronous),
                );
            }
        }
    };
}
cases!(synchronous, false);
cases!(asynchronous, true);

#[test]
fn asynchronous_aliases_release_the_same_runtime_once() {
    isolated("asynchronous_aliases_release_the_same_runtime_once", || {
        let catalog = AsyncEngineCatalog::new();
        let (runtime, stats) = Observed::runtime("one", Failure::Plain);
        let first = catalog.register("first", runtime.clone()).unwrap();
        let second = catalog.register("second", runtime).unwrap();
        drop(catalog);
        stats.assert_retired(0);
        assert!(first.unregister().is_none());
        assert!(second.unregister().is_none());
    });
}
