use super::configuration_contracts::stack;
use super::*;
use crate::{
    actix::ArbiterPool, test_support::isolated, thread::native_budget::NativeWorkerBudget,
};
use std::time::Duration;

enum Live {
    Dedicated(Box<DedicatedThreadScheduler>),
    Sharded(Arc<ShardedScheduler>),
    Actix(ArbiterPool),
}
impl Live {
    async fn close(self) {
        match self {
            Self::Dedicated(scheduler) => scheduler.shutdown().await.unwrap(),
            Self::Sharded(scheduler) => scheduler.shutdown_result().unwrap(),
            Self::Actix(pool) => drop(pool),
        }
    }
}

async fn start(kind: usize) -> Live {
    if kind == 2 {
        let pool = ArbiterPool::try_new(1).unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        assert!(pool.next_arbiter().spawn(async move {
            sent.send(73).unwrap();
        }));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), received)
                .await
                .unwrap()
                .unwrap(),
            73
        );
        return Live::Actix(pool);
    }
    let (mailbox, processor) = stack("/global-budget");
    let live = if kind == 0 {
        let scheduler = DedicatedThreadScheduler::try_new(None)
            .unwrap()
            .with_runtime_handle(Handle::current());
        scheduler
            .schedule_with_processor(
                "/global-budget",
                mailbox.clone(),
                processor.clone(),
                Default::default(),
            )
            .unwrap();
        Live::Dedicated(Box::new(scheduler))
    } else {
        let scheduler = ShardedScheduler::try_start(1, 1).unwrap();
        scheduler
            .schedule_affinity("/global-budget", mailbox.clone(), "global")
            .unwrap();
        Live::Sharded(scheduler)
    };
    mailbox
        .push(
            Box::new(73u64),
            crate::thread::config::BackpressureStrategy::Block,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while processor.values.lock().unwrap().is_empty() {
            processor.changed.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*processor.values.lock().unwrap(), vec![73]);
    live
}

async fn rejected(kind: usize) {
    let error = match kind {
        0 => {
            let scheduler = DedicatedThreadScheduler::try_new(None).unwrap();
            let (mailbox, processor) = stack("/rejected-global");
            let error = scheduler
                .schedule_with_processor(
                    "/rejected-global",
                    mailbox,
                    processor.clone(),
                    Default::default(),
                )
                .unwrap_err()
                .to_string();
            assert_eq!(scheduler.worker_count(), 0);
            assert!(!processor.is_initialized());
            scheduler.shutdown().await.unwrap();
            error
        }
        1 => ShardedScheduler::try_start(1, 1).unwrap_err().to_string(),
        2 => ArbiterPool::try_new(1).unwrap_err().to_string(),
        _ => unreachable!(),
    };
    assert!(error.contains("native worker budget exhausted"), "{error}");
}

async fn capacity_recovers(budget: &Arc<NativeWorkerBudget>) {
    let reservation = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(reservation) = budget.try_reserve() {
                break reservation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("normal constructor's OS join did not return global capacity");
    reservation.release_after_join();
}

#[test]
fn normal_native_worker_constructors_share_the_real_process_budget() {
    isolated(
        "thread::scheduler::global_budget_contracts::normal_native_worker_constructors_share_the_real_process_budget",
        || {
            actix::System::new().block_on(async {
                let budget = NativeWorkerBudget::global();
                let capacity = parrot_config::ThreadResolved::MAX_WORKERS;
                // Reserve all but one real global slot without spawning 4095 OS threads.
                // Every constructor below is the ordinary production entry point.
                let reserved: Vec<_> = (1..capacity)
                    .map(|_| budget.try_reserve().unwrap())
                    .collect();
                for winner in 0..3 {
                    let live = start(winner).await;
                    assert!(
                        budget.try_reserve().is_err(),
                        "constructor {winner} bypassed the singleton"
                    );
                    for kind in 0..3 {
                        rejected(kind).await;
                    }
                    live.close().await;
                    capacity_recovers(&budget).await;
                    for partial in [
                        ShardedScheduler::try_start(2, 1).unwrap_err().to_string(),
                        ArbiterPool::try_new(2).unwrap_err().to_string(),
                    ] {
                        assert!(
                            partial.contains("native worker budget exhausted"),
                            "{partial}"
                        );
                        capacity_recovers(&budget).await;
                    }
                    for kind in 0..3 {
                        start(kind).await.close().await;
                        capacity_recovers(&budget).await;
                    }
                }
                for reservation in reserved {
                    reservation.release_after_join();
                }
                let all: Vec<_> = (0..capacity)
                    .map(|_| budget.try_reserve().unwrap())
                    .collect();
                assert!(budget.try_reserve().is_err());
                for reservation in all {
                    reservation.release_after_join();
                }
            });
        },
    );
}
