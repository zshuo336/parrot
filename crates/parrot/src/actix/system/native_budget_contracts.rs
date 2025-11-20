use super::*;
use crate::thread::native_budget::NativeWorkerBudget;
use crate::thread::scheduler::sharded::ShardedScheduler;
use std::sync::{Condvar, mpsc};

const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    expired: std::sync::atomic::AtomicBool,
}
impl Gate {
    fn wait(&self) {
        let open = self.open.lock().unwrap();
        let (open, _) = self
            .changed
            .wait_timeout_while(open, WAIT, |open| !*open)
            .unwrap();
        if !*open {
            self.expired.store(true, Ordering::SeqCst);
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
fn pool(budget: Arc<NativeWorkerBudget>, size: usize) -> Result<ArbiterPool, SystemError> {
    ArbiterPool::with_distribution_and_budget(
        size,
        parrot_api::runtime::LoadBalancingStrategy::RoundRobin,
        budget,
    )
}
async fn progress(pool: &ArbiterPool) {
    let (sent, received) = tokio::sync::oneshot::channel();
    assert!(pool.next_arbiter().spawn(async move {
        sent.send(73).unwrap();
    }));
    assert_eq!(
        tokio::time::timeout(WAIT, received).await.unwrap().unwrap(),
        73
    );
}
async fn restored_pool(budget: Arc<NativeWorkerBudget>) -> ArbiterPool {
    tokio::time::timeout(WAIT, async {
        loop {
            match pool(budget.clone(), 1) {
                Ok(pool) => return pool,
                Err(error) => {
                    assert!(
                        error.to_string().contains("native worker budget exhausted"),
                        "{error}"
                    );
                    tokio::task::yield_now().await;
                }
            }
        }
    })
    .await
    .expect("OS join did not return capacity")
}

#[actix::test]
async fn thread_and_actix_pools_share_capacity_until_real_join() {
    let budget = Arc::new(NativeWorkerBudget::with_capacity(1));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let held = gate.clone();
    let (entered, observed) = mpsc::channel();
    let shards = ShardedScheduler::try_start_with_budget_and_startup(
        1,
        1,
        move || {
            entered.send(()).unwrap();
            held.wait();
        },
        budget.clone(),
    )
    .unwrap();
    observed.recv_timeout(WAIT).unwrap();
    let shutdown = std::thread::spawn(move || shards.shutdown_result());
    let rejected = pool(budget.clone(), 1).unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("native worker budget exhausted"),
        "{rejected}"
    );
    gate.release();
    shutdown.join().unwrap().unwrap();
    let recovered = pool(budget, 1).unwrap();
    progress(&recovered).await;
    recovered.stop_and_join().await.unwrap();
    assert!(!gate.expired.load(Ordering::SeqCst));
}

#[actix::test]
async fn dropped_sharded_pool_keeps_capacity_until_reaper_join() {
    let budget = Arc::new(NativeWorkerBudget::with_capacity(1));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let held = gate.clone();
    let (entered, observed) = mpsc::channel();
    let shards = ShardedScheduler::try_start_with_budget_and_startup(
        1,
        1,
        move || {
            entered.send(()).unwrap();
            held.wait();
        },
        budget.clone(),
    )
    .unwrap();
    observed.recv_timeout(WAIT).unwrap();
    drop(shards);
    assert!(pool(budget.clone(), 1).is_err());
    gate.release();
    let recovered = restored_pool(budget).await;
    progress(&recovered).await;
    recovered.stop_and_join().await.unwrap();
    assert!(!gate.expired.load(Ordering::SeqCst));
}

struct Held(Arc<Gate>, mpsc::Sender<()>);
impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.1.send(());
        self.0.wait();
    }
}

#[actix::test]
async fn actix_retirement_blocks_thread_admission_after_waiter_cancellation() {
    let budget = Arc::new(NativeWorkerBudget::with_capacity(1));
    let active = pool(budget.clone(), 1).unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (entered, observed) = mpsc::channel();
    let held = Held(gate.clone(), entered);
    let (started, ready) = mpsc::channel();
    assert!(active.next_arbiter().spawn(async move {
        let _held = held;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    }));
    ready.recv_timeout(WAIT).unwrap();
    let mut closing = Box::pin(active.stop_and_join());
    assert!(futures::poll!(&mut closing).is_pending());
    drop(closing);
    observed.recv_timeout(WAIT).unwrap();
    assert!(
        ShardedScheduler::try_start_with_budget_and_startup(1, 1, || {}, budget.clone()).is_err()
    );
    gate.release();
    tokio::time::timeout(WAIT, active.stop_and_join())
        .await
        .unwrap()
        .unwrap();
    let shards =
        ShardedScheduler::try_start_with_budget_and_startup(1, 1, || {}, budget.clone()).unwrap();
    shards.shutdown_result().unwrap();
    let recovered = pool(budget, 1).unwrap();
    progress(&recovered).await;
    recovered.stop_and_join().await.unwrap();
    assert!(!gate.expired.load(Ordering::SeqCst));
}

#[actix::test]
async fn partial_pool_budget_failure_returns_every_started_workers_capacity() {
    let budget = Arc::new(NativeWorkerBudget::with_capacity(1));
    let error = pool(budget.clone(), 2).unwrap_err();
    assert!(
        error.to_string().contains("native worker budget exhausted"),
        "{error}"
    );
    let error = ShardedScheduler::try_start_with_budget_and_startup(2, 1, || {}, budget.clone())
        .unwrap_err();
    assert!(
        error.to_string().contains("native worker budget exhausted"),
        "{error}"
    );
    let recovered = pool(budget.clone(), 1).unwrap();
    progress(&recovered).await;
    recovered.stop_and_join().await.unwrap();
    let reservation = budget.try_reserve().unwrap();
    assert!(budget.try_reserve().is_err());
    reservation.release_after_join();
}
