//! Raw sender aliases must not lose popped values or re-enter locked mailbox state.
use futures::FutureExt;
use parrot::thread::{
    config::BackpressureStrategy,
    mailbox::{Mailbox, mpsc::MpscMailbox, spsc::SpscMailbox, spsc_ringbuf::SpscRingbufMailbox},
    scheduler::queue::SchedulingQueue,
};
use parrot_api::address::ActorPath;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Wake, Waker},
};

struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct PanicPayload;
impl Drop for PanicPayload {
    fn drop(&mut self) {
        panic!("raw notification payload destructor");
    }
}
struct Observer {
    wakes: AtomicUsize,
    unlocked: AtomicBool,
    probe: Box<dyn Fn() -> bool + Send + Sync>,
    fail: bool,
}
impl Wake for Observer {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        self.unlocked.store((self.probe)(), Ordering::SeqCst);
        if self.fail {
            std::panic::panic_any(PanicPayload);
        }
    }
}
fn observer(mailbox: &Arc<SpscMailbox>, fail: bool) -> (Arc<Observer>, Waker) {
    let weak = Arc::downgrade(mailbox);
    let owner = Arc::new(Observer {
        wakes: AtomicUsize::new(0),
        unlocked: AtomicBool::new(false),
        fail,
        probe: Box::new(move || {
            weak.upgrade()
                .is_none_or(|m| m.len().now_or_never().is_some())
        }),
    });
    let waker = Waker::from(owner.clone());
    (owner, waker)
}
fn check_no_unwind<T>(outcome: std::thread::Result<T>, message: &str) -> T {
    match outcome {
        Ok(result) => result,
        Err(payload) => {
            if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(secondary);
            }
            panic!("{message}");
        }
    }
}
#[tokio::test]
async fn raw_capacity_notification_runs_after_unlock_and_preserves_the_popped_value() {
    let mailbox = Arc::new(SpscMailbox::new(1, ActorPath::placeholder("/raw-capacity")));
    let sender = mailbox.sender();
    let drops = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(Payload(drops.clone())),
            BackpressureStrategy::Block,
        )
        .await
        .unwrap();
    let reserving = sender.reserve();
    tokio::pin!(reserving);
    let (owner, waker) = observer(&mailbox, true);
    assert!(
        reserving
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let value = check_no_unwind(
        AssertUnwindSafe(mailbox.pop()).catch_unwind().await,
        "raw capacity observer erased a popped message",
    )
    .unwrap();
    assert!(
        owner.unlocked.load(Ordering::SeqCst),
        "capacity observer ran under the receiver lock"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(reserving.await.unwrap());
    mailbox.close().await;
}
#[tokio::test]
async fn raw_capacity_and_closed_observers_both_finish_outside_mailbox_locks() {
    let mailbox = Arc::new(SpscMailbox::new(1, ActorPath::placeholder("/raw-close")));
    let sender = mailbox.sender();
    let drops = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(Payload(drops.clone())),
            BackpressureStrategy::Block,
        )
        .await
        .unwrap();
    let reserving = sender.reserve();
    let closing = sender.closed();
    tokio::pin!(reserving, closing);
    let (bad, bad_waker) = observer(&mailbox, true);
    let (healthy, healthy_waker) = observer(&mailbox, false);
    assert!(
        reserving
            .as_mut()
            .poll(&mut Context::from_waker(&bad_waker))
            .is_pending()
    );
    assert!(
        closing
            .as_mut()
            .poll(&mut Context::from_waker(&healthy_waker))
            .is_pending()
    );
    check_no_unwind(
        AssertUnwindSafe(mailbox.close()).catch_unwind().await,
        "raw capacity callback prevented close notifications and physical drain",
    );
    assert!(bad.unlocked.load(Ordering::SeqCst));
    assert!(healthy.unlocked.load(Ordering::SeqCst));
    assert_eq!(bad.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        healthy.wakes.load(Ordering::SeqCst),
        1,
        "close skipped the independent sender.closed observer"
    );
    assert!(reserving.await.is_err());
    closing.await;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(mailbox.len().await, 0);
    mailbox.close().await;
}
#[tokio::test]
async fn final_mailbox_drop_keeps_raw_close_callbacks_from_skipping_messages() {
    let mailbox = Arc::new(SpscMailbox::new(1, ActorPath::placeholder("/raw-drop")));
    let sender = mailbox.sender();
    let drops = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(Payload(drops.clone())),
            BackpressureStrategy::Block,
        )
        .await
        .unwrap();
    let closing = sender.closed();
    tokio::pin!(closing);
    let (owner, waker) = observer(&mailbox, true);
    assert!(
        closing
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    check_no_unwind(
        catch_unwind(AssertUnwindSafe(|| drop(mailbox))),
        "raw close observer unwound the final mailbox owner",
    );
    assert_eq!(owner.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    closing.await;
}

#[test]
fn failed_ready_observer_still_wakes_the_real_queue_consumer() {
    let queue = SchedulingQueue::new(1);
    let notify = queue.notify_handle();
    let bad = Arc::new(Observer {
        wakes: AtomicUsize::new(0),
        unlocked: AtomicBool::new(false),
        probe: Box::new(|| true),
        fail: true,
    });
    let healthy = Arc::new(Observer {
        wakes: AtomicUsize::new(0),
        unlocked: AtomicBool::new(false),
        probe: Box::new(|| true),
        fail: false,
    });
    let bad_waker = Waker::from(bad.clone());
    let healthy_waker = Waker::from(healthy.clone());
    let bad_wait = notify.notified();
    let taking = queue.pop();
    tokio::pin!(bad_wait, taking);
    assert!(
        bad_wait
            .as_mut()
            .poll(&mut Context::from_waker(&bad_waker))
            .is_pending()
    );
    assert!(
        taking
            .as_mut()
            .poll(&mut Context::from_waker(&healthy_waker))
            .is_pending()
    );
    let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
        1,
        ActorPath::placeholder("/queue-healthy"),
    ));
    queue.push(mailbox.clone()).unwrap();
    assert_eq!(
        healthy.wakes.load(Ordering::SeqCst),
        1,
        "a committed queue item lost the healthy consumer wake"
    );
    let value = taking
        .as_mut()
        .poll(&mut Context::from_waker(&healthy_waker));
    assert!(matches!(value, std::task::Poll::Ready(ref actual) if Arc::ptr_eq(actual, &mailbox)));
    assert!(queue.is_empty());
}

#[tokio::test]
async fn failed_readiness_observer_does_not_starve_healthy_mailbox_observers() {
    let path = ActorPath::placeholder("/healthy-readiness");
    let mpsc = Arc::new(MpscMailbox::new(1, path.clone()));
    let spsc = Arc::new(SpscMailbox::new(1, path.clone()));
    let ring = Arc::new(SpscRingbufMailbox::new(1, path));
    let inputs: Vec<(Arc<dyn Mailbox>, _)> = vec![
        (mpsc.clone(), mpsc.notify_ref()),
        (spsc.clone(), spsc.notify_ref()),
        (ring.clone(), ring.notify_ref()),
    ];
    for (mailbox, notify) in inputs {
        let bad = Arc::new(Observer {
            wakes: AtomicUsize::new(0),
            unlocked: AtomicBool::new(false),
            probe: Box::new(|| true),
            fail: true,
        });
        let healthy = Arc::new(Observer {
            wakes: AtomicUsize::new(0),
            unlocked: AtomicBool::new(false),
            probe: Box::new(|| true),
            fail: false,
        });
        let bad_waker = Waker::from(bad.clone());
        let healthy_waker = Waker::from(healthy.clone());
        let bad_wait = notify.notified();
        let healthy_wait = notify.notified();
        tokio::pin!(bad_wait, healthy_wait);
        assert!(
            bad_wait
                .as_mut()
                .poll(&mut Context::from_waker(&bad_waker))
                .is_pending()
        );
        assert!(
            healthy_wait
                .as_mut()
                .poll(&mut Context::from_waker(&healthy_waker))
                .is_pending()
        );
        mailbox
            .push(Box::new(17_u64), BackpressureStrategy::Block)
            .await
            .unwrap();
        assert_eq!(healthy.wakes.load(Ordering::SeqCst), 1);
        assert!(
            healthy_wait
                .as_mut()
                .poll(&mut Context::from_waker(&healthy_waker))
                .is_ready()
        );
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 17);
        mailbox.close().await;
    }
}
