use super::*;
use crate::{owned_payload, thread::native_budget::NativeWorkerBudget};
use std::{cell::RefCell, sync::atomic::AtomicUsize, time::Duration};

const WAIT: Duration = Duration::from_secs(5);
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn block(&self) {
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT * 3, |open| !*open)
            .unwrap();
        assert!(*open, "release helper TLS before joining its owner");
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct LocalExit {
    gate: Arc<Gate>,
    entered: mpsc::Sender<()>,
    exited: Arc<AtomicUsize>,
}
impl Drop for LocalExit {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        self.gate.block();
        self.exited.fetch_add(1, Ordering::SeqCst);
    }
}
thread_local! {
    static EXIT: RefCell<Option<LocalExit>> = const { RefCell::new(None) };
}
struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn check(last_owner: bool, saturate: bool) {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
        .unwrap();
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let exited = Arc::new(AtomicUsize::new(0));
    let business_drops = Arc::new(AtomicUsize::new(0));
    let budget = Arc::new(NativeWorkerBudget::with_capacity(1));
    let (entered, receive_entered) = mpsc::channel();
    let local = LocalExit {
        gate,
        entered,
        exited: exited.clone(),
    };
    let retirement = actix::System::new().block_on(async {
        let business = Counted(business_drops.clone());
        let owner = actix::Arbiter::try_new().unwrap();
        let (started, ready) = mpsc::channel();
        assert!(owner.spawn(async move {
            let _business = business;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        ready.recv_timeout(WAIT).unwrap();
        Retirement::with_spawner_and_reservations(
            vec![owner],
            vec![budget.try_reserve().unwrap()],
            move |job| {
                std::thread::Builder::new().spawn(move || {
                    EXIT.with(|slot| *slot.borrow_mut() = Some(local));
                    job();
                })
            },
        )
        .unwrap()
    });
    retirement.request();
    host.block_on(retirement.receipt()).unwrap();
    receive_entered.recv_timeout(WAIT).unwrap();
    assert_eq!(business_drops.load(Ordering::SeqCst), 1);
    let helper = retirement.helper.clone();
    let driver = retirement.runtime.clone();
    let retirement = Arc::new(retirement);

    let queued = Arc::new(AtomicUsize::new(0));
    let first_slots = Arc::new(Semaphore::new(1));
    let filler_slots = Arc::new(Semaphore::new(8));
    // Establish the eight-worker pool before holding all its active permits.
    let mut first_filler = saturate.then(|| {
        owned_payload::OwnedPayload::new(Counted(queued.clone()), host.handle(), &filler_slots)
    });
    let first_active = first_slots.clone().try_acquire_owned().unwrap();
    let mut active = Vec::new();
    for _ in 0..8 {
        active.push(filler_slots.clone().try_acquire_owned().unwrap());
    }
    let mut receipts = Vec::new();
    if saturate {
        for index in 0..owned_payload::MAX_PENDING_CLEANUPS_GLOBAL {
            let slots = if index == 0 {
                &first_slots
            } else {
                &filler_slots
            };
            let input = if index == 1 {
                first_filler.take().unwrap()
            } else {
                owned_payload::OwnedPayload::new(Counted(queued.clone()), host.handle(), slots)
            };
            receipts.push(input.retire());
        }
        assert_eq!(queued.load(Ordering::SeqCst), 0);
        assert!(owned_payload::try_pending_cleanup_permit(helper_cleanup_slots()).is_none());
    }
    let native_held = budget.try_reserve().is_err();
    let (started, ready) = mpsc::channel();
    let retained = (!last_owner).then(|| retirement.clone());
    let task = driver.spawn(async move {
        started.send(()).unwrap();
        if last_owner {
            drop(retirement);
        } else {
            retirement.join().await.unwrap();
        }
    });
    ready.recv_timeout(WAIT).unwrap();
    let deadline = std::time::Instant::now() + WAIT;
    while !helper.scheduled.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let scheduled = helper.scheduled.load(Ordering::Acquire);
    let (progress, observe_progress) = mpsc::channel();
    driver.spawn(async move {
        progress.send(()).unwrap();
    });
    let progressing = observe_progress
        .recv_timeout(Duration::from_millis(150))
        .is_ok();

    // Return exactly one pending permit; all 8191 other owners stay queued.
    drop(first_active);
    if saturate {
        host.block_on(receipts.remove(0)).unwrap();
    }
    let deadline = std::time::Instant::now() + WAIT;
    while helper.thread.lock().unwrap().is_some() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let claimed = helper.thread.lock().unwrap().is_none();
    let quota_held =
        !saturate || owned_payload::try_pending_cleanup_permit(helper_cleanup_slots()).is_none();
    let incomplete = helper.finished.borrow().is_none();
    let native_still_held = budget.try_reserve().is_err();
    drop(release);
    host.block_on(async {
        tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
        tokio::time::timeout(WAIT, wait(helper.finished.subscribe()))
            .await
            .unwrap()
            .unwrap();
        if let Some(retirement) = retained {
            retirement.join().await.unwrap();
            assert_eq!(retirement.owned_threads(1), 0);
        }
    });
    helper.join_blocking();
    drop(active);
    host.block_on(async {
        tokio::time::timeout(WAIT, async {
            for receipt in receipts {
                receipt.await.unwrap();
            }
        })
        .await
        .unwrap();
    });
    assert_eq!(exited.load(Ordering::SeqCst), 1);
    assert_eq!(
        queued.load(Ordering::SeqCst),
        if saturate { 8192 } else { 0 }
    );
    let restored = budget
        .try_reserve()
        .expect("actual helper OS join must return its lease");
    assert!(budget.try_reserve().is_err());
    restored.release_after_join();
    assert!(
        scheduled && claimed,
        "helper OS join was never actually claimed"
    );
    assert!(incomplete, "worker receipt bypassed helper TLS exit");
    assert!(
        progressing,
        "saturated helper join blocked the shared lifecycle driver"
    );
    assert!(
        native_held && native_still_held,
        "native leases escaped before actual helper OS join"
    );
    assert!(
        quota_held,
        "helper join lost the single admitted pending quota"
    );
    assert!(owned_payload::try_pending_cleanup_permit(helper_cleanup_slots()).is_some());
}
macro_rules! contract {
    ($name:ident, $last:expr, $saturate:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "actix::pool_retirement::saturation_contracts::",
                    stringify!($name)
                ),
                || check($last, $saturate),
            );
        }
    };
}
contract!(
    explicit_join_keeps_driver_live_under_real_global_saturation,
    false,
    true
);
contract!(
    last_owner_keeps_driver_live_under_real_global_saturation,
    true,
    true
);
contract!(
    native_lease_survives_worker_receipt_until_helper_os_join,
    false,
    false
);
