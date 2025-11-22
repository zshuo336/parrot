use super::*;
use crate::owned_payload;
use parrot_api::supervisor::{BasicDecisionFn, OneForAllStrategy, OneForOneStrategy};
use std::{
    sync::{Condvar, atomic::AtomicUsize, mpsc},
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
struct Capture {
    gate: Arc<Gate>,
    panics: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.gate.entered.notify_one();
        let (open, _) = self
            .gate
            .changed
            .wait_timeout_while(self.gate.open.lock().unwrap(), WAIT * 2, |open| !*open)
            .unwrap();
        assert!(*open, "release the actual policy destructor");
        self.gate.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panics, "policy receipt quota witness");
    }
}
struct Observe {
    slots: Arc<Semaphore>,
    gate: Arc<Gate>,
    snapshots: mpsc::Sender<(bool, usize, usize)>,
}
impl futures::task::ArcWake for Observe {
    fn wake_by_ref(this: &Arc<Self>) {
        let pending = owned_payload::try_pending_cleanup_permit(&this.slots);
        let _ = this.snapshots.send((
            pending.is_some(),
            this.slots.available_permits(),
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
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let capture = Capture {
        gate: gate.clone(),
        panics,
    };
    let decider = BasicDecisionFn::new(move |_| {
        std::hint::black_box(&capture);
        panic!("disposing a policy must not invoke its decider");
    });
    let strategy = if global {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 1,
            within: WAIT,
            decider,
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: WAIT,
            decider,
        })
    };
    let store = PolicyStore::new(Some(strategy), slots.clone());
    let filler = if global {
        Arc::new(Semaphore::new(8))
    } else {
        slots.clone()
    };
    let count = if global {
        owned_payload::MAX_PENDING_CLEANUPS_GLOBAL
    } else {
        owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT
    };
    let fillers: Vec<_> = (0..count - 1)
        .map(|_| owned_payload::try_pending_cleanup_permit(&filler).unwrap())
        .collect();
    let mut cancelled = Box::pin(store.dispose());
    {
        let _entered = host.enter();
        assert!(
            cancelled
                .as_mut()
                .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
                .is_pending()
        );
    }
    drop(cancelled);
    let (snapshots, observed) = mpsc::channel();
    let waker = futures::task::waker(Arc::new(Observe {
        slots: slots.clone(),
        gate: gate.clone(),
        snapshots,
    }));
    let mut receipt = Box::pin(store.dispose());
    assert!(
        receipt
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    host.block_on(async {
        tokio::time::timeout(WAIT, gate.entered.notified())
            .await
            .expect("the actual policy destructor must start");
    });
    assert!(store.get().is_none());
    assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
    assert_eq!(slots.available_permits(), 0);
    assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
    if host_exit {
        host.shutdown_timeout(Duration::from_millis(20));
        assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
        assert_eq!(slots.available_permits(), 0);
        assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
    }
    drop(release);
    let snapshot = observed.recv_timeout(WAIT);
    let result = futures::executor::block_on(receipt);
    let repeated = futures::executor::block_on(store.dispose());
    if panics {
        let first = result.unwrap_err().to_string();
        assert!(first.contains("policy receipt quota witness"));
        assert_eq!(first, repeated.unwrap_err().to_string());
    } else {
        result.unwrap();
        repeated.unwrap();
    }
    assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
    drop(fillers);
    assert_eq!(
        snapshot.unwrap(),
        (true, 1, 1),
        "policy completion waker must observe actual disposal and reusable cleanup quotas"
    );
}

macro_rules! contract {
    ($name:ident, $global:expr, $panics:expr, $host_exit:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!("supervision_policy::receipt_contracts::", stringify!($name)),
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
