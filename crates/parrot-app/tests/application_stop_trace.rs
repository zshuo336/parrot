#![cfg(feature = "host")]

use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus},
    trace::{self, SpanKind, SpanStatus, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
    host::{ApplicationLookup, GatewayRoutes},
    plan, AppManifest, ComponentSpec, LocalTopology,
};
use parrot_remote::{RemoteActorSystem, RemoteConfig};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::Poll,
    time::Duration,
};

struct State {
    events: Mutex<Vec<(&'static str, Option<TraceContext>)>>,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    disposed: AtomicUsize,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            disposed: AtomicUsize::new(0),
        })
    }
    fn record(&self, stage: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((stage, TraceContext::current()));
    }
}
struct Guard(Arc<State>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.record("drop");
    }
}
struct Probe(Arc<State>);
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.record("construct");
        let guard = Guard(self.0.clone());
        Box::pin(async move {
            guard.0.record("poll");
            guard.0.entered.add_permits(1);
            guard.0.release.acquire().await.unwrap().forget();
            guard.0.record("resume");
            Ok(())
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.disposed.fetch_add(1, Ordering::SeqCst);
    }
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    primary: Arc<State>,
    shadow: Arc<State>,
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
        let mut refs = Vec::new();
        for path in paths {
            let state = if path == "/user/primary" {
                &self.primary
            } else {
                &self.shadow
            };
            let actor = self
                .engine
                .spawn_at(Probe(state.clone()), path, None, Default::default())
                .await
                .unwrap();
            actor.send(Box::new(7u64)).await.unwrap();
            refs.push(Box::new(actor) as BoxedActorRef);
        }
        Ok(refs)
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("must stop original actor identities")
    }
}
struct NoGateway;
#[async_trait::async_trait]
impl GatewayFactory for NoGateway {
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
#[derive(Clone, Copy)]
enum ParentMode {
    Recorded,
    Untraced,
    IdentityOnly,
    ClosedRecorder,
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("application stop identity deadline")
}
async fn contract(identity: ParentMode, mirrored: bool, cloned: bool, cancel: bool) {
    let host = Host {
        engine: ThreadActorSystem::shared(Default::default()),
        primary: State::new(),
        shadow: State::new(),
    };
    host.shadow.release.add_permits(1);
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp("stop-trace-app", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(ApplicationLookup {
            ts: host.engine.clone(),
            routes: Arc::new(GatewayRoutes::default()),
        }),
    )
    .unwrap();
    let mut manifest = AppManifest::from_toml("name='stop-trace'\nversion='1.0.0'\n[[components]]\nname='primary'\nengine='parrot'\nartifact={Props={factory='probe'}}\n[[components]]\nname='shadow'\nengine='parrot'\nartifact={Props={factory='probe'}}\n[[wiring]]\nfrom='shadow:/user/out'\nto='primary:/user/primary'\n").unwrap();
    if mirrored {
        manifest
            .mirrors
            .push(parrot_app::mirror::MirrorSpec::new("primary", "shadow"));
    }
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &host,
    };
    let mut context = AssemblingContext::new().with_mirror_host(&remote);
    TraceContext::root()
        .scope(context.assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        ))
        .await
        .unwrap();
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = match identity {
        ParentMode::Recorded => Some(recorder.root()),
        ParentMode::Untraced => None,
        ParentMode::IdentityOnly => Some(TraceContext::root()),
        ParentMode::ClosedRecorder => {
            let root = recorder.root();
            recorder.close();
            Some(root)
        }
    };
    let poll_recorder = TraceRecorder::new(8).unwrap();
    let poller = poll_recorder.root();
    let original = context.component_ref("primary").unwrap();
    let copy = original.clone_boxed();
    let actor = if cloned { &copy } else { original };
    // Constructing and dropping a stop future must not claim stop ownership.
    drop(poller.in_scope(|| actor.stop()));
    assert!(host.primary.events.lock().unwrap().is_empty());
    let mut stop = trace::in_scope(parent.clone(), || actor.stop());
    assert!(poller
        .scope(std::future::poll_fn(|cx| Poll::Ready(
            stop.as_mut().poll(cx)
        )))
        .await
        .is_pending());
    assert!(TraceContext::current().is_none(), "poll leaked TLS context");
    bounded(host.primary.entered.acquire())
        .await
        .unwrap()
        .forget();
    let pending = recorder.snapshot();
    // Keep these observations until cleanup; a baseline failure must not leave
    // the real hook blocked or rely on runtime shutdown to retire the actor.
    let pending_records = pending.records.len();
    let pending_started = pending.counts.started;
    let pending_flight = pending.counts.in_flight;
    if cancel {
        drop(stop);
        stop = poller.in_scope(|| actor.stop());
        assert!(poller
            .scope(std::future::poll_fn(|cx| Poll::Ready(
                stop.as_mut().poll(cx)
            )))
            .await
            .is_pending());
    }
    host.primary.release.add_permits(1);
    bounded(poller.scope(stop)).await.unwrap();
    assert!(matches!(
        actor.retirement_status(),
        RetirementStatus::Retired(Ok(()))
    ));
    let primary_disposed = host.primary.disposed.load(Ordering::SeqCst);
    let shadow_disposed = host.shadow.disposed.load(Ordering::SeqCst);
    let shadow_events = host.shadow.events.lock().unwrap().len();
    bounded(context.teardown(&deployer)).await.unwrap();
    remote.shutdown().await.unwrap();
    host.engine.shutdown_internal().await.unwrap();

    assert_eq!(primary_disposed, 1);
    assert_eq!(shadow_disposed, 0, "stop was mirrored to shadow");
    assert_eq!(shadow_events, 0, "primary stop ran a shadow hook");
    assert_eq!(host.primary.disposed.load(Ordering::SeqCst), 1);
    assert_eq!(host.shadow.disposed.load(Ordering::SeqCst), 1);
    let events = host.primary.events.lock().unwrap().clone();
    assert_eq!(
        events.iter().map(|event| event.0).collect::<Vec<_>>(),
        ["construct", "poll", "resume", "drop"]
    );
    let child = events[0].1.clone();
    match (&child, &parent) {
        (Some(child), Some(parent)) => {
            assert_eq!(
                child.trace_id(),
                parent.trace_id(),
                "stop adopted polling context"
            );
            assert_eq!(child.parent_span_id(), Some(parent.span_id()));
            assert_ne!(child.span_id(), parent.span_id());
        }
        (None, None) => {}
        mismatch => panic!("stop lost construction-time identity: {mismatch:?}"),
    }
    assert!(
        events.iter().all(|event| event.1 == child),
        "hook identity changed between stages"
    );
    let expected = u64::from(matches!(identity, ParentMode::Recorded));
    assert_eq!(pending_records, 0, "pending hook was published complete");
    assert_eq!(pending_started, expected);
    assert_eq!(pending_flight, expected);
    recorder.close();
    let snapshot = recorder.snapshot();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.started, expected);
    assert_eq!(snapshot.counts.completed, expected);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(snapshot.records.len() as u64, expected);
    if expected == 1 {
        let record = &snapshot.records[0];
        let child = child.unwrap();
        assert_eq!(record.kind, SpanKind::ActorBeforeStop);
        assert_eq!(record.status, SpanStatus::Ok);
        assert_eq!(record.target, "/user/primary");
        assert_eq!(record.trace_id, child.trace_id());
        assert_eq!(record.span_id, child.span_id());
        assert_eq!(record.parent_span_id, child.parent_span_id());
    }
    let unrelated = poll_recorder.snapshot();
    assert_eq!(unrelated.counts.started, 0, "poller acquired a stop span");
    assert!(unrelated.records.is_empty());
    assert!(TraceContext::current().is_none());
}
#[tokio::test]
async fn mirrored_stop_preserves_recorded_parent() {
    contract(ParentMode::Recorded, true, false, false).await;
}
#[tokio::test]
async fn mirrored_clone_stop_preserves_recorded_parent() {
    contract(ParentMode::Recorded, true, true, false).await;
}
#[tokio::test]
async fn mirrored_stop_preserves_explicit_none() {
    contract(ParentMode::Untraced, true, false, false).await;
}
#[tokio::test]
async fn mirrored_clone_stop_preserves_explicit_none() {
    contract(ParentMode::Untraced, true, true, false).await;
}
#[tokio::test]
async fn mirrored_stop_preserves_identity_without_recorder() {
    contract(ParentMode::IdentityOnly, true, false, false).await;
}
#[tokio::test]
async fn mirrored_clone_stop_preserves_identity_without_recorder() {
    contract(ParentMode::IdentityOnly, true, true, false).await;
}
#[tokio::test]
async fn mirrored_stop_preserves_closed_recorder_identity() {
    contract(ParentMode::ClosedRecorder, true, false, false).await;
}
#[tokio::test]
async fn mirrored_clone_stop_preserves_closed_recorder_identity() {
    contract(ParentMode::ClosedRecorder, true, true, false).await;
}
#[tokio::test]
async fn cancelled_mirrored_stop_keeps_first_recorded_caller() {
    contract(ParentMode::Recorded, true, true, true).await;
}
#[tokio::test]
async fn cancelled_mirrored_stop_keeps_first_untraced_caller() {
    contract(ParentMode::Untraced, true, true, true).await;
}
#[tokio::test]
async fn plain_stop_keeps_existing_recorded_identity() {
    contract(ParentMode::Recorded, false, true, false).await;
}
#[tokio::test]
async fn plain_stop_keeps_existing_explicit_none() {
    contract(ParentMode::Untraced, false, true, false).await;
}
