use super::*;
use parrot_api::trace::{SpanKind, SpanStatus, TraceContext, TraceRecorder};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub(super) struct HostileFuture {
    pub(super) poll_panics: bool,
    pub(super) drop_panics: bool,
    pub(super) observed_drop: Arc<Mutex<Option<TraceContext>>>,
}
impl Future for HostileFuture {
    type Output = R;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<R> {
        assert!(
            TraceContext::current().is_some(),
            "poll lost caller identity"
        );
        assert!(!self.poll_panics, "injected management poll panic");
        Poll::Ready(R::Drained {
            req_id: 7,
            drained: 1,
            aborted: 0,
        })
    }
}
impl Drop for HostileFuture {
    fn drop(&mut self) {
        *self.observed_drop.lock().unwrap() = TraceContext::current();
        assert!(!self.drop_panics, "injected management cleanup panic");
    }
}

#[tokio::test]
async fn owned_work_preserves_context_through_poll_and_disposal() {
    let engine = ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::new(engine.clone());
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let observed = Arc::new(Mutex::new(None));
    let reply = root
        .scope(executor.owned(
            7,
            "node/test",
            HostileFuture {
                poll_panics: false,
                drop_panics: false,
                observed_drop: observed.clone(),
            },
        ))
        .await;
    engine.shutdown_internal().await.unwrap();
    assert!(matches!(reply, R::Drained { aborted: 0, .. }));
    let context = observed
        .lock()
        .unwrap()
        .clone()
        .expect("cleanup lost caller identity");
    assert_eq!(context.trace_id(), root.trace_id());
    assert_eq!(context.parent_span_id(), Some(root.span_id()));
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.in_flight, 0);
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.records[0].span_id, context.span_id());
    assert_eq!(snapshot.records[0].kind, SpanKind::ManagedTask);
    assert_eq!(snapshot.records[0].status, SpanStatus::Ok);
}

#[tokio::test]
async fn cleanup_panic_is_reported_before_sending_success() {
    for (poll_panics, drop_panics) in [(false, true), (true, true), (true, false)] {
        let engine = ThreadActorSystem::shared(Default::default());
        let executor = NodeComponentExecutor::new(engine.clone());
        let recorder = TraceRecorder::new(8).unwrap();
        let reply = recorder
            .root()
            .scope(executor.owned(
                7,
                "node/test",
                HostileFuture {
                    poll_panics,
                    drop_panics,
                    observed_drop: Default::default(),
                },
            ))
            .await;
        engine.shutdown_internal().await.unwrap();
        assert!(
            matches!(reply, R::Failed { req_id: 7, ref detail, .. }
            if detail == "node management operation panicked"),
            "{reply:?}"
        );
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].status, SpanStatus::Panicked);
        assert_eq!(
            executor.owner.admission.available_permits(),
            MAX_PENDING_OPERATIONS
        );
    }
}

#[tokio::test]
async fn cancelled_waiter_does_not_retire_owned_work() {
    let engine = ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::new(engine.clone());
    let recorder = TraceRecorder::new(8).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut waiter = Box::pin(recorder.root().scope(executor.owned(7, "node/test", {
        let started = started.clone();
        let release = release.clone();
        async move {
            started.notify_one();
            release.notified().await;
            R::Drained {
                req_id: 7,
                drained: 0,
                aborted: 1,
            }
        }
    })));
    assert!(futures::poll!(&mut waiter).is_pending());
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    drop(waiter);
    recorder.close();
    let before = recorder.snapshot();
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while executor.owner.admission.available_permits() != MAX_PENDING_OPERATIONS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    engine.shutdown_internal().await.unwrap();
    assert_eq!(before.counts.in_flight, 1);
    assert!(!before.terminal);
    let after = recorder.snapshot();
    assert!(after.terminal);
    assert_eq!(after.records.len(), 1);
    assert_eq!(after.records[0].status, SpanStatus::Error);
}

#[tokio::test]
async fn rejected_work_is_cleaned_before_releasing_its_record() {
    let engine = ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::new(engine.clone());
    let held = executor
        .owner
        .admission
        .clone()
        .acquire_many_owned(MAX_PENDING_OPERATIONS as u32)
        .await
        .unwrap();
    for drop_panics in [false, true] {
        let recorder = TraceRecorder::new(8).unwrap();
        let observed = Default::default();
        let reply = recorder
            .root()
            .scope(executor.owned(
                7,
                "node/test",
                HostileFuture {
                    poll_panics: true,
                    drop_panics,
                    observed_drop: observed,
                },
            ))
            .await;
        assert!(
            matches!(reply, R::Failed { ref detail, .. } if detail == if drop_panics {
            "node management operation panicked"
        } else { "node management operation limit reached" }),
            "{reply:?}"
        );
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(
            snapshot.records[0].status,
            if drop_panics {
                SpanStatus::Panicked
            } else {
                SpanStatus::Error
            }
        );
    }
    drop(held);
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn aborted_owner_protects_unpolled_future_cleanup() {
    for drop_panics in [false, true] {
        let recorder = TraceRecorder::new(8).unwrap();
        let root = recorder.root();
        let observed = Arc::new(Mutex::new(None));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let mut operation = root.in_scope(|| {
            operation::Operation::new(
                HostileFuture {
                    poll_panics: true,
                    drop_panics,
                    observed_drop: observed.clone(),
                },
                "node/test",
            )
        });
        operation.permit = Some(permits.clone().acquire_owned().await.unwrap());
        let started = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let started = started.clone();
            async move {
                let _operation = operation;
                started.notify_one();
                futures::future::pending::<()>().await;
            }
        });
        started.notified().await;
        recorder.close();
        assert_eq!(recorder.snapshot().counts.in_flight, 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(permits.available_permits(), 1);
        let context = observed.lock().unwrap().clone().unwrap();
        assert_eq!(context.parent_span_id(), Some(root.span_id()));
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(
            snapshot.records[0].status,
            if drop_panics {
                SpanStatus::Panicked
            } else {
                SpanStatus::Cancelled
            }
        );
    }
}

#[tokio::test]
async fn untraced_work_and_identity_only_work_do_not_create_recordings() {
    let engine = ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::new(engine.clone());
    let recorder = TraceRecorder::new(8).unwrap();
    for parent in [None, Some(TraceContext::root())] {
        let expected = parent.clone();
        let reply = recorder
            .root()
            .scope(parrot_api::trace::scope(
                parent,
                executor.owned(7, "node/test", async move {
                    let actual = TraceContext::current();
                    match (expected, actual) {
                        (None, None) => {}
                        (Some(parent), Some(child)) => {
                            assert_eq!(child.trace_id(), parent.trace_id());
                            assert_eq!(child.parent_span_id(), Some(parent.span_id()));
                        }
                        _ => panic!("unexpected management context"),
                    }
                    R::Drained {
                        req_id: 7,
                        drained: 0,
                        aborted: 0,
                    }
                }),
            ))
            .await;
        assert!(matches!(reply, R::Drained { .. }));
    }
    engine.shutdown_internal().await.unwrap();
    assert_eq!(recorder.snapshot().counts.started, 0);
}

#[tokio::test]
async fn drain_deadline_failure_is_cancelled_instead_of_success() {
    let engine = ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::new(engine.clone());
    let recorder = TraceRecorder::new(8).unwrap();
    let reply = recorder
        .root()
        .scope(executor.owned(7, "node/drain", async {
            failed_v2(7, v2_err::DRAIN_TIMEOUT, "deadline expired")
        }))
        .await;
    engine.shutdown_internal().await.unwrap();
    assert!(matches!(reply, R::Failed { code, .. } if code == v2_err::DRAIN_TIMEOUT));
    assert_eq!(recorder.snapshot().records[0].status, SpanStatus::Cancelled);
}
