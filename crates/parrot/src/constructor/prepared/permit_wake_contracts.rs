use super::*;
use crate::owned_payload::permit_wake_contracts::quota;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Output(Arc<AtomicUsize>, bool);
impl Drop for Output {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "prepared quota original disposal witness");
    }
}

#[test]
fn prepared_discard_publishes_actual_result_after_multiple_quota_wake_panics() {
    crate::test_support::isolated(
        "constructor::prepared::permit_wake_contracts::prepared_discard_publishes_actual_result_after_multiple_quota_wake_panics",
        || {
            for failure in [false, true] {
                let host = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let drops = Arc::new(AtomicUsize::new(0));
                let moved = drops.clone();
                let factory = Factory::new(
                    move || Output(moved.clone(), failure),
                    Arc::new(Semaphore::new(1)),
                    Arc::new(Semaphore::new(1)),
                );
                let mut prepared = host
                    .block_on(factory.prepare::<()>(Arc::default()))
                    .unwrap();
                let (actor, invocation) = prepared.parts.take().unwrap();
                let (active, active_waiter) = quota();
                let (waiter, waiter_observer) = quota();
                let (finished, receipt) = watch::channel(None);
                let repeated = receipt.clone();
                let owner = Discard {
                    output: OutputLease::new(actor, invocation, None),
                    finished,
                    pending: None,
                    active: Some(active),
                    waiter: Some(waiter),
                };
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(owner))).is_ok()
                );
                for receipt in [receipt, repeated] {
                    let result = futures::executor::block_on(wait_receipt(receipt));
                    if failure {
                        assert!(
                            result
                                .unwrap_err()
                                .contains("prepared quota original disposal witness")
                        );
                    } else {
                        result.unwrap();
                    }
                }
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                drop(active_waiter.ready().unwrap());
                drop(waiter_observer.ready().unwrap());
            }
        },
    );
}
