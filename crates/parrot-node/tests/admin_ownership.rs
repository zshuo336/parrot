//! Real actors, explicit rendezvous, and externally observable management state.
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::host::ThreadPropsFactory;
use parrot_node::NodeComponentExecutor;
use parrot_remote::admin_v2::{
    AdminArtifactRef, AdminInstancePolicy, AdminReplyV2 as R, ComponentDeploy, ComponentExecutor,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Probe {
    factory_context: Mutex<Option<parrot_api::trace::TraceContext>>,
    stop_contexts: Mutex<Vec<(&'static str, Option<parrot_api::trace::TraceContext>)>>,
    init_started: Notify,
    init_release: Notify,
    initialized: AtomicUsize,
    dropped: AtomicUsize,
    second_started: Notify,
    factory_release: Notify,
    handler_started: Notify,
    handler_release: Notify,
    stop_started: Notify,
    stop_release: Notify,
    stopped: AtomicUsize,
    block_init: bool,
    block_factory: bool,
    fail_factory: bool,
    block_stop: bool,
    fail_stop: bool,
    reject_stop: Arc<AtomicBool>,
    block_drop: bool,
    fail_drop: bool,
    drop_entered: AtomicBool,
    drop_exited: AtomicBool,
    drop_expired: AtomicBool,
    drop_release: (Mutex<bool>, Condvar),
}

#[derive(Debug)]
struct RetryStopRef {
    actor: BoxedActorRef,
    reject: Arc<AtomicBool>,
}
impl ActorRef for RetryStopRef {
    fn retirement_status(&self) -> parrot_api::address::RetirementStatus {
        self.actor.retirement_status()
    }
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
        Box::pin(async move {
            if self.reject.load(Ordering::SeqCst) {
                return Err(ActorError::InternalError(
                    "temporary cleanup transport rejection".into(),
                ));
            }
            self.actor.stop().await
        })
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
            reject: self.reject.clone(),
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn probes() -> &'static Mutex<HashMap<String, Arc<Probe>>> {
    static PROBES: OnceLock<Mutex<HashMap<String, Arc<Probe>>>> = OnceLock::new();
    PROBES.get_or_init(Default::default)
}
struct Block;
struct Observed {
    probe: Arc<Probe>,
    total: u64,
}
struct StopGuard(Arc<Probe>);
impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0
            .stop_contexts
            .lock()
            .unwrap()
            .push(("drop", parrot_api::trace::TraceContext::current()));
    }
}
impl Actor for Observed {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.probe.init_started.notify_one();
            if self.probe.block_init {
                self.probe.init_release.notified().await;
            }
            self.probe.initialized.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Block>() {
                self.probe.handler_started.notify_one();
                self.probe.handler_release.notified().await;
            }
            if let Some(delta) = message.downcast_ref::<u64>() {
                self.total += delta;
            }
            Ok(Box::new(self.total) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.probe
            .stop_contexts
            .lock()
            .unwrap()
            .push(("construct", parrot_api::trace::TraceContext::current()));
        let guard = StopGuard(self.probe.clone());
        Box::pin(async move {
            let _guard = guard;
            self.probe
                .stop_contexts
                .lock()
                .unwrap()
                .push(("poll", parrot_api::trace::TraceContext::current()));
            self.probe.stop_started.notify_one();
            if self.probe.block_stop {
                self.probe.stop_release.notified().await;
            }
            self.probe.stopped.fetch_add(1, Ordering::SeqCst);
            if self.probe.fail_stop {
                Err(ActorError::InternalError("injected stop failure".into()))
            } else {
                Ok(())
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
impl Drop for Observed {
    fn drop(&mut self) {
        if self.probe.block_drop {
            self.probe.drop_entered.store(true, Ordering::SeqCst);
            let (released, changed) = &self.probe.drop_release;
            let (guard, timeout) = changed
                .wait_timeout_while(released.lock().unwrap(), Duration::from_secs(15), |ready| {
                    !*ready
                })
                .unwrap();
            drop(guard);
            self.probe
                .drop_expired
                .store(timeout.timed_out(), Ordering::SeqCst);
            self.probe.drop_exited.store(true, Ordering::SeqCst);
            if self.probe.fail_drop {
                panic!("late Node actor destructor failure");
            }
        }
        self.probe.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let key = config.as_ref().unwrap()["probe"].as_str().unwrap();
        let probe = probes().lock().unwrap().get(key).unwrap().clone();
        *probe.factory_context.lock().unwrap() = parrot_api::trace::TraceContext::current();
        if path.ends_with("-1") {
            if probe.block_factory {
                probe.second_started.notify_one();
                probe.factory_release.notified().await;
            }
            if probe.fail_factory {
                return Err(ActorError::InitializationError(
                    "second factory rejected".into(),
                ));
            }
        }
        let reject = probe.reject_stop.clone();
        let direct = probe.block_drop;
        engine
            .spawn_at(
                Observed { probe, total: 0 },
                &path,
                None,
                Default::default(),
            )
            .await
            .map(|actor| {
                if direct {
                    return Box::new(actor) as BoxedActorRef;
                }
                Box::new(RetryStopRef {
                    actor: Box::new(actor),
                    reject,
                }) as BoxedActorRef
            })
            .map_err(|e| ActorError::InitializationError(e.to_string()))
    })
}

#[tokio::test]
async fn failed_contextual_pool_remains_owned_and_can_retry_failed_cleanup() {
    let setup = Setup::new(
        Probe {
            fail_factory: true,
            ..Default::default()
        },
        3,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let reply = setup.executor.deploy(1, &setup.command).await;
    assert!(matches!(reply, R::Failed { .. }));
    let live = setup
        .engine
        .get_actor_ref(&format!("{}-0", setup.path))
        .unwrap();
    assert_eq!(
        *live
            .send(Box::new(7u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    let status = setup.executor.status(2, &setup.path).await;
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    let stopped = setup.executor.stop(3, &setup.path).await;
    let leaked = setup
        .engine
        .get_actor_ref(&format!("{}-0", setup.path))
        .is_some();
    // Also clean the baseline's orphan before asserting the management contract.
    setup.shutdown().await;
    assert!(
        matches!(status, R::Status { states, .. } if states.len() == 1 && states[0].state == "cleanup_failed"),
        "failed cleanup lost its management record"
    );
    assert!(matches!(stopped, R::Stopped { .. }), "{stopped:?}");
    assert!(!leaked, "successful retry left the original actor alive");
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "contract.node.contextual", spawn } }
struct Setup {
    engine: Arc<ThreadActorSystem>,
    executor: NodeComponentExecutor,
    command: ComponentDeploy,
    probe: Arc<Probe>,
    path: String,
}
impl Setup {
    fn new(probe: Probe, count: usize) -> Self {
        Self::with_timeout(probe, count, None)
    }
    fn with_timeout(probe: Probe, count: usize, timeout: Option<Duration>) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("admin-owned-{}", NEXT.fetch_add(1, Ordering::SeqCst));
        let probe = Arc::new(probe);
        probes().lock().unwrap().insert(name.clone(), probe.clone());
        let engine = ThreadActorSystem::shared(parrot::thread::config::ThreadActorSystemConfig {
            shutdown_timeout: timeout.unwrap_or_else(|| {
                parrot::thread::config::ThreadActorSystemConfig::default().shutdown_timeout
            }),
            ..Default::default()
        });
        let executor = NodeComponentExecutor::new(engine.clone());
        Self {
            engine,
            executor,
            command: ComponentDeploy {
                name: name.clone(),
                version: "1.2.3".into(),
                artifact: AdminArtifactRef::Props {
                    factory: "contract.node.contextual".into(),
                },
                instances: if count == 1 {
                    AdminInstancePolicy::Singleton
                } else {
                    AdminInstancePolicy::Pool { count }
                },
                config: Some(format!("probe = '{name}'").into_bytes()),
            },
            probe,
            path: format!("/user/{name}"),
        }
    }
    async fn deploy(&self) -> Vec<String> {
        match self.executor.deploy(1, &self.command).await {
            R::Deployed { instances, .. } => {
                for path in &instances {
                    self.ready(path).await;
                }
                instances
            }
            other => panic!("{other:?}"),
        }
    }
    async fn ready(&self, path: &str) {
        let actor = self.engine.get_actor_ref(path).unwrap();
        let reply = actor.send(Box::new(0u64)).await.unwrap();
        assert!(
            reply.is::<u64>(),
            "readiness must dispatch a real actor message"
        );
    }
    async fn shutdown(&self) {
        self.engine.shutdown_internal().await.unwrap();
    }
}
impl Drop for Setup {
    fn drop(&mut self) {
        self.release_drop();
        probes().lock().unwrap().remove(&self.command.name);
    }
}
impl Setup {
    fn release_drop(&self) {
        *self.probe.drop_release.0.lock().unwrap() = true;
        self.probe.drop_release.1.notify_all();
    }
}
async fn wait(signal: &Notify) {
    tokio::time::timeout(Duration::from_secs(5), signal.notified())
        .await
        .unwrap();
}

async fn late_retirement_contract(rollback: bool, fail_drop: bool) {
    let setup = Setup::with_timeout(
        Probe {
            block_drop: true,
            fail_drop,
            fail_factory: rollback,
            ..Default::default()
        },
        if rollback { 2 } else { 1 },
        Some(Duration::from_millis(100)),
    );
    if !rollback {
        setup.deploy().await;
    }
    let executor = setup.executor.clone();
    let command = setup.command.clone();
    let prefix = setup.path.clone();
    let job = tokio::spawn(async move {
        if rollback {
            executor.deploy(2, &command).await
        } else {
            executor.stop(2, &prefix).await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !setup.probe.drop_entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    let path = if rollback {
        format!("{}-0", setup.path)
    } else {
        setup.path.clone()
    };
    let original = setup.engine.get_actor_ref(&path).unwrap();
    let first = job.await.unwrap();
    assert!(matches!(first, R::Failed { .. }), "{first:?}");
    let pending = setup.executor.stop(3, &setup.path).await;
    assert!(
        matches!(pending, R::Failed { .. }),
        "pending retirement cannot be confirmed: {pending:?}"
    );
    assert!(setup.engine.get_actor_ref(&path).is_some());
    setup.release_drop();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            original.retirement_status(),
            parrot_api::address::RetirementStatus::Retired(_)
        ) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(setup.probe.drop_exited.load(Ordering::SeqCst));
    assert!(!setup.probe.drop_expired.load(Ordering::SeqCst));
    let replacement = setup
        .engine
        .spawn_at(
            Observed {
                probe: Arc::new(Probe::default()),
                total: 0,
            },
            &path,
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let retry = setup.executor.stop(4, &setup.path).await;
    let status = setup.executor.status(5, &setup.path).await;
    let reply = replacement.send(Box::new(41u64)).await.unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 41);
    replacement.stop().await.unwrap();
    assert!(matches!(
        original.stop().await,
        Err(ActorError::TimeoutDetail(_))
    ));
    setup.shutdown().await;
    if fail_drop {
        assert!(matches!(retry, R::Failed { .. }), "{retry:?}");
        assert!(
            format!("{retry:?}").contains("late Node actor destructor failure"),
            "{retry:?}"
        );
        assert!(
            matches!(status, R::Status { states, .. } if states.len() == 1 && states[0].state == "cleanup_failed")
        );
    } else {
        assert!(
            matches!(retry, R::Stopped { .. }),
            "actual successful retirement must clear the historical timeout: {retry:?}"
        );
        assert!(
            matches!(status, R::Failed { code, .. } if code == parrot_remote::admin_v2::v2_err::COMPONENT_NOT_FOUND),
            "removed records must return component-not-found: {status:?}"
        );
    }
}

#[tokio::test]
async fn late_retirement_success_clears_deployed_node_record_without_stopping_replacement() {
    late_retirement_contract(false, false).await;
}
#[tokio::test]
async fn late_retirement_failure_survives_deployed_node_stop_timeout() {
    late_retirement_contract(false, true).await;
}
#[tokio::test]
async fn late_retirement_success_clears_failed_node_pool_without_stopping_replacement() {
    late_retirement_contract(true, false).await;
}
#[tokio::test]
async fn late_retirement_failure_survives_failed_node_pool_stop_timeout() {
    late_retirement_contract(true, true).await;
}

fn lifecycle_record<'a>(
    snapshot: &'a parrot_api::trace::RecordingSnapshot,
    kind: parrot_api::trace::SpanKind,
    path: &str,
    parent: &parrot_api::trace::SpanRecord,
) -> &'a parrot_api::trace::SpanRecord {
    let hooks: Vec<_> = snapshot
        .records
        .iter()
        .filter(|record| record.kind == kind && record.target == path)
        .collect();
    assert_eq!(hooks.len(), 1, "one actual {kind:?} hook");
    let hook = hooks[0];
    assert_eq!(hook.trace_id, parent.trace_id);
    assert_eq!(hook.parent_span_id, Some(parent.span_id));
    assert_ne!(hook.span_id, parent.span_id);
    assert_eq!(hook.status, parrot_api::trace::SpanStatus::Ok);
    hook
}

#[tokio::test]
async fn deploy_factory_inherits_management_operation_and_next_stop_is_isolated() {
    use parrot_api::trace::{SpanKind, SpanStatus, TraceRecorder};
    let setup = Setup::new(Probe::default(), 1);
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    assert!(matches!(
        root.scope(setup.executor.deploy(1, &setup.command)).await,
        R::Deployed { .. }
    ));
    setup.ready(&setup.path).await;
    let context = setup.probe.factory_context.lock().unwrap().clone();
    let reply = setup.executor.stop(2, &setup.path).await;
    setup.shutdown().await;
    assert!(matches!(reply, R::Stopped { .. }));
    let context = context.expect("contextual factory lost deployment context");
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.in_flight, 0);
    assert_eq!(snapshot.records.len(), 2);
    let record = snapshot
        .records
        .iter()
        .find(|record| record.kind == SpanKind::ManagedTask)
        .unwrap();
    assert_eq!(record.kind, SpanKind::ManagedTask);
    assert_eq!(record.target, "node/deploy");
    assert_eq!(record.status, SpanStatus::Ok);
    assert_eq!(record.parent_span_id, Some(root.span_id()));
    assert_eq!(context.span_id(), record.span_id);
    assert_eq!(context.trace_id(), root.trace_id());
    lifecycle_record(&snapshot, SpanKind::ActorInit, &setup.path, record);
    let stops = setup.probe.stop_contexts.lock().unwrap();
    assert_eq!(
        stops.iter().map(|event| event.0).collect::<Vec<_>>(),
        ["construct", "poll", "drop"]
    );
    assert!(
        stops.iter().all(|event| event.1.is_none()),
        "untraced stop reused deployment identity: {stops:?}"
    );
}

#[tokio::test]
async fn tcp_management_roots_are_explicit_local_and_reach_real_factory() {
    use parrot_api::trace::{SpanKind, SpanStatus, TraceRecorder};
    use parrot_remote::{
        IncomingTracePolicy, LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
    };
    struct Empty;
    #[async_trait::async_trait]
    impl LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
            None
        }
    }
    use IncomingTracePolicy::{PropagateOnly, StartRoots};
    for (policy, traced, stop) in [
        (PropagateOnly, false, false),
        (StartRoots, false, false),
        (PropagateOnly, true, false),
        (StartRoots, true, false),
        (PropagateOnly, false, true),
        (StartRoots, false, true),
        (PropagateOnly, true, true),
        (StartRoots, true, true),
    ] {
        let setup = Setup::new(Probe::default(), 1);
        let target = RemoteActorSystem::new(
            RemoteConfig::tcp("managed-receiver", Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            Arc::new(Empty),
        )
        .unwrap();
        target.set_component_executor(Some(Arc::new(setup.executor.clone())));
        let source = RemoteActorSystem::new(
            RemoteConfig::tcp("managed-caller", None).with_insecure_development(),
            Arc::new(Empty),
        )
        .unwrap();
        let recorder = TraceRecorder::new(16).unwrap();
        let _attachment = target.attach_trace_recorder_with_policy(recorder.clone(), policy);
        target.start().await.unwrap();
        source.start().await.unwrap();
        source
            .connect(&NodeAddr::tcp(
                "managed-receiver",
                target.local_addr().unwrap(),
            ))
            .await
            .unwrap();
        let caller = TraceRecorder::new(8).unwrap().root();
        let context = traced.then(|| caller.clone());
        let deployed = parrot_api::trace::scope(
            context.clone(),
            source.deploy_component("managed-receiver", setup.command.clone()),
        )
        .await;
        setup.ready(&setup.path).await;
        let factory = setup.probe.factory_context.lock().unwrap().clone();
        let status = parrot_api::trace::scope(
            context.clone(),
            source.component_status("managed-receiver", &setup.path),
        )
        .await
        .unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].state, "running");
        let metrics =
            parrot_api::trace::scope(context.clone(), source.metrics_report("managed-receiver"))
                .await
                .unwrap();
        assert_eq!(metrics.components, 1);
        assert_eq!(metrics.component_states, status);
        let measured =
            parrot_api::trace::scope(context.clone(), source.measured_metrics("managed-receiver"))
                .await
                .unwrap();
        assert_eq!(measured.snapshot.component_states, status);
        let drained = parrot_api::trace::scope(context, async {
            if stop {
                source
                    .stop_component("managed-receiver", &setup.path)
                    .await
                    .map(|()| (1, 0))
            } else {
                source
                    .drain_component("managed-receiver", &setup.path, Duration::from_secs(5))
                    .await
            }
        })
        .await;
        source.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
        setup.shutdown().await;
        assert_eq!(deployed.unwrap(), vec![setup.path.clone()]);
        assert_eq!(drained.unwrap(), (1, 0));
        assert_eq!(setup.probe.stopped.load(Ordering::SeqCst), 1);
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.in_flight, 0);
        if !traced && policy == IncomingTracePolicy::PropagateOnly {
            assert!(factory.is_none());
            assert_eq!(snapshot.counts.started, 0);
            assert!(setup
                .probe
                .stop_contexts
                .lock()
                .unwrap()
                .iter()
                .all(|event| event.1.is_none()));
        } else {
            let records: Vec<_> = snapshot
                .records
                .iter()
                .filter(|record| record.kind == SpanKind::ManagedTask)
                .collect();
            assert_eq!(records.len(), 2);
            let queries: Vec<_> = snapshot
                .records
                .iter()
                .filter(|record| record.kind == SpanKind::Application)
                .collect();
            assert_eq!(queries.len(), 3);
            for target in ["node/status", "node/metrics", "node/measured_metrics"] {
                assert_eq!(
                    queries
                        .iter()
                        .filter(|record| record.target == target)
                        .count(),
                    1
                );
            }
            let trace_ids: std::collections::BTreeSet<_> = snapshot
                .records
                .iter()
                .map(|record| record.trace_id)
                .collect();
            assert_eq!(
                trace_ids.len(),
                if traced { 1 } else { 5 },
                "preserve the incoming parent; only untraced requests start roots"
            );
            for record in queries {
                assert_eq!(record.status, SpanStatus::Ok);
                assert!(record.parent_span_id.is_some());
                if traced {
                    assert_eq!(record.trace_id, caller.trace_id());
                    assert_eq!(record.parent_span_id, Some(caller.span_id()));
                } else {
                    assert_ne!(record.trace_id, caller.trace_id());
                }
            }
            let deploy = records
                .iter()
                .find(|record| record.target == "node/deploy")
                .unwrap();
            let drain = records
                .iter()
                .find(|record| record.target == if stop { "node/stop" } else { "node/drain" })
                .unwrap();
            assert_eq!(snapshot.records.len(), if stop { 7 } else { 8 });
            lifecycle_record(&snapshot, SpanKind::ActorInit, &setup.path, deploy);
            let stop_hook =
                lifecycle_record(&snapshot, SpanKind::ActorBeforeStop, &setup.path, drain);
            let handlers: Vec<_> = snapshot
                .records
                .iter()
                .filter(|record| record.kind == SpanKind::DynamicHandler)
                .collect();
            assert_eq!(handlers.len(), usize::from(!stop));
            for handler in handlers {
                assert_eq!(handler.target, setup.path);
                assert_eq!(handler.trace_id, drain.trace_id);
                assert_eq!(handler.parent_span_id, Some(drain.span_id));
                assert_eq!(handler.status, SpanStatus::Ok);
            }
            let factory = factory.unwrap();
            assert_eq!(factory.span_id(), deploy.span_id);
            assert_eq!(factory.trace_id(), deploy.trace_id);
            assert_eq!(deploy.trace_id == drain.trace_id, traced);
            let stops = setup.probe.stop_contexts.lock().unwrap();
            assert_eq!(
                stops.iter().map(|event| event.0).collect::<Vec<_>>(),
                ["construct", "poll", "drop"]
            );
            for (_, context) in stops.iter() {
                let context = context
                    .as_ref()
                    .expect("Node drain lost identity before real hook");
                assert_eq!(context.trace_id(), drain.trace_id);
                assert_eq!(context.span_id(), stop_hook.span_id);
                assert_eq!(context.parent_span_id(), Some(drain.span_id));
            }
            for record in records {
                if traced {
                    assert_eq!(record.trace_id, caller.trace_id());
                    assert_eq!(record.parent_span_id, Some(caller.span_id()));
                } else {
                    assert_ne!(record.trace_id, caller.trace_id());
                }
                assert!(record.parent_span_id.is_some());
                assert_eq!(record.status, SpanStatus::Ok);
            }
        }
    }
}

#[tokio::test]
async fn management_parent_crosses_real_process_boundary() {
    use parrot_api::trace::{SpanKind, SpanStatus, TraceContext, TraceRecorder};
    use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
    use std::{
        fs,
        path::PathBuf,
        process::{Child, Command, Stdio},
    };

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Ready {
        addr: String,
        path: String,
        command: ComponentDeploy,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Expected {
        trace_id: [u8; 16],
        span_id: [u8; 8],
    }
    struct Empty;
    #[async_trait::async_trait]
    impl LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
            None
        }
    }
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    if let Some(directory) = std::env::var_os("PARROT_ADMIN_TRACE_CHILD") {
        let directory = PathBuf::from(directory);
        let setup = Setup::new(Probe::default(), 1);
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp("process-receiver", Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            Arc::new(Empty),
        )
        .unwrap();
        remote.set_component_executor(Some(Arc::new(setup.executor.clone())));
        let recorder = TraceRecorder::new(16).unwrap();
        remote.set_trace_recorder(Some(recorder.clone()));
        remote.start().await.unwrap();
        let ready = Ready {
            addr: remote.local_addr().unwrap().to_string(),
            path: setup.path.clone(),
            command: setup.command.clone(),
        };
        fs::write(
            directory.join("ready.tmp"),
            toml::to_string(&ready).unwrap(),
        )
        .unwrap();
        fs::rename(directory.join("ready.tmp"), directory.join("ready.toml")).unwrap();
        // Expected IDs are published only after all management RPCs have returned.
        let text = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match fs::read_to_string(directory.join("expected.toml")) {
                    Ok(text) => break text,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("expected identity read failed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        let expected: Expected = toml::from_str(&text).unwrap();
        remote.shutdown().await.unwrap();
        setup.shutdown().await;
        assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(setup.probe.stopped.load(Ordering::SeqCst), 1);
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.records.len(), 8, "records: {:?}", snapshot.records);
        for (target, kind) in [
            ("node/deploy", SpanKind::ManagedTask),
            ("node/status", SpanKind::Application),
            ("node/metrics", SpanKind::Application),
            ("node/measured_metrics", SpanKind::Application),
            ("node/drain", SpanKind::ManagedTask),
        ] {
            let records: Vec<_> = snapshot
                .records
                .iter()
                .filter(|record| record.target == target)
                .collect();
            assert_eq!(records.len(), 1);
            let record = records[0];
            assert_eq!(record.kind, kind);
            assert_eq!(record.status, SpanStatus::Ok);
            assert_eq!(record.trace_id, expected.trace_id);
            assert_eq!(record.parent_span_id, Some(expected.span_id));
        }
        let deploy = snapshot
            .records
            .iter()
            .find(|record| record.target == "node/deploy")
            .unwrap();
        let factory = setup.probe.factory_context.lock().unwrap().clone().unwrap();
        assert_eq!(factory.span_id(), deploy.span_id);
        assert_eq!(factory.trace_id(), deploy.trace_id);
        let drain = snapshot
            .records
            .iter()
            .find(|record| record.target == "node/drain")
            .unwrap();
        lifecycle_record(&snapshot, SpanKind::ActorInit, &setup.path, deploy);
        let stop_hook = lifecycle_record(&snapshot, SpanKind::ActorBeforeStop, &setup.path, drain);
        // Drain's actual health-check handler is a child of the Node operation.
        let handlers: Vec<_> = snapshot
            .records
            .iter()
            .filter(|record| record.kind == SpanKind::DynamicHandler)
            .collect();
        assert_eq!(handlers.len(), 1);
        assert_eq!(handlers[0].target, setup.path);
        assert_eq!(handlers[0].trace_id, drain.trace_id);
        assert_eq!(handlers[0].parent_span_id, Some(drain.span_id));
        assert_eq!(handlers[0].status, SpanStatus::Ok);
        let stops = setup.probe.stop_contexts.lock().unwrap();
        assert_eq!(
            stops.iter().map(|event| event.0).collect::<Vec<_>>(),
            ["construct", "poll", "drop"]
        );
        for (_, context) in stops.iter() {
            let context = context.as_ref().unwrap();
            assert_eq!(context.trace_id(), drain.trace_id);
            assert_eq!(context.span_id(), stop_hook.span_id);
            assert_eq!(context.parent_span_id(), Some(drain.span_id));
        }
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let log_path = directory.path().join("child.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "management_parent_crosses_real_process_boundary",
                "--nocapture",
            ])
            .env("PARROT_ADMIN_TRACE_CHILD", directory.path())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let ready: Ready = loop {
        match fs::read_to_string(directory.path().join("ready.toml")) {
            Ok(text) => break toml::from_str(&text).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("receiver readiness read failed: {error}"),
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "receiver exited before readiness: {}",
            fs::read_to_string(&log_path).unwrap()
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "receiver readiness watchdog expired: {}",
            fs::read_to_string(&log_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp("process-caller", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    remote.start().await.unwrap();
    remote
        .connect(&NodeAddr::tcp(
            "process-receiver",
            ready.addr.parse().unwrap(),
        ))
        .await
        .unwrap();
    let root = TraceContext::root();
    let paths = root
        .scope(remote.deploy_component("process-receiver", ready.command))
        .await
        .unwrap();
    assert_eq!(paths.as_slice(), std::slice::from_ref(&ready.path));
    let states = root
        .scope(remote.component_status("process-receiver", &ready.path))
        .await
        .unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].state, "running");
    let metrics = root
        .scope(remote.metrics_report("process-receiver"))
        .await
        .unwrap();
    assert_eq!(metrics.components, 1);
    assert_eq!(metrics.component_states, states);
    let measured = root
        .scope(remote.measured_metrics("process-receiver"))
        .await
        .unwrap();
    assert_eq!(measured.snapshot.component_states, states);
    assert_eq!(
        root.scope(remote.drain_component("process-receiver", &ready.path, Duration::from_secs(5)))
            .await
            .unwrap(),
        (1, 0)
    );
    remote.shutdown().await.unwrap();
    let expected = Expected {
        trace_id: root.trace_id(),
        span_id: root.span_id(),
    };
    fs::write(
        directory.path().join("expected.tmp"),
        toml::to_string(&expected).unwrap(),
    )
    .unwrap();
    fs::rename(
        directory.path().join("expected.tmp"),
        directory.path().join("expected.toml"),
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "receiver assertions failed: {status}\n{}",
                fs::read_to_string(&log_path).unwrap()
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "receiver retirement watchdog expired: {}",
            fs::read_to_string(&log_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn cancelled_deploy_is_owned_until_registered_and_manageable() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            ..Default::default()
        },
        3,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { reply = &mut deploy => panic!("unexpected early reply {reply:?}"), _ = wait(&setup.probe.second_started) => {} }
    assert!(setup
        .engine
        .get_actor_ref(&format!("{}-0", setup.path))
        .is_some());
    drop(deploy);
    setup.probe.factory_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop { if matches!(setup.executor.status(2, &setup.path).await, R::Status { states, .. } if states.len()==3 && states.iter().all(|state| state.state == "running")) { break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    for index in 0..3 {
        setup.ready(&format!("{}-{index}", setup.path)).await;
    }
    assert!(matches!(
        setup.executor.stop(3, &setup.path).await,
        R::Stopped { .. }
    ));
    assert_eq!(setup.probe.stopped.load(Ordering::SeqCst), 3);
    setup.shutdown().await;
}

#[tokio::test]
async fn cancelled_failed_deploy_rolls_back_partial_pool() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            fail_factory: true,
            ..Default::default()
        },
        3,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { reply = &mut deploy => panic!("unexpected early reply {reply:?}"), _ = wait(&setup.probe.second_started) => {} }
    setup.ready(&format!("{}-0", setup.path)).await;
    drop(deploy);
    setup.probe.factory_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while setup.probe.stopped.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
        while setup
            .engine
            .get_actor_ref(&format!("{}-0", setup.path))
            .is_some()
        {
            tokio::task::yield_now().await;
        }
        // Registry retirement precedes the management owner's error handling.
        // Wait for both public observations rather than relying on scheduling.
        while !matches!(
            setup.executor.status(2, &setup.path).await,
            R::Failed { .. }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        setup.executor.status(2, &setup.path).await,
        R::Failed { .. }
    ));
    assert!(setup
        .engine
        .get_actor_ref(&format!("{}-2", setup.path))
        .is_none());
    setup.shutdown().await;
}

#[tokio::test]
async fn cancelled_stop_finishes_and_updates_management_record() {
    let setup = Setup::new(
        Probe {
            block_stop: true,
            ..Default::default()
        },
        1,
    );
    setup.deploy().await;
    let mut stop = Box::pin(setup.executor.stop(2, &setup.path));
    tokio::select! { reply = &mut stop => panic!("unexpected early reply {reply:?}"), _ = wait(&setup.probe.stop_started) => {} }
    drop(stop);
    setup.probe.stop_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            setup.executor.status(3, &setup.path).await,
            R::Failed { .. }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(setup.engine.get_actor_ref(&setup.path).is_none());
    setup.shutdown().await;
}

#[tokio::test]
async fn stopping_during_initialization_retires_storage_without_a_normal_stop_hook() {
    let setup = Setup::new(
        Probe {
            block_init: true,
            ..Default::default()
        },
        1,
    );
    assert!(matches!(
        setup.executor.deploy(1, &setup.command).await,
        R::Deployed { .. }
    ));
    wait(&setup.probe.init_started).await;
    assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 0);
    let stopped =
        tokio::time::timeout(Duration::from_secs(5), setup.executor.stop(2, &setup.path)).await;
    let dropped_before_shutdown = setup.probe.dropped.load(Ordering::SeqCst);
    let registered_before_shutdown = setup.engine.actor_count();
    let status_before_shutdown = setup.executor.status(3, &setup.path).await;
    setup.probe.init_release.notify_one();
    setup.shutdown().await;
    assert!(matches!(stopped, Ok(R::Stopped { .. })), "{stopped:?}");
    assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(setup.probe.stopped.load(Ordering::SeqCst), 0);
    assert_eq!(setup.probe.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        dropped_before_shutdown, 1,
        "Stop must retire storage before returning"
    );
    assert_eq!(registered_before_shutdown, 0);
    assert!(matches!(status_before_shutdown, R::Failed { .. }));
    assert!(setup.engine.get_actor_ref(&setup.path).is_none());
    assert!(matches!(
        setup.executor.status(3, &setup.path).await,
        R::Failed { .. }
    ));
}

#[tokio::test]
async fn cancelled_partial_deploy_retires_an_initializing_member_without_a_stop_hook() {
    let setup = Setup::new(
        Probe {
            block_init: true,
            block_factory: true,
            fail_factory: true,
            ..Default::default()
        },
        3,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { reply = &mut deploy => panic!("unexpected early reply {reply:?}"), _ = wait(&setup.probe.second_started) => {} }
    wait(&setup.probe.init_started).await;
    assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 0);
    drop(deploy);
    setup.probe.factory_release.notify_one();
    let retired = tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            setup.executor.status(2, &setup.path).await,
            R::Failed { .. }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let dropped_before_shutdown = setup.probe.dropped.load(Ordering::SeqCst);
    let registered_before_shutdown = setup.engine.actor_count();
    setup.probe.init_release.notify_one();
    setup.shutdown().await;
    assert!(
        retired.is_ok(),
        "partial deployment retained an initializing member"
    );
    assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(setup.probe.stopped.load(Ordering::SeqCst), 0);
    assert_eq!(setup.probe.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        dropped_before_shutdown, 1,
        "rollback must retire storage before clearing ownership"
    );
    assert_eq!(registered_before_shutdown, 0);
    assert_eq!(setup.engine.actor_count(), 0);
    assert!(matches!(
        setup.executor.status(3, &setup.path).await,
        R::Failed { .. }
    ));
}

#[tokio::test]
async fn stop_failure_is_reported_and_remains_inspectable() {
    let setup = Setup::new(
        Probe {
            fail_stop: true,
            ..Default::default()
        },
        1,
    );
    setup.deploy().await;
    let stopped = setup.executor.stop(2, &setup.path).await;
    assert!(
        matches!(stopped, R::Failed { ref detail, .. } if detail.contains("injected stop failure")),
        "{stopped:?}"
    );
    assert!(
        matches!(setup.executor.status(3, &setup.path).await, R::Status { states, .. } if states[0].state == "cleanup_failed")
    );
    assert!(setup.engine.get_actor_ref(&setup.path).is_none());
    // Physical registry removal is not proof that the stop hook succeeded.
    // A completed native stop keeps its first receipt. Retrying a transport
    // rejection before native stop starts is a separate, recoverable case
    // covered by failed_contextual_pool_remains_owned_and_can_retry_failed_cleanup.
    let first_detail = match stopped {
        R::Failed { detail, .. } => detail,
        _ => unreachable!("checked first stop failure"),
    };
    for request_id in [4, 5] {
        let retry = setup.executor.stop(request_id, &setup.path).await;
        assert!(
            matches!(retry, R::Failed { ref detail, .. } if detail == &first_detail),
            "completed cleanup failure changed on retry: {retry:?}"
        );
        assert!(
            matches!(setup.executor.status(request_id + 10, &setup.path).await,
                R::Status { states, .. } if states.len() == 1 && states[0].state == "cleanup_failed")
        );
        assert!(setup.engine.get_actor_ref(&setup.path).is_none());
        assert_eq!(
            setup.probe.stopped.load(Ordering::SeqCst),
            1,
            "retry must not run an already completed stop hook again"
        );
    }
    setup.shutdown().await;
}

#[tokio::test]
async fn drain_budget_includes_the_current_handler_even_when_queue_is_empty() {
    let setup = Setup::new(Probe::default(), 1);
    setup.deploy().await;
    let actor = setup.engine.get_actor_ref(&setup.path).unwrap();
    let task = tokio::spawn(async move { actor.send(Box::new(Block)).await });
    wait(&setup.probe.handler_started).await;
    assert!(
        setup
            .engine
            .get_mailbox(&setup.path)
            .unwrap()
            .is_empty()
            .await
    );
    let reply = tokio::time::timeout(
        Duration::from_millis(500),
        setup.executor.drain(2, &setup.path, 40),
    )
    .await;
    // Release regardless of the assertion outcome to avoid a blocked test actor.
    setup.probe.handler_release.notify_one();
    task.await.unwrap().unwrap();
    assert!(
        matches!(
            reply,
            Ok(R::Drained {
                drained: 0,
                aborted: 1,
                ..
            })
        ),
        "{reply:?}"
    );
    assert!(
        setup
            .engine
            .get_actor_ref(&setup.path)
            .unwrap()
            .is_alive()
            .await
    );
    assert!(matches!(
        setup.executor.drain(3, &setup.path, 1000).await,
        R::Drained {
            drained: 1,
            aborted: 0,
            ..
        }
    ));
    setup.shutdown().await;
}

#[tokio::test]
async fn tcp_drain_duration_boundaries_wait_for_the_current_handler() {
    use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
    struct Empty;
    #[async_trait::async_trait]
    impl LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
            None
        }
    }
    let mut outcomes = Vec::new();
    for duration in [
        Duration::ZERO,
        Duration::from_secs(5),
        Duration::from_millis(u64::MAX),
        Duration::new(18_446_744_073_709_551, 616_000_000),
        Duration::MAX,
    ] {
        let setup = Setup::new(Probe::default(), 1);
        setup.deploy().await;
        let target = RemoteActorSystem::new(
            RemoteConfig::tcp("drain-target", Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            Arc::new(Empty),
        )
        .unwrap();
        target.set_component_executor(Some(Arc::new(setup.executor.clone())));
        let source = RemoteActorSystem::new(
            RemoteConfig::tcp("drain-source", None).with_insecure_development(),
            Arc::new(Empty),
        )
        .unwrap();
        target.start().await.unwrap();
        source.start().await.unwrap();
        source
            .connect(&NodeAddr::tcp("drain-target", target.local_addr().unwrap()))
            .await
            .unwrap();
        let actor = setup.engine.get_actor_ref(&setup.path).unwrap();
        let handler = tokio::spawn(async move { actor.send(Box::new(Block)).await });
        wait(&setup.probe.handler_started).await;
        let caller = source.clone();
        let path = setup.path.clone();
        let drain = tokio::spawn(async move {
            caller
                .drain_component("drain-target", &path, duration)
                .await
        });
        let mailbox = setup.engine.get_mailbox(&setup.path).unwrap();
        // The drain health check queues behind Block. An expired budget instead
        // returns early; release the actor in both cases before asserting.
        let admitted = tokio::time::timeout(Duration::from_secs(5), async {
            while !drain.is_finished() && mailbox.is_empty().await {
                tokio::task::yield_now().await;
            }
        })
        .await;
        setup.probe.handler_release.notify_one();
        handler.await.unwrap().unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .unwrap()
            .unwrap();
        let retained = setup.engine.get_actor_ref(&setup.path).is_some();
        source.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
        setup.shutdown().await;
        assert!(
            admitted.is_ok(),
            "drain never reached the actor: {duration:?}"
        );
        outcomes.push((duration, reply, retained));
    }
    for (duration, reply, retained) in outcomes {
        let expected = if duration.is_zero() { (0, 1) } else { (1, 0) };
        assert_eq!(reply.unwrap(), expected, "drain result for {duration:?}");
        assert_eq!(
            retained,
            duration.is_zero(),
            "actor retained for {duration:?}"
        );
    }
}

#[tokio::test]
async fn old_deployment_cannot_report_or_stop_a_replacement_at_the_same_path() {
    let setup = Setup::new(Probe::default(), 1);
    setup.deploy().await;
    setup
        .engine
        .get_actor_ref(&setup.path)
        .unwrap()
        .stop()
        .await
        .unwrap();
    let replacement = setup
        .engine
        .spawn_at(
            Observed {
                probe: Arc::default(),
                total: 42,
            },
            &setup.path,
            None,
            Default::default(),
        )
        .await
        .unwrap();
    assert!(
        matches!(setup.executor.status(2, &setup.path).await, R::Status { states, .. } if states[0].state == "unavailable")
    );
    assert!(matches!(
        setup.executor.stop(3, &setup.path).await,
        R::Stopped { .. }
    ));
    assert!(replacement.is_alive().await);
    assert_eq!(
        *replacement
            .send(Box::new(0u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    replacement.stop().await.unwrap();
    setup.shutdown().await;
}

#[tokio::test]
async fn stopping_one_pool_member_keeps_other_versions_and_identical_retry_keeps_state() {
    let setup = Setup::new(Probe::default(), 3);
    let paths = setup.deploy().await;
    let actor = setup.engine.get_actor_ref(&paths[1]).unwrap();
    actor.send(Box::new(7u64)).await.unwrap();
    assert_eq!(setup.deploy().await, paths);
    assert_eq!(
        *actor
            .send(Box::new(0u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    assert!(matches!(
        setup.executor.stop(2, &paths[0]).await,
        R::Stopped { .. }
    ));
    assert!(
        matches!(setup.executor.status(3, &setup.path).await, R::Status { states, .. } if states.len()==2 && states.iter().all(|s|s.version=="1.2.3"))
    );
    assert!(
        matches!(
            setup.executor.deploy(4, &setup.command).await,
            R::Failed { .. }
        ),
        "partial pool must not pretend to satisfy full retry"
    );
    setup.executor.stop(5, &setup.path).await;
    setup.shutdown().await;
}

#[tokio::test]
async fn malformed_direct_requests_are_rejected_before_spawning() {
    let setup = Setup::new(Probe::default(), 1);
    for change in 0..5 {
        let mut command = setup.command.clone();
        match change {
            0 => command.instances = AdminInstancePolicy::Pool { count: usize::MAX },
            1 => command.name = "../escape".into(),
            2 => command.version = "not-semver".into(),
            3 => command.config = Some(vec![0xff]),
            _ => command.config = Some(vec![b'x'; parrot_node::executor_v2::MAX_CONFIG_BYTES + 1]),
        }
        assert!(matches!(
            setup.executor.deploy(1, &command).await,
            R::Failed { .. }
        ));
        assert_eq!(setup.engine.actor_count(), 0);
    }
    setup.shutdown().await;
}

#[tokio::test]
async fn invalid_semver_identifiers_are_rejected_before_actor_creation() {
    for version in [
        "1.0.0-alpha..1",
        "1.0.0-.alpha",
        "1.0.0-alpha.",
        "1.0.0-01",
        "1.0.0-alpha.00+build.01",
        "1.0.0+.",
        "1.0.0+.build",
        "1.0.0+build.",
        "1.0.0+build..1",
    ] {
        let mut setup = Setup::new(Probe::default(), 1);
        setup.command.version = version.into();
        let reply = setup.executor.deploy(1, &setup.command).await;
        let actors = setup.engine.actor_count();
        let initialized = setup.probe.initialized.load(Ordering::SeqCst);
        // Retire any unexpected successful deployment before reporting a failure.
        setup.executor.stop(2, &setup.path).await;
        setup.shutdown().await;
        assert!(matches!(reply, R::Failed { code, detail, .. }
            if code == parrot_remote::admin_v2::v2_err::SPAWN_FAILED
                && detail == "invalid component semantic version"));
        assert_eq!(actors, 0, "invalid version created an actor: {version}");
        assert_eq!(
            initialized, 0,
            "invalid version initialized an actor: {version}"
        );
    }
}

#[tokio::test]
async fn valid_semver_identifiers_keep_exact_versions_and_node_byte_limit() {
    let boundary = format!("1.0.0+{}", "x".repeat(250));
    assert_eq!(boundary.len(), 256);
    for version in [
        "1.0.0-alpha.0+build.01",
        "1.0.0-01a",
        "1.0.0--+00",
        &boundary,
    ] {
        let mut setup = Setup::new(Probe::default(), 1);
        setup.command.version = version.into();
        let paths = setup.deploy().await;
        let actor = setup.engine.get_actor_ref(&paths[0]).unwrap();
        assert_eq!(
            *actor
                .send(Box::new(7u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            7
        );
        assert!(matches!(setup.executor.status(2, &setup.path).await,
            R::Status { states, .. } if states.len() == 1 && states[0].version == version));
        assert!(matches!(
            setup.executor.stop(3, &setup.path).await,
            R::Stopped { .. }
        ));
        setup.shutdown().await;
    }
    let mut setup = Setup::new(Probe::default(), 1);
    setup.command.version = format!("{boundary}x");
    let reply = setup.executor.deploy(1, &setup.command).await;
    let actors = setup.engine.actor_count();
    setup.executor.stop(2, &setup.path).await;
    setup.shutdown().await;
    assert!(matches!(reply, R::Failed { code, detail, .. }
        if code == parrot_remote::admin_v2::v2_err::SPAWN_FAILED
            && detail == "invalid component semantic version"));
    assert_eq!(actors, 0);
}

#[tokio::test]
async fn blocked_factory_does_not_block_an_unrelated_components_stop() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            ..Default::default()
        },
        2,
    );
    let mut neighbor = setup.command.clone();
    neighbor.name.push_str("-neighbor");
    neighbor.instances = AdminInstancePolicy::Singleton;
    assert!(matches!(
        setup.executor.deploy(1, &neighbor).await,
        R::Deployed { .. }
    ));
    let mut pending = Box::pin(setup.executor.deploy(2, &setup.command));
    tokio::select! { result = &mut pending => panic!("unexpected {result:?}"), _ = wait(&setup.probe.second_started) => {} }
    let stopped = tokio::time::timeout(
        Duration::from_millis(500),
        setup.executor.stop(3, &format!("/user/{}", neighbor.name)),
    )
    .await;
    setup.probe.factory_release.notify_one();
    assert!(matches!(pending.await, R::Deployed { .. }));
    assert!(
        matches!(stopped, Ok(R::Stopped { .. })),
        "unrelated component was blocked: {stopped:?}"
    );
    setup.executor.stop(4, &setup.path).await;
    setup.shutdown().await;
}

#[cfg(not(feature = "wasm"))]
#[tokio::test]
async fn node_without_wasm_feature_explicitly_rejects_wasm_artifacts() {
    let setup = Setup::new(Probe::default(), 1);
    let mut command = setup.command.clone();
    command.artifact = AdminArtifactRef::Wasm {
        digest: "0".repeat(64),
        uri: "file:///not-loaded.wasm".into(),
    };
    assert!(
        matches!(setup.executor.deploy(1, &command).await, R::Failed { code, .. } if code == parrot_remote::admin_v2::v2_err::DIALECT_MISMATCH)
    );
    assert_eq!(setup.engine.actor_count(), 0);
    setup.shutdown().await;
}

#[tokio::test]
async fn pending_pool_reserves_member_selectors_until_deploy_finishes() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            ..Default::default()
        },
        2,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { result = &mut deploy => panic!("unexpected {result:?}"), _ = wait(&setup.probe.second_started) => {} }
    let member = format!("{}-1", setup.path);
    // A zero-budget drain must find the reservation and wait on the deployment,
    // rather than falsely reporting that this member does not exist.
    assert!(matches!(setup.executor.drain(2, &member, 0).await,
        R::Failed { code, .. } if code == parrot_remote::admin_v2::v2_err::DRAIN_TIMEOUT));
    setup.probe.factory_release.notify_one();
    assert!(matches!(deploy.await, R::Deployed { .. }));
    assert!(matches!(
        setup.executor.stop(3, &member).await,
        R::Stopped { .. }
    ));
    assert!(setup
        .engine
        .get_actor_ref(&format!("{}-0", setup.path))
        .is_some());
    setup.executor.stop(4, &setup.path).await;
    setup.shutdown().await;
}

#[tokio::test]
async fn component_roots_cannot_alias_another_pools_member_even_while_pending() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            ..Default::default()
        },
        2,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { result = &mut deploy => panic!("unexpected {result:?}"), _ = wait(&setup.probe.second_started) => {} }
    let mut alias = setup.command.clone();
    alias.name.push_str("-1");
    // Its own members have distinct actor paths, but its component selector
    // would shadow the original pool's second member.
    assert!(matches!(
        setup.executor.deploy(2, &alias).await,
        R::Failed { .. }
    ));
    setup.probe.factory_release.notify_one();
    assert!(matches!(deploy.await, R::Deployed { .. }));
    assert!(matches!(
        setup.executor.deploy(3, &alias).await,
        R::Failed { .. }
    ));
    setup.executor.stop(4, &setup.path).await;
    setup.probe.factory_release.notify_one();
    assert!(matches!(
        setup.executor.deploy(5, &alias).await,
        R::Deployed { .. }
    ));
    assert!(matches!(
        setup.executor.deploy(6, &setup.command).await,
        R::Failed { .. }
    ));
    setup
        .executor
        .stop(7, &format!("/user/{}", alias.name))
        .await;
    setup.shutdown().await;
}
