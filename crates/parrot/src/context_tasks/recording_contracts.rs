use super::*;
use parrot_api::{
    address::ActorRef,
    trace::{SpanStatus, TraceContext, TraceRecorder},
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn poll(task: &mut OwnedTask) -> Poll<anyhow::Result<()>> {
    Pin::new(task).poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
}
fn outcome(recorder: &TraceRecorder, parent: &TraceContext, status: SpanStatus) {
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.in_flight, 0);
    assert_eq!(
        snapshot.records.len(),
        1,
        "admitted task must publish exactly one lifecycle"
    );
    let record = &snapshot.records[0];
    assert_eq!(format!("{:?}", record.kind), "ManagedTask");
    assert_eq!(record.trace_id, parent.trace_id());
    assert_eq!(record.parent_span_id, Some(parent.span_id()));
    assert_ne!(record.span_id, parent.span_id());
    assert_eq!(record.status, status);
}
struct Probe {
    parent: TraceContext,
    recorder: TraceRecorder,
    mode: u8,
    drops: Arc<AtomicUsize>,
}
impl Future for Probe {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        assert_eq!(TraceContext::current(), Some(self.parent.clone()));
        match self.mode {
            0 | 4 => Poll::Ready(Ok(())),
            1 => Poll::Ready(Err(ActorError::Stopped)),
            2 => Poll::Ready(Err(ActorError::Panic("returned panic".into()))),
            3 => panic!("poll failed"),
            _ => Poll::Pending,
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        assert_eq!(TraceContext::current(), Some(self.parent.clone()));
        assert_eq!(
            self.recorder.snapshot().counts.completed,
            0,
            "record precedes cleanup"
        );
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.mode == 4 || self.mode == 6 {
            panic!("cleanup failed");
        }
    }
}
#[tokio::test]
async fn result_and_cleanup_are_classified_after_owned_disposal() {
    for (mode, status) in [
        (0, SpanStatus::Ok),
        (1, SpanStatus::Error),
        (2, SpanStatus::Panicked),
        (3, SpanStatus::Panicked),
        (4, SpanStatus::Panicked),
    ] {
        let recorder = TraceRecorder::new(16).unwrap();
        let parent = recorder.root();
        let owner = ContextTasks::new(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut task = parent.in_scope(|| {
            owner
                .user_task(Probe {
                    parent: parent.clone(),
                    recorder: recorder.clone(),
                    mode,
                    drops: drops.clone(),
                })
                .unwrap()
        });
        assert_eq!(recorder.snapshot().counts.in_flight, 1);
        let foreign = TraceContext::root();
        foreign.in_scope(|| {
            assert!(poll(&mut task).is_ready());
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
            drop(task);
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
        });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        outcome(&recorder, &parent, status);
        assert_eq!(owner.join().await.is_err(), mode == 3 || mode == 4);
    }
}
#[tokio::test]
async fn abandonment_and_owner_cancellation_include_cleanup() {
    for initial_poll in [false, true] {
        for close in [false, true] {
            for mode in [5, 6] {
                let recorder = TraceRecorder::new(4).unwrap();
                let parent = recorder.root();
                let owner = ContextTasks::new(1);
                let drops = Arc::new(AtomicUsize::new(0));
                let mut task = parent.in_scope(|| {
                    owner
                        .user_task(Probe {
                            parent: parent.clone(),
                            recorder: recorder.clone(),
                            mode,
                            drops: drops.clone(),
                        })
                        .unwrap()
                });
                if initial_poll {
                    assert!(poll(&mut task).is_pending());
                }
                if close {
                    owner.close();
                    assert!(poll(&mut task).is_ready());
                }
                drop(task);
                outcome(
                    &recorder,
                    &parent,
                    if mode == 6 {
                        SpanStatus::Panicked
                    } else {
                        SpanStatus::Cancelled
                    },
                );
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(owner.join().await.is_err(), mode == 6);
            }
        }
    }
}
#[tokio::test]
async fn one_tasks_panic_does_not_contaminate_another() {
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = recorder.root();
    let owner = ContextTasks::new(2);
    let mut bad = parent.in_scope(|| {
        owner
            .user_task(async {
                panic!("only bad task");
                #[allow(unreachable_code)]
                Ok(())
            })
            .unwrap()
    });
    let mut good = parent.in_scope(|| owner.user_task(async { Ok(()) }).unwrap());
    assert!(poll(&mut bad).is_ready());
    assert!(poll(&mut good).is_ready());
    drop((bad, good));
    let snapshot = recorder.snapshot();
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|r| r.status)
            .collect::<Vec<_>>(),
        [SpanStatus::Panicked, SpanStatus::Ok]
    );
    assert_ne!(snapshot.records[0].span_id, snapshot.records[1].span_id);
    assert!(owner.join().await.is_err());
}
#[tokio::test]
async fn no_record_for_rejected_or_maintenance_work() {
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = recorder.root();
    let owner = ContextTasks::new(1);
    let task = parent.in_scope(|| owner.user_task(std::future::pending()).unwrap());
    parent.in_scope(|| {
        assert!(owner.user_task(async { Ok(()) }).is_err());
        drop(owner.idle_worker(async { Ok(()) }).unwrap());
    });
    assert_eq!(recorder.snapshot().counts.started, 1);
    owner.close();
    assert!(
        parent
            .in_scope(|| owner.user_task(async { Ok(()) }))
            .is_err()
    );
    drop(task);
    outcome(&recorder, &parent, SpanStatus::Cancelled);
    owner.join().await.unwrap();
}
#[test]
fn closed_recorder_and_untraced_work_preserve_isolation() {
    let recorder = TraceRecorder::new(8).unwrap();
    recorder.close();
    let parent = recorder.root();
    let owner = ContextTasks::new(2);
    drop(parent.in_scope(|| owner.user_task(async { Ok(()) }).unwrap()));
    let mut task = owner
        .user_task(async {
            assert!(TraceContext::current().is_none());
            Ok(())
        })
        .unwrap();
    parent.in_scope(|| assert!(poll(&mut task).is_ready()));
    assert_eq!(recorder.snapshot().counts.started, 0);
    assert_eq!(recorder.snapshot().counts.rejected_after_close, 1);
}
#[tokio::test]
async fn flushed_input_failure_is_still_attributed_to_its_task() {
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = recorder.root();
    let owner = ContextTasks::new(1);
    let failures = InputFailures::new();
    let mut task = parent.in_scope(|| {
        owner
            .user_task(async { Err(ActorError::Stopped) })
            .unwrap()
            .with_input_failures(failures.clone())
    });
    failures.record("input cleanup".into());
    assert!(poll(&mut task).is_ready());
    outcome(&recorder, &parent, SpanStatus::Panicked);
    assert!(owner.join().await.is_err());
}
#[tokio::test]
async fn late_transferred_input_failure_does_not_rewrite_retired_lifecycle() {
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = recorder.root();
    let owner = ContextTasks::new(1);
    let failures = InputFailures::new();
    let mut task = parent.in_scope(|| {
        owner
            .user_task(async { Ok(()) })
            .unwrap()
            .with_input_failures(failures.clone())
    });
    assert!(poll(&mut task).is_ready());
    outcome(&recorder, &parent, SpanStatus::Ok);
    failures.record("late transferred payload".into());
    outcome(&recorder, &parent, SpanStatus::Ok);
    assert!(owner.join().await.is_err());
}
#[derive(Debug)]
struct Target {
    panic_on_drop: bool,
}
struct Delivery {
    panic_on_drop: bool,
}
impl Future for Delivery {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}
impl Drop for Delivery {
    fn drop(&mut self) {
        if self.panic_on_drop {
            panic!("cancelled delivery cleanup");
        }
    }
}
impl ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("unexpected ask")
    }
    fn send_with_timeout<'a>(
        &'a self,
        m: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(m)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(Delivery {
            panic_on_drop: self.panic_on_drop,
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        panic!("diagnostics must not call arbitrary target path")
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Target {
            panic_on_drop: self.panic_on_drop,
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
#[tokio::test(start_paused = true)]
async fn explicit_schedule_cancel_is_not_success_even_during_delivery() {
    for periodic in [false, true] {
        for delivered in [false, true] {
            for panic_on_drop in [false, true] {
                let recorder = TraceRecorder::new(8).unwrap();
                let parent = recorder.root();
                let owner = ContextTasks::new(1);
                let (id, mut task) = parent
                    .in_scope(|| {
                        let target: BoxedActorRef = Box::new(Target { panic_on_drop });
                        if periodic {
                            owner.periodic_with_id(
                                target,
                                CloneableMessage::from_cloneable(()),
                                Duration::from_millis(1),
                                Duration::from_secs(1),
                            )
                        } else {
                            owner.once_with_id(target, Box::new(()), Duration::from_millis(1))
                        }
                    })
                    .unwrap();
                assert!(poll(&mut task).is_pending());
                if delivered {
                    tokio::time::advance(Duration::from_millis(2)).await;
                    assert!(poll(&mut task).is_pending());
                }
                owner.cancel_schedule(id);
                assert!(poll(&mut task).is_ready());
                drop(task);
                let panicked = delivered && panic_on_drop;
                outcome(
                    &recorder,
                    &parent,
                    if panicked {
                        SpanStatus::Panicked
                    } else {
                        SpanStatus::Cancelled
                    },
                );
                assert_eq!(owner.join().await.is_err(), panicked);
            }
        }
    }
}

#[derive(Debug)]
struct HostileError {
    format: bool,
    destructor: bool,
    drops: Arc<AtomicUsize>,
}
impl std::fmt::Display for HostileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        assert!(!self.format, "error formatter failed");
        write!(f, "ordinary error")
    }
}
impl std::error::Error for HostileError {}
impl Drop for HostileError {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.destructor, "error disposal failed");
    }
}
#[test]
fn hostile_error_format_and_disposal_have_independent_boundaries() {
    for format in [false, true] {
        for destructor in [false, true] {
            let recorder = TraceRecorder::new(8).unwrap();
            let parent = recorder.root();
            let owner = ContextTasks::new(1);
            let drops = Arc::new(AtomicUsize::new(0));
            let error = HostileError {
                format,
                destructor,
                drops: drops.clone(),
            };
            let mut task = parent.in_scope(|| {
                owner
                    .user_task(async move { Err(ActorError::Other(error.into())) })
                    .unwrap()
            });
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_writer(std::io::sink)
                .finish();
            tracing::subscriber::with_default(subscriber, || assert!(poll(&mut task).is_ready()));
            drop(task);
            outcome(
                &recorder,
                &parent,
                if format || destructor {
                    SpanStatus::Panicked
                } else {
                    SpanStatus::Error
                },
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(
                owner.state.status.lock().unwrap().panic.is_some(),
                format || destructor
            );
        }
    }
}
#[test]
fn cancellation_after_successful_poll_does_not_relabel_success() {
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = recorder.root();
    let owner = ContextTasks::new(1);
    let close = owner.clone();
    let mut task = parent.in_scope(|| {
        owner
            .user_task(async move {
                close.close();
                Ok(())
            })
            .unwrap()
    });
    assert!(poll(&mut task).is_ready());
    drop(task);
    outcome(&recorder, &parent, SpanStatus::Ok);
}
#[test]
fn rejected_cleanup_panic_never_creates_or_poison_admitted_record() {
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = recorder.root();
    let owner = ContextTasks::new(1);
    let mut good = parent.in_scope(|| owner.user_task(async { Ok(()) }).unwrap());
    let drops = Arc::new(AtomicUsize::new(0));
    let result = parent.in_scope(|| {
        owner.user_task(Probe {
            parent: parent.clone(),
            recorder: recorder.clone(),
            mode: 6,
            drops: drops.clone(),
        })
    });
    assert!(matches!(result, Err(ActorError::Panic(_))));
    assert_eq!(recorder.snapshot().counts.started, 1);
    assert!(owner.state.status.lock().unwrap().panic.is_none());
    assert!(poll(&mut good).is_ready());
    drop(good);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    outcome(&recorder, &parent, SpanStatus::Ok);
}
