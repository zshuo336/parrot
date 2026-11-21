use super::*;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Overflow {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    runtime: tokio::runtime::Handle,
    slots: Arc<Semaphore>,
    nested: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Overflow {
    fn drop(&mut self) {
        assert!(DisposalScope::active());
        self.entered.send(()).unwrap();
        self.release.recv_timeout(WAIT * 2).unwrap();
        let nested = OwnedPayload::new(Counted(self.nested.clone()), &self.runtime, &self.slots);
        futures::executor::block_on(nested.retire()).unwrap();
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("synchronous overflow disposal witness");
    }
}

fn check(global: bool) {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
        .unwrap();
    let queued = Arc::new(AtomicUsize::new(0));
    let queues = if global {
        MAX_PENDING_CLEANUPS_GLOBAL / MAX_PENDING_CLEANUPS_PER_SLOT
    } else {
        1
    };
    let mut active = Vec::new();
    let mut pools = Vec::new();
    let mut receipts = Vec::new();
    for _ in 0..queues {
        let slots = Arc::new(Semaphore::new(1));
        active.push(slots.clone().try_acquire_owned().unwrap());
        for _ in 0..MAX_PENDING_CLEANUPS_PER_SLOT {
            let input = OwnedPayload::new(Counted(queued.clone()), host.handle(), &slots);
            receipts.push(input.retire());
        }
        assert_eq!(pending_cleanup_slots(&slots).available_permits(), 0);
        pools.push(slots);
    }
    assert_eq!(queued.load(Ordering::SeqCst), 0);
    let global_before = global_pending_cleanup_slots().available_permits();
    assert_eq!(
        global_before,
        MAX_PENDING_CLEANUPS_GLOBAL - queues * MAX_PENDING_CLEANUPS_PER_SLOT
    );
    // Global overflow must give its unused local quota back immediately.
    let slots = if global {
        Arc::new(Semaphore::new(1))
    } else {
        pools[0].clone()
    };
    let pending = pending_cleanup_slots(&slots);
    let local_before = pending.available_permits();
    let admission = Arc::new(Semaphore::new(1));
    let reservation = Arc::new(admission.clone().try_acquire_owned().unwrap());
    let nested = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (entered, receive_entered) = mpsc::channel();
    let (release, receive_release) = mpsc::channel();
    let input = OwnedPayload::new(
        Overflow {
            entered,
            release: receive_release,
            runtime: host.handle().clone(),
            slots: slots.clone(),
            nested: nested.clone(),
            drops: drops.clone(),
        },
        host.handle(),
        &slots,
    )
    .with_reservation(&reservation);
    drop(reservation);
    let mut first = input.receipt();
    let second = input.receipt();
    let abandoned = input.receipt();
    let (returned, receive_returned) = mpsc::channel();
    let owner = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(input.retire());
        }));
        returned.send(result.is_ok()).unwrap();
        assert!(!DisposalScope::active());
    });
    let started = receive_entered.recv_timeout(WAIT);
    let returned_early = receive_returned.try_recv();
    let incomplete = first
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        .is_pending();
    drop(abandoned);
    let admission_while_blocked = admission.available_permits();
    let local_while_blocked = pending.available_permits();
    let global_while_blocked = global_pending_cleanup_slots().available_permits();
    let drops_while_blocked = drops.load(Ordering::SeqCst);
    let _ = release.send(());
    owner.join().unwrap();
    assert!(
        started.is_ok(),
        "overflow must actually enter synchronous disposal"
    );
    assert!(matches!(returned_early, Err(mpsc::TryRecvError::Empty)));
    assert!(receive_returned.recv_timeout(WAIT).unwrap());
    assert!(incomplete, "receipt must not precede actual disposal");
    assert_eq!(admission_while_blocked, 0);
    assert_eq!(drops_while_blocked, 0);
    assert_eq!(local_while_blocked, local_before);
    assert_eq!(global_while_blocked, global_before);
    assert_eq!(admission.available_permits(), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(nested.load(Ordering::SeqCst), 1);
    for receipt in [first, second] {
        let error = futures::executor::block_on(receipt)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("synchronous overflow disposal witness"),
            "{error}"
        );
    }
    assert_eq!(queued.load(Ordering::SeqCst), 0);
    drop(active);
    host.block_on(async {
        tokio::time::timeout(WAIT * 2, async {
            for receipt in receipts {
                receipt.await.unwrap();
            }
        })
        .await
        .expect("queued cleanup must drain after real worker capacity recovers");
    });
    assert_eq!(
        queued.load(Ordering::SeqCst),
        queues * MAX_PENDING_CLEANUPS_PER_SLOT
    );
    assert_eq!(
        global_pending_cleanup_slots().available_permits(),
        MAX_PENDING_CLEANUPS_GLOBAL
    );
    for slots in pools {
        assert_eq!(
            pending_cleanup_slots(&slots).available_permits(),
            MAX_PENDING_CLEANUPS_PER_SLOT
        );
    }
    assert!(!DisposalScope::active());
}

#[test]
fn local_overflow_retains_reservation_and_real_receipt_through_blocking_disposal() {
    crate::test_support::isolated(
        "owned_payload::saturation_contracts::local_overflow_retains_reservation_and_real_receipt_through_blocking_disposal",
        || check(false),
    );
}

#[test]
fn global_overflow_releases_partial_quota_and_preserves_nested_disposal() {
    crate::test_support::isolated(
        "owned_payload::saturation_contracts::global_overflow_releases_partial_quota_and_preserves_nested_disposal",
        || check(true),
    );
}
