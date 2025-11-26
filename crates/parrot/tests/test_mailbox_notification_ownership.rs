//! Observers cannot interrupt a committed mailbox admission or physical cleanup.
use futures::FutureExt;
use parrot::thread::{
    config::BackpressureStrategy,
    mailbox::{Mailbox, mpsc::MpscMailbox, spsc::SpscMailbox, spsc_ringbuf::SpscRingbufMailbox},
    scheduler::queue::SchedulingQueue,
};
use parrot_api::address::ActorPath;
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Wake, Waker},
};
use tokio::sync::Notify;

struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct PanicPayload(Arc<AtomicUsize>);
impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("notification panic payload destructor");
    }
}
struct Observer {
    wakes: AtomicUsize,
    payloads: Arc<AtomicUsize>,
}
impl Wake for Observer {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(PanicPayload(self.payloads.clone()));
    }
}
fn observer() -> (Arc<Observer>, Waker) {
    let owner = Arc::new(Observer {
        wakes: AtomicUsize::new(0),
        payloads: Arc::new(AtomicUsize::new(0)),
    });
    let waker = Waker::from(owner.clone());
    (owner, waker)
}
fn mailbox(kind: usize) -> (Arc<dyn Mailbox>, Arc<Notify>) {
    let path = ActorPath::placeholder("/notification-ownership");
    match kind {
        0 => {
            let mailbox = Arc::new(MpscMailbox::new(1, path));
            let notify = mailbox.notify_ref();
            (mailbox, notify)
        }
        1 => {
            let mailbox = Arc::new(SpscMailbox::new(1, path));
            let notify = mailbox.notify_ref();
            (mailbox, notify)
        }
        _ => {
            let mailbox = Arc::new(SpscRingbufMailbox::new(1, path));
            let notify = mailbox.notify_ref();
            (mailbox, notify)
        }
    }
}
async fn ready(kind: usize) {
    let (mailbox, notify) = mailbox(kind);
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let calls = hook_calls.clone();
    mailbox.set_wake_hook(Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
    }));
    let (owner, waker) = observer();
    let notification = notify.notified();
    tokio::pin!(notification);
    assert!(
        notification
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let drops = Arc::new(AtomicUsize::new(0));
    let outcome = AssertUnwindSafe(mailbox.push(
        Box::new(Payload(drops.clone())),
        BackpressureStrategy::Block,
    ))
    .catch_unwind()
    .await;
    if let Err(payload) = outcome {
        // Dispose a recursive panic outside assertion unwinding in the old implementation.
        let nested = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload)));
        if let Err(payload) = nested {
            std::mem::forget(payload);
        }
        panic!("readiness observer unwound a committed mailbox push");
    }
    outcome.unwrap().unwrap();
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        1,
        "committed work never reached its scheduler hook"
    );
    assert_eq!(owner.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(owner.payloads.load(Ordering::SeqCst), 1);
    assert_eq!(mailbox.len().await, 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(mailbox.pop().await.unwrap());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    mailbox.close().await;
}
async fn close(kind: usize) {
    let (mailbox, notify) = mailbox(kind);
    let drops = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(Payload(drops.clone())),
            BackpressureStrategy::Block,
        )
        .await
        .unwrap();
    assert!(
        notify.notified().now_or_never().is_some(),
        "consume the original readiness permit before observing close"
    );
    let (owner, waker) = observer();
    let notification = notify.notified();
    tokio::pin!(notification);
    assert!(
        notification
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let outcome = AssertUnwindSafe(mailbox.close()).catch_unwind().await;
    if let Err(payload) = outcome {
        let nested = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload)));
        if let Err(payload) = nested {
            std::mem::forget(payload);
        }
        panic!("close observer prevented physical mailbox drain");
    }
    assert!(mailbox.is_closed().await);
    assert_eq!(mailbox.len().await, 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(owner.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(owner.payloads.load(Ordering::SeqCst), 1);
    mailbox.close().await;
}
async fn capacity(kind: usize) {
    let (mailbox, _) = mailbox(kind);
    let first = Arc::new(AtomicUsize::new(0));
    let second = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(Payload(first.clone())),
            BackpressureStrategy::Block,
        )
        .await
        .unwrap();
    let pushing = mailbox.push(
        Box::new(Payload(second.clone())),
        BackpressureStrategy::Block,
    );
    tokio::pin!(pushing);
    let (owner, waker) = observer();
    assert!(
        pushing
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let outcome = AssertUnwindSafe(mailbox.pop()).catch_unwind().await;
    if let Err(payload) = outcome {
        let nested = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload)));
        if let Err(payload) = nested {
            std::mem::forget(payload);
        }
        panic!("capacity observer lost the dequeued business message");
    }
    let value = outcome.unwrap().unwrap();
    assert_eq!(first.load(Ordering::SeqCst), 0);
    drop(value);
    assert_eq!(first.load(Ordering::SeqCst), 1);
    assert_eq!(owner.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(owner.payloads.load(Ordering::SeqCst), 1);
    pushing.await.unwrap();
    drop(mailbox.pop().await.unwrap());
    assert_eq!(second.load(Ordering::SeqCst), 1);
    mailbox.close().await;
}
#[test]
fn ready_queue_observer_cannot_unwind_its_committed_admission() {
    let queue = SchedulingQueue::new(1);
    let notify = queue.notify_handle();
    let notification = notify.notified();
    tokio::pin!(notification);
    let (owner, waker) = observer();
    assert!(
        notification
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let (mailbox, _) = mailbox(0);
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| queue.push(mailbox.clone())));
    if let Err(payload) = outcome {
        let nested = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload)));
        if let Err(payload) = nested {
            std::mem::forget(payload);
        }
        panic!("ready queue observer unwound committed scheduling ownership");
    }
    outcome.unwrap().unwrap();
    assert!(Arc::ptr_eq(&queue.try_pop().unwrap(), &mailbox));
    assert!(queue.try_pop().is_none());
    assert_eq!(owner.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(owner.payloads.load(Ordering::SeqCst), 1);
}
macro_rules! contract {
    ($name:ident, $check:ident, $kind:expr) => {
        #[tokio::test]
        async fn $name() {
            $check($kind).await;
        }
    };
}
contract!(mpsc_readiness_does_not_skip_scheduler_hook, ready, 0);
contract!(spsc_readiness_does_not_skip_scheduler_hook, ready, 1);
contract!(ring_readiness_does_not_skip_scheduler_hook, ready, 2);
contract!(mpsc_close_observer_cannot_abandon_drain, close, 0);
contract!(spsc_close_observer_cannot_abandon_drain, close, 1);
contract!(ring_close_observer_cannot_abandon_drain, close, 2);
contract!(
    mpsc_capacity_observer_cannot_erase_popped_message,
    capacity,
    0
);
contract!(
    spsc_capacity_observer_cannot_erase_popped_message,
    capacity,
    1
);
contract!(
    ring_capacity_observer_cannot_erase_popped_message,
    capacity,
    2
);
