use futures::task::noop_waker_ref;
use parrot_api::trace::{self, TraceContext};
use std::{
    future::Future,
    marker::PhantomPinned,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

type Events = Arc<Mutex<Vec<(&'static str, Option<TraceContext>)>>>;
fn snapshot(events: &Events) -> Vec<(&'static str, Option<TraceContext>)> {
    events.lock().unwrap().clone()
}
struct Probe {
    events: Events,
    ready: bool,
    panic_poll: bool,
    panic_drop: bool,
    _pinned: PhantomPinned,
}
impl Future for Probe {
    type Output = u32;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u32> {
        self.events
            .lock()
            .unwrap()
            .push(("poll", TraceContext::current()));
        trace::in_scope(None, || assert_eq!(TraceContext::current(), None));
        assert!(!self.panic_poll, "deliberate poll panic");
        if self.ready {
            Poll::Ready(73)
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.events
            .lock()
            .unwrap()
            .push(("drop", TraceContext::current()));
        assert!(!self.panic_drop, "deliberate drop panic");
    }
}
fn probe(events: &Events, ready: bool, panic_poll: bool, panic_drop: bool) -> Probe {
    Probe {
        events: events.clone(),
        ready,
        panic_poll,
        panic_drop,
        _pinned: PhantomPinned,
    }
}

#[test]
fn nested_scopes_restore_some_and_none_after_unwind() {
    let outer = TraceContext::root();
    let inner = outer.child();
    outer.in_scope(|| {
        for context in [Some(inner.clone()), None] {
            assert!(catch_unwind(AssertUnwindSafe(|| trace::in_scope(
                context.clone(),
                || {
                    assert_eq!(TraceContext::current(), context);
                    inner.in_scope(|| assert_eq!(TraceContext::current(), Some(inner.clone())));
                    assert_eq!(TraceContext::current(), context);
                    panic!("deliberate nested unwind");
                }
            )))
            .is_err());
            assert_eq!(TraceContext::current(), Some(outer.clone()));
        }
    });
    assert_eq!(TraceContext::current(), None);
}

#[test]
fn pinned_future_migrates_without_leaking_context_into_either_poller() {
    for context in [Some(TraceContext::root()), None] {
        let events = Events::default();
        let mut future = Box::pin(trace::scope(
            context.clone(),
            probe(&events, false, false, false),
        ));
        for _ in 0..3 {
            future = std::thread::spawn(move || {
                let foreign = TraceContext::root();
                foreign.in_scope(|| {
                    assert!(future
                        .as_mut()
                        .poll(&mut Context::from_waker(noop_waker_ref()))
                        .is_pending());
                    assert_eq!(TraceContext::current(), Some(foreign.clone()));
                });
                assert_eq!(TraceContext::current(), None);
                future
            })
            .join()
            .unwrap();
        }
        let foreign = TraceContext::root();
        foreign.in_scope(|| {
            drop(future);
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
        });
        assert_eq!(
            snapshot(&events),
            vec![
                ("poll", context.clone()),
                ("poll", context.clone()),
                ("poll", context.clone()),
                ("drop", context)
            ]
        );
    }
}

#[test]
fn ready_future_disposes_once_before_restoring_caller() {
    for context in [Some(TraceContext::root()), None] {
        let events = Events::default();
        let mut future = Box::pin(trace::scope(
            context.clone(),
            probe(&events, true, false, false),
        ));
        let foreign = TraceContext::root();
        foreign.in_scope(|| {
            assert_eq!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(noop_waker_ref())),
                Poll::Ready(73)
            );
            assert_eq!(
                snapshot(&events),
                vec![("poll", context.clone()), ("drop", context.clone())]
            );
            assert!(catch_unwind(AssertUnwindSafe(|| future
                .as_mut()
                .poll(&mut Context::from_waker(noop_waker_ref()))))
            .is_err());
            drop(future);
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
        });
        assert_eq!(snapshot(&events).len(), 2);
    }
}

#[test]
fn unpolled_and_cancelled_futures_dispose_under_owned_context() {
    for context in [Some(TraceContext::root()), None] {
        for polled in [false, true] {
            let events = Events::default();
            let mut future = Box::pin(trace::scope(
                context.clone(),
                probe(&events, false, false, false),
            ));
            let foreign = TraceContext::root();
            foreign.in_scope(|| {
                if polled {
                    assert!(future
                        .as_mut()
                        .poll(&mut Context::from_waker(noop_waker_ref()))
                        .is_pending());
                }
                drop(future);
                assert_eq!(TraceContext::current(), Some(foreign.clone()));
            });
            let expected = if polled {
                vec![("poll", context.clone()), ("drop", context.clone())]
            } else {
                vec![("drop", context.clone())]
            };
            assert_eq!(snapshot(&events), expected);
        }
    }
}

#[test]
fn poll_and_separate_disposal_panics_restore_caller() {
    for context in [Some(TraceContext::root()), None] {
        let events = Events::default();
        let mut future = Box::pin(trace::scope(
            context.clone(),
            probe(&events, false, true, true),
        ));
        let foreign = TraceContext::root();
        foreign.in_scope(|| {
            assert!(catch_unwind(AssertUnwindSafe(|| future
                .as_mut()
                .poll(&mut Context::from_waker(noop_waker_ref()))))
            .is_err());
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
            assert!(catch_unwind(AssertUnwindSafe(|| drop(future))).is_err());
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
        });
        assert_eq!(
            snapshot(&events),
            vec![("poll", context.clone()), ("drop", context)]
        );
    }
}

#[test]
fn capture_uses_construction_context_and_preserves_local_non_send_futures() {
    let parent = TraceContext::root();
    let foreign = TraceContext::root();
    for context in [Some(parent), None] {
        let owned = std::rc::Rc::new(37);
        let expected = context.clone();
        let future = trace::in_scope(context, || {
            trace::capture(async move {
                assert_eq!(TraceContext::current(), expected);
                *owned
            })
        });
        foreign.in_scope(|| {
            assert_eq!(futures::executor::block_on(future), 37);
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
        });
    }
}
