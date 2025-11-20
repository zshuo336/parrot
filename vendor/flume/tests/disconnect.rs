//! Explicit disconnection preserves queued values and returns pending inputs.
#![cfg(feature = "async")]
use futures::{
    task::{noop_waker, waker_ref, ArcWake},
    Future,
};
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
fn disconnect_defers_callbacks_and_returns_pending_send_without_refilling() {
    let (sender, receiver) = flume::bounded(1);
    sender.try_send(11).unwrap();
    let calls = Arc::new(Count(AtomicUsize::new(0)));
    let waker = waker_ref(&calls);
    let mut pending = Box::pin(sender.send_async(22));
    assert!(pending
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    let notifications = receiver.disconnect_deferred();
    assert!(sender.is_disconnected());
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    assert_eq!(receiver.drain().collect::<Vec<_>>(), [11]);
    drop(receiver.disconnect_deferred());
    assert_eq!(sender.len(), 0);
    drop(receiver);
    assert_eq!(sender.len(), 0);
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    drop(notifications);
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        pending.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(Err(flume::SendError(22)))
    );
}

#[test]
fn disconnect_notifies_receivers_and_leaves_committed_values_readable() {
    let (sender, receiver) = flume::bounded(1);
    let calls = Arc::new(Count(AtomicUsize::new(0)));
    let waker = waker_ref(&calls);
    let mut pending = Box::pin(receiver.recv_async());
    assert!(pending
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    let notifications = receiver.disconnect_deferred();
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    drop(notifications);
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        pending.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(Err(flume::RecvError::Disconnected))
    );
    assert_eq!(
        sender.try_send(33),
        Err(flume::TrySendError::Disconnected(33))
    );

    let (sender, receiver) = flume::bounded(1);
    sender.try_send(44).unwrap();
    drop(receiver.disconnect_deferred());
    assert_eq!(receiver.try_recv(), Ok(44));
    assert_eq!(receiver.try_recv(), Err(flume::TryRecvError::Disconnected));
}

#[test]
fn dropping_last_receiver_does_not_commit_a_pending_send() {
    let (sender, receiver) = flume::bounded(1);
    sender.try_send(55).unwrap();
    let mut pending = Box::pin(sender.send_async(66));
    let waker = noop_waker();
    assert!(pending
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert_eq!(receiver.drain().collect::<Vec<_>>(), [55]);
    drop(receiver);
    assert_eq!(sender.len(), 0);
    assert_eq!(
        pending.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(Err(flume::SendError(66)))
    );
}
