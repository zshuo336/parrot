use super::*;
use crate::owned_payload::permit_wake_contracts::quota;
use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy};
use std::sync::atomic::AtomicUsize;

struct Capture(Arc<AtomicUsize>, bool);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "policy quota original disposal witness");
    }
}

#[test]
fn policy_disposal_keeps_actual_result_when_worker_quota_wake_panics() {
    crate::test_support::isolated(
        "supervision_policy::permit_wake_contracts::policy_disposal_keeps_actual_result_when_worker_quota_wake_panics",
        || {
            for failure in [false, true] {
                let drops = Arc::new(AtomicUsize::new(0));
                let capture = Capture(drops.clone(), failure);
                let strategy =
                    OwnedStrategy::new(SupervisorStrategyType::OneForOne(OneForOneStrategy {
                        max_restarts: 0,
                        within: std::time::Duration::from_secs(1),
                        decider: BasicDecisionFn::new(move |_| {
                            std::hint::black_box(&capture);
                            panic!("disposal must not invoke the decider");
                        }),
                    }));
                let (active, observer) = quota();
                let (finished, receipt) = watch::channel(None);
                let repeated = receipt.clone();
                let owner = Disposal {
                    strategy: Some(strategy),
                    finished,
                    pending: None,
                    active: Some(active),
                };
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(owner))).is_ok()
                );
                for mut receipt in [receipt, repeated] {
                    let result = receipt
                        .borrow_and_update()
                        .clone()
                        .expect("actual policy receipt was not published");
                    if failure {
                        assert!(
                            result
                                .unwrap_err()
                                .contains("policy quota original disposal witness")
                        );
                    } else {
                        result.unwrap();
                    }
                }
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                drop(observer.ready().unwrap());
            }
        },
    );
}
