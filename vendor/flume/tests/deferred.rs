//! Deferred notifications must preserve ordinary channel semantics while
//! allowing callbacks to reenter the committed queue.
#![cfg(feature = "async")]
use futures::{task::ArcWake, Future, Stream, StreamExt};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

struct Count(AtomicUsize);
impl ArcWake for Count {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn cancelled_selected_receiver_hands_notification_to_next_waiter() {
    let (tx, rx) = flume::bounded(1);
    let first_calls = Arc::new(Count(AtomicUsize::new(0)));
    let second_calls = Arc::new(Count(AtomicUsize::new(0)));
    let first_waker = futures::task::waker_ref(&first_calls);
    let second_waker = futures::task::waker_ref(&second_calls);
    let mut first = Box::pin(rx.recv_async());
    let mut second = Box::pin(rx.recv_async());
    assert!(first
        .as_mut()
        .poll(&mut Context::from_waker(&first_waker))
        .is_pending());
    assert!(second
        .as_mut()
        .poll(&mut Context::from_waker(&second_waker))
        .is_pending());
    let (result, notifications) = tx.try_send_deferred(42);
    result.unwrap();
    drop(first);
    drop(notifications);
    assert!(
        second_calls.0.load(Ordering::SeqCst) > 0,
        "a cancelled selected receiver must not strand the queued message"
    );
    assert_eq!(
        second
            .as_mut()
            .poll(&mut Context::from_waker(&second_waker)),
        Poll::Ready(Ok(42))
    );
}

#[test]
fn spurious_repoll_does_not_duplicate_receiver_registration() {
    let (tx, rx) = flume::bounded(1);
    let waker = futures::task::noop_waker();
    let mut receive = Box::pin(rx.recv_async());
    let mut cx = Context::from_waker(&waker);
    assert!(receive.as_mut().poll(&mut cx).is_pending());
    let (result, wakes) = tx.try_send_deferred(1);
    result.unwrap();
    // A competing consumer takes the message before this task runs.
    assert_eq!(rx.try_recv(), Ok(1));
    assert!(receive.as_mut().poll(&mut cx).is_pending());
    assert!(receive.as_mut().poll(&mut cx).is_pending());
    drop(wakes);
    tx.try_send(2).unwrap();
    assert_eq!(
        tx.try_send(3),
        Err(flume::TrySendError::Full(3)),
        "one task must not leave duplicate reservations after spurious polls"
    );
    assert_eq!(receive.as_mut().poll(&mut cx), Poll::Ready(Ok(2)));
    assert!(rx.try_recv().is_err());
}

#[test]
fn message_committed_during_waker_replacement_is_received() {
    let (tx, rx) = flume::bounded(1);
    let old_waker = futures::task::noop_waker();
    let mut receive = Box::pin(rx.recv_async());
    assert!(receive
        .as_mut()
        .poll(&mut Context::from_waker(&old_waker))
        .is_pending());
    tx.try_send(1).unwrap();
    assert_eq!(rx.try_recv(), Ok(1));
    // Replacing an already notified waker invokes the new waker. Commit
    // during that callback, between the initial empty read and rearming.
    let new_waker = waker_fn::waker_fn(move || tx.try_send(42).unwrap());
    assert_eq!(
        receive.as_mut().poll(&mut Context::from_waker(&new_waker)),
        Poll::Ready(Ok(42))
    );
    assert!(rx.try_recv().is_err());
}

// A broken lock boundary must fail a bounded child, never hang the test runner.
fn isolated(name: &str, scenario: impl FnOnce()) {
    if std::env::var("FLUME_REENTRANT_CHILD").as_deref() == Ok(name) {
        scenario();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("FLUME_REENTRANT_CHILD", name)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: child failed: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: reentrant callback deadlocked");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

struct Reenter {
    receiver: flume::Receiver<u64>,
    calls: AtomicUsize,
    consume: bool,
}
impl ArcWake for Reenter {
    fn wake_by_ref(this: &Arc<Self>) {
        if this.consume {
            assert_eq!(this.receiver.try_recv(), Ok(42));
        } else {
            let _ = this.receiver.len();
        }
        this.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn ordinary_send_callback_can_consume_committed_message() {
    isolated(
        "ordinary_send_callback_can_consume_committed_message",
        || {
            let (tx, rx) = flume::bounded(1);
            let observer = Arc::new(Reenter {
                receiver: rx.clone(),
                calls: AtomicUsize::new(0),
                consume: true,
            });
            let waker = futures::task::waker_ref(&observer);
            let mut future = Box::pin(rx.recv_async());
            assert!(future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            tx.try_send(42).unwrap();
            assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
            assert!(rx.try_recv().is_err());
        },
    );
}

#[test]
fn cancellation_handoff_callback_can_consume_committed_message() {
    isolated(
        "cancellation_handoff_callback_can_consume_committed_message",
        || {
            let (tx, rx) = flume::bounded(1);
            let observer = Arc::new(Reenter {
                receiver: rx.clone(),
                calls: AtomicUsize::new(0),
                consume: true,
            });
            let waker = futures::task::waker_ref(&observer);
            let quiet = futures::task::noop_waker();
            let mut first = Box::pin(rx.recv_async());
            let mut second = Box::pin(rx.recv_async());
            assert!(first
                .as_mut()
                .poll(&mut Context::from_waker(&quiet))
                .is_pending());
            assert!(second
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            let (result, wakes) = tx.try_send_deferred(42);
            result.unwrap();
            drop(first);
            drop(wakes);
            assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn pending_sender_callback_can_query_channel() {
    isolated("pending_sender_callback_can_query_channel", || {
        let (tx, rx) = flume::bounded(0);
        let observer = Arc::new(Reenter {
            receiver: rx.clone(),
            calls: AtomicUsize::new(0),
            consume: false,
        });
        let waker = futures::task::waker_ref(&observer);
        let mut send = Box::pin(tx.send_async(42));
        assert!(send
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        assert_eq!(rx.try_recv(), Ok(42));
        assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            send.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(()))
        );
    });
}

#[test]
fn disconnect_callback_can_query_channel() {
    isolated("disconnect_callback_can_query_channel", || {
        let (tx, rx) = flume::bounded(1);
        let observer = Arc::new(Reenter {
            receiver: rx.clone(),
            calls: AtomicUsize::new(0),
            consume: false,
        });
        let waker = futures::task::waker_ref(&observer);
        let mut receive = Box::pin(rx.recv_async());
        assert!(receive
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        drop(tx);
        assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            receive.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Err(flume::RecvError::Disconnected))
        );
    });
}

#[test]
fn notification_guard_defers_callback_until_after_commit() {
    let (tx, rx) = flume::bounded(1);
    let calls = Arc::new(Count(AtomicUsize::new(0)));
    let waker = futures::task::waker_ref(&calls);
    let mut receive = Box::pin(rx.recv_async());
    assert!(receive
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    let (result, wakes) = tx.try_send_deferred(42);
    result.unwrap();
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    assert_eq!(rx.len(), 1);
    drop(wakes);
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    assert_eq!(futures::executor::block_on(receive).unwrap(), 42);
}

#[test]
fn stream_waiters_are_all_notified_without_duplicate_delivery() {
    let (tx, rx) = flume::bounded(1);
    let calls = Arc::new(Count(AtomicUsize::new(0)));
    let waker = futures::task::waker_ref(&calls);
    let mut a = rx.stream();
    let mut b = rx.stream();
    let mut first = Box::pin(a.next());
    let mut second = Box::pin(b.next());
    for future in [&mut first, &mut second] {
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
    }
    let (result, wakes) = tx.try_send_deferred(42);
    result.unwrap();
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    drop(wakes);
    assert_eq!(calls.0.load(Ordering::SeqCst), 2);
    assert_eq!(
        first.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(Some(42))
    );
    assert!(second
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(tx);
    assert_eq!(futures::executor::block_on(second), None);
}

#[test]
fn full_and_disconnected_return_the_original_payload() {
    let (tx, rx) = flume::bounded(1);
    tx.try_send(1).unwrap();
    let (result, wakes) = tx.try_send_deferred(2);
    assert_eq!(result, Err(flume::TrySendError::Full(2)));
    drop(wakes);
    assert_eq!(rx.try_recv().unwrap(), 1);
    drop(rx);
    let (result, wakes) = tx.try_send_deferred(3);
    assert_eq!(result, Err(flume::TrySendError::Disconnected(3)));
    drop(wakes);
}

#[test]
fn notification_can_consume_the_committed_message_reentrantly() {
    struct Receiver(flume::Receiver<u64>, AtomicUsize);
    impl ArcWake for Receiver {
        fn wake_by_ref(this: &Arc<Self>) {
            if let Ok(value) = this.0.try_recv() {
                this.1.store(value as usize, Ordering::SeqCst);
            }
        }
    }
    let (tx, rx) = flume::bounded(1);
    let observer = Arc::new(Receiver(rx.clone(), AtomicUsize::new(0)));
    let waker = futures::task::waker_ref(&observer);
    let mut receive = Box::pin(rx.recv_async());
    assert!(receive
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    let (result, wakes) = tx.try_send_deferred(42);
    result.unwrap();
    drop(wakes);
    assert_eq!(observer.1.load(Ordering::SeqCst), 42);
    drop(receive);
    assert!(rx.try_recv().is_err());
}

#[test]
fn panicking_waiter_does_not_suppress_other_waiters() {
    struct Panic;
    impl ArcWake for Panic {
        fn wake_by_ref(_: &Arc<Self>) {
            panic!("requested wake failure");
        }
    }
    let (tx, rx) = flume::bounded(1);
    let panics = Arc::new(Panic);
    let counts = Arc::new(Count(AtomicUsize::new(0)));
    let bad = futures::task::waker_ref(&panics);
    let good = futures::task::waker_ref(&counts);
    let mut a = rx.stream();
    let mut b = rx.stream();
    let mut first = Box::pin(a.next());
    let mut second = Box::pin(b.next());
    assert!(first
        .as_mut()
        .poll(&mut Context::from_waker(&bad))
        .is_pending());
    assert!(second
        .as_mut()
        .poll(&mut Context::from_waker(&good))
        .is_pending());
    let (result, wakes) = tx.try_send_deferred(42);
    result.unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(wakes))).is_err());
    assert_eq!(counts.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        second.as_mut().poll(&mut Context::from_waker(&good)),
        Poll::Ready(Some(42))
    );
    drop(first);
}

#[test]
fn panicking_waker_destructor_does_not_suppress_other_waiters() {
    struct DropPanic;
    impl ArcWake for DropPanic {
        fn wake_by_ref(_: &Arc<Self>) {}
    }
    impl Drop for DropPanic {
        fn drop(&mut self) {
            panic!("requested waker destructor failure");
        }
    }
    let (tx, rx) = flume::bounded(1);
    let bad = futures::task::waker(Arc::new(DropPanic));
    let counts = Arc::new(Count(AtomicUsize::new(0)));
    let good = futures::task::waker_ref(&counts);
    let mut first = rx.stream();
    let mut second = rx.stream();
    assert!(std::pin::Pin::new(&mut first)
        .poll_next(&mut Context::from_waker(&bad))
        .is_pending());
    assert!(std::pin::Pin::new(&mut second)
        .poll_next(&mut Context::from_waker(&good))
        .is_pending());
    let (result, wakes) = tx.try_send_deferred(42);
    result.unwrap();
    drop(first);
    drop(bad);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(wakes))).is_err());
    assert_eq!(
        counts.0.load(Ordering::SeqCst),
        1,
        "a failed waker destructor must not suppress a sibling notification"
    );
    assert_eq!(
        std::pin::Pin::new(&mut second).poll_next(&mut Context::from_waker(&good)),
        Poll::Ready(Some(42))
    );
}
