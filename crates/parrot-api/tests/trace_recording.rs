use parrot_api::trace::{self, SpanGuard, SpanKind, SpanStatus, TraceContext, TraceRecorder};
use std::{
    collections::HashSet,
    future::Future,
    panic::AssertUnwindSafe,
    task::{Context, Poll, Waker},
};

#[test]
fn capacity_rejects_zero_and_above_bound() {
    assert!(TraceRecorder::new(0).is_err());
    assert!(TraceRecorder::new(trace::MAX_RECORD_CAPACITY + 1).is_err());
    assert_eq!(TraceRecorder::new(1).unwrap().snapshot().capacity, 1);
}
fn done(context: &TraceContext, status: SpanStatus) {
    context.in_scope(|| SpanGuard::current(SpanKind::Application, "/work").finish(status));
}
#[test]
fn context_only_and_untraced_calls_are_inert() {
    let recorder = TraceRecorder::new(4).unwrap();
    done(&TraceContext::root(), SpanStatus::Ok);
    SpanGuard::current(SpanKind::Application, "/none").finish(SpanStatus::Ok);
    assert_eq!(recorder.snapshot().counts.started, 0);
}
#[test]
fn descendants_preserve_ids_without_reusing_root_span() {
    let recorder = TraceRecorder::new(4).unwrap();
    let root = recorder.root();
    let child = root.child();
    let grandchild = child.child();
    done(&grandchild, SpanStatus::Error);
    done(&child, SpanStatus::Ok);
    let result = recorder.snapshot();
    assert_eq!(result.records.len(), 2);
    for (record, context) in result.records.iter().zip([grandchild, child]) {
        assert_eq!(record.trace_id, root.trace_id());
        assert_eq!(record.span_id, context.span_id());
        assert_eq!(record.parent_span_id, context.parent_span_id());
        assert_ne!(record.span_id, root.span_id());
        assert!(record.started + record.duration <= result.elapsed);
    }
    assert!(!result.terminal);
    assert_eq!(result.counts.ok, 1);
    assert_eq!(result.counts.errors, 1);
}
#[test]
fn result_classification_does_not_call_user_display() {
    #[derive(Debug)]
    struct Hostile;
    impl std::fmt::Display for Hostile {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("user Display must not run");
        }
    }
    impl std::error::Error for Hostile {}
    let recorder = TraceRecorder::new(4).unwrap();
    let root = recorder.root();
    for result in [
        Ok(()),
        Err(parrot_api::errors::ActorError::Stopped),
        Err(parrot_api::errors::ActorError::Other(Hostile.into())),
        Err(parrot_api::errors::ActorError::Panic("expected".into())),
    ] {
        root.child().in_scope(|| {
            SpanGuard::current(SpanKind::TypedHandler, "/typed").finish_result(&result)
        });
    }
    let snapshot = recorder.snapshot();
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|r| r.status)
            .collect::<Vec<_>>(),
        [
            SpanStatus::Ok,
            SpanStatus::Error,
            SpanStatus::Error,
            SpanStatus::Panicked
        ]
    );
}
#[test]
fn unwind_and_ordinary_drop_have_distinct_terminal_status() {
    let recorder = TraceRecorder::new(4).unwrap();
    let root = recorder.root();
    root.in_scope(|| drop(SpanGuard::current(SpanKind::Application, "/cancel")));
    let error = std::panic::catch_unwind(AssertUnwindSafe(|| {
        root.child().in_scope(|| {
            let _owner = SpanGuard::current(SpanKind::Application, "/panic");
            std::panic::panic_any(37u32);
        })
    }))
    .unwrap_err();
    assert_eq!(*error.downcast::<u32>().unwrap(), 37);
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.cancelled, 1);
    assert_eq!(snapshot.counts.panicked, 1);
    assert_eq!(snapshot.counts.in_flight, 0);
}
#[test]
fn completion_order_capacity_and_loss_receipt() {
    let recorder = TraceRecorder::new(2).unwrap();
    let root = recorder.root();
    let first = root
        .child()
        .in_scope(|| SpanGuard::current(SpanKind::Application, "first"));
    done(&root.child(), SpanStatus::Ok);
    done(&root.child(), SpanStatus::Error);
    first.finish(SpanStatus::Cancelled);
    let snapshot = recorder.snapshot();
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>(),
        [3, 1]
    );
    assert_eq!(snapshot.counts.started, 3);
    assert_eq!(snapshot.counts.completed, 3);
    assert_eq!(snapshot.counts.overwritten, 1);
    assert_eq!(snapshot.counts.in_flight, 0);
    assert_eq!(
        snapshot.counts.ok + snapshot.counts.errors + snapshot.counts.cancelled,
        3
    );
}
#[test]
fn drain_retains_cumulative_accounting() {
    let recorder = TraceRecorder::new(2).unwrap();
    let root = recorder.root();
    done(&root, SpanStatus::Ok);
    let drained = recorder.drain();
    assert_eq!(drained.records.len(), 1);
    assert_eq!(drained.retained, 0);
    assert_eq!(drained.counts.drained, 1);
    assert_eq!(recorder.snapshot().records.len(), 0);
    done(&root.child(), SpanStatus::Error);
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.completed, 2);
    assert_eq!(snapshot.retained, 1);
    assert_eq!(
        snapshot.counts.completed,
        snapshot.counts.drained + snapshot.counts.overwritten + snapshot.retained as u64
    );
    assert_eq!(snapshot.counts.drained, 1);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.recorder_id, drained.recorder_id);
    assert_eq!(snapshot.origin, drained.origin);
}
#[test]
fn close_seals_admission_but_waits_for_accepted_owner() {
    let recorder = TraceRecorder::new(2).unwrap();
    let root = recorder.root();
    let owner = root.in_scope(|| SpanGuard::current(SpanKind::Application, "/accepted"));
    recorder.close();
    recorder.close();
    assert!(recorder.snapshot().closed);
    assert!(!recorder.snapshot().terminal);
    done(&root.child(), SpanStatus::Ok);
    owner.finish(SpanStatus::Error);
    let result = recorder.drain();
    assert!(result.terminal);
    assert_eq!(result.counts.started, 1);
    assert_eq!(result.counts.rejected_after_close, 1);
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].target, "/accepted");
    assert_eq!(result.counts.errors, 1);
}
#[test]
fn target_length_has_utf8_boundary_and_truncation_receipt() {
    let recorder = TraceRecorder::new(2).unwrap();
    let root = recorder.root();
    let target = "汉".repeat(300);
    root.in_scope(|| SpanGuard::current(SpanKind::Application, &target).finish(SpanStatus::Ok));
    let result = recorder.snapshot();
    let record = &result.records[0];
    assert!(record.target.len() <= trace::MAX_TARGET_BYTES);
    assert!(target.starts_with(&record.target));
    assert!(record.target_truncated);
    assert_eq!(result.counts.targets_truncated, 1);
}
#[test]
fn independent_recorders_do_not_leak_across_scopes() {
    let a = TraceRecorder::new(2).unwrap();
    let b = TraceRecorder::new(2).unwrap();
    let ar = a.root();
    let br = b.root();
    ar.in_scope(|| {
        done(&br.child(), SpanStatus::Error);
        SpanGuard::current(SpanKind::Application, "/a").finish(SpanStatus::Ok);
    });
    assert_eq!(a.snapshot().counts.ok, 1);
    assert_eq!(b.snapshot().counts.errors, 1);
    assert_ne!(a.snapshot().recorder_id, b.snapshot().recorder_id);
    assert!(TraceContext::current().is_none());
}
#[test]
fn suspended_scope_cancellation_retires_once_and_unpolled_is_absent() {
    let recorder = TraceRecorder::new(4).unwrap();
    let root = recorder.root();
    let operation = || async {
        let _owner = SpanGuard::current(SpanKind::Application, "/suspend");
        std::future::pending::<()>().await;
    };
    drop(root.scope(operation()));
    assert_eq!(recorder.snapshot().counts.started, 0);
    let mut operation = Box::pin(root.scope(operation()));
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    assert!(matches!(operation.as_mut().poll(&mut cx), Poll::Pending));
    assert_eq!(recorder.snapshot().counts.in_flight, 1);
    assert!(TraceContext::current().is_none());
    drop(operation);
    let result = recorder.snapshot();
    assert_eq!(result.counts.completed, 1);
    assert_eq!(result.counts.cancelled, 1);
    assert_eq!(result.counts.in_flight, 0);
}
#[test]
fn concurrent_finishing_and_draining_have_exact_loss_accounting() {
    let recorder = TraceRecorder::new(7).unwrap();
    let root = recorder.root();
    let mut seen = HashSet::new();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        done(&root.child(), SpanStatus::Ok);
                    }
                })
            })
            .collect();
        while workers.iter().any(|worker| !worker.is_finished()) {
            for record in recorder.drain().records {
                assert!(seen.insert(record.sequence));
            }
            std::thread::yield_now();
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    recorder.close();
    let result = recorder.drain();
    for record in &result.records {
        assert!(seen.insert(record.sequence));
    }
    assert!(result.terminal);
    assert_eq!(result.counts.started, 800);
    assert_eq!(result.counts.completed, 800);
    assert_eq!(result.counts.ok, 800);
    assert_eq!(result.counts.drained, seen.len() as u64);
    assert_eq!(
        result.counts.completed,
        result.counts.drained + result.counts.overwritten
    );
    assert!(!result.counts.counters_overflowed);
}

#[test]
fn inert_recording_guard_uses_only_one_optional_pointer() {
    assert_eq!(
        std::mem::size_of::<parrot_api::trace::SpanGuard>(),
        std::mem::size_of::<usize>()
    );
}

#[test]
fn child_lifetime_is_distinct_without_changing_execution_identity() {
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = recorder.root();
    parent.in_scope(|| {
        let root = SpanGuard::current(SpanKind::Application, "root");
        let first = SpanGuard::child(SpanKind::ManagedTask, "first");
        let second = SpanGuard::child(SpanKind::ManagedTask, "second");
        assert_eq!(TraceContext::current(), Some(parent.clone()));
        assert_eq!(recorder.snapshot().counts.in_flight, 3);
        second.finish(SpanStatus::Error);
        first.finish(SpanStatus::Ok);
        root.finish(SpanStatus::Ok);
    });
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.records.len(), 3);
    let ids: HashSet<_> = snapshot.records.iter().map(|r| r.span_id).collect();
    assert_eq!(ids.len(), 3);
    for child in &snapshot.records[..2] {
        assert_eq!(child.parent_span_id, Some(parent.span_id()));
        assert_eq!(child.trace_id, parent.trace_id());
    }
    assert!(TraceContext::current().is_none());
    TraceContext::root().in_scope(|| drop(SpanGuard::child(SpanKind::ManagedTask, "unrecorded")));
    assert_eq!(recorder.snapshot().counts.started, 3);
}
