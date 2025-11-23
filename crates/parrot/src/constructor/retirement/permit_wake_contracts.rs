use super::*;
use crate::owned_payload::permit_wake_contracts::quota;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn invocation_quota_wake_runs_after_releasing_factory_state_lock() {
    use std::{
        future::Future,
        sync::atomic::AtomicBool,
        task::{Context, Wake, Waker},
    };
    struct Observe {
        state: Arc<Mutex<FactoryState<()>>>,
        calls: AtomicUsize,
        locked: AtomicBool,
    }
    impl Wake for Observe {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.locked
                .store(self.state.try_lock().is_err(), Ordering::SeqCst);
            panic!("invocation quota wake witness");
        }
    }
    let factory = Factory::new(
        || (),
        Arc::new(Semaphore::new(1)),
        Arc::new(Semaphore::new(1)),
    );
    let slots = Arc::new(Semaphore::new(1));
    let cleanup = Arc::new(slots.clone().try_acquire_owned().unwrap());
    let observer = Arc::new(Observe {
        state: factory.state.clone(),
        calls: AtomicUsize::new(0),
        locked: AtomicBool::new(false),
    });
    let waker = Waker::from(observer.clone());
    let mut waiting = Box::pin(slots.clone().acquire_owned());
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let function = {
        let mut state = factory.state.lock().unwrap();
        state.active = 1;
        state.function.clone()
    };
    let mut invocation = Invocation {
        function,
        state: factory.state.clone(),
        changed: factory.changed.clone(),
        cleanup: Some(cleanup),
    };
    invocation_finished(&mut invocation).unwrap();
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    assert!(
        !observer.locked.load(Ordering::SeqCst),
        "quota waker ran under factory state lock"
    );
    assert_eq!(factory.state.lock().unwrap().active, 0);
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
}

struct Capture(Arc<AtomicUsize>, bool);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "factory quota original disposal witness");
    }
}

#[test]
fn capture_retirement_keeps_receipts_after_multiple_quota_wake_panics() {
    crate::test_support::isolated(
        "constructor::retirement::permit_wake_contracts::capture_retirement_keeps_receipts_after_multiple_quota_wake_panics",
        || {
            for failure in [false, true] {
                let drops = Arc::new(AtomicUsize::new(0));
                let capture = Capture(drops.clone(), failure);
                let (creation, creation_waiter) = quota();
                let (cleanup, cleanup_waiter) = quota();
                let (finished, receipt) = watch::channel(None);
                let repeated = receipt.clone();
                let owner = CaptureDisposal {
                    context: None,
                    function: Some(Arc::new(move || {
                        std::hint::black_box(&capture);
                    })),
                    failure: None,
                    finished: Some(finished),
                    cleanup: Some(Arc::new(cleanup)),
                    pending: None,
                    creation_reservation: Some(Arc::new(creation)),
                };
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(owner))).is_ok()
                );
                for receipt in [receipt, repeated] {
                    let result = futures::executor::block_on(wait(receipt));
                    if failure {
                        assert!(
                            result
                                .unwrap_err()
                                .contains("factory quota original disposal witness")
                        );
                    } else {
                        result.unwrap();
                    }
                }
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                drop(creation_waiter.ready().unwrap());
                drop(cleanup_waiter.ready().unwrap());
            }
        },
    );
}
