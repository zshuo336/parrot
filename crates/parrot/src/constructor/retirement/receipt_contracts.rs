use super::*;
use crate::owned_payload;
use std::{
    sync::{Condvar, atomic::AtomicUsize, mpsc},
    task::Context,
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

fn receipt_diagnostic(during_unwind: bool) {
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let captured = Counted(drops.clone());
    let (sender, _) = watch::channel(None);
    let disposal = CaptureDisposal::<()> {
        context: None,
        function: Some(Arc::new(move || {
            std::hint::black_box(&captured);
        })),
        failure: Some("original factory retirement failure".into()),
        finished: Some(sender.clone()),
        cleanup: None,
        pending: None,
        creation_reservation: None,
    };
    owned_payload::diagnostic_contracts::verify(
        &sender,
        during_unwind,
        1,
        || drop(disposal),
        |value| {
            assert_eq!(
                value.as_ref().unwrap_err(),
                "original factory retirement failure"
            );
        },
    );
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
}
owned_payload::diagnostic_contracts::contract!(
    receipt_diagnostic_factory,
    receipt_diagnostic(false)
);
owned_payload::diagnostic_contracts::contract!(
    receipt_diagnostic_factory_unwind,
    receipt_diagnostic(true)
);

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    changed: Condvar,
    entered: tokio::sync::Notify,
    drops: AtomicUsize,
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
struct Probe {
    gate: Arc<Gate>,
    failure: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.gate.entered.notify_one();
        let (released, _) = self
            .gate
            .changed
            .wait_timeout_while(self.gate.released.lock().unwrap(), WAIT * 2, |open| !*open)
            .unwrap();
        assert!(*released, "release user disposal before joining its owner");
        self.gate.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.failure, "completion quota disposal witness");
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

fn check(kind: u8, global: bool) {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let workers = Arc::new(Semaphore::new(1));
    let slots = Arc::new(Semaphore::new(1));
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let factory = if kind == 0 {
        let capture = Probe {
            gate: gate.clone(),
            failure: global,
        };
        Factory::<Probe>::new(
            move || {
                std::hint::black_box(&capture);
                unreachable!("retiring an idle factory must not invoke its constructor");
            },
            workers,
            slots.clone(),
        )
    } else {
        let output_gate = gate.clone();
        Factory::new(
            move || Probe {
                gate: output_gate.clone(),
                failure: global,
            },
            workers,
            slots.clone(),
        )
    };
    let prepared = (kind != 0).then(|| {
        host.block_on(factory.prepare::<()>(Arc::default()))
            .unwrap()
    });
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
    let mut receipt = match kind {
        0 => {
            let _entered = host.enter();
            factory.retire()
        }
        1 => prepared.unwrap().discard(),
        2 => prepared.unwrap().discard_in_lifecycle(),
        _ => unreachable!(),
    };
    let (snapshots, observed) = mpsc::channel();
    let observer = Arc::new(Observe {
        slots: slots.clone(),
        gate: gate.clone(),
        snapshots,
    });
    let waker = futures::task::waker(observer);
    assert!(
        receipt
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    host.block_on(async {
        tokio::time::timeout(WAIT, gate.entered.notified())
            .await
            .expect("the real user destructor must start");
    });
    assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
    assert_eq!(slots.available_permits(), 0);
    assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
    drop(release);
    let snapshot = observed.recv_timeout(WAIT);
    let result = host.block_on(receipt);
    if global {
        assert!(
            result
                .unwrap_err()
                .contains("completion quota disposal witness")
        );
    } else {
        result.unwrap();
    }
    assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
    drop(fillers);
    assert_eq!(
        snapshot.unwrap(),
        (true, 1, 1),
        "completion waker must observe disposed input and reusable pending/worker quotas"
    );
}

macro_rules! contract {
    ($name:ident, $kind:expr, $global:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "constructor::retirement::receipt_contracts::",
                    stringify!($name)
                ),
                || check($kind, $global),
            );
        }
    };
}
contract!(
    factory_local_quota_is_reusable_in_completion_waker,
    0,
    false
);
contract!(factory_global_quota_is_reusable_in_failure_waker, 0, true);
contract!(
    prepared_local_quota_is_reusable_in_completion_waker,
    1,
    false
);
contract!(prepared_global_quota_is_reusable_in_failure_waker, 1, true);
contract!(
    lifecycle_local_quota_is_reusable_in_completion_waker,
    2,
    false
);
contract!(lifecycle_global_quota_is_reusable_in_failure_waker, 2, true);
