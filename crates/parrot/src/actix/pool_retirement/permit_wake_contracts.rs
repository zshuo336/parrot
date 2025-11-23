use super::*;
use crate::owned_payload::permit_wake_contracts::quota;

#[test]
fn helper_join_keeps_actual_receipt_after_cleanup_quota_wake_panics() {
    crate::test_support::isolated(
        "actix::pool_retirement::permit_wake_contracts::helper_join_keeps_actual_receipt_after_cleanup_quota_wake_panics",
        || {
            for failure in [false, true] {
                let (active, observer) = quota();
                let (finished, receipt) = watch::channel(None);
                let repeated = receipt.clone();
                let budget =
                    Arc::new(crate::thread::native_budget::NativeWorkerBudget::with_capacity(1));
                let reservation = budget.try_reserve().unwrap();
                let worker = std::thread::spawn(move || {
                    assert!(!failure, "helper quota original join witness");
                });
                let helper = HelperJoin {
                    thread: Mutex::new(Some(worker)),
                    reservations: Arc::new(Mutex::new(vec![reservation])),
                    finished,
                    scheduled: AtomicBool::new(false),
                    changed: Condvar::new(),
                    cleanup: Mutex::new(HelperCleanup {
                        closed: false,
                        pending: None,
                        active: Some(active),
                    }),
                };
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || helper.join_blocking()
                    ))
                    .is_ok()
                );
                for receipt in [receipt, repeated] {
                    let result = futures::executor::block_on(wait(receipt));
                    if failure {
                        assert!(
                            result
                                .unwrap_err()
                                .contains("helper quota original join witness")
                        );
                    } else {
                        result.unwrap();
                    }
                }
                drop(observer.ready().unwrap());
                budget.try_reserve().unwrap().release_after_join();
                helper.join_blocking();
            }
        },
    );
}
