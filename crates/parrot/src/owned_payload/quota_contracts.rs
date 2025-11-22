use super::*;
use std::{
    sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    task::Context,
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: tokio::sync::Notify,
    drops: AtomicUsize,
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.open.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
struct Input {
    gate: Arc<Gate>,
    panics: bool,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.gate.entered.notify_one();
        let (open, _) = self
            .gate
            .changed
            .wait_timeout_while(self.gate.open.lock().unwrap(), WAIT * 2, |open| !*open)
            .unwrap();
        assert!(*open, "release the actual input destructor");
        self.gate.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panics, "input receipt quota witness");
    }
}
struct Observe {
    slots: Arc<Semaphore>,
    admission: Arc<Semaphore>,
    gate: Arc<Gate>,
    snapshots: mpsc::Sender<(bool, usize, usize, usize)>,
}
impl futures::task::ArcWake for Observe {
    fn wake_by_ref(this: &Arc<Self>) {
        let pending = try_pending_cleanup_permit(&this.slots);
        let _ = this.snapshots.send((
            pending.is_some(),
            this.slots.available_permits(),
            this.admission.available_permits(),
            this.gate.drops.load(Ordering::SeqCst),
        ));
    }
}

fn check(global: bool, panics: bool, host_exit: bool) {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let admission = Arc::new(Semaphore::new(1));
    let reservation = Arc::new(admission.clone().try_acquire_owned().unwrap());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let filler = if global {
        Arc::new(Semaphore::new(8))
    } else {
        slots.clone()
    };
    let count = if global {
        MAX_PENDING_CLEANUPS_GLOBAL
    } else {
        MAX_PENDING_CLEANUPS_PER_SLOT
    };
    let fillers: Vec<_> = (0..count - 1)
        .map(|_| try_pending_cleanup_permit(&filler).unwrap())
        .collect();
    let input = OwnedPayload::new(
        Input {
            gate: gate.clone(),
            panics,
        },
        host.handle(),
        &slots,
    )
    .with_reservation(&reservation);
    drop(reservation);
    let mut receipt = input.receipt();
    let repeated = input.receipt();
    let cancelled = input.receipt();
    let (snapshots, observed) = mpsc::channel();
    let waker = futures::task::waker(Arc::new(Observe {
        slots: slots.clone(),
        admission: admission.clone(),
        gate: gate.clone(),
        snapshots,
    }));
    assert!(
        receipt
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(input);
    drop(cancelled);
    host.block_on(async {
        tokio::time::timeout(WAIT, gate.entered.notified())
            .await
            .expect("the actual input destructor must start");
    });
    assert!(try_pending_cleanup_permit(&slots).is_none());
    assert_eq!(slots.available_permits(), 0);
    assert_eq!(admission.available_permits(), 0);
    assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
    if host_exit {
        host.shutdown_timeout(Duration::from_millis(20));
        assert!(try_pending_cleanup_permit(&slots).is_none());
        assert_eq!(slots.available_permits(), 0);
        assert_eq!(admission.available_permits(), 0);
        assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
    }
    drop(release);
    let snapshot = observed.recv_timeout(WAIT);
    for receipt in [receipt, repeated] {
        let result = futures::executor::block_on(receipt);
        if panics {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("input receipt quota witness")
            );
        } else {
            result.unwrap();
        }
    }
    assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
    drop(fillers);
    assert_eq!(
        snapshot.unwrap(),
        (true, 1, 1, 1),
        "input completion waker must observe disposal and reusable pending, worker and creation quotas"
    );
}

macro_rules! contract {
    ($name:ident, $global:expr, $panics:expr, $host_exit:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!("owned_payload::quota_contracts::", stringify!($name)),
                || check($global, $panics, $host_exit),
            );
        }
    };
}
contract!(local_quota_is_reusable_at_success, false, false, false);
contract!(local_quota_is_reusable_at_failure, false, true, false);
contract!(global_quota_is_reusable_at_success, true, false, false);
contract!(global_quota_is_reusable_at_failure, true, true, false);
contract!(
    local_quota_is_reusable_after_host_exit_success,
    false,
    false,
    true
);
contract!(
    local_quota_is_reusable_after_host_exit_failure,
    false,
    true,
    true
);
contract!(
    global_quota_is_reusable_after_host_exit_success,
    true,
    false,
    true
);
contract!(
    global_quota_is_reusable_after_host_exit_failure,
    true,
    true,
    true
);
