#![cfg(feature = "host")]

use futures::FutureExt;
use parrot::thread::{ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
    manifest::ArtifactRef,
    plan, AppManifest, ComponentSpec, InstancePolicy, LocalTopology,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    LocalConfig,
    ArtifactCheck,
    GatewayConfig,
    Props,
    Artifact,
    Gateway,
}
impl Stage {
    fn preflight(self) -> bool {
        matches!(
            self,
            Self::LocalConfig | Self::ArtifactCheck | Self::GatewayConfig
        )
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Clean,
    ConstructPanic,
    PollPanic,
    PayloadPollPanic,
    PendingDrop,
    ReadyDrop,
    ErrorDrop,
    ErrorCleanupDrop,
    PartialErrorDrop,
}
impl Mode {
    fn error(self) -> bool {
        matches!(
            self,
            Self::ErrorDrop | Self::ErrorCleanupDrop | Self::PartialErrorDrop
        )
    }
}
#[derive(Debug)]
struct State {
    enabled: AtomicBool,
    futures: AtomicUsize,
    payloads: AtomicUsize,
    actors: AtomicUsize,
    references: AtomicUsize,
    fail_drain: AtomicBool,
    entered: tokio::sync::Notify,
    starts: Mutex<Vec<String>>,
    stops: Mutex<Vec<String>>,
}
struct Payload(Arc<State>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("host callback payload disposal");
    }
}
struct Echo(Arc<State>, String);
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.downcast_ref::<parrot_app::ParrotAppHook>()
                == Some(&parrot_app::ParrotAppHook(2))
                && self.1 == "/user/healthy"
                && self.0.fail_drain.swap(false, Ordering::SeqCst)
            {
                return Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "dependency drain failed".into(),
                ));
            }
            if msg.downcast_ref::<parrot_app::ParrotAppHook>()
                == Some(&parrot_app::ParrotAppHook(0))
            {
                self.0.starts.lock().unwrap().push(self.1.clone());
            }
            Ok(msg)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.stops.lock().unwrap().push(self.1.clone());
            Ok(())
        })
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.0.actors.fetch_add(1, Ordering::SeqCst);
    }
}
struct Callback<'a, T> {
    state: Arc<State>,
    mode: Mode,
    inner: BoxedFuture<'a, T>,
}
impl<T> Future for Callback<'_, T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        this.state.entered.notify_one();
        match this.mode {
            Mode::PendingDrop => Poll::Pending,
            Mode::PollPanic => panic!("host callback poll"),
            Mode::PayloadPollPanic => std::panic::panic_any(Payload(this.state.clone())),
            _ => this.inner.as_mut().poll(cx),
        }
    }
}
impl<T> Drop for Callback<'_, T> {
    fn drop(&mut self) {
        self.state.futures.fetch_add(1, Ordering::SeqCst);
        if self.mode != Mode::Clean {
            std::panic::panic_any(Payload(self.state.clone()));
        }
    }
}
#[derive(Debug)]
struct ThrowingRef(BoxedActorRef, Arc<State>);
impl Drop for ThrowingRef {
    fn drop(&mut self) {
        self.1.references.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Payload(self.1.clone()));
    }
}
impl ActorRef for ThrowingRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send(msg)
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send_with_timeout(msg, timeout)
    }
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.deliver(msg)
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.stop()
    }
    fn retirement_status(&self) -> RetirementStatus {
        self.0.retirement_status()
    }
    fn path(&self) -> String {
        panic!("failure diagnostics must use declared paths")
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        self.0.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        panic!("failed handoff must retain originals without cloning user refs")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    state: Arc<State>,
    originals: Mutex<Vec<(String, BoxedActorRef)>>,
    stage: Stage,
    mode: Mode,
    throwing_refs: bool,
}
impl Host {
    fn new(stage: Stage, mode: Mode, throwing_refs: bool) -> Self {
        Self {
            engine: ThreadActorSystem::shared(Default::default()),
            state: Arc::new(State {
                enabled: AtomicBool::new(true),
                futures: AtomicUsize::new(0),
                payloads: AtomicUsize::new(0),
                actors: AtomicUsize::new(0),
                references: AtomicUsize::new(0),
                fail_drain: AtomicBool::new(mode == Mode::ErrorCleanupDrop),
                entered: tokio::sync::Notify::new(),
                starts: Mutex::new(Vec::new()),
                stops: Mutex::new(Vec::new()),
            }),
            originals: Mutex::new(Vec::new()),
            stage,
            mode,
            throwing_refs,
        }
    }
    fn active(&self, stage: Stage) -> bool {
        self.stage == stage && self.state.enabled.load(Ordering::SeqCst)
    }
    fn sync(&self, stage: Stage) -> Result<(), DeployError> {
        if self.active(stage) {
            std::panic::panic_any(Payload(self.state.clone()));
        }
        Ok(())
    }
    fn callback<'a, T: 'a>(&self, inner: BoxedFuture<'a, T>) -> BoxedFuture<'a, T> {
        if self.mode == Mode::ConstructPanic {
            std::panic::panic_any(Payload(self.state.clone()));
        }
        Box::pin(Callback {
            state: self.state.clone(),
            mode: self.mode,
            inner,
        })
    }
    fn configure_gateway(&self) -> BoxedFuture<'_, Result<(), DeployError>> {
        if self.active(Stage::GatewayConfig) {
            self.callback(Box::pin(async {
                if self.mode.error() {
                    Err(DeployError::Artifact("first callback failure".into()))
                } else {
                    Ok(())
                }
            }))
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn spawning<'a>(
        &'a self,
        stage: Stage,
        paths: &'a [String],
    ) -> BoxedFuture<'a, Result<Vec<BoxedActorRef>, DeployError>> {
        if self.active(stage) && paths[0].starts_with("/user/bad") {
            self.callback(Box::pin(async move {
                if self.mode == Mode::PartialErrorDrop {
                    let actors = self.spawn(paths).await?;
                    Err(DeployError::Cleanup {
                        source: Box::new(DeployError::Artifact("first callback failure".into())),
                        retained: paths
                            .iter()
                            .zip(actors)
                            .map(|(path, actor)| parrot_app::cleanup::RetainedActor {
                                path: path.clone(),
                                error: "partial host creation".into(),
                                actor: actor.into(),
                            })
                            .collect(),
                        detail: "partial host creation".into(),
                    })
                } else if self.mode.error() {
                    Err(DeployError::Artifact("first callback failure".into()))
                } else {
                    self.spawn(paths).await
                }
            }))
        } else {
            Box::pin(self.spawn(paths))
        }
    }
    async fn spawn(&self, paths: &[String]) -> Result<Vec<BoxedActorRef>, DeployError> {
        let mut refs = Vec::new();
        for path in paths {
            let actor = self
                .engine
                .spawn_at(
                    Echo(self.state.clone(), path.clone()),
                    path,
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
            actor.send(Box::new(7u64)).await.unwrap();
            self.originals
                .lock()
                .unwrap()
                .push((path.clone(), actor.clone_boxed()));
            let actor = Box::new(actor) as BoxedActorRef;
            refs.push(
                if self.throwing_refs
                    && self.state.enabled.load(Ordering::SeqCst)
                    && path.starts_with("/user/bad")
                {
                    Box::new(ThrowingRef(actor, self.state.clone())) as BoxedActorRef
                } else {
                    actor
                },
            );
        }
        Ok(refs)
    }
    fn retired(&self) -> bool {
        self.originals.lock().unwrap().iter().all(|(_, actor)| {
            matches!(actor.retirement_status(), RetirementStatus::Retired(Ok(())))
        })
    }
    async fn stop_gateway_actors(&self) -> Result<(), DeployError> {
        let actors: Vec<_> = self
            .originals
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.starts_with("/user/bad"))
            .map(|(_, actor)| actor.clone_boxed())
            .collect();
        for actor in actors {
            parrot_api::address::stop_for_owned_cleanup(&*actor)
                .await
                .unwrap();
        }
        Ok(())
    }
    async fn close(&self, context: &mut AssemblingContext, deployer: &LocalDeployer<'_>) {
        context.teardown(deployer).await.unwrap();
        let actors: Vec<_> = self
            .originals
            .lock()
            .unwrap()
            .iter()
            .map(|(_, actor)| actor.clone_boxed())
            .collect();
        for actor in actors {
            parrot_api::address::stop_for_owned_cleanup(&*actor)
                .await
                .unwrap();
        }
    }
}
#[async_trait::async_trait]
impl ParrotSpawner for Host {
    fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        self.sync(Stage::LocalConfig)
    }
    fn validate_artifact(&self, artifact: &ArtifactRef) -> Result<(), DeployError> {
        if matches!(artifact, ArtifactRef::Props { factory } if factory == "bad") {
            self.sync(Stage::ArtifactCheck)?;
        }
        Ok(())
    }
    async fn spawn_props(
        &self,
        _: &str,
        paths: &[String],
        _: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.spawning(Stage::Props, paths).await
    }
    fn spawn_artifact<'life0, 'life1, 'life2, 'life3, 'async_trait>(
        &'life0 self,
        _: &'life1 ArtifactRef,
        paths: &'life2 [String],
        _: Option<&'life3 toml::Table>,
    ) -> BoxedFuture<'async_trait, Result<Vec<BoxedActorRef>, DeployError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        'life3: 'async_trait,
        Self: 'async_trait,
    {
        self.spawning(Stage::Artifact, paths)
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("cleanup must use original identities")
    }
}
struct PropsOnly<'a>(&'a Host);
#[async_trait::async_trait]
impl ParrotSpawner for PropsOnly<'_> {
    fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    fn spawn_props<'life0, 'life1, 'life2, 'life3, 'async_trait>(
        &'life0 self,
        _: &'life1 str,
        paths: &'life2 [String],
        _: Option<&'life3 toml::Table>,
    ) -> BoxedFuture<'async_trait, Result<Vec<BoxedActorRef>, DeployError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        'life3: 'async_trait,
        Self: 'async_trait,
    {
        self.0.spawning(Stage::Props, paths)
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("cleanup must use original identities")
    }
}
#[async_trait::async_trait]
impl GatewayFactory for Host {
    fn configure<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _: &'life1 parrot_config::Resolved,
    ) -> BoxedFuture<'async_trait, Result<(), DeployError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.configure_gateway()
    }
    fn start_gateway<'life0, 'life1, 'life2, 'life3, 'async_trait>(
        &'life0 self,
        _: &'life1 ComponentSpec,
        paths: &'life2 [String],
        _: &'life3 str,
    ) -> BoxedFuture<'async_trait, Result<Vec<BoxedActorRef>, DeployError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        'life3: 'async_trait,
        Self: 'async_trait,
    {
        self.spawning(Stage::Gateway, paths)
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        self.stop_gateway_actors().await
    }
}
fn dispose_payload(payload: Box<dyn std::any::Any + Send>) {
    if let Err(nested) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))) {
        drop(nested);
    }
}
async fn scenario(stage: Stage, mode: Mode, throwing_refs: bool) {
    let host = Host::new(stage, mode, throwing_refs);
    let props = PropsOnly(&host);
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &host,
        parrot_spawner: if stage == Stage::Props { &props } else { &host },
    };
    let mut manifest = AppManifest::from_toml("name='host-ownership'\nversion='1.0.0'\n[[components]]\nname='healthy'\nengine='parrot'\nartifact={Props={factory='healthy'}}\n[components.hooks]\non_start='.'\n[[components]]\nname='bad'\ndeps=['healthy']\nengine='parrot'\nartifact={Props={factory='bad'}}\n[components.hooks]\non_start='.'\n").unwrap();
    manifest.components[1].instances = InstancePolicy::Pool(2);
    manifest.components[0].hooks.on_drain = Some(".".into());
    if stage == Stage::Gateway {
        manifest.components[1].engine = parrot_app::EngineKind::Erlang;
        manifest.components[1].artifact = ArtifactRef::Beam {
            app: "bad".into(),
            uri: None,
        };
    }
    let plan = plan(&manifest, &LocalTopology).unwrap();
    let cfg = Default::default();
    let mut context = AssemblingContext::new();
    let mut escaped = false;
    if mode == Mode::PendingDrop {
        let mut assembly = Box::pin(context.assemble(&plan, &deployer, &manifest, &cfg));
        tokio::select! {
            result = &mut assembly => panic!("unexpected assembly completion: {result:?}"),
            ready = tokio::time::timeout(Duration::from_secs(3), host.state.entered.notified()) => ready.unwrap(),
        }
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(assembly)))
        {
            escaped = true;
            dispose_payload(payload);
        }
        assert_eq!(
            context.started().len(),
            if stage.preflight() { 0 } else { 1 }
        );
        assert_eq!(host.state.actors.load(Ordering::SeqCst), 0);
    } else {
        let result =
            std::panic::AssertUnwindSafe(context.assemble(&plan, &deployer, &manifest, &cfg))
                .catch_unwind()
                .await;
        let (completed, primary_preserved, failed) = match result {
            Ok(Ok(())) => (true, false, false),
            Ok(Err(error)) => {
                let preserved = matches!(error.primary_error(), DeployError::Artifact(text) if text == "first callback failure");
                if mode.error() {
                    assert!(
                        error.to_string().contains("future disposal panicked"),
                        "{error:?}"
                    );
                }
                if mode == Mode::ErrorCleanupDrop {
                    assert!(
                        error.to_string().contains("dependency drain failed"),
                        "{error:?}"
                    );
                }
                (false, preserved, true)
            }
            Err(payload) => {
                escaped = true;
                dispose_payload(payload);
                (false, false, false)
            }
        };
        let retired_before_fallback = host.retired();
        let dropped_before_fallback = host.state.actors.load(Ordering::SeqCst);
        let empty_before_fallback = context.started().is_empty();
        let starts = host.state.starts.lock().unwrap().clone();
        let stops = host.state.stops.lock().unwrap().clone();
        let created = host.originals.lock().unwrap().len();
        host.state.enabled.store(false, Ordering::SeqCst);
        host.close(&mut context, &deployer).await;
        assert!(!escaped, "host callback panic escaped assembly");
        if mode == Mode::Clean {
            assert!(completed, "clean completion must publish the cohort");
            assert_eq!(created, 3);
            assert_eq!(starts.len(), 3);
        } else {
            assert!(
                failed,
                "host callback failure incorrectly published success"
            );
            assert!(
                retired_before_fallback,
                "returned actor ownership was lost before rollback"
            );
            assert_eq!(
                dropped_before_fallback, created,
                "fallback hid skipped actual disposal"
            );
            assert!(
                empty_before_fallback,
                "automatic rollback left owned components"
            );
            assert_eq!(
                created,
                if stage.preflight() {
                    0
                } else if matches!(mode, Mode::ReadyDrop | Mode::PartialErrorDrop) {
                    3
                } else {
                    1
                }
            );
            assert!(
                !starts.iter().any(|path| path.starts_with("/user/bad")),
                "failed handoff ran application START"
            );
            if mode.error() {
                assert!(
                    primary_preserved,
                    "future disposal replaced the first callback error"
                );
            }
            if matches!(mode, Mode::ReadyDrop | Mode::PartialErrorDrop) && !stage.preflight() {
                assert_eq!(stops.len(), 3);
                assert_eq!(
                    stops.last().map(String::as_str),
                    Some("/user/healthy"),
                    "rollback must retire the dependent cohort before its dependency"
                );
            }
        }
    }
    host.state.enabled.store(false, Ordering::SeqCst);
    host.close(&mut context, &deployer).await;
    context
        .assemble(&plan, &deployer, &manifest, &cfg)
        .await
        .unwrap();
    assert_eq!(context.started().len(), 2);
    host.close(&mut context, &deployer).await;
    assert!(host.retired());
    assert_eq!(
        host.state.actors.load(Ordering::SeqCst),
        host.originals.lock().unwrap().len()
    );
    host.engine.shutdown_internal().await.unwrap();
    assert!(!escaped, "host callback disposal escaped cancellation");
    assert_eq!(
        host.state.futures.load(Ordering::SeqCst),
        usize::from(mode != Mode::ConstructPanic)
    );
    let reference_payloads = if throwing_refs { 2 } else { 0 };
    assert_eq!(
        host.state.references.load(Ordering::SeqCst),
        reference_payloads
    );
    assert_eq!(
        host.state.payloads.load(Ordering::SeqCst),
        usize::from(mode != Mode::Clean)
            + usize::from(mode == Mode::PayloadPollPanic)
            + reference_payloads
    );
}
fn isolated(name: &str, stage: Stage, mode: Mode, throwing_refs: bool) {
    const FLAG: &str = "PARROT_HOST_FUTURE_OWNERSHIP_CHILD";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(scenario(stage, mode, throwing_refs));
        return;
    }
    let mut log = tempfile::tempfile().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(FLAG, name)
        .env("RUST_BACKTRACE", "0")
        .stdout(log.try_clone().unwrap())
        .stderr(log.try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            use std::io::{Read, Seek};
            log.rewind().unwrap();
            let mut output = String::new();
            log.read_to_string(&mut output).unwrap();
            assert!(status.success(), "child {name}: {status}\n{output}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("host callback contract exceeded process deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! contract {
    ($name:ident, $stage:ident, $mode:ident) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), Stage::$stage, Mode::$mode, false);
        }
    };
}
contract!(
    local_config_throwing_panic_is_a_recoverable_error,
    LocalConfig,
    ConstructPanic
);
contract!(
    artifact_check_throwing_panic_is_a_recoverable_error,
    ArtifactCheck,
    ConstructPanic
);
contract!(
    gateway_config_construction_panic_is_a_recoverable_error,
    GatewayConfig,
    ConstructPanic
);
contract!(
    gateway_config_poll_and_disposal_panics_do_not_abort,
    GatewayConfig,
    PollPanic
);
contract!(
    gateway_config_throwing_poll_payload_and_disposal_are_independent,
    GatewayConfig,
    PayloadPollPanic
);
contract!(
    gateway_config_cancellation_does_not_unwind,
    GatewayConfig,
    PendingDrop
);
contract!(
    gateway_config_failed_disposal_cannot_publish_success,
    GatewayConfig,
    ReadyDrop
);
contract!(
    gateway_config_first_error_survives_failed_disposal,
    GatewayConfig,
    ErrorDrop
);
contract!(
    gateway_config_clean_handoff_keeps_assembly_usable,
    GatewayConfig,
    Clean
);
contract!(
    props_default_construction_panic_rolls_back,
    Props,
    ConstructPanic
);
contract!(
    props_default_poll_and_disposal_panics_do_not_abort,
    Props,
    PollPanic
);
contract!(
    props_default_cancellation_retains_explicit_cleanup,
    Props,
    PendingDrop
);
contract!(
    props_default_failed_disposal_rolls_back_returned_actors,
    Props,
    ReadyDrop
);
contract!(
    props_default_first_error_survives_failed_disposal,
    Props,
    ErrorDrop
);
contract!(
    artifact_construction_panic_rolls_back,
    Artifact,
    ConstructPanic
);
contract!(
    artifact_poll_and_disposal_panics_do_not_abort,
    Artifact,
    PollPanic
);
contract!(
    artifact_throwing_poll_payload_and_disposal_are_independent,
    Artifact,
    PayloadPollPanic
);
contract!(
    artifact_cancellation_retains_explicit_cleanup,
    Artifact,
    PendingDrop
);
contract!(
    artifact_failed_disposal_rolls_back_returned_actors,
    Artifact,
    ReadyDrop
);
contract!(
    artifact_first_error_survives_failed_disposal,
    Artifact,
    ErrorDrop
);
contract!(
    gateway_start_construction_panic_rolls_back,
    Gateway,
    ConstructPanic
);
contract!(
    gateway_start_poll_and_disposal_panics_do_not_abort,
    Gateway,
    PollPanic
);
contract!(
    gateway_start_cancellation_retains_explicit_cleanup,
    Gateway,
    PendingDrop
);
contract!(
    gateway_start_failed_disposal_rolls_back_returned_actors,
    Gateway,
    ReadyDrop
);
contract!(
    gateway_start_first_error_survives_failed_disposal,
    Gateway,
    ErrorDrop
);
contract!(
    props_error_survives_future_and_rollback_failures,
    Props,
    ErrorCleanupDrop
);
contract!(
    artifact_error_survives_future_and_rollback_failures,
    Artifact,
    ErrorCleanupDrop
);
contract!(
    gateway_error_survives_future_and_rollback_failures,
    Gateway,
    ErrorCleanupDrop
);
contract!(
    props_partial_error_retires_original_actors,
    Props,
    PartialErrorDrop
);
contract!(
    artifact_partial_error_retires_original_actors,
    Artifact,
    PartialErrorDrop
);
contract!(
    gateway_partial_error_retires_original_actors,
    Gateway,
    PartialErrorDrop
);
contract!(
    props_default_clean_handoff_publishes_and_retires_real_cohort,
    Props,
    Clean
);
contract!(
    artifact_clean_handoff_publishes_and_retires_real_cohort,
    Artifact,
    Clean
);
contract!(
    gateway_clean_handoff_publishes_and_retires_real_cohort,
    Gateway,
    Clean
);
#[test]
fn failed_handoff_retires_originals_then_isolates_each_returned_reference_disposal() {
    isolated(
        "failed_handoff_retires_originals_then_isolates_each_returned_reference_disposal",
        Stage::Artifact,
        Mode::ReadyDrop,
        true,
    );
}
