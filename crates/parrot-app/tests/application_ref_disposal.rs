//! User reference destructors must not unwind through other owned references.
#![cfg(feature = "host")]
use futures::FutureExt;
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
#[derive(Default, Debug)]
struct State {
    panic_kind: AtomicUsize,
    dropped: AtomicUsize,
    stopped: AtomicUsize,
    resolving: tokio::sync::Notify,
}
struct Echo(Arc<State>);
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.stopped.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[derive(Debug)]
struct Ref {
    actor: BoxedActorRef,
    state: Arc<State>,
    cloned: bool,
}
struct ThrowingPayload;
impl Drop for ThrowingPayload {
    fn drop(&mut self) {
        std::panic::panic_any(Self);
    }
}
impl Drop for Ref {
    fn drop(&mut self) {
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
        let kind = self.state.panic_kind.load(Ordering::SeqCst);
        if kind == 4 {
            std::panic::panic_any(ThrowingPayload);
        }
        if kind == 3 || kind == if self.cloned { 2 } else { 1 } {
            panic!("injected reference destructor panic");
        }
    }
}
impl ActorRef for Ref {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send_with_timeout(message, timeout)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.deliver(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.stop()
    }
    fn path(&self) -> String {
        self.actor.path()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.actor.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            actor: self.actor.clone_boxed(),
            state: self.state.clone(),
            cloned: true,
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    state: Arc<State>,
}
#[async_trait::async_trait]
impl ParrotSpawner for Host {
    fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn spawn_props(
        &self,
        _: &str,
        paths: &[String],
        _: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        let mut actors = Vec::new();
        for path in paths {
            let actor = self
                .engine
                .spawn_at(Echo(self.state.clone()), path, None, Default::default())
                .await
                .unwrap();
            actor.send(Box::new(7u64)).await.unwrap();
            actors.push(Box::new(Ref {
                actor: Box::new(actor),
                state: self.state.clone(),
                cloned: false,
            }) as BoxedActorRef);
        }
        Ok(actors)
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("must stop original identities")
    }
    async fn hook_target(&self, _: &str) -> Result<BoxedActorRef, DeployError> {
        self.state.resolving.notify_one();
        std::future::pending().await
    }
}
#[async_trait::async_trait]
impl GatewayFactory for Host {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("unexpected gateway")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("unexpected gateway")
    }
}
fn manifest(mode: &str) -> AppManifest {
    let mut text = String::from("name='disposal'\nversion='1.0.0'\n");
    for name in ["a", "b"] {
        text += &format!("[[components]]\nname='{name}'\nengine='parrot'\nartifact={{Props={{factory='echo'}}}}\ninstances={{Pool=2}}\n");
        if name == "b" {
            text += "deps=['a']\n";
        }
        if mode == "hooks" || mode == "drop" {
            text += "[components.hooks]\non_start='.'\non_stop='.'\n";
        }
    }
    if matches!(mode, "wires" | "cancelled" | "drop" | "drain") {
        text += "[[wiring]]\nfrom='b:/user/first'\nto='a:/user/a-0'\n";
        let target = if mode == "cancelled" {
            "a:/user/a-1/pending"
        } else {
            "a:/user/a-1"
        };
        text += &format!("[[wiring]]\nfrom='b:/user/second'\nto='{target}'\n");
    }
    AppManifest::from_toml(&text).unwrap()
}
async fn exercise(mode: &str) {
    let engine = ThreadActorSystem::shared(Default::default());
    let state = Arc::new(State::default());
    let host = Host {
        engine: engine.clone(),
        state: state.clone(),
    };
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &host,
        parrot_spawner: &host,
    };
    let manifest = manifest(mode);
    let plan = plan(&manifest, &LocalTopology).unwrap();
    let mut context = AssemblingContext::new();
    if mode == "cancelled" {
        let cfg = Default::default();
        let mut assemble = Box::pin(context.assemble(&plan, &deployer, &manifest, &cfg));
        tokio::select! {
            result = &mut assemble => panic!("unexpected assembly result: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(3), state.resolving.notified()) => result.unwrap(),
        }
        state.panic_kind.store(2, Ordering::SeqCst);
        let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(assemble)));
        assert!(
            cancelled.is_ok(),
            "cancelled resolution unwound while dropping an accumulated reference"
        );
        assert!(
            context.wired_ref("b:/user/first").is_none(),
            "partial wiring was published"
        );
    } else {
        context
            .assemble(&plan, &deployer, &manifest, &Default::default())
            .await
            .unwrap();
        state.panic_kind.store(
            if mode == "roots" {
                1
            } else if mode == "payload" {
                4
            } else if mode == "drop" {
                3
            } else {
                2
            },
            Ordering::SeqCst,
        );
    }
    if mode == "drain" {
        let result = std::panic::AssertUnwindSafe(context.drain(&deployer))
            .catch_unwind()
            .await;
        assert!(
            matches!(result, Ok(Err(_))),
            "drain must report disposal failures: {result:?}"
        );
        assert!(context.wired_ref("b:/user/first").is_none());
        assert_eq!(
            engine.actor_count(),
            4,
            "drain must not terminate these actors"
        );
        context.drain(&deployer).await.unwrap();
    }
    if mode == "drop" {
        // Drop only releases references; actual retirement is explicitly awaited.
        engine.shutdown_internal().await.unwrap();
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(context)));
        assert!(
            dropped.is_ok(),
            "context Drop escaped a reference destructor panic"
        );
    } else {
        let result = std::panic::AssertUnwindSafe(context.teardown(&deployer))
            .catch_unwind()
            .await;
        if mode == "drain" {
            result.unwrap().unwrap();
        } else {
            let error = result
                .expect("disposal unwound")
                .expect_err("disposal failure was hidden");
            assert!(error.to_string().contains("reference panicked"), "{error}");
        }
        for name in ["a", "b"] {
            assert!(context.component_refs(name).is_none());
            assert!(context.component_paths(name).is_none());
        }
        assert!(
            context.started().is_empty(),
            "retired component metadata remained inconsistent"
        );
        assert_eq!(
            engine.actor_count(),
            0,
            "reference destructor skipped another component's retirement"
        );
        context.teardown(&deployer).await.unwrap();
        // Clearing a finished component must also leave this context reusable.
        state.panic_kind.store(0, Ordering::SeqCst);
        let fresh = self::manifest("roots");
        context
            .assemble(
                &parrot_app::planner::plan(&fresh, &LocalTopology).unwrap(),
                &deployer,
                &fresh,
                &Default::default(),
            )
            .await
            .unwrap();
        context.teardown(&deployer).await.unwrap();
        engine.shutdown_internal().await.unwrap();
    }
    assert_eq!(engine.actor_count(), 0);
    let expected_drops = match mode {
        "hooks" => 12,
        "wires" | "drain" | "drop" => 10,
        "cancelled" => 9,
        _ => 8,
    };
    assert_eq!(
        state.dropped.load(Ordering::SeqCst),
        expected_drops,
        "every owned reference must be released exactly once"
    );
    assert_eq!(
        state.stopped.load(Ordering::SeqCst),
        if mode == "drop" { 4 } else { 8 }
    );
}
// Baselines with two panicking destructors can abort, so isolate every case.
fn child(name: &str, mode: &str) {
    if std::env::var("PARROT_REF_DISPOSAL_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(mode));
    } else {
        let output = tempfile::NamedTempFile::new().unwrap();
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_REF_DISPOSAL_CHILD", name)
            .stdout(output.as_file().try_clone().unwrap())
            .stderr(output.as_file().try_clone().unwrap())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = process.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                process.kill().unwrap();
                process.wait().unwrap();
                panic!(
                    "disposal child timed out: {}",
                    std::fs::read_to_string(output.path()).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.success(),
            "disposal child failed: {status}\n{}",
            std::fs::read_to_string(output.path()).unwrap()
        );
    }
}
#[test]
fn registry_reference_panics_do_not_skip_other_components() {
    child(
        "registry_reference_panics_do_not_skip_other_components",
        "roots",
    );
}
#[test]
fn hook_reference_panics_do_not_corrupt_context_state() {
    child(
        "hook_reference_panics_do_not_corrupt_context_state",
        "hooks",
    );
}
#[test]
fn wire_reference_panics_do_not_prevent_teardown() {
    child("wire_reference_panics_do_not_prevent_teardown", "wires");
}
#[test]
fn context_drop_isolates_reference_panics() {
    child("context_drop_isolates_reference_panics", "drop");
}
#[test]
fn cancelled_wire_resolution_retains_owned_references() {
    child(
        "cancelled_wire_resolution_retains_owned_references",
        "cancelled",
    );
}

#[test]
fn reference_panic_payload_destructor_is_bounded() {
    child("reference_panic_payload_destructor_is_bounded", "payload");
}
#[test]
fn drain_reports_wire_disposal_and_remains_retryable() {
    child("drain_reports_wire_disposal_and_remains_retryable", "drain");
}
