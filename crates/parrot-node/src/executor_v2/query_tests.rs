use super::*;
use parrot_api::{
    trace::{self, SpanKind, SpanStatus, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
};

#[derive(Debug, Default)]
struct Probe {
    events: Mutex<Vec<(&'static str, Option<TraceContext>)>>,
    mode: AtomicUsize,
}
impl Probe {
    fn observe(&self, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((phase, TraceContext::current()));
    }
}
#[derive(Debug)]
struct QueryRef(Arc<Probe>);
struct Liveness(Arc<Probe>);
impl Future for Liveness {
    type Output = bool;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<bool> {
        self.0.observe("poll");
        match self.0.mode.load(Ordering::SeqCst) {
            0 | 3 => Poll::Ready(true),
            1 | 4 => Poll::Pending,
            2 => panic!("query liveness poll panic"),
            _ => unreachable!(),
        }
    }
}
impl Drop for Liveness {
    fn drop(&mut self) {
        self.0.observe("drop");
        assert!(
            !matches!(self.0.mode.load(Ordering::SeqCst), 3 | 4),
            "query cleanup panic"
        );
    }
}
impl ActorRef for QueryRef {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Err(ActorError::Stopped) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Err(ActorError::Stopped) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/user/query".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.0.observe("construct");
        Box::pin(Liveness(self.0.clone()))
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self(self.0.clone()))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn executor(engine: Arc<ThreadActorSystem>, probe: Arc<Probe>) -> NodeComponentExecutor {
    let executor = NodeComponentExecutor::new(engine);
    executor.owner.records.lock().unwrap().insert(
        "query".into(),
        Deployment {
            command: ComponentDeploy {
                name: "query".into(),
                version: "1.2.3".into(),
                artifact: AdminArtifactRef::Props {
                    factory: "query-fixture".into(),
                },
                instances: AdminInstancePolicy::Singleton,
                config: None,
            },
            generation: Arc::new(()),
            committed: true,
            instances: vec![Instance {
                path: "/user/query".into(),
                actor: ReferenceOwner::new("/user/query".into(), Arc::new(QueryRef(probe))),
                cleanup_error: None,
            }],
        },
    );
    executor
}
async fn query(executor: &NodeComponentExecutor, kind: usize) -> R {
    match kind {
        0 => executor.status(91, "/user/query").await,
        1 => executor.metrics(91).await,
        2 => R::MeasuredMetrics {
            req_id: 91,
            metrics: executor.measured_metrics().await,
        },
        _ => unreachable!(),
    }
}
fn target(kind: usize) -> &'static str {
    ["node/status", "node/metrics", "node/measured_metrics"][kind]
}
#[tokio::test]
async fn queries_record_one_child_and_preserve_callback_identity() {
    for kind in 0..3 {
        let engine = ThreadActorSystem::shared(Default::default());
        let probe = Arc::new(Probe::default());
        let executor = executor(engine.clone(), probe.clone());
        let recorder = TraceRecorder::new(8).unwrap();
        let root = recorder.root();
        let reply = root.scope(query(&executor, kind)).await;
        assert_eq!(reply.req_id(), 91);
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.records.len(), 1, "query must record exactly once");
        assert_eq!(snapshot.counts.in_flight, 0);
        let record = &snapshot.records[0];
        assert_eq!(record.kind, SpanKind::Application);
        assert_eq!(record.target, target(kind));
        assert_eq!(record.status, SpanStatus::Ok);
        assert_eq!(record.trace_id, root.trace_id());
        assert_eq!(record.parent_span_id, Some(root.span_id()));
        {
            let events = probe.events.lock().unwrap();
            assert_eq!(
                events.iter().map(|(phase, _)| *phase).collect::<Vec<_>>(),
                ["construct", "poll", "drop"]
            );
            for (_, context) in events.iter() {
                let context = context.as_ref().unwrap();
                assert_eq!(context.span_id(), record.span_id);
                assert_eq!(context.trace_id(), record.trace_id);
            }
        }
        engine.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn missing_status_records_error_without_exposing_selector() {
    let engine = ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::new(engine.clone());
    let recorder = TraceRecorder::new(8).unwrap();
    let reply = recorder
        .root()
        .scope(executor.status(11, "/secret/selector"))
        .await;
    assert!(matches!(
        reply,
        R::Failed {
            code: v2_err::COMPONENT_NOT_FOUND,
            ..
        }
    ));
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.records[0].target, "node/status");
    assert_eq!(snapshot.records[0].status, SpanStatus::Error);
    engine.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn cancelled_queries_retire_callback_before_terminal_record() {
    for kind in 0..3 {
        for mode in [1, 4] {
            let engine = ThreadActorSystem::shared(Default::default());
            let probe = Arc::new(Probe::default());
            probe.mode.store(mode, Ordering::SeqCst);
            let executor = executor(engine.clone(), probe.clone());
            let recorder = TraceRecorder::new(8).unwrap();
            let root = recorder.root();
            let mut work = Box::pin(root.scope(query(&executor, kind)));
            assert!(futures::poll!(&mut work).is_pending());
            assert_eq!(recorder.snapshot().counts.in_flight, 1);
            recorder.close();
            assert!(!recorder.snapshot().terminal);
            drop(work);
            let snapshot = recorder.snapshot();
            assert!(snapshot.terminal);
            assert_eq!(snapshot.records.len(), 1);
            let record = &snapshot.records[0];
            assert_eq!(
                record.status,
                if mode == 4 {
                    SpanStatus::Panicked
                } else {
                    SpanStatus::Cancelled
                }
            );
            {
                let events = probe.events.lock().unwrap();
                assert_eq!(events.last().unwrap().0, "drop");
                assert_eq!(
                    events.last().unwrap().1.as_ref().unwrap().span_id(),
                    record.span_id
                );
            }
            assert_eq!(
                executor.owner.admission.available_permits(),
                MAX_PENDING_OPERATIONS
            );
            engine.shutdown_internal().await.unwrap();
        }
    }
}
#[tokio::test]
async fn handled_callback_failures_keep_unavailable_reply_and_error_record() {
    for kind in 0..3 {
        for mode in [2, 3] {
            let engine = ThreadActorSystem::shared(Default::default());
            let probe = Arc::new(Probe::default());
            probe.mode.store(mode, Ordering::SeqCst);
            let executor = executor(engine.clone(), probe);
            let healthy = Arc::new(Probe::default());
            {
                let mut records = executor.owner.records.lock().unwrap();
                let deployment = records.get_mut("query").unwrap();
                deployment.command.instances = AdminInstancePolicy::Pool { count: 2 };
                deployment.instances.push(Instance {
                    path: "/user/query-healthy".into(),
                    actor: ReferenceOwner::new(
                        "/user/query-healthy".into(),
                        Arc::new(QueryRef(healthy.clone())),
                    ),
                    cleanup_error: None,
                });
            }
            let recorder = TraceRecorder::new(8).unwrap();
            let reply = recorder.root().scope(query(&executor, kind)).await;
            let states = match reply {
                R::Status { states, .. } => states,
                R::Metrics { snapshot, .. } => snapshot.component_states,
                R::MeasuredMetrics { metrics, .. } => metrics.snapshot.component_states,
                other => panic!("lost query result: {other:?}"),
            };
            assert_eq!(states.len(), 2);
            assert_eq!(states[0].state, "unavailable");
            assert_eq!(states[1].state, "running");
            assert_eq!(
                healthy.events.lock().unwrap().len(),
                3,
                "failed callback skipped a healthy member"
            );
            let snapshot = recorder.snapshot();
            assert_eq!(snapshot.records.len(), 1);
            assert_eq!(snapshot.records[0].status, SpanStatus::Error);
            engine.shutdown_internal().await.unwrap();
        }
    }
}
#[tokio::test]
async fn unpolled_queries_do_not_record_and_untraced_queries_clear_ambient_identity() {
    let engine = ThreadActorSystem::shared(Default::default());
    let probe = Arc::new(Probe::default());
    let executor = executor(engine.clone(), probe.clone());
    let recorder = TraceRecorder::new(8).unwrap();
    for kind in 0..3 {
        let root = recorder.root();
        drop(root.scope(query(&executor, kind)));
        root.scope(trace::scope(None, query(&executor, kind))).await;
    }
    assert_eq!(recorder.snapshot().counts.started, 0);
    assert!(probe
        .events
        .lock()
        .unwrap()
        .iter()
        .all(|(_, context)| context.is_none()));
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn queries_keep_identity_without_recording_and_ignore_mutation_quota() {
    let engine = ThreadActorSystem::shared(Default::default());
    let probe = Arc::new(Probe::default());
    let executor = executor(engine.clone(), probe.clone());
    let _permits = executor
        .owner
        .admission
        .clone()
        .acquire_many_owned(MAX_PENDING_OPERATIONS as u32)
        .await
        .unwrap();
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = TraceContext::root();
    for kind in 0..3 {
        let reply = recorder
            .root()
            .scope(parent.scope(query(&executor, kind)))
            .await;
        assert!(!matches!(reply, R::Failed { .. }));
    }
    assert_eq!(recorder.snapshot().counts.started, 0);
    {
        let events = probe.events.lock().unwrap();
        assert_eq!(events.len(), 9);
        for group in events.as_chunks::<3>().0 {
            let child = group[0].1.as_ref().unwrap();
            assert_eq!(child.trace_id(), parent.trace_id());
            assert_eq!(child.parent_span_id(), Some(parent.span_id()));
            assert!(group
                .iter()
                .all(|(_, context)| context.as_ref() == Some(child)));
        }
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn unhandled_query_poll_and_disposal_panics_return_failure_after_recording() {
    use super::operation_tests::HostileFuture;
    for (poll_panics, drop_panics) in [(true, false), (false, true), (true, true)] {
        let engine = ThreadActorSystem::shared(Default::default());
        let executor = NodeComponentExecutor::new(engine.clone());
        let recorder = TraceRecorder::new(8).unwrap();
        let observed = Arc::new(Mutex::new(None));
        let reply = recorder
            .root()
            .scope(executor.query(7, "node/status", |_| HostileFuture {
                poll_panics,
                drop_panics,
                observed_drop: observed.clone(),
            }))
            .await;
        assert!(
            matches!(reply, R::Failed { req_id: 7, ref detail, .. } if detail == "node management operation panicked")
        );
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].status, SpanStatus::Panicked);
        assert_eq!(
            observed.lock().unwrap().as_ref().unwrap().span_id(),
            snapshot.records[0].span_id
        );
        engine.shutdown_internal().await.unwrap();
    }
}
