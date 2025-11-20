//! Final framework owners retire sibling registrations independently.
use futures::FutureExt;
use parrot::{
    mirror::{MirrorCopier, MirrorPolicy},
    system::{ParrotActorSystem, PrefixResolver, ReceptionistGateway, RemoteGateway},
};
use parrot_api::{
    address::ActorRef,
    engine::{AsyncEngineRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    receptionist::{ReceptionistKey, ReceptionistStream},
    system::{SystemError, SystemResources, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Probe {
    retired: AtomicUsize,
    payloads: AtomicUsize,
    reentered: AtomicUsize,
    throws: bool,
    facade: Weak<ParrotActorSystem>,
}
struct Owner(Arc<Probe>);
struct OwnerPanic(Arc<Probe>);
impl Drop for OwnerPanic {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("registration storage payload drop");
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.retired.fetch_add(1, Ordering::SeqCst);
        if let Some(facade) = self.0.facade.upgrade() {
            let _ = facade.unregister_system("missing");
            let _ = facade.unregister_prefix_handler("/missing/");
            self.0.reentered.fetch_add(1, Ordering::SeqCst);
        }
        if self.0.throws {
            std::panic::panic_any(OwnerPanic(self.0.clone()));
        }
    }
}
fn owner(
    probes: &mut Vec<Arc<Probe>>,
    facade: Option<&Arc<ParrotActorSystem>>,
    throws: bool,
) -> Arc<Owner> {
    let probe = Arc::new(Probe {
        retired: AtomicUsize::new(0),
        payloads: AtomicUsize::new(0),
        reentered: AtomicUsize::new(0),
        throws,
        facade: facade.map(Arc::downgrade).unwrap_or_default(),
    });
    probes.push(probe.clone());
    Arc::new(Owner(probe))
}
fn check(probes: &[Arc<Probe>], reentry: bool) {
    for probe in probes {
        assert_eq!(
            probe.retired.load(Ordering::SeqCst),
            1,
            "one actual owner retirement"
        );
        assert_eq!(
            probe.payloads.load(Ordering::SeqCst),
            usize::from(probe.throws),
            "panic payload retired independently"
        );
        assert_eq!(probe.reentered.load(Ordering::SeqCst), usize::from(reentry));
    }
}
fn retire_payload(payload: Box<dyn Any + Send>) {
    if let Err(second) = catch_unwind(AssertUnwindSafe(|| drop(payload)))
        && let Err(last) = catch_unwind(AssertUnwindSafe(|| drop(second)))
    {
        std::mem::forget(last);
    }
}
#[derive(Debug, PartialEq)]
enum Outcome {
    Clean,
    SafeDisposalPanic,
    OtherPanic,
}
fn classify(result: std::thread::Result<()>) -> Outcome {
    match result {
        Ok(()) => Outcome::Clean,
        Err(payload) => {
            let safe = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .is_some_and(|text| text.contains("disposal"));
            retire_payload(payload);
            if safe {
                Outcome::SafeDisposalPanic
            } else {
                Outcome::OtherPanic
            }
        }
    }
}

struct Reference {
    _owner: Option<Arc<Owner>>,
}
impl std::fmt::Debug for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StorageReference")
    }
}
impl ActorRef for Reference {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        Box::pin(async { true })
    }
    fn path(&self) -> String {
        "/storage".into()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self { _owner: None })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
impl PrefixResolver for Owner {
    fn resolve(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
impl RemoteGateway for Owner {
    fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
#[async_trait::async_trait]
impl ReceptionistGateway for Owner {
    async fn register(&self, _: ReceptionistKey, _: String) -> ActorResult<()> {
        Ok(())
    }
    async fn deregister(&self, _: ReceptionistKey, _: String) -> ActorResult<()> {
        Ok(())
    }
    async fn subscribe(&self, _: ReceptionistKey) -> ActorResult<ReceptionistStream> {
        Ok(tokio::sync::mpsc::channel(1).1.into())
    }
}
impl AsyncEngineRuntime for Owner {
    fn spawn<'a>(
        &'a self,
        _: RuntimeActor,
        _: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        panic!("storage fixture must not execute actors");
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async { None })
    }
    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: SystemState::Stopped,
            active_actors: 0,
            uptime: Duration::ZERO,
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        }
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Thread,
    Actix,
    Gateways,
    Static,
    Resolver,
    MixedPrefix,
    Runtime,
    Copier,
    CopierRegistry,
    MultipleCopiers,
    Mixed,
}
enum Subject {
    Thread(Arc<parrot::thread::ThreadActorSystem>),
    Actix(Box<parrot::actix::ActixActorSystem>),
    Facade(Arc<ParrotActorSystem>),
    Copier(MirrorCopier),
}
impl Clone for Subject {
    fn clone(&self) -> Self {
        match self {
            Self::Thread(value) => Self::Thread(value.clone()),
            Self::Actix(value) => Self::Actix(value.clone()),
            Self::Facade(value) => Self::Facade(value.clone()),
            Self::Copier(value) => Self::Copier(value.clone()),
        }
    }
}
fn copier(probes: &mut Vec<Arc<Probe>>, throws: bool, count: usize) -> MirrorCopier {
    copier_for::<u64>(probes, throws, count)
}
fn copier_for<T: Copy + Send + 'static>(
    probes: &mut Vec<Arc<Probe>>,
    throws: bool,
    count: usize,
) -> MirrorCopier {
    let measure = owner(probes, None, throws);
    let copy = owner(probes, None, throws && count > 1);
    MirrorCopier::new::<T>(
        move |_| {
            let _ = &measure;
            std::mem::size_of::<T>()
        },
        move |value| {
            let _ = &copy;
            Ok(*value)
        },
    )
}
async fn setup(
    kind: Kind,
    count: usize,
    throws: bool,
    observer: Option<&Arc<ParrotActorSystem>>,
) -> (Subject, Vec<Arc<Probe>>) {
    let mut probes = Vec::new();
    let subject = match kind {
        Kind::Thread => {
            let engine = parrot::thread::ThreadActorSystem::shared(Default::default());
            engine.shutdown_internal().await.unwrap();
            engine.set_remote_gateway(owner(&mut probes, observer, throws));
            if count > 1 {
                engine.set_receptionist_gateway(owner(&mut probes, observer, throws));
            }
            Subject::Thread(engine)
        }
        Kind::Actix => {
            let engine = parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap();
            engine.clone().shutdown().await.unwrap();
            engine.set_remote_gateway(owner(&mut probes, observer, throws));
            if count > 1 {
                engine.set_receptionist_gateway(owner(&mut probes, observer, throws));
            }
            Subject::Actix(Box::new(engine))
        }
        Kind::Copier => Subject::Copier(copier(&mut probes, throws, count)),
        _ => {
            let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
            if matches!(kind, Kind::Gateways | Kind::Mixed) {
                facade
                    .register_remote_gateway(owner(&mut probes, observer, throws))
                    .await
                    .unwrap();
                if count > 1 {
                    facade
                        .register_receptionist_gateway(owner(&mut probes, observer, throws))
                        .await
                        .unwrap();
                }
            }
            if matches!(kind, Kind::Static | Kind::Mixed) {
                for i in 0..count {
                    facade
                        .register_prefix_handler(
                            format!("/static-{i}/"),
                            Arc::new(Reference {
                                _owner: Some(owner(&mut probes, observer, throws)),
                            }),
                        )
                        .unwrap();
                }
            }
            if matches!(kind, Kind::Resolver | Kind::MixedPrefix | Kind::Mixed) {
                for i in 0..count {
                    facade
                        .register_prefix_resolver(
                            format!("/resolver-{i}/"),
                            owner(&mut probes, observer, throws),
                        )
                        .unwrap();
                }
            }
            if matches!(kind, Kind::MixedPrefix) {
                facade
                    .register_prefix_handler(
                        "/static/",
                        Arc::new(Reference {
                            _owner: Some(owner(&mut probes, observer, throws)),
                        }),
                    )
                    .unwrap();
            }
            if matches!(kind, Kind::Runtime | Kind::Mixed) {
                for i in 0..count {
                    facade
                        .register_runtime(
                            format!("runtime-{i}"),
                            owner(&mut probes, observer, throws),
                            false,
                        )
                        .unwrap();
                }
            }
            if matches!(
                kind,
                Kind::CopierRegistry | Kind::MultipleCopiers | Kind::Mixed
            ) {
                facade
                    .register_mirror_copier(copier(&mut probes, throws, count))
                    .unwrap();
            }
            if matches!(kind, Kind::MultipleCopiers) {
                facade
                    .register_mirror_copier(copier_for::<u32>(&mut probes, throws, count))
                    .unwrap();
            }
            Subject::Facade(facade)
        }
    };
    (subject, probes)
}
async fn normal(kind: Kind, count: usize, throws: bool) {
    let (subject, probes) = setup(kind, count, throws, None).await;
    let outcome = classify(catch_unwind(AssertUnwindSafe(|| drop(subject))));
    check(&probes, false);
    assert_eq!(
        outcome,
        if throws {
            Outcome::SafeDisposalPanic
        } else {
            Outcome::Clean
        },
        "final Drop must report one safe cleanup failure after sibling retirement"
    );
}
#[derive(Debug)]
struct OriginalPanic;
async fn unwinding(kind: Kind) {
    let (subject, probes) = setup(kind, 2, true, None).await;
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _subject = subject;
        std::panic::panic_any(OriginalPanic);
    }));
    let original = match result {
        Err(payload) => {
            let original = payload.is::<OriginalPanic>();
            retire_payload(payload);
            original
        }
        Ok(()) => false,
    };
    check(&probes, false);
    assert!(
        original,
        "registration retirement must preserve an existing unwind"
    );
}
async fn shared(kind: Kind) {
    let (subject, probes) = setup(kind, 2, true, None).await;
    let last = subject.clone();
    let first = classify(catch_unwind(AssertUnwindSafe(|| drop(subject))));
    let pending = probes.iter().all(|p| p.retired.load(Ordering::SeqCst) == 0);
    let final_owner = classify(catch_unwind(AssertUnwindSafe(|| drop(last))));
    check(&probes, false);
    assert_eq!(first, Outcome::Clean);
    assert!(pending, "a nonfinal clone must retain its registrations");
    assert_eq!(final_owner, Outcome::SafeDisposalPanic);
}
async fn deferred_copiers(existing_unwind: bool, throws: bool) {
    let (subject, probes) = setup(Kind::MultipleCopiers, 2, throws, None).await;
    let Subject::Facade(facade) = subject else {
        unreachable!();
    };
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/shadow/"))
        .unwrap();
    let last = handle.clone();
    let facade_outcome = classify(catch_unwind(AssertUnwindSafe(|| drop(facade))));
    // Worker completion does not relinquish the caller's session ownership.
    handle.shutdown().await;
    let handle_outcome = classify(catch_unwind(AssertUnwindSafe(|| drop(handle))));
    let pending = probes.iter().all(|p| p.retired.load(Ordering::SeqCst) == 0);
    let final_outcome = if existing_unwind {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _last = last;
            std::panic::panic_any(OriginalPanic);
        }));
        match result {
            Err(payload) => {
                let original = payload.is::<OriginalPanic>();
                retire_payload(payload);
                assert!(original, "late session cleanup replaced the existing panic");
            }
            Ok(()) => panic!("original panic was lost"),
        }
        Outcome::Clean
    } else {
        classify(catch_unwind(AssertUnwindSafe(|| drop(last))))
    };
    check(&probes, false);
    assert_eq!(facade_outcome, Outcome::Clean);
    assert_eq!(handle_outcome, Outcome::Clean);
    assert!(
        pending,
        "a remaining handle must retain the copier registry"
    );
    assert_eq!(
        final_outcome,
        if throws && !existing_unwind {
            Outcome::SafeDisposalPanic
        } else {
            Outcome::Clean
        }
    );
}
#[derive(Clone, Copy)]
enum Input {
    Cancel,
    Reject,
    CrossShutdown,
}
async fn input(kind: Kind, mode: Input) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    if matches!(mode, Input::Reject) {
        facade.shutdown_runtime().await.unwrap();
    }
    let (subject, probes) = setup(kind, 2, true, Some(&facade)).await;
    let future: BoxedFuture<'_, Result<(), SystemError>> = match subject {
        Subject::Thread(engine) => {
            Box::pin(facade.register_thread_system("input".into(), engine, true))
        }
        Subject::Actix(engine) => {
            Box::pin(facade.register_actix_system("input".into(), *engine, true))
        }
        _ => unreachable!(),
    };
    if matches!(mode, Input::CrossShutdown) {
        facade.shutdown_runtime().await.unwrap();
    }
    let (outcome, error) = if matches!(mode, Input::Cancel) {
        (
            classify(catch_unwind(AssertUnwindSafe(|| drop(future)))),
            None,
        )
    } else {
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(result) => (Outcome::Clean, result.err()),
            Err(payload) => (classify(Err(payload)), None),
        }
    };
    if !facade.list_registered_systems().unwrap().is_empty() {
        let _ = classify(catch_unwind(AssertUnwindSafe(|| drop(facade))));
        check(&probes, false);
        panic!("closed or unpolled input was unexpectedly published");
    }
    check(&probes, true);
    assert_eq!(
        outcome,
        Outcome::Clean,
        "input cleanup must remain isolated"
    );
    if !matches!(mode, Input::Cancel) {
        let error = error.expect("failed input disposal must remain observable");
        assert!(matches!(error, SystemError::ActorError(_)), "{error}");
        assert!(error.to_string().contains("disposal"), "{error}");
    }
    facade.shutdown_runtime().await.unwrap();
}
async fn replacement(kind: Kind) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let (old, probes) = setup(kind, 2, true, Some(&facade)).await;
    let (new, new_probes) = setup(kind, 2, false, None).await;
    let result = match (old, new) {
        (Subject::Thread(old), Subject::Thread(new)) => {
            facade
                .register_thread_system("engine".into(), old, true)
                .await
                .unwrap();
            AssertUnwindSafe(facade.register_thread_system("engine".into(), new, true))
                .catch_unwind()
                .await
        }
        (Subject::Actix(old), Subject::Actix(new)) => {
            facade
                .register_actix_system("engine".into(), *old, true)
                .await
                .unwrap();
            AssertUnwindSafe(facade.register_actix_system("engine".into(), *new, true))
                .catch_unwind()
                .await
        }
        _ => unreachable!(),
    };
    let current = facade.list_registered_systems().unwrap();
    let retained = new_probes
        .iter()
        .all(|p| p.retired.load(Ordering::SeqCst) == 0);
    facade.shutdown_runtime().await.unwrap();
    let cleanup = classify(catch_unwind(AssertUnwindSafe(|| drop(facade))));
    let error = match result {
        Ok(result) => result.err(),
        Err(payload) => {
            let _ = classify(Err(payload));
            None
        }
    };
    check(&probes, true);
    check(&new_probes, false);
    assert_eq!(cleanup, Outcome::Clean);
    assert!(retained);
    assert_eq!(current, ["engine"]);
    assert!(
        error
            .expect("displaced engine cleanup failure must be returned")
            .to_string()
            .contains("registration committed")
    );
}

fn isolated(name: &str, test: impl FnOnce()) {
    if std::env::var("PARROT_STORAGE_CHILD").ok().as_deref() == Some(name) {
        test();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_STORAGE_CHILD", name)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "storage child failed: {name}: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("storage callback deadlocked: {name}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident, $future:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || actix::System::new().block_on($future));
        }
    };
}
case!(
    thread_final_owner_retires_both_gateways,
    normal(Kind::Thread, 2, true)
);
case!(
    actix_final_owner_retires_both_gateways,
    normal(Kind::Actix, 2, true)
);
case!(
    facade_final_owner_retires_both_gateways,
    normal(Kind::Gateways, 2, true)
);
case!(
    facade_final_owner_retires_all_static_prefixes,
    normal(Kind::Static, 2, true)
);
case!(
    facade_final_owner_retires_all_resolvers,
    normal(Kind::Resolver, 2, true)
);
case!(
    facade_final_owner_retires_mixed_prefixes,
    normal(Kind::MixedPrefix, 2, true)
);
case!(
    facade_final_owner_retires_all_external_runtimes,
    normal(Kind::Runtime, 2, true)
);
case!(
    copier_final_owner_retires_both_captures,
    normal(Kind::Copier, 2, true)
);
case!(
    facade_final_owner_retires_copier_captures,
    normal(Kind::CopierRegistry, 2, true)
);
case!(
    facade_final_owner_retires_every_registration_family,
    normal(Kind::Mixed, 2, true)
);
case!(
    thread_single_gateway_reports_safe_disposal_failure,
    normal(Kind::Thread, 1, true)
);
case!(
    actix_single_gateway_reports_safe_disposal_failure,
    normal(Kind::Actix, 1, true)
);
case!(
    facade_single_prefix_reports_safe_disposal_failure,
    normal(Kind::Static, 1, true)
);
case!(
    facade_single_resolver_reports_safe_disposal_failure,
    normal(Kind::Resolver, 1, true)
);
case!(
    facade_single_runtime_reports_safe_disposal_failure,
    normal(Kind::Runtime, 1, true)
);
case!(
    copier_single_capture_reports_safe_disposal_failure,
    normal(Kind::Copier, 1, true)
);
case!(
    facade_retires_every_copier_type_after_capture_failure,
    normal(Kind::MultipleCopiers, 1, true)
);
case!(
    mirror_handle_final_owner_retires_copiers_after_facade_drop,
    deferred_copiers(false, true)
);
case!(
    mirror_handle_cleanup_preserves_existing_unwind_after_facade_drop,
    deferred_copiers(true, true)
);
case!(
    healthy_mirror_handle_retires_copiers_after_facade_drop,
    deferred_copiers(false, false)
);
case!(
    thread_cleanup_preserves_existing_unwind,
    unwinding(Kind::Thread)
);
case!(
    actix_cleanup_preserves_existing_unwind,
    unwinding(Kind::Actix)
);
case!(
    facade_cleanup_preserves_existing_unwind,
    unwinding(Kind::Mixed)
);
case!(
    copier_cleanup_preserves_existing_unwind,
    unwinding(Kind::Copier)
);
case!(
    thread_nonfinal_clone_keeps_gateway_owners,
    shared(Kind::Thread)
);
case!(
    actix_nonfinal_clone_keeps_gateway_owners,
    shared(Kind::Actix)
);
case!(
    facade_nonfinal_clone_keeps_registration_owners,
    shared(Kind::Mixed)
);
case!(
    copier_nonfinal_clone_keeps_capture_owners,
    shared(Kind::Copier)
);
case!(
    unpolled_thread_input_retires_both_gateways,
    input(Kind::Thread, Input::Cancel)
);
case!(
    unpolled_actix_input_retires_both_gateways,
    input(Kind::Actix, Input::Cancel)
);
case!(
    rejected_thread_input_reports_both_gateway_retirements,
    input(Kind::Thread, Input::Reject)
);
case!(
    rejected_actix_input_reports_both_gateway_retirements,
    input(Kind::Actix, Input::Reject)
);
case!(
    pending_thread_input_remains_owned_after_shutdown,
    input(Kind::Thread, Input::CrossShutdown)
);
case!(
    pending_actix_input_remains_owned_after_shutdown,
    input(Kind::Actix, Input::CrossShutdown)
);
case!(
    thread_replacement_commits_before_displaced_engine_cleanup,
    replacement(Kind::Thread)
);
case!(
    actix_replacement_commits_before_displaced_engine_cleanup,
    replacement(Kind::Actix)
);
case!(
    healthy_thread_storage_drops_cleanly,
    normal(Kind::Thread, 2, false)
);
case!(
    healthy_actix_storage_drops_cleanly,
    normal(Kind::Actix, 2, false)
);
case!(
    healthy_facade_storage_drops_cleanly,
    normal(Kind::Mixed, 2, false)
);
case!(
    healthy_copier_storage_drops_cleanly,
    normal(Kind::Copier, 2, false)
);
