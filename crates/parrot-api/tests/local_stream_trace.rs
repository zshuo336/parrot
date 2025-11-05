//! Registry ownership remains bound to registration across pollers and disposal.
use futures::{task::noop_waker_ref, Stream};
use parrot_api::{
    local_typed::{LocalStreamEventKind, LocalStreamRegistry},
    trace::{self, TraceContext},
};
use std::{
    cell::RefCell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

type Events = Rc<RefCell<Vec<(&'static str, Option<TraceContext>)>>>;
struct Source {
    events: Events,
    mode: u8,
}
impl Stream for Source {
    type Item = Rc<()>;
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.events
            .borrow_mut()
            .push(("poll", TraceContext::current()));
        match self.mode {
            1 => Poll::Pending,
            2 => panic!("local source poll failed"),
            _ => Poll::Ready(None),
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.events
            .borrow_mut()
            .push(("drop", TraceContext::current()));
    }
}
fn drive(mode: u8) {
    for traced in [true, false] {
        let parent = traced.then(TraceContext::root);
        let foreign = TraceContext::root();
        let events: Events = Rc::default();
        let mut registry = LocalStreamRegistry::new();
        trace::in_scope(parent.clone(), || {
            registry
                .add_stream(Source {
                    events: events.clone(),
                    mode,
                })
                .unwrap()
        });
        foreign.in_scope(|| {
            let started = futures::executor::block_on(registry.next_event()).unwrap();
            assert_eq!(started.kind(), LocalStreamEventKind::Started);
            assert_eq!(started.trace_context(), parent);
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
            started.complete(&mut registry, true);
            let mut next = registry.next_event();
            let event = next
                .as_mut()
                .poll(&mut Context::from_waker(noop_waker_ref()));
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
            drop(next);
            match event {
                Poll::Pending => assert_eq!(mode, 1),
                Poll::Ready(Some(event)) => {
                    assert_eq!(event.trace_context(), parent);
                    assert_eq!(
                        event.kind(),
                        if mode == 2 {
                            LocalStreamEventKind::Error
                        } else {
                            LocalStreamEventKind::Finished
                        }
                    );
                    event.complete(&mut registry, false);
                }
                Poll::Ready(None) => panic!("registered source disappeared"),
            }
            drop(registry);
            assert_eq!(TraceContext::current(), Some(foreign.clone()));
        });
        assert!(TraceContext::current().is_none());
        let events = events.borrow();
        assert_eq!(
            events.iter().map(|e| e.0).collect::<Vec<_>>(),
            ["poll", "drop"]
        );
        for (_, context) in events.iter() {
            assert_eq!(*context, parent);
        }
    }
}
#[test]
fn completed_source_keeps_registration_identity_under_foreign_poller() {
    drive(0);
}
#[test]
fn pending_source_disposal_keeps_registration_identity_under_foreign_poller() {
    drive(1);
}
#[test]
fn panicking_source_retains_registration_identity_on_error_event() {
    drive(2);
}

#[test]
fn unpolled_started_event_disposes_source_under_registration_identity() {
    for traced in [true, false] {
        let parent = traced.then(TraceContext::root);
        let foreign = TraceContext::root();
        let events: Events = Rc::default();
        let mut registry = LocalStreamRegistry::new();
        trace::in_scope(parent.clone(), || {
            registry
                .add_stream(Source {
                    events: events.clone(),
                    mode: 0,
                })
                .unwrap()
        });
        let event =
            foreign.in_scope(|| futures::executor::block_on(registry.next_event()).unwrap());
        foreign.in_scope(|| drop(event));
        assert_eq!(*events.borrow(), vec![("drop", parent)]);
        assert!(registry.is_empty());
    }
}

#[test]
fn interleaved_sources_retain_their_independent_registration_identities() {
    let foreign = TraceContext::root();
    let parents = [Some(TraceContext::root()), None, Some(TraceContext::root())];
    let mut registry = LocalStreamRegistry::new();
    let events: Vec<Events> = (0..3).map(|_| Rc::default()).collect();
    for (parent, events) in parents.iter().zip(&events) {
        trace::in_scope(parent.clone(), || {
            registry
                .add_stream(Source {
                    events: events.clone(),
                    mode: 0,
                })
                .unwrap()
        });
    }
    foreign.in_scope(|| {
        for parent in &parents {
            let event = futures::executor::block_on(registry.next_event()).unwrap();
            assert_eq!(event.kind(), LocalStreamEventKind::Started);
            assert_eq!(&event.trace_context(), parent);
            event.complete(&mut registry, true);
        }
        for parent in &parents {
            let event = futures::executor::block_on(registry.next_event()).unwrap();
            assert_eq!(event.kind(), LocalStreamEventKind::Finished);
            assert_eq!(&event.trace_context(), parent);
            event.complete(&mut registry, false);
        }
    });
    for (parent, events) in parents.iter().zip(events) {
        assert_eq!(
            *events.borrow(),
            vec![("poll", parent.clone()), ("drop", parent.clone())]
        );
    }
    assert!(registry.is_empty());
}
