use super::*;
use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
};

struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Self(self.0.clone()));
    }
}
struct Subscriber {
    events: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl tracing::Subscriber for Subscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN && metadata.target().starts_with("parrot::")
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Payload(self.payloads.clone()));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
struct Wake {
    calls: Arc<AtomicUsize>,
    panic_payloads: Option<Arc<AtomicUsize>>,
}
impl futures::task::ArcWake for Wake {
    fn wake_by_ref(this: &Arc<Self>) {
        this.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(payloads) = &this.panic_payloads {
            std::panic::panic_any(Payload(payloads.clone()));
        }
    }
}
struct OnDrop<F: FnOnce()>(Option<F>);
impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        self.0.take().unwrap()();
    }
}
fn observer<T: Clone + 'static>(
    mut receipt: watch::Receiver<Option<T>>,
) -> Pin<Box<dyn Future<Output = T>>> {
    Box::pin(async move {
        receipt.changed().await.unwrap();
        receipt.borrow().clone().unwrap()
    })
}

pub(crate) fn verify<T: Clone + 'static>(
    sender: &watch::Sender<Option<T>>,
    during_unwind: bool,
    expected_events: usize,
    publish: impl FnOnce(),
    inspect: impl Fn(&T),
) {
    // Only called inside an isolated child; recursive-panic backtraces are not
    // part of the contract and can exhaust the parent's completion deadline.
    std::panic::set_hook(Box::new(|_| {}));
    let events = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(Subscriber {
        events: events.clone(),
        payloads: payloads.clone(),
    })
    .unwrap();
    let hostile_calls = Arc::new(AtomicUsize::new(0));
    let hostile_payloads = Arc::new(AtomicUsize::new(0));
    let hostile_waker = futures::task::waker(Arc::new(Wake {
        calls: hostile_calls.clone(),
        panic_payloads: Some(hostile_payloads.clone()),
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let normal_waker = futures::task::waker(Arc::new(Wake {
        calls: calls.clone(),
        panic_payloads: None,
    }));
    let mut hostile = observer(sender.subscribe());
    assert!(
        hostile
            .as_mut()
            .poll(&mut Context::from_waker(&hostile_waker))
            .is_pending()
    );
    let mut observers: Vec<_> = (0..64).map(|_| observer(sender.subscribe())).collect();
    for receipt in &mut observers {
        assert!(
            receipt
                .as_mut()
                .poll(&mut Context::from_waker(&normal_waker))
                .is_pending()
        );
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        if during_unwind {
            let _cleanup = OnDrop(Some(publish));
            panic!("original receipt cleanup unwind");
        } else {
            publish();
        }
    }))
    .map_err(panic_message);
    if during_unwind {
        assert_eq!(result.unwrap_err(), "original receipt cleanup unwind");
    } else {
        assert!(result.is_ok(), "diagnostic escaped cleanup: {result:?}");
    }
    assert_eq!(events.load(Ordering::SeqCst), expected_events);
    assert_eq!(payloads.load(Ordering::SeqCst), expected_events);
    assert_eq!(hostile_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hostile_payloads.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 64);
    inspect(sender.borrow().as_ref().expect("terminal receipt lost"));
    for receipt in observers.iter_mut().chain(std::iter::once(&mut hostile)) {
        match receipt
            .as_mut()
            .poll(&mut Context::from_waker(&normal_waker))
        {
            Poll::Ready(value) => inspect(&value),
            Poll::Pending => panic!("a completed observer remained pending"),
        }
    }
}

macro_rules! contract {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            let name = concat!(module_path!(), "::", stringify!($name));
            let name = name.strip_prefix("parrot::").unwrap();
            crate::test_support::isolated(name, || $body);
        }
    };
}
pub(crate) use contract;

fn check_value(modified: bool, during_unwind: bool) {
    let (sender, _) = watch::channel(None);
    let expected: Result<(), String> = Err("original terminal failure".into());
    verify(
        &sender,
        during_unwind,
        1,
        || {
            if modified {
                publish_watch_if_modified(&sender, |slot| {
                    *slot = Some(expected.clone());
                    true
                });
            } else {
                publish_watch_value(&sender, Some(expected.clone()));
            }
        },
        |value| assert_eq!(value, &expected),
    );
}
fn check_disposal(during_unwind: bool) {
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("original input disposal failure");
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let input = OwnedPayload::unbound(Counted(drops.clone()));
    let sender = input.completion().clone();
    verify(
        &sender,
        during_unwind,
        1,
        || drop(input),
        |value| {
            assert!(
                value
                    .as_ref()
                    .unwrap_err()
                    .contains("original input disposal failure")
            );
        },
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
contract!(receipt_diagnostic_value, check_value(false, false));
contract!(receipt_diagnostic_value_unwind, check_value(false, true));
contract!(receipt_diagnostic_modified, check_value(true, false));
contract!(receipt_diagnostic_modified_unwind, check_value(true, true));
contract!(receipt_diagnostic_owned_disposal, check_disposal(false));
contract!(
    receipt_diagnostic_owned_disposal_unwind,
    check_disposal(true)
);
