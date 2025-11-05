//! Public catalog calls preserve callback panics while separately owning inputs.
use parrot_api::engine::{
    self, AsyncEngineCatalog, AsyncEngineRuntime, BoxedActorRefErased, EngineCatalog,
    EngineRuntime, ErasedActor, InstanceConfig, RuntimeActor, SpawnOptions,
};
use parrot_api::system::{SystemError, SystemStatus};
use parrot_api::types::{BoxedActorRef, BoxedFuture};
use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

type Callback = Box<dyn FnOnce() + Send + Sync>;
#[derive(Clone, Copy)]
enum Failure {
    None,
    Plain,
    Payload,
}
struct BadPayload(Arc<AtomicUsize>);
impl Drop for BadPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("secondary runtime payload destructor");
    }
}
struct PrimaryPanic(Arc<()>);
struct Runtime {
    key: &'static str,
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    payload_drops: Arc<AtomicUsize>,
    failure: Failure,
    on_name: Mutex<Option<Callback>>,
    on_drop: Mutex<Option<Callback>>,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(callback) = self.on_drop.get_mut().unwrap().take() {
            callback();
        }
        match self.failure {
            Failure::None => (),
            Failure::Plain => panic!("runtime destructor"),
            Failure::Payload => std::panic::panic_any(BadPayload(self.payload_drops.clone())),
        }
    }
}
impl EngineRuntime for Runtime {
    fn name(&self) -> &'static str {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let callback = self.on_name.lock().unwrap().take();
        if let Some(callback) = callback {
            callback();
        }
        self.key
    }
    fn spawn_erased(
        &self,
        _: Box<dyn ErasedActor>,
        _: &str,
    ) -> Result<BoxedActorRefErased, String> {
        panic!("registration must not create actors")
    }
}
impl AsyncEngineRuntime for Runtime {
    fn spawn<'a>(
        &'a self,
        _: RuntimeActor,
        _: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        panic!("registration must not create actors")
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        panic!("unexpected lookup")
    }
    fn status(&self) -> SystemStatus {
        panic!("unexpected status")
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        panic!("registration must not shut down engines")
    }
}
struct NameInput {
    key: &'static str,
    calls: Arc<AtomicUsize>,
    callback: Callback,
}
impl From<NameInput> for String {
    fn from(value: NameInput) -> Self {
        value.calls.fetch_add(1, Ordering::SeqCst);
        (value.callback)();
        value.key.into()
    }
}
struct Observed {
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    payload_drops: Arc<AtomicUsize>,
}
impl Observed {
    fn runtime(key: &'static str, failure: Failure) -> (Arc<Runtime>, Self) {
        let observed = Self {
            calls: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
            payload_drops: Arc::new(AtomicUsize::new(0)),
        };
        let runtime = Arc::new(Runtime {
            key,
            calls: observed.calls.clone(),
            drops: observed.drops.clone(),
            payload_drops: observed.payload_drops.clone(),
            failure,
            on_name: Mutex::new(None),
            on_drop: Mutex::new(None),
        });
        (runtime, observed)
    }
    fn check(&self, drops: usize, payloads: usize) {
        assert_eq!(
            self.calls.load(Ordering::SeqCst),
            1,
            "name callback must run exactly once"
        );
        assert_eq!(
            self.drops.load(Ordering::SeqCst),
            drops,
            "input reference ownership changed"
        );
        assert_eq!(
            self.payload_drops.load(Ordering::SeqCst),
            payloads,
            "runtime panic payload was not retired independently"
        );
    }
}
enum Handle {
    Sync(Arc<dyn EngineRuntime>),
    Async(Arc<dyn AsyncEngineRuntime>),
}
impl Handle {
    fn identity(&self) -> *const () {
        match self {
            Self::Sync(x) => Arc::as_ptr(x) as *const (),
            Self::Async(x) => Arc::as_ptr(x) as *const (),
        }
    }
}
enum Submission {
    Inserted(Option<Handle>),
    Registered(Box<dyn Any>),
    Duplicate(String),
}
#[derive(Clone)]
enum Catalog {
    Sync(EngineCatalog),
    Async(AsyncEngineCatalog),
}
impl Catalog {
    fn new(asynchronous: bool) -> Self {
        if asynchronous {
            Self::Async(AsyncEngineCatalog::new())
        } else {
            Self::Sync(EngineCatalog::new())
        }
    }
    fn list(&self) -> Vec<String> {
        match self {
            Self::Sync(c) => c.list(),
            Self::Async(c) => c.list(),
        }
    }
    fn get(&self, key: &str) -> Option<Handle> {
        match self {
            Self::Sync(c) => c.lookup(key).map(Handle::Sync),
            Self::Async(c) => c.lookup(key).map(Handle::Async),
        }
    }
    fn remove(&self, key: &str) -> Option<Handle> {
        match self {
            Self::Sync(c) => c.remove(key).map(Handle::Sync),
            Self::Async(c) => c.remove(key).map(Handle::Async),
        }
    }
    fn submit(&self, insert: bool, runtime: Arc<Runtime>, callback: Callback) -> Submission {
        match self {
            Self::Sync(c) => {
                *runtime.on_name.lock().unwrap() = Some(callback);
                if insert {
                    Submission::Inserted(c.insert(runtime).map(Handle::Sync))
                } else {
                    match c.register(runtime) {
                        Ok(t) => Submission::Registered(Box::new(t)),
                        Err(e) => Submission::Duplicate(e.0),
                    }
                }
            }
            Self::Async(c) => {
                let name = NameInput {
                    key: runtime.key,
                    calls: runtime.calls.clone(),
                    callback,
                };
                if insert {
                    Submission::Inserted(c.insert(name, runtime).map(Handle::Async))
                } else {
                    match c.register(name, runtime) {
                        Ok(t) => Submission::Registered(Box::new(t)),
                        Err(e) => Submission::Duplicate(e.0),
                    }
                }
            }
        }
    }
}
fn primary(action: impl FnOnce(), identity: &Arc<()>) {
    let error = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
        Err(error) => error,
        Ok(()) => panic!("callback panic was swallowed"),
    };
    let payload = error
        .downcast_ref::<PrimaryPanic>()
        .expect("original callback panic type was replaced");
    assert!(
        Arc::ptr_eq(&payload.0, identity),
        "original callback panic identity was replaced"
    );
}
fn failed_name(asynchronous: bool, insert: bool, failure: Failure, external: bool) {
    let catalog = Catalog::new(asynchronous);
    let (existing, _) = Observed::runtime("existing", Failure::None);
    let existing_id = Arc::as_ptr(&existing) as *const ();
    assert!(matches!(
        catalog.submit(true, existing, Box::new(|| {})),
        Submission::Inserted(None)
    ));
    let (runtime, stats) = Observed::runtime("target", failure);
    let retained = external.then(|| runtime.clone());
    let cleanup_check = catalog.clone();
    *runtime.on_drop.lock().unwrap() = Some(Box::new(move || {
        assert_eq!(
            cleanup_check.list(),
            ["existing"],
            "failed name committed or poisoned the catalog"
        );
    }));
    let identity = Arc::new(());
    let raised = identity.clone();
    primary(
        || {
            catalog.submit(
                insert,
                runtime,
                Box::new(move || std::panic::panic_any(PrimaryPanic(raised))),
            );
        },
        &identity,
    );
    assert_eq!(catalog.list(), ["existing"]);
    assert_eq!(catalog.get("existing").unwrap().identity(), existing_id);
    assert!(
        catalog.get("target").is_none(),
        "failed callback published a runtime"
    );
    if let Some(retained) = retained {
        stats.check(0, 0);
        let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(retained)));
        assert!(
            error.is_err(),
            "external final reference lost its original Drop behavior"
        );
    }
    stats.check(1, usize::from(matches!(failure, Failure::Payload)));
    let (healthy, healthy_stats) = Observed::runtime("healthy", Failure::None);
    assert!(matches!(
        catalog.submit(true, healthy, Box::new(|| {})),
        Submission::Inserted(None)
    ));
    drop(catalog.remove("healthy"));
    healthy_stats.check(1, 0);
    drop(catalog.remove("existing"));
}
fn successful_name(asynchronous: bool, insert: bool) {
    let catalog = Catalog::new(asynchronous);
    let (runtime, stats) = Observed::runtime("target", Failure::None);
    let identity = Arc::as_ptr(&runtime) as *const ();
    let reentry = catalog.clone();
    let result = catalog.submit(
        insert,
        runtime,
        Box::new(move || {
            assert!(
                reentry.list().is_empty(),
                "name callback must precede publication and run unlocked"
            );
        }),
    );
    stats.check(0, 0);
    assert_eq!(catalog.get("target").unwrap().identity(), identity);
    match result {
        Submission::Registered(token) => drop(token),
        Submission::Inserted(None) => drop(catalog.remove("target")),
        _ => panic!("successful registration returned the wrong outcome"),
    }
    assert!(catalog.list().is_empty());
    stats.check(1, 0);
}
fn duplicate(asynchronous: bool, panic: bool) {
    let catalog = Catalog::new(asynchronous);
    let (existing, _) = Observed::runtime("target", Failure::None);
    let existing_id = Arc::as_ptr(&existing) as *const ();
    catalog.submit(true, existing, Box::new(|| {}));
    let (runtime, stats) =
        Observed::runtime("target", if panic { Failure::Plain } else { Failure::None });
    let reentry = catalog.clone();
    *runtime.on_drop.lock().unwrap() =
        Some(Box::new(move || assert_eq!(reentry.list(), ["target"])));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        catalog.submit(false, runtime, Box::new(|| {}))
    }));
    if panic {
        assert!(
            result.is_err(),
            "successful name must retain explicit duplicate Drop panic behavior"
        );
    } else {
        assert!(matches!(result, Ok(Submission::Duplicate(ref name)) if name == "target"));
    }
    stats.check(1, 0);
    assert_eq!(catalog.get("target").unwrap().identity(), existing_id);
    drop(catalog.remove("target"));
}
fn replacement(asynchronous: bool) {
    let catalog = Catalog::new(asynchronous);
    let (old, old_stats) = Observed::runtime("target", Failure::Plain);
    let identity = Arc::as_ptr(&old) as *const ();
    catalog.submit(true, old, Box::new(|| {}));
    let (new, new_stats) = Observed::runtime("target", Failure::None);
    let new_identity = Arc::as_ptr(&new) as *const ();
    let old = match catalog.submit(true, new, Box::new(|| {})) {
        Submission::Inserted(Some(old)) => old,
        _ => panic!("replacement did not return the old runtime"),
    };
    assert_eq!(old.identity(), identity);
    old_stats.check(0, 0);
    assert_eq!(catalog.get("target").unwrap().identity(), new_identity);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(old))).is_err(),
        "returned old reference lost its Drop behavior"
    );
    old_stats.check(1, 0);
    drop(catalog.remove("target"));
    new_stats.check(1, 0);
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_CATALOG_INPUT_CASE").ok().as_deref() == Some(name) {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_CATALOG_INPUT_CASE", name)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "catalog input child failed: {name}: {status}"
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("catalog input callback deadlocked: {name}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
macro_rules! operation {
    ($module:ident, $asynchronous:expr, $insert:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn original_panic_survives_normal_input_disposal() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::original_panic_survives_normal_input_disposal"
                    ),
                    || failed_name($asynchronous, $insert, Failure::None, false),
                );
            }
            #[test]
            fn original_panic_survives_throwing_input_disposal() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::original_panic_survives_throwing_input_disposal"
                    ),
                    || failed_name($asynchronous, $insert, Failure::Plain, false),
                );
            }
            #[test]
            fn original_panic_survives_throwing_cleanup_payload() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::original_panic_survives_throwing_cleanup_payload"
                    ),
                    || failed_name($asynchronous, $insert, Failure::Payload, false),
                );
            }
            #[test]
            fn captured_runtime_is_not_retired_early() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::captured_runtime_is_not_retired_early"
                    ),
                    || failed_name($asynchronous, $insert, Failure::Plain, true),
                );
            }
            #[test]
            fn successful_callback_reenters_before_actual_publication() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::successful_callback_reenters_before_actual_publication"
                    ),
                    || successful_name($asynchronous, $insert),
                );
            }
        }
    };
}
operation!(sync_insert, false, true);
operation!(sync_register, false, false);
operation!(async_insert, true, true);
operation!(async_register, true, false);
macro_rules! compatibility {
    ($module:ident, $asynchronous:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn duplicate_returns_original_error_and_preserves_old_entry() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::duplicate_returns_original_error_and_preserves_old_entry"
                    ),
                    || duplicate($asynchronous, false),
                );
            }
            #[test]
            fn duplicate_keeps_explicit_runtime_drop_panic() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::duplicate_keeps_explicit_runtime_drop_panic"
                    ),
                    || duplicate($asynchronous, true),
                );
            }
            #[test]
            fn replacement_transfers_old_runtime_to_caller() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::replacement_transfers_old_runtime_to_caller"
                    ),
                    || replacement($asynchronous),
                );
            }
        }
    };
}
compatibility!(sync_compatibility, false);
compatibility!(async_compatibility, true);
fn global_failure(scoped: bool) {
    let (runtime, stats) = Observed::runtime("catalog-input-global", Failure::Payload);
    let identity = Arc::new(());
    let raised = identity.clone();
    *runtime.on_name.lock().unwrap() = Some(Box::new(move || {
        std::panic::panic_any(PrimaryPanic(raised))
    }));
    primary(
        || {
            if scoped {
                drop(engine::register_engine_scoped(runtime));
            } else {
                engine::register_engine(runtime);
            }
        },
        &identity,
    );
    stats.check(1, 1);
    assert!(engine::lookup_engine("catalog-input-global").is_none());
    let (healthy, stats) = Observed::runtime("healthy-global", Failure::None);
    engine::register_engine(healthy);
    drop(engine::unregister_engine("healthy-global"));
    stats.check(1, 0);
}
#[test]
fn global_persistent_registration_preserves_original_panic() {
    isolated(
        "global_persistent_registration_preserves_original_panic",
        || global_failure(false),
    );
}
#[test]
fn global_scoped_registration_preserves_original_panic() {
    isolated(
        "global_scoped_registration_preserves_original_panic",
        || global_failure(true),
    );
}

mod dispatch {
    use super::*;
    #[derive(Default)]
    struct Stats {
        received: AtomicUsize,
        actor_drops: AtomicUsize,
        runtime_drops: AtomicUsize,
        reply_drops: AtomicUsize,
        payload_drops: Arc<AtomicUsize>,
        reply_identity: AtomicUsize,
    }
    struct Actor(Arc<Stats>);
    impl ErasedActor for Actor {
        fn receive(&mut self, message: parrot_api::types::BoxedMessage) -> engine::BoxedResult {
            self.0.received.fetch_add(1, Ordering::SeqCst);
            Ok(message)
        }
    }
    impl Drop for Actor {
        fn drop(&mut self) {
            self.0.actor_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Reply {
        stats: Arc<Stats>,
        fails: bool,
    }
    impl engine::ActorRefErased for Reply {
        fn tell(&self, _: parrot_api::types::BoxedMessage) {
            panic!("unexpected TELL")
        }
        fn ask(&self, message: parrot_api::types::BoxedMessage) -> engine::BoxedResult {
            Ok(message)
        }
        fn path(&self) -> String {
            "/actual-reply".into()
        }
    }
    impl Drop for Reply {
        fn drop(&mut self) {
            self.stats.reply_drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.fails, "returned reference destructor failure");
        }
    }
    struct Provider {
        catalog: EngineCatalog,
        stats: Arc<Stats>,
        delegate_panics: bool,
        delegate_error: bool,
        runtime_failure: Failure,
        reply_failure: bool,
        delegate_identity: Arc<()>,
        runtime_identity: Arc<()>,
    }
    impl EngineRuntime for Provider {
        fn name(&self) -> &'static str {
            "dispatch"
        }
        fn spawn_erased(
            &self,
            mut actor: Box<dyn ErasedActor>,
            path: &str,
        ) -> Result<BoxedActorRefErased, String> {
            assert_eq!(path, "/requested");
            let reply = actor.receive(Box::new(42_u64)).unwrap();
            assert_eq!(reply.downcast_ref::<u64>(), Some(&42));
            drop(actor);
            // Removal releases discovery; the dispatch call owns the last capture.
            drop(
                self.catalog
                    .remove("dispatch")
                    .expect("runtime was not registered"),
            );
            if self.delegate_panics {
                std::panic::panic_any(PrimaryPanic(self.delegate_identity.clone()));
            }
            if self.delegate_error {
                return Err("delegate business rejection".into());
            }
            let reply = Arc::new(Reply {
                stats: self.stats.clone(),
                fails: self.reply_failure,
            });
            self.stats
                .reply_identity
                .store(Arc::as_ptr(&reply) as usize, Ordering::SeqCst);
            Ok(reply)
        }
    }
    impl Drop for Provider {
        fn drop(&mut self) {
            self.stats.runtime_drops.fetch_add(1, Ordering::SeqCst);
            assert!(
                self.catalog.list().is_empty(),
                "capture disposal ran under a live registration"
            );
            match self.runtime_failure {
                Failure::None => (),
                Failure::Plain => {
                    std::panic::panic_any(PrimaryPanic(self.runtime_identity.clone()))
                }
                Failure::Payload => {
                    std::panic::panic_any(BadPayload(self.stats.payload_drops.clone()))
                }
            }
        }
    }
    fn exercise(
        delegate_panics: bool,
        delegate_error: bool,
        runtime_failure: Failure,
        reply_failure: bool,
    ) {
        let catalog = EngineCatalog::new();
        let stats = Arc::new(Stats::default());
        let delegate_identity = Arc::new(());
        let runtime_identity = Arc::new(());
        assert!(catalog
            .insert(Arc::new(Provider {
                catalog: catalog.clone(),
                stats: stats.clone(),
                delegate_panics,
                delegate_error,
                runtime_failure,
                reply_failure,
                delegate_identity: delegate_identity.clone(),
                runtime_identity: runtime_identity.clone()
            }))
            .is_none());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            catalog.spawn("dispatch", Box::new(Actor(stats.clone())), "/requested")
        }));
        assert!(catalog.list().is_empty());
        assert_eq!(stats.received.load(Ordering::SeqCst), 1);
        assert_eq!(stats.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            stats.runtime_drops.load(Ordering::SeqCst),
            1,
            "captured runtime was leaked"
        );
        assert_eq!(
            stats.payload_drops.load(Ordering::SeqCst),
            usize::from(matches!(runtime_failure, Failure::Payload))
        );
        if delegate_panics || !matches!(runtime_failure, Failure::None) {
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("delegate/capture panic was suppressed"),
            };
            let primary = error
                .downcast_ref::<PrimaryPanic>()
                .expect("dispatch replaced original panic type");
            let expected = if delegate_panics {
                &delegate_identity
            } else {
                &runtime_identity
            };
            assert!(
                Arc::ptr_eq(&primary.0, expected),
                "dispatch replaced original panic identity"
            );
            assert_eq!(
                stats.reply_drops.load(Ordering::SeqCst),
                usize::from(!delegate_panics && !delegate_error),
                "unreturned reply was not retired exactly once"
            );
        } else if delegate_error {
            let error = result.unwrap().err().expect("delegate rejection was lost");
            assert_eq!(error, "delegate business rejection");
            assert_eq!(stats.reply_drops.load(Ordering::SeqCst), 0);
        } else {
            let reply = result.unwrap().unwrap();
            assert_eq!(
                Arc::as_ptr(&reply) as *const () as usize,
                stats.reply_identity.load(Ordering::SeqCst)
            );
            assert_eq!(reply.path(), "/actual-reply");
            assert_eq!(
                reply.ask(Box::new(17_u64)).unwrap().downcast_ref::<u64>(),
                Some(&17)
            );
            assert_eq!(
                stats.reply_drops.load(Ordering::SeqCst),
                0,
                "returned reply was disposed early"
            );
            drop(reply);
            assert_eq!(stats.reply_drops.load(Ordering::SeqCst), 1);
        }
    }
    macro_rules! case {
        ($name:ident, $panic:expr, $error:expr, $runtime:expr, $reply:expr) => {
            #[test]
            fn $name() {
                isolated(concat!("dispatch::", stringify!($name)), || {
                    exercise($panic, $error, $runtime, $reply)
                });
            }
        };
    }
    case!(
        delegate_panic_survives_capture_drop_panic,
        true,
        false,
        Failure::Plain,
        false
    );
    case!(
        delegate_panic_survives_capture_payload_panic,
        true,
        false,
        Failure::Payload,
        false
    );
    case!(
        throwing_reply_is_retired_after_capture_drop_panic,
        false,
        false,
        Failure::Plain,
        true
    );
    case!(
        normal_reply_is_retired_after_capture_drop_panic,
        false,
        false,
        Failure::Plain,
        false
    );
    case!(
        delegate_panic_survives_normal_capture_drop,
        true,
        false,
        Failure::None,
        false
    );
    case!(
        capture_drop_panic_after_business_rejection_is_preserved,
        false,
        true,
        Failure::Plain,
        false
    );
    case!(
        success_transfers_the_actual_reference,
        false,
        false,
        Failure::None,
        false
    );
    case!(
        business_rejection_is_preserved,
        false,
        true,
        Failure::None,
        false
    );
}
