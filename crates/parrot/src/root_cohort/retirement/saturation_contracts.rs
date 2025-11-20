use super::*;
use crate::{owned_payload, test_support::isolated};
use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision};
use std::sync::{
    Condvar,
    atomic::{AtomicUsize, Ordering},
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
struct Capture(Arc<Gate>);
impl Capture {
    fn decide(&self) -> SupervisionDecision {
        SupervisionDecision::Stop
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let (open, _) = self
            .0
            .changed
            .wait_timeout_while(self.0.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(*open, "test must release the policy destructor");
        self.0.drops.fetch_add(1, Ordering::SeqCst);
        panic!("budgeted root policy disposal failure");
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn saturated_retirement(global: bool) {
    let host = runtime();
    let gate = Arc::new(Gate::default());
    // Release blocked user code before shutting down any runtime, even on failure.
    let release = Release(gate.clone());
    let owner = Arc::new(RootCohorts::new().unwrap());
    let healthy = Arc::new(RootCohorts::new().unwrap());
    let slots = Arc::new(Semaphore::new(1));
    let capture = Capture(gate.clone());
    let group = host
        .block_on(owner.create(
            SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 0,
                within: WAIT,
                decider: BasicDecisionFn::new(move |_| capture.decide()),
            }),
            WAIT,
            slots.clone(),
        ))
        .unwrap();
    let independent = host
        .block_on(healthy.create(
            SupervisorStrategyType::default(),
            WAIT,
            Arc::new(Semaphore::new(1)),
        ))
        .unwrap();
    let filler = if global {
        Arc::new(Semaphore::new(8))
    } else {
        slots.clone()
    };
    let limit = if global {
        owned_payload::MAX_PENDING_CLEANUPS_GLOBAL
    } else {
        owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT
    };
    let mut permits: Vec<_> = (0..limit)
        .map(|_| owned_payload::try_pending_cleanup_permit(&filler).unwrap())
        .collect();
    assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
    // Cancelling this public waiter must not cancel process-owned retirement.
    drop(group.stop());
    host.block_on(async {
        // The observer's timer runs on an independent host. A timer on the
        // stalled retirement driver could never diagnose its own starvation.
        let early_drop =
            tokio::time::timeout(Duration::from_millis(150), gate.entered.notified()).await;
        assert!(
            early_drop.is_err(),
            "saturation executed user Drop before cleanup admission"
        );
        tokio::time::timeout(WAIT, independent.stop())
            .await
            .expect("one saturated system blocked independent root retirement")
            .unwrap();
        tokio::time::timeout(WAIT, healthy.retirement_runtime().spawn(async { 17 }))
            .await
            .expect("shared lifecycle driver stopped progressing")
            .unwrap();
        assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
    });
    // Exit the original observer host while admission is still blocked.
    drop(host);
    let observer = runtime();
    observer.block_on(async {
        let mut first = Box::pin(group.stop());
        let mut second = Box::pin(group.stop());
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        // Only one freed slot is sufficient; all other owners remain retained.
        drop(permits.pop());
        tokio::time::timeout(WAIT, gate.entered.notified())
            .await
            .expect("one returned cleanup slot did not admit retirement");
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        tokio::time::timeout(WAIT, healthy.retirement_runtime().spawn(async { 23 }))
            .await
            .expect("admitted blocking destructor stalled lifecycle driver")
            .unwrap();
        drop(release);
        let (first, second) = tokio::time::timeout(WAIT, async { tokio::join!(first, second) })
            .await
            .expect("actual disposal did not finish shared receipts");
        let first = first.unwrap_err().to_string();
        assert!(
            first.contains("budgeted root policy disposal failure"),
            "{first}"
        );
        assert_eq!(first, second.unwrap_err().to_string());
        assert_eq!(first, group.stop().await.unwrap_err().to_string());
        assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
    });
    drop(permits);
    observer.block_on(async {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(permit) = owned_payload::try_pending_cleanup_permit(&slots) {
                    drop(permit);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup capacity was not recovered");
    });
}

#[test]
fn local_cleanup_saturation_preserves_independent_root_retirement() {
    isolated(
        "root_cohort::retirement::saturation_contracts::local_cleanup_saturation_preserves_independent_root_retirement",
        || saturated_retirement(false),
    );
}

#[test]
fn global_cleanup_saturation_preserves_independent_root_retirement() {
    isolated(
        "root_cohort::retirement::saturation_contracts::global_cleanup_saturation_preserves_independent_root_retirement",
        || saturated_retirement(true),
    );
}
