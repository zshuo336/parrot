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
use tokio::sync::{watch, Notify, Semaphore};

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

fn semaphore_release(count: usize, owned: bool) {
    let semaphore = Arc::new(Semaphore::new(if owned { count * 3 + 11 } else { 0 }));
    let held = owned.then(|| {
        semaphore
            .clone()
            .try_acquire_many_owned((count * 3 + 11) as u32)
            .unwrap()
    });
    let observers = observers(count);
    let mut pending: Vec<_> = observers
        .iter()
        .map(|observer| {
            let mut future = Box::pin(semaphore.clone().acquire_many_owned(3));
            register(&mut future, observer);
            future
        })
        .collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if owned {
            drop(held);
        } else {
            semaphore.add_permits(count * 3 + 11);
        }
    }));
    assert!(result.is_err());
    assert_eq!(
        semaphore.available_permits(),
        11,
        "unassigned permits were lost after wake panic"
    );
    let mut acquired = Vec::new();
    for (future, observer) in pending.iter_mut().zip(&observers) {
        assert_eq!(
            observer.calls.load(Ordering::SeqCst),
            1,
            "semaphore waiter lost its wake"
        );
        match future.as_mut().poll(&mut Context::from_waker(&noop())) {
            std::task::Poll::Ready(Ok(permit)) => acquired.push(permit),
            other => panic!("assigned semaphore waiter did not complete: {other:?}"),
        }
    }
    drop(acquired);
    assert_eq!(semaphore.available_permits(), count * 3 + 11);
}

#[test]
fn semaphore_add_permits_finishes_every_batch_and_preserves_excess() {
    semaphore_release(96, false);
}

#[test]
fn semaphore_owned_permit_drop_finishes_every_batch_and_preserves_excess() {
    semaphore_release(96, true);
}

#[test]
fn semaphore_exact_batch_release_keeps_all_assigned_permits() {
    semaphore_release(32, false);
}

fn semaphore_close(cancel: bool) {
    let semaphore = Semaphore::new(0);
    let observers = observers(192);
    let mut pending: Vec<_> = observers
        .iter()
        .enumerate()
        .map(|(index, observer)| {
            let mut future = Box::pin(semaphore.acquire());
            register(&mut future, observer);
            if cancel && index % 3 == 0 {
                None
            } else {
                Some(future)
            }
        })
        .collect();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| semaphore.close())).is_err());
    assert!(semaphore.is_closed());
    for (index, (future, observer)) in pending.iter_mut().zip(&observers).enumerate() {
        assert_eq!(
            observer.calls.load(Ordering::SeqCst),
            usize::from(!cancel || index % 3 != 0),
            "closed semaphore failed to wake an independent waiter"
        );
        if let Some(future) = future {
            assert!(matches!(
                future.as_mut().poll(&mut Context::from_waker(&noop())),
                std::task::Poll::Ready(Err(_))
            ));
        }
    }
    semaphore.close();
    assert!(matches!(
        semaphore.try_acquire(),
        Err(tokio::sync::TryAcquireError::Closed)
    ));
}

#[test]
fn semaphore_close_notifies_every_batch_despite_a_panicking_waker() {
    semaphore_close(false);
}

#[test]
fn semaphore_close_after_cancellation_notifies_all_remaining_waiters() {
    semaphore_close(true);
}

#[test]
fn semaphore_cancelled_partial_acquisition_returns_every_assigned_permit() {
    let semaphore = Semaphore::new(0);
    let mut head = Box::pin(semaphore.acquire_many(128));
    assert!(head
        .as_mut()
        .poll(&mut Context::from_waker(&noop()))
        .is_pending());
    let observers = observers(96);
    let mut pending: Vec<_> = observers
        .iter()
        .map(|observer| {
            let mut future = Box::pin(semaphore.acquire());
            register(&mut future, observer);
            future
        })
        .collect();
    semaphore.add_permits(96);
    assert_eq!(semaphore.available_permits(), 0);
    assert!(observers
        .iter()
        .all(|observer| observer.calls.load(Ordering::SeqCst) == 0));
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(head))).is_err());
    let mut acquired = Vec::new();
    for (future, observer) in pending.iter_mut().zip(&observers) {
        assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
        match future.as_mut().poll(&mut Context::from_waker(&noop())) {
            std::task::Poll::Ready(Ok(permit)) => acquired.push(permit),
            other => panic!("cancelled head lost assigned permits: {other:?}"),
        }
    }
    drop(acquired);
    assert_eq!(semaphore.available_permits(), 96);
}

#[test]
fn semaphore_reentrant_acquisition_stays_behind_existing_waiters() {
    type Acquisition = Pin<
        Box<
            dyn Future<
                    Output = Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError>,
                > + Send,
        >,
    >;
    struct Reenter {
        semaphore: Arc<Semaphore>,
        next: Mutex<Option<Acquisition>>,
        stayed_pending: AtomicBool,
    }
    impl Wake for Reenter {
        fn wake(self: Arc<Self>) {
            let mut next: Acquisition = Box::pin(self.semaphore.clone().acquire_owned());
            self.stayed_pending.store(
                next.as_mut()
                    .poll(&mut Context::from_waker(&noop()))
                    .is_pending(),
                Ordering::SeqCst,
            );
            *self.next.lock().unwrap() = Some(next);
            panic!("reentrant semaphore wake witness");
        }
    }
    let semaphore = Arc::new(Semaphore::new(0));
    let reenter = Arc::new(Reenter {
        semaphore: semaphore.clone(),
        next: Mutex::new(None),
        stayed_pending: AtomicBool::new(false),
    });
    let mut head = Box::pin(semaphore.clone().acquire_owned());
    assert!(head
        .as_mut()
        .poll(&mut Context::from_waker(&Waker::from(reenter.clone())))
        .is_pending());
    let observers = observers(96);
    // This case has one hostile reentrant waker; ordinary followers never panic.
    observers[0].first.store(true, Ordering::SeqCst);
    let mut pending: Vec<_> = observers
        .iter()
        .map(|observer| {
            let mut future = Box::pin(semaphore.clone().acquire_owned());
            register(&mut future, observer);
            future
        })
        .collect();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| semaphore.add_permits(97)))
            .is_err()
    );
    assert!(reenter.stayed_pending.load(Ordering::SeqCst));
    let mut next = reenter.next.lock().unwrap().take().unwrap();
    assert!(next
        .as_mut()
        .poll(&mut Context::from_waker(&noop()))
        .is_pending());
    let mut acquired = Vec::new();
    for future in std::iter::once(&mut head).chain(pending.iter_mut()) {
        match future.as_mut().poll(&mut Context::from_waker(&noop())) {
            std::task::Poll::Ready(Ok(permit)) => acquired.push(permit),
            other => panic!("reentrant waiter bypassed earlier queued work: {other:?}"),
        }
    }
    assert!(observers
        .iter()
        .all(|observer| observer.calls.load(Ordering::SeqCst) == 1));
    assert_eq!(semaphore.available_permits(), 0);
    drop(acquired);
    let permit = match next.as_mut().poll(&mut Context::from_waker(&noop())) {
        std::task::Poll::Ready(Ok(permit)) => permit,
        other => panic!("reentrant acquisition never received returned capacity: {other:?}"),
    };
    drop(permit);
    assert_eq!(semaphore.available_permits(), 97);
}

#[test]
fn semaphore_preserves_first_panic_across_batches_with_hostile_payloads() {
    struct Payload {
        id: usize,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            panic!("semaphore panic payload disposal witness");
        }
    }
    struct Hostile {
        id: usize,
        drops: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }
    impl Wake for Hostile {
        fn wake(self: Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(Payload {
                id: self.id,
                drops: self.drops.clone(),
            });
        }
    }
    let semaphore = Semaphore::new(0);
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut pending: Vec<_> = (0..96)
        .map(|id| {
            let mut future = Box::pin(semaphore.acquire());
            let waker = Waker::from(Arc::new(Hostile {
                id,
                drops: drops.clone(),
                calls: calls.clone(),
            }));
            assert!(future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            future
        })
        .collect();
    let payload =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| semaphore.close())).unwrap_err();
    let id = payload.downcast_ref::<Payload>().map(|payload| payload.id);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))).is_err());
    assert_eq!(id, Some(0));
    assert_eq!(calls.load(Ordering::SeqCst), 96);
    assert_eq!(drops.load(Ordering::SeqCst), 96);
    for future in &mut pending {
        assert!(matches!(
            future.as_mut().poll(&mut Context::from_waker(&noop())),
            std::task::Poll::Ready(Err(_))
        ));
    }
    assert_eq!(semaphore.available_permits(), 0);
}
