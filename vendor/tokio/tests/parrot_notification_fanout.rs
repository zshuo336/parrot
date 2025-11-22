#![cfg(panic = "unwind")]

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Wake, Waker},
};
use tokio::sync::{watch, Notify};

struct Noop;
impl Wake for Noop {
    fn wake(self: Arc<Self>) {}
}

fn noop() -> Waker {
    Waker::from(Arc::new(Noop))
}

struct Observer {
    first: Arc<AtomicBool>,
    calls: AtomicUsize,
}
impl Wake for Observer {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.first.swap(true, Ordering::SeqCst),
            "first wake panic witness"
        );
    }
}

fn observers(count: usize) -> Vec<Arc<Observer>> {
    let first = Arc::new(AtomicBool::new(false));
    (0..count)
        .map(|_| {
            Arc::new(Observer {
                first: first.clone(),
                calls: AtomicUsize::new(0),
            })
        })
        .collect()
}

fn register<F: Future>(future: &mut Pin<Box<F>>, observer: &Arc<Observer>) {
    let waker = Waker::from(observer.clone());
    assert!(future
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
}

#[test]
fn notify_fanout_finishes_every_batch_before_propagating_the_first_panic() {
    let notify = Notify::new();
    let observers = observers(256);
    let mut pending: Vec<_> = observers
        .iter()
        .map(|observer| {
            let mut future = Box::pin(notify.notified());
            register(&mut future, observer);
            future
        })
        .collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify.notify_waiters()));
    assert!(result.is_err());
    for observer in &observers {
        assert_eq!(
            observer.calls.load(Ordering::SeqCst),
            1,
            "an independent waiter lost its wake"
        );
    }
    for future in &mut pending {
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(&noop()))
            .is_ready());
    }
}

#[test]
fn notify_cancellation_does_not_lose_the_remaining_waiters() {
    let notify = Notify::new();
    let observers = observers(256);
    let mut pending: Vec<_> = observers
        .iter()
        .enumerate()
        .map(|(index, observer)| {
            let mut future = Box::pin(notify.notified());
            register(&mut future, observer);
            if index % 3 == 0 {
                None
            } else {
                Some(future)
            }
        })
        .collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify.notify_waiters()));
    assert!(result.is_err());
    for (index, observer) in observers.iter().enumerate() {
        assert_eq!(
            observer.calls.load(Ordering::SeqCst),
            usize::from(index % 3 != 0)
        );
    }
    for future in pending.iter_mut().flatten() {
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(&noop()))
            .is_ready());
    }
}

#[test]
fn notify_preserves_the_first_payload_when_later_payload_destructors_also_panic() {
    struct Payload {
        id: usize,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            panic!("secondary panic payload disposal witness");
        }
    }
    struct Hostile {
        id: usize,
        calls: Arc<Mutex<Vec<usize>>>,
        drops: Arc<AtomicUsize>,
    }
    impl Wake for Hostile {
        fn wake(self: Arc<Self>) {
            self.calls.lock().unwrap().push(self.id);
            std::panic::panic_any(Payload {
                id: self.id,
                drops: self.drops.clone(),
            });
        }
    }
    let notify = Notify::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let pending: Vec<_> = (0..96)
        .map(|id| {
            let waker = Waker::from(Arc::new(Hostile {
                id,
                calls: calls.clone(),
                drops: drops.clone(),
            }));
            let mut future = Box::pin(notify.notified());
            assert!(future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            future
        })
        .collect();
    let payload =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify.notify_waiters()))
            .unwrap_err();
    let id = payload.downcast_ref::<Payload>().map(|payload| payload.id);
    // Dispose the hostile payload before asserting, including on the old-code counterexample.
    let disposed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)));
    assert!(disposed.is_err());
    assert_eq!(id, calls.lock().unwrap().first().copied());
    assert_eq!(calls.lock().unwrap().len(), pending.len());
    assert_eq!(drops.load(Ordering::SeqCst), pending.len());
}

#[test]
fn reentrant_notify_registration_waits_for_the_next_generation() {
    struct Reenter {
        notify: Arc<Notify>,
        observer: Arc<Observer>,
        next: Mutex<Option<Pin<Box<tokio::sync::futures::OwnedNotified>>>>,
    }
    impl Wake for Reenter {
        fn wake(self: Arc<Self>) {
            let mut next = Box::pin(self.notify.clone().notified_owned());
            register(&mut next, &self.observer);
            *self.next.lock().unwrap() = Some(next);
            panic!("reentrant wake panic witness");
        }
    }
    let notify = Arc::new(Notify::new());
    let observer = Arc::new(Observer {
        first: Arc::new(AtomicBool::new(true)),
        calls: AtomicUsize::new(0),
    });
    let reenter = Arc::new(Reenter {
        notify: notify.clone(),
        observer: observer.clone(),
        next: Mutex::new(None),
    });
    let waker = Waker::from(reenter.clone());
    let mut first = Box::pin(notify.notified());
    assert!(first
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify.notify_waiters())).is_err()
    );
    assert_eq!(observer.calls.load(Ordering::SeqCst), 0);
    notify.notify_waiters();
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    let mut next = reenter.next.lock().unwrap().take().unwrap();
    assert!(next
        .as_mut()
        .poll(&mut Context::from_waker(&noop()))
        .is_ready());
}

fn watch_fanout(mode: u8) {
    let (sender, receiver) = watch::channel(None::<u8>);
    let observers = observers(1024);
    let mut pending: Vec<_> = observers
        .iter()
        .map(|observer| {
            let mut receiver = receiver.clone();
            let mut future = Box::pin(async move { receiver.changed().await });
            register(&mut future, observer);
            future
        })
        .collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match mode {
        0 => {
            sender.send_replace(Some(7));
        }
        1 => {
            sender.send_if_modified(|value| {
                *value = Some(7);
                true
            });
        }
        2 => {
            drop(sender);
        }
        _ => unreachable!(),
    }));
    assert!(result.is_err());
    assert_eq!(*receiver.borrow(), if mode == 2 { None } else { Some(7) });
    for observer in &observers {
        assert_eq!(
            observer.calls.load(Ordering::SeqCst),
            1,
            "a watch shard lost an independent waiter"
        );
    }
    for future in &mut pending {
        match future.as_mut().poll(&mut Context::from_waker(&noop())) {
            std::task::Poll::Ready(result) => assert_eq!(result.is_err(), mode == 2),
            std::task::Poll::Pending => panic!("notified watch observer did not complete"),
        }
    }
}

#[test]
fn watch_send_replace_notifies_all_shards_despite_a_panicking_waker() {
    watch_fanout(0);
}
#[test]
fn watch_send_if_modified_notifies_all_shards_despite_a_panicking_waker() {
    watch_fanout(1);
}
#[test]
fn watch_sender_drop_notifies_all_shards_despite_a_panicking_waker() {
    watch_fanout(2);
}

#[test]
fn watch_unchanged_update_remains_silent_and_the_next_update_notifies_everyone() {
    let (sender, receiver) = watch::channel(None::<u8>);
    let observers = observers(128);
    let mut pending: Vec<_> = observers
        .iter()
        .map(|observer| {
            let mut receiver = receiver.clone();
            let mut future = Box::pin(async move { receiver.changed().await });
            register(&mut future, observer);
            future
        })
        .collect();
    assert!(!sender.send_if_modified(|_| false));
    assert!(observers
        .iter()
        .all(|observer| observer.calls.load(Ordering::SeqCst) == 0));
    assert!(std::panic::catch_unwind(
        std::panic::AssertUnwindSafe(|| sender.send_replace(Some(7)))
    )
    .is_err());
    for observer in &observers {
        assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    }
    for future in &mut pending {
        assert!(matches!(
            future.as_mut().poll(&mut Context::from_waker(&noop())),
            std::task::Poll::Ready(Ok(()))
        ));
    }
}
