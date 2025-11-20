//! Registration owns unpublished inputs before conversion, polling or rejection.
use futures::FutureExt;
use parrot::system::{ParrotActorSystem, PrefixResolver, ReceptionistGateway, RemoteGateway};
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
    payload_drops: AtomicUsize,
    reentered: AtomicUsize,
    throws: bool,
    facade: Weak<ParrotActorSystem>,
}
impl Probe {
    fn new(facade: &Arc<ParrotActorSystem>, throws: bool) -> Arc<Self> {
        Arc::new(Self {
            retired: AtomicUsize::new(0),
            payload_drops: AtomicUsize::new(0),
            reentered: AtomicUsize::new(0),
            throws,
            facade: Arc::downgrade(facade),
        })
    }
    fn check(&self) {
        assert_eq!(self.retired.load(Ordering::SeqCst), 1);
        assert_eq!(self.reentered.load(Ordering::SeqCst), 1);
        assert_eq!(
            self.payload_drops.load(Ordering::SeqCst),
            usize::from(self.throws)
        );
    }
}
struct ThrowingPayload(Arc<Probe>);
impl Drop for ThrowingPayload {
    fn drop(&mut self) {
        self.0.payload_drops.fetch_add(1, Ordering::SeqCst);
        panic!("registration panic payload destructor");
    }
}
struct Owner(Arc<Probe>);
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.retired.fetch_add(1, Ordering::SeqCst);
        if let Some(facade) = self.0.facade.upgrade() {
            // Reenter both topology and lifecycle locks, not just a read lock.
            let _ = facade.unregister_system("missing");
            self.0.reentered.fetch_add(1, Ordering::SeqCst);
        }
        if self.0.throws {
            std::panic::panic_any(ThrowingPayload(self.0.clone()));
        }
    }
}
fn retire_payload(payload: Box<dyn Any + Send>) {
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload)))
        && let Err(last) = catch_unwind(AssertUnwindSafe(|| drop(secondary)))
    {
        std::mem::forget(last);
    }
}
fn did_not_unwind<T>(result: std::thread::Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(payload) => {
            retire_payload(payload);
            panic!("registration input cleanup escaped its owner boundary");
        }
    }
}
fn cleanup_error(result: Result<(), SystemError>, throws: bool) {
    let error = result.expect_err("rejected input cannot be published");
    if throws {
        assert!(error.to_string().contains("disposal"), "{error}");
    } else {
        assert!(matches!(error, SystemError::ShuttingDown));
    }
}

struct Reference {
    _owner: Option<Owner>,
}
impl std::fmt::Debug for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RegistrationReference")
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
        "/registration".into()
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
        Some(Box::new(Reference { _owner: None }))
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
        panic!("rejected runtime must never spawn");
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

#[derive(Debug)]
struct ConversionPanic;
struct Prefix(bool);
impl From<Prefix> for String {
    fn from(prefix: Prefix) -> Self {
        if prefix.0 {
            std::panic::panic_any(ConversionPanic);
        }
        "/invalid".into()
    }
}
async fn prefix_input(resolver: bool, conversion_panics: bool, throws: bool) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let probe = Probe::new(&facade, throws);
    let result = catch_unwind(AssertUnwindSafe(|| {
        if resolver {
            facade
                .register_prefix_resolver(Prefix(conversion_panics), Arc::new(Owner(probe.clone())))
        } else {
            facade.register_prefix_handler(
                Prefix(conversion_panics),
                Arc::new(Reference {
                    _owner: Some(Owner(probe.clone())),
                }),
            )
        }
    }));
    if matches!(result, Ok(Ok(()))) {
        // A broken validator may accept the throwing fixture. Remove it first.
        let _ = facade.unregister_prefix_handler("/invalid");
    }
    if conversion_panics {
        let payload = result.expect_err("conversion panic must remain visible");
        let original = payload.is::<ConversionPanic>();
        retire_payload(payload);
        assert!(original, "input cleanup replaced the conversion failure");
    } else {
        let error = did_not_unwind(result).unwrap_err();
        if throws {
            assert!(error.to_string().contains("disposal"), "{error}");
        } else {
            assert!(matches!(error, SystemError::ActorCreationError(_)));
        }
    }
    probe.check();
    assert!(!facade.unregister_prefix_handler("/invalid").unwrap());
    facade
        .register_prefix_handler("/healthy/", Arc::new(Reference { _owner: None }))
        .unwrap();
    assert!(
        facade
            .internal_get_actor(&parrot_api::address::ActorPath::placeholder("/healthy/a"))
            .await
            .is_some()
    );
    facade.shutdown_runtime().await.unwrap();
}
async fn prefix_closed(resolver: bool) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    facade.shutdown_runtime().await.unwrap();
    let probe = Probe::new(&facade, false);
    let result = if resolver {
        facade.register_prefix_resolver("/closed/", Arc::new(Owner(probe.clone())))
    } else {
        facade.register_prefix_handler(
            "/closed/",
            Arc::new(Reference {
                _owner: Some(Owner(probe.clone())),
            }),
        )
    };
    let published = facade.unregister_prefix_handler("/closed/").unwrap();
    cleanup_error(result, false);
    assert!(!published, "shutdown must fence prefix publication");
    probe.check();
}
async fn gateway_input(remote: bool, closed: bool) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    if closed {
        facade.shutdown_runtime().await.unwrap();
    }
    let probe = Probe::new(&facade, true);
    let future: BoxedFuture<'_, Result<(), SystemError>> = if remote {
        Box::pin(facade.register_remote_gateway(Arc::new(Owner(probe.clone()))))
    } else {
        Box::pin(facade.register_receptionist_gateway(Arc::new(Owner(probe.clone()))))
    };
    let outcome = if closed {
        Some(did_not_unwind(
            AssertUnwindSafe(future).catch_unwind().await,
        ))
    } else {
        did_not_unwind(catch_unwind(AssertUnwindSafe(|| drop(future))));
        None
    };
    let published = facade.remote_gateway().is_some() || facade.receptionist_gateway().is_some();
    if published {
        // Retire an unexpectedly accepted throwing fixture before asserting.
        did_not_unwind(catch_unwind(AssertUnwindSafe(|| drop(facade))));
        if let Some(result) = outcome {
            cleanup_error(result, true);
        }
        panic!("registration published an input after rejection or before polling");
    }
    if let Some(result) = outcome {
        cleanup_error(result, true);
    }
    probe.check();
    facade.shutdown_runtime().await.unwrap();
}
async fn gateway_closed_clean(remote: bool) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    facade.shutdown_runtime().await.unwrap();
    let probe = Probe::new(&facade, false);
    let result = if remote {
        facade
            .register_remote_gateway(Arc::new(Owner(probe.clone())))
            .await
    } else {
        facade
            .register_receptionist_gateway(Arc::new(Owner(probe.clone())))
            .await
    };
    let published = if remote {
        facade.remote_gateway().is_some()
    } else {
        facade.receptionist_gateway().is_some()
    };
    cleanup_error(result, false);
    assert!(!published, "shutdown must fence gateway publication");
    probe.check();
}
async fn engine_input(actix_kind: bool, closed: bool) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    if closed {
        facade.shutdown_runtime().await.unwrap();
    }
    let probe = Probe::new(&facade, true);
    let future: BoxedFuture<'_, Result<(), SystemError>> = if actix_kind {
        let engine = parrot::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap();
        engine.clone().shutdown().await.unwrap();
        engine.set_receptionist_gateway(Arc::new(Owner(probe.clone())));
        Box::pin(facade.register_actix_system("engine".into(), engine, true))
    } else {
        let engine = parrot::thread::ThreadActorSystem::shared(Default::default());
        engine.shutdown_internal().await.unwrap();
        engine.set_receptionist_gateway(Arc::new(Owner(probe.clone())));
        Box::pin(facade.register_thread_system("engine".into(), engine, true))
    };
    let outcome = if closed {
        Some(did_not_unwind(
            AssertUnwindSafe(future).catch_unwind().await,
        ))
    } else {
        did_not_unwind(catch_unwind(AssertUnwindSafe(|| drop(future))));
        None
    };
    if !facade.list_registered_systems().unwrap().is_empty() {
        did_not_unwind(catch_unwind(AssertUnwindSafe(|| drop(facade))));
        if let Some(result) = outcome {
            cleanup_error(result, true);
        }
        panic!("engine registration published before polling or after rejection");
    }
    if let Some(result) = outcome {
        cleanup_error(result, true);
    }
    assert!(facade.list_registered_systems().unwrap().is_empty());
    probe.check();
    facade.shutdown_runtime().await.unwrap();
}
async fn runtime_input() {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    facade.shutdown_runtime().await.unwrap();
    let probe = Probe::new(&facade, true);
    let result = did_not_unwind(catch_unwind(AssertUnwindSafe(|| {
        facade.register_runtime("rejected".into(), Arc::new(Owner(probe.clone())), true)
    })));
    if !facade.list_registered_systems().unwrap().is_empty() {
        did_not_unwind(catch_unwind(AssertUnwindSafe(|| drop(facade))));
        cleanup_error(result.map(|_| ()), true);
        panic!("external runtime registration published after rejection");
    }
    cleanup_error(result.map(|_| ()), true);
    assert!(facade.list_registered_systems().unwrap().is_empty());
    probe.check();
}
async fn publication_waits_for_poll(remote: bool) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let probe = Probe::new(&facade, false);
    let future: BoxedFuture<'_, Result<(), SystemError>> = if remote {
        Box::pin(facade.register_remote_gateway(Arc::new(Owner(probe.clone()))))
    } else {
        Box::pin(facade.register_receptionist_gateway(Arc::new(Owner(probe.clone()))))
    };
    assert!(facade.remote_gateway().is_none());
    assert!(facade.receptionist_gateway().is_none());
    future.await.unwrap();
    assert_eq!(facade.remote_gateway().is_some(), remote);
    assert_eq!(facade.receptionist_gateway().is_some(), !remote);
    assert_eq!(probe.retired.load(Ordering::SeqCst), 0);
    let replacement = Arc::new(Owner(Probe::new(&facade, false)));
    if remote {
        facade.register_remote_gateway(replacement).await.unwrap();
    } else {
        facade
            .register_receptionist_gateway(replacement)
            .await
            .unwrap();
    }
    probe.check();
    facade.shutdown_runtime().await.unwrap();
}

async fn registration_crosses_shutdown(kind: u8) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let probe = Probe::new(&facade, false);
    let future: BoxedFuture<'_, Result<(), SystemError>> = match kind {
        0 => Box::pin(facade.register_remote_gateway(Arc::new(Owner(probe.clone())))),
        1 => Box::pin(facade.register_receptionist_gateway(Arc::new(Owner(probe.clone())))),
        2 => {
            let engine = parrot::thread::ThreadActorSystem::shared(Default::default());
            engine.shutdown_internal().await.unwrap();
            engine.set_receptionist_gateway(Arc::new(Owner(probe.clone())));
            Box::pin(facade.register_thread_system("late".into(), engine, true))
        }
        3 => {
            let engine = parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap();
            engine.clone().shutdown().await.unwrap();
            engine.set_receptionist_gateway(Arc::new(Owner(probe.clone())));
            Box::pin(facade.register_actix_system("late".into(), engine, true))
        }
        _ => unreachable!(),
    };
    assert!(facade.remote_gateway().is_none());
    assert!(facade.receptionist_gateway().is_none());
    assert!(facade.list_registered_systems().unwrap().is_empty());
    facade.shutdown_runtime().await.unwrap();
    let result = future.await;
    let published = facade.remote_gateway().is_some()
        || facade.receptionist_gateway().is_some()
        || !facade.list_registered_systems().unwrap().is_empty();
    cleanup_error(result, false);
    assert!(
        !published,
        "admission must be checked when publication is attempted"
    );
    probe.check();
}

fn isolated(name: &str, test: impl FnOnce()) {
    if std::env::var("PARROT_REGISTRATION_CHILD").ok().as_deref() == Some(name) {
        test();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_REGISTRATION_CHILD", name)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "registration child failed: {name}: {status}"
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("registration callback deadlocked: {name}");
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
    invalid_static_prefix_retires_throwing_input,
    prefix_input(false, false, true)
);
case!(
    invalid_resolver_prefix_retires_throwing_input,
    prefix_input(true, false, true)
);
case!(
    static_conversion_preserves_original_panic_after_throwing_cleanup,
    prefix_input(false, true, true)
);
case!(
    resolver_conversion_preserves_original_panic_after_throwing_cleanup,
    prefix_input(true, true, true)
);
case!(
    shutdown_fences_static_prefix_publication,
    prefix_closed(false)
);
case!(shutdown_fences_resolver_publication, prefix_closed(true));
case!(
    unpolled_remote_registration_owns_input_cleanup,
    gateway_input(true, false)
);
case!(
    unpolled_receptionist_registration_owns_input_cleanup,
    gateway_input(false, false)
);
case!(
    rejected_remote_registration_reports_cleanup_failure,
    gateway_input(true, true)
);
case!(
    rejected_receptionist_registration_reports_cleanup_failure,
    gateway_input(false, true)
);
case!(
    shutdown_fences_remote_gateway_publication,
    gateway_closed_clean(true)
);
case!(
    shutdown_fences_receptionist_gateway_publication,
    gateway_closed_clean(false)
);
case!(
    unpolled_thread_registration_owns_input_cleanup,
    engine_input(false, false)
);
case!(
    unpolled_actix_registration_owns_input_cleanup,
    engine_input(true, false)
);
case!(
    rejected_thread_registration_reports_cleanup_failure,
    engine_input(false, true)
);
case!(
    rejected_actix_registration_reports_cleanup_failure,
    engine_input(true, true)
);
case!(
    rejected_external_runtime_reports_cleanup_failure,
    runtime_input()
);
case!(
    healthy_gateway_publication_starts_only_when_polled,
    publication_waits_for_poll(true)
);
case!(
    healthy_receptionist_publication_starts_only_when_polled,
    publication_waits_for_poll(false)
);
case!(
    invalid_static_prefix_preserves_validation_error,
    prefix_input(false, false, false)
);
case!(
    invalid_resolver_prefix_preserves_validation_error,
    prefix_input(true, false, false)
);
case!(
    static_conversion_without_cleanup_failure_preserves_original_panic,
    prefix_input(false, true, false)
);
case!(
    resolver_conversion_without_cleanup_failure_preserves_original_panic,
    prefix_input(true, true, false)
);
case!(
    pending_remote_registration_is_fenced_after_shutdown,
    registration_crosses_shutdown(0)
);
case!(
    pending_receptionist_registration_is_fenced_after_shutdown,
    registration_crosses_shutdown(1)
);
case!(
    pending_thread_registration_is_fenced_after_shutdown,
    registration_crosses_shutdown(2)
);
case!(
    pending_actix_registration_is_fenced_after_shutdown,
    registration_crosses_shutdown(3)
);
