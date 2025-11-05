//! A source owns its capacity and recording until actual owner-thread retirement.
use futures::{task::noop_waker_ref, Stream};
use parrot_api::{
    local_typed::{LocalStreamEventKind, LocalStreamRegistry, DEFAULT_MAX_LOCAL_STREAMS},
    trace::{self, SpanKind, SpanStatus, TraceContext, TraceRecorder},
};
use std::{
    cell::RefCell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

type Seen = Rc<RefCell<Vec<(&'static str, Option<TraceContext>)>>>;
struct Source {
    mode: u8,
    seen: Seen,
    recorder: TraceRecorder,
    drop_panic: bool,
}
impl Stream for Source {
    type Item = Rc<()>;
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.seen
            .borrow_mut()
            .push(("poll", TraceContext::current()));
        match self.mode {
            0 => Poll::Ready(None),
            1 => Poll::Pending,
            _ => panic!("source poll"),
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.seen
            .borrow_mut()
            .push(("drop", TraceContext::current()));
        assert!(
            self.recorder.snapshot().records.is_empty(),
            "record published before source retirement"
        );
        if self.drop_panic {
            panic!("source drop")
        }
    }
}
fn run(mode: u8, drop_panic: bool, drive: bool) {
    for traced in [true, false] {
        let recorder = TraceRecorder::new(8).unwrap();
        let parent = traced.then(|| recorder.root());
        let foreign = TraceContext::root();
        let seen: Seen = Rc::default();
        let mut registry = LocalStreamRegistry::new();
        trace::in_scope(parent.clone(), || {
            registry
                .add_stream(Source {
                    mode,
                    seen: seen.clone(),
                    recorder: recorder.clone(),
                    drop_panic,
                })
                .unwrap()
        });
        assert_eq!(recorder.snapshot().counts.in_flight, u64::from(traced));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            foreign.in_scope(|| {
                if drive {
                    let started = futures::executor::block_on(registry.next_event()).unwrap();
                    started.complete(&mut registry, true);
                    let mut next = registry.next_event();
                    let event = next
                        .as_mut()
                        .poll(&mut Context::from_waker(noop_waker_ref()));
                    drop(next);
                    if let Poll::Ready(Some(event)) = event {
                        event.complete(&mut registry, false);
                    }
                }
                drop(registry);
            })
        }));
        assert_eq!(result.is_err(), drop_panic);
        assert!(TraceContext::current().is_none());
        assert!(seen.borrow().iter().all(|(_, context)| *context == parent));
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.records.len(), usize::from(traced));
        if let Some(record) = snapshot.records.first() {
            let parent = parent.as_ref().unwrap();
            assert_eq!(record.kind, SpanKind::ManagedTask);
            assert_eq!(record.target, "context.stream.local");
            assert_eq!(record.trace_id, parent.trace_id());
            assert_eq!(record.parent_span_id, Some(parent.span_id()));
            assert_ne!(record.span_id, parent.span_id());
            assert_eq!(
                record.status,
                if drop_panic || (drive && mode == 2) {
                    SpanStatus::Panicked
                } else if drive && mode == 0 {
                    SpanStatus::Ok
                } else {
                    SpanStatus::Cancelled
                }
            );
        }
    }
}
#[test]
fn exhausted_source_records_only_after_cleanup() {
    run(0, false, true);
}
#[test]
fn pending_source_records_cancellation() {
    run(1, false, true);
}
#[test]
fn unpolled_source_records_cancellation() {
    run(0, false, false);
}
#[test]
fn source_poll_panic_survives_error_event_consumption() {
    run(2, false, true);
}
#[test]
fn successful_source_drop_panic_overrides_success() {
    run(0, true, true);
}
#[test]
fn cancelled_source_drop_panic_overrides_cancellation() {
    run(1, true, true);
}
#[test]
fn checked_out_event_keeps_generation_capacity() {
    let mut registry = LocalStreamRegistry::new();
    for _ in 0..DEFAULT_MAX_LOCAL_STREAMS {
        registry
            .add_stream(futures::stream::pending::<Rc<()>>())
            .unwrap();
    }
    let outstanding = futures::executor::block_on(registry.next_event()).unwrap();
    assert!(
        registry
            .add_stream(futures::stream::pending::<()>())
            .is_err(),
        "checked-out source is still alive"
    );
    drop(outstanding);
    registry
        .add_stream(futures::stream::pending::<()>())
        .unwrap();
    assert!(registry
        .add_stream(futures::stream::pending::<()>())
        .is_err());
}
#[test]
fn returning_event_cannot_transfer_capacity_to_another_registry() {
    let mut a = LocalStreamRegistry::new();
    let mut b = LocalStreamRegistry::new();
    a.add_stream(futures::stream::pending::<()>()).unwrap();
    let event = futures::executor::block_on(a.next_event()).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        event.complete(&mut b, true)
    }));
    assert!(
        result.is_err(),
        "an event cannot change its owning generation"
    );
    assert!(a.is_empty() && b.is_empty());
    a.add_stream(futures::stream::empty::<()>()).unwrap();
}
#[test]
fn finished_event_remains_in_flight_until_consumer_retires_it() {
    let recorder = TraceRecorder::new(4).unwrap();
    let mut registry = LocalStreamRegistry::new();
    recorder
        .root()
        .in_scope(|| registry.add_stream(futures::stream::empty::<()>()).unwrap());
    futures::executor::block_on(registry.next_event())
        .unwrap()
        .complete(&mut registry, true);
    let finished = futures::executor::block_on(registry.next_event()).unwrap();
    assert_eq!(finished.kind(), LocalStreamEventKind::Finished);
    assert_eq!(recorder.snapshot().counts.in_flight, 1);
    drop(registry);
    recorder.close();
    assert!(!recorder.snapshot().terminal);
    drop(finished);
    let snapshot = recorder.snapshot();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.records[0].status, SpanStatus::Cancelled);
}
#[test]
fn rejected_registration_does_not_start_a_record() {
    let recorder = TraceRecorder::new(4).unwrap();
    let mut registry = LocalStreamRegistry::new();
    for _ in 0..DEFAULT_MAX_LOCAL_STREAMS {
        registry
            .add_stream(futures::stream::pending::<()>())
            .unwrap();
    }
    let event = futures::executor::block_on(registry.next_event()).unwrap();
    recorder
        .root()
        .in_scope(|| assert!(registry.add_stream(futures::stream::empty::<()>()).is_err()));
    assert_eq!(recorder.snapshot().counts.started, 0);
    drop(event);
}
#[test]
fn closed_recorder_does_not_reject_source_admission() {
    let recorder = TraceRecorder::new(4).unwrap();
    recorder.close();
    let mut registry = LocalStreamRegistry::new();
    recorder
        .root()
        .in_scope(|| registry.add_stream(futures::stream::empty::<()>()).unwrap());
    drop(registry);
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.rejected_after_close, 1);
    assert_eq!(snapshot.counts.started, 0);
}

#[test]
fn one_source_failure_does_not_mark_other_sources_panicked() {
    struct DropSource {
        fail: bool,
    }
    impl Stream for DropSource {
        type Item = ();
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<()>> {
            Poll::Pending
        }
    }
    impl Drop for DropSource {
        fn drop(&mut self) {
            if self.fail {
                panic!("first source only")
            }
        }
    }
    let recorder = TraceRecorder::new(4).unwrap();
    let mut registry = LocalStreamRegistry::new();
    recorder.root().in_scope(|| {
        registry.add_stream(DropSource { fail: true }).unwrap();
        registry.add_stream(DropSource { fail: false }).unwrap();
    });
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(registry))).is_err());
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.in_flight, 0);
    assert_eq!(snapshot.counts.panicked, 1);
    assert_eq!(snapshot.counts.cancelled, 1);
    assert_ne!(snapshot.records[0].span_id, snapshot.records[1].span_id);
}
#[test]
fn caller_unwind_does_not_invent_a_source_failure() {
    let recorder = TraceRecorder::new(4).unwrap();
    let parent = recorder.root();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut registry = LocalStreamRegistry::new();
        parent.in_scope(|| {
            registry
                .add_stream(futures::stream::pending::<()>())
                .unwrap()
        });
        panic!("unrelated caller failure");
    }));
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.counts.cancelled, 1);
    assert_eq!(snapshot.counts.panicked, 0);
}
#[test]
fn discarded_item_cleanup_failure_is_recorded_before_source_retirement() {
    struct Item;
    impl Drop for Item {
        fn drop(&mut self) {
            panic!("unconsumed item")
        }
    }
    let recorder = TraceRecorder::new(4).unwrap();
    let mut registry = LocalStreamRegistry::new();
    recorder
        .root()
        .in_scope(|| registry.add_stream(futures::stream::iter([Item])).unwrap());
    futures::executor::block_on(registry.next_event())
        .unwrap()
        .complete(&mut registry, true);
    let item = futures::executor::block_on(registry.next_event()).unwrap();
    assert_eq!(item.kind(), LocalStreamEventKind::Item);
    let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        item.complete(&mut registry, false)
    }))
    .unwrap_err();
    assert_eq!(error.downcast_ref::<String>().unwrap(), "unconsumed item");
    assert_eq!(recorder.snapshot().counts.panicked, 1);
    assert_eq!(recorder.snapshot().counts.in_flight, 0);
}
#[test]
fn exhausted_event_can_release_capacity_without_reopening_the_recorder() {
    let recorder = TraceRecorder::new(4).unwrap();
    let mut registry = LocalStreamRegistry::new();
    let root = recorder.root();
    root.in_scope(|| registry.add_stream(futures::stream::empty::<()>()).unwrap());
    futures::executor::block_on(registry.next_event())
        .unwrap()
        .complete(&mut registry, true);
    let event = futures::executor::block_on(registry.next_event()).unwrap();
    recorder.close();
    event.complete(&mut registry, false);
    let snapshot = recorder.snapshot();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.ok, 1);
    root.in_scope(|| registry.add_stream(futures::stream::empty::<()>()).unwrap());
    assert_eq!(recorder.snapshot().counts.rejected_after_close, 1);
}
