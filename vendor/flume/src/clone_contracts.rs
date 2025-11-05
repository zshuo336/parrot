//! RawWaker cloning is executable user code, just like waking and disposal.
use super::*;
use std::mem::ManuallyDrop;
use std::task::{RawWaker, RawWakerVTable};

fn queue_available<T>(shared: &Shared<T>) -> bool {
    #[cfg(feature = "spin")]
    {
        shared.chan.try_lock().is_some()
    }
    #[cfg(not(feature = "spin"))]
    {
        shared.chan.try_lock().is_ok()
    }
}

struct Probe {
    clone: Box<dyn Fn() + Send + Sync>,
    wakes: AtomicUsize,
}

unsafe fn clone_raw(data: *const ()) -> RawWaker {
    // Each raw pointer owns one Arc count. Borrow without consuming that count
    // for clone/wake_by_ref; transfer one fresh count to the returned RawWaker.
    let probe = ManuallyDrop::new(unsafe { Arc::from_raw(data.cast::<Probe>()) });
    (probe.clone)();
    RawWaker::new(Arc::into_raw(Arc::clone(&probe)).cast(), &VTABLE)
}
unsafe fn wake_raw(data: *const ()) {
    let probe = unsafe { Arc::from_raw(data.cast::<Probe>()) };
    probe.wakes.fetch_add(1, Ordering::SeqCst);
}
unsafe fn wake_ref_raw(data: *const ()) {
    let probe = ManuallyDrop::new(unsafe { Arc::from_raw(data.cast::<Probe>()) });
    probe.wakes.fetch_add(1, Ordering::SeqCst);
}
unsafe fn drop_raw(data: *const ()) {
    drop(unsafe { Arc::from_raw(data.cast::<Probe>()) });
}
static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_raw, wake_raw, wake_ref_raw, drop_raw);

fn waker(callback: impl Fn() + Send + Sync + 'static) -> (Waker, Arc<Probe>) {
    let probe = Arc::new(Probe {
        clone: Box::new(callback),
        wakes: AtomicUsize::new(0),
    });
    // Probe is Send+Sync and all raw operations preserve Arc ownership.
    let waker =
        unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(probe.clone()).cast(), &VTABLE)) };
    (waker, probe)
}

#[test]
fn receiver_registration_clone_releases_queue_lock() {
    let (_tx, rx) = bounded::<u8>(1);
    let shared = Arc::downgrade(&rx.shared);
    let busy = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (recorded_busy, recorded_calls) = (busy.clone(), calls.clone());
    let (waker, _) = waker(move || {
        recorded_calls.fetch_add(1, Ordering::SeqCst);
        if !queue_available(&shared.upgrade().unwrap()) {
            recorded_busy.fetch_add(1, Ordering::SeqCst);
        }
    });
    let mut receive = Box::pin(rx.recv_async());
    assert!(receive
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert!(calls.load(Ordering::SeqCst) > 0);
    assert_eq!(
        busy.load(Ordering::SeqCst),
        0,
        "receiver clone callback holds queue lock"
    );
}

#[test]
fn sender_registration_clone_releases_queue_lock() {
    let (tx, _rx) = bounded::<u8>(1);
    tx.try_send(1).unwrap();
    let shared = Arc::downgrade(&tx.shared);
    let busy = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (recorded_busy, recorded_calls) = (busy.clone(), calls.clone());
    let (waker, _) = waker(move || {
        recorded_calls.fetch_add(1, Ordering::SeqCst);
        if !queue_available(&shared.upgrade().unwrap()) {
            recorded_busy.fetch_add(1, Ordering::SeqCst);
        }
    });
    let mut send = Box::pin(tx.send_async(2));
    assert!(send
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert!(calls.load(Ordering::SeqCst) > 0);
    assert_eq!(
        busy.load(Ordering::SeqCst),
        0,
        "sender clone callback holds queue lock"
    );
}

#[test]
fn replacement_clone_releases_registration_lock() {
    let old = futures::task::noop_waker();
    let hook = Hook::<(), _>::trigger(AsyncSignal::new(&Context::from_waker(&old), false));
    let weak = Arc::downgrade(&hook);
    let busy = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (recorded_busy, recorded_calls) = (busy.clone(), calls.clone());
    let (new, _) = waker(move || {
        recorded_calls.fetch_add(1, Ordering::SeqCst);
        if weak.upgrade().unwrap().signal().waker.try_lock().is_none() {
            recorded_busy.fetch_add(1, Ordering::SeqCst);
        }
    });
    hook.update_waker(&new);
    assert!(calls.load(Ordering::SeqCst) > 0);
    assert_eq!(
        busy.load(Ordering::SeqCst),
        0,
        "replacement clone callback holds registration lock"
    );
}

#[test]
fn notification_clone_releases_registration_lock() {
    let holder = Arc::new(std::sync::Mutex::new(
        None::<std::sync::Weak<Hook<(), AsyncSignal>>>,
    ));
    let holder_for_clone = holder.clone();
    let busy = Arc::new(AtomicUsize::new(0));
    let recorded_busy = busy.clone();
    let (waker, probe) = waker(move || {
        if let Some(weak) = holder_for_clone.lock().unwrap().as_ref() {
            if weak.upgrade().unwrap().signal().waker.try_lock().is_none() {
                recorded_busy.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    let hook = Hook::<(), _>::trigger(AsyncSignal::new(&Context::from_waker(&waker), false));
    *holder.lock().unwrap() = Some(Arc::downgrade(&hook));
    hook.signal().prepare();
    hook.signal().fire();
    assert_eq!(
        probe.wakes.load(Ordering::SeqCst),
        1,
        "the selected task must actually be notified"
    );
    assert_eq!(
        busy.load(Ordering::SeqCst),
        0,
        "notification clone callback holds registration lock"
    );
}

#[test]
fn receive_replacement_clone_can_commit_message() {
    let (tx, rx) = bounded(1);
    let old = futures::task::noop_waker();
    let mut receive = Box::pin(rx.recv_async());
    assert!(receive
        .as_mut()
        .poll(&mut Context::from_waker(&old))
        .is_pending());
    let (new, probe) = waker(move || tx.try_send(42).unwrap());
    assert_eq!(
        receive.as_mut().poll(&mut Context::from_waker(&new)),
        Poll::Ready(Ok(42))
    );
    assert!(probe.wakes.load(Ordering::SeqCst) > 0);
    assert!(rx.try_recv().is_err());
}

#[test]
fn send_replacement_clone_can_release_capacity() {
    let (tx, rx) = bounded(1);
    tx.try_send(1).unwrap();
    let old = futures::task::noop_waker();
    let mut send = Box::pin(tx.send_async(2));
    assert!(send
        .as_mut()
        .poll(&mut Context::from_waker(&old))
        .is_pending());
    let consumer = rx.clone();
    let (new, probe) = waker(move || assert_eq!(consumer.try_recv(), Ok(1)));
    assert!(send
        .as_mut()
        .poll(&mut Context::from_waker(&new))
        .is_pending());
    // Poll again only after proving the new task received a notification.
    assert!(probe.wakes.load(Ordering::SeqCst) > 0);
    assert_eq!(
        send.as_mut().poll(&mut Context::from_waker(&new)),
        Poll::Ready(Ok(()))
    );
    assert_eq!(rx.try_recv(), Ok(2));
    assert!(rx.try_recv().is_err());
}

#[test]
fn receive_clone_panic_does_not_poison_queue() {
    let (tx, rx) = bounded(1);
    let (bad, _) = waker(|| panic!("requested registration clone panic"));
    let mut receive = Box::pin(rx.recv_async());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        receive.as_mut().poll(&mut Context::from_waker(&bad))
    }));
    assert!(outcome.is_err());
    drop(receive);
    tx.try_send(42).unwrap();
    assert_eq!(rx.try_recv(), Ok(42));
}

#[test]
fn send_clone_panic_does_not_poison_queue() {
    let (tx, rx) = bounded(1);
    tx.try_send(1).unwrap();
    let (bad, _) = waker(|| panic!("requested registration clone panic"));
    let mut send = Box::pin(tx.send_async(2));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        send.as_mut().poll(&mut Context::from_waker(&bad))
    }));
    assert!(outcome.is_err());
    drop(send);
    assert_eq!(rx.try_recv(), Ok(1));
    tx.try_send(3).unwrap();
    assert_eq!(rx.try_recv(), Ok(3));
}

#[test]
fn replacement_clone_panic_preserves_registered_receiver() {
    let (tx, rx) = bounded(1);
    let (old, probe) = waker(|| {});
    let mut receive = Box::pin(rx.recv_async());
    assert!(receive
        .as_mut()
        .poll(&mut Context::from_waker(&old))
        .is_pending());
    let (bad, _) = waker(|| panic!("requested replacement clone panic"));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        receive.as_mut().poll(&mut Context::from_waker(&bad))
    }));
    assert!(outcome.is_err());
    tx.try_send(42).unwrap();
    assert_eq!(probe.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        receive.as_mut().poll(&mut Context::from_waker(&old)),
        Poll::Ready(Ok(42))
    );
}
