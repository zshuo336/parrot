//! User destructors must not escape into another source's unwind boundary.
use futures::Stream;
use parrot_api::{
    errors::ActorError,
    local_typed::{LocalStreamEventKind, LocalStreamRegistry},
    trace::{self, TraceContext},
};
use std::{
    cell::RefCell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
type Observed = Rc<RefCell<Vec<(&'static str, Option<TraceContext>)>>>;
#[derive(Clone, Copy)]
enum Failure {
    None,
    Plain,
    Payload,
}
struct Payload;
impl Drop for Payload {
    fn drop(&mut self) {
        panic!("payload destructor");
    }
}
struct Item(Observed);
impl Drop for Item {
    fn drop(&mut self) {
        self.0.borrow_mut().push(("item", TraceContext::current()));
        panic!("item destructor");
    }
}
struct Source {
    seen: Observed,
    name: &'static str,
    failure: Failure,
    poll_payload: bool,
    item: bool,
}
impl Stream for Source {
    type Item = Item;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Item>> {
        if self.poll_payload {
            std::panic::panic_any(Payload);
        }
        if self.item {
            self.item = false;
            Poll::Ready(Some(Item(self.seen.clone())))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.seen
            .borrow_mut()
            .push((self.name, TraceContext::current()));
        match self.failure {
            Failure::None => (),
            Failure::Plain => panic!("source destructor {}", self.name),
            Failure::Payload => std::panic::panic_any(Payload),
        }
    }
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_LOCAL_RETIREMENT_CASE")
        .ok()
        .as_deref()
        == Some(name)
    {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_LOCAL_RETIREMENT_CASE", name)
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "retirement child {name}: {status}");
            return;
        }
        if Instant::now() >= end {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("retirement deadlocked: {name}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn message(result: std::thread::Result<()>) -> String {
    let payload = result.expect_err("cleanup failure must be reported");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .expect("sanitized panic")
}
fn multiple(unwinding: bool, hostile: bool) {
    let seen = Observed::default();
    let root = TraceContext::root();
    let ambient = TraceContext::root();
    let mut registry = LocalStreamRegistry::new();
    for (name, failure, parent) in [
        (
            "first",
            if hostile {
                Failure::Payload
            } else {
                Failure::Plain
            },
            Some(root.clone()),
        ),
        ("second", Failure::Plain, None),
        ("third", Failure::None, Some(root.clone())),
    ] {
        trace::in_scope(parent, || {
            registry
                .add_stream(Source {
                    seen: seen.clone(),
                    name,
                    failure,
                    poll_payload: false,
                    item: false,
                })
                .unwrap()
        });
    }
    ambient.in_scope(|| {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _registry = registry;
            if unwinding {
                panic!("original caller panic");
            }
        }));
        let text = message(result);
        if unwinding {
            assert_eq!(text, "original caller panic");
        } else if hostile {
            assert!(text.contains("panic payload destructor panicked"), "{text}");
        } else {
            assert_eq!(text, "source destructor first");
        }
        assert_eq!(TraceContext::current(), Some(ambient.clone()));
    });
    assert_eq!(
        *seen.borrow(),
        vec![
            ("first", Some(root.clone())),
            ("second", None),
            ("third", Some(root))
        ]
    );
}
#[test]
fn multiple_throwing_sources_report_first_after_all_retire() {
    isolated(
        "multiple_throwing_sources_report_first_after_all_retire",
        || multiple(false, false),
    );
}
#[test]
fn multiple_sources_during_unwind_preserve_original() {
    isolated("multiple_sources_during_unwind_preserve_original", || {
        multiple(true, false)
    });
}
#[test]
fn hostile_drop_payload_is_sanitized() {
    isolated("hostile_drop_payload_is_sanitized", || {
        multiple(false, true)
    });
}
#[test]
fn hostile_payload_during_unwind_is_contained() {
    isolated("hostile_payload_during_unwind_is_contained", || {
        multiple(true, true)
    });
}
#[test]
fn poll_payload_failure_remains_an_error_event() {
    isolated("poll_payload_failure_remains_an_error_event", || {
        let seen = Observed::default();
        let root = TraceContext::root();
        let mut registry = LocalStreamRegistry::new();
        root.in_scope(|| {
            registry
                .add_stream(Source {
                    seen: seen.clone(),
                    name: "source",
                    failure: Failure::Plain,
                    poll_payload: true,
                    item: false,
                })
                .unwrap()
        });
        futures::executor::block_on(registry.next_event())
            .unwrap()
            .complete(&mut registry, true);
        let mut event = futures::executor::block_on(registry.next_event()).unwrap();
        assert_eq!(event.kind(), LocalStreamEventKind::Error);
        assert!(
            matches!(event.take_error(),Some(ActorError::Panic(text)) if text.contains("panic payload destructor panicked"))
        );
        assert_eq!(
            message(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || drop(event)
            ))),
            "source destructor source"
        );
        assert!(registry.is_empty());
        assert_eq!(*seen.borrow(), vec![("source", Some(root))]);
    });
}
fn discarded_item(complete: bool) {
    let seen = Observed::default();
    let root = TraceContext::root();
    let mut registry = LocalStreamRegistry::new();
    root.in_scope(|| {
        registry
            .add_stream(Source {
                seen: seen.clone(),
                name: "source",
                failure: Failure::Payload,
                poll_payload: false,
                item: true,
            })
            .unwrap()
    });
    futures::executor::block_on(registry.next_event())
        .unwrap()
        .complete(&mut registry, true);
    let event = futures::executor::block_on(registry.next_event()).unwrap();
    assert_eq!(event.kind(), LocalStreamEventKind::Item);
    let text = message(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || {
            if complete {
                event.complete(&mut registry, false)
            } else {
                drop(event)
            }
        },
    )));
    assert_eq!(text, "item destructor");
    assert_eq!(*seen.borrow(), vec![("item", None), ("source", Some(root))]);
    assert!(registry.is_empty());
}
#[test]
fn abandoned_item_and_source_retire_independently() {
    isolated("abandoned_item_and_source_retire_independently", || {
        discarded_item(false)
    });
}
#[test]
fn completed_item_and_source_retire_independently() {
    isolated("completed_item_and_source_retire_independently", || {
        discarded_item(true)
    });
}
#[test]
fn successful_sources_do_not_invent_failure() {
    isolated("successful_sources_do_not_invent_failure", || {
        let seen = Observed::default();
        let mut registry = LocalStreamRegistry::new();
        for name in ["a", "b"] {
            registry
                .add_stream(Source {
                    seen: seen.clone(),
                    name,
                    failure: Failure::None,
                    poll_payload: false,
                    item: false,
                })
                .unwrap();
        }
        drop(registry);
        assert_eq!(*seen.borrow(), vec![("a", None), ("b", None)]);
    });
}
