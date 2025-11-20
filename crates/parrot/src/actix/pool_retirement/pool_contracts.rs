use super::*;
use futures::FutureExt;
use std::sync::{Condvar, mpsc};

const WAIT: Duration = Duration::from_secs(8);
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
    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Held {
    gate: Arc<Gate>,
    entered: mpsc::Sender<()>,
    exited: mpsc::Sender<()>,
}
impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        self.gate.wait();
        let _ = self.exited.send(());
    }
}

#[actix::test]
async fn repeated_pool_shutdown_waits_for_the_same_real_worker_retirement_after_cancellation() {
    let pool = ArbiterPool::new(1);
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (entered, receive_entered) = mpsc::channel();
    let (exited, receive_exited) = mpsc::channel();
    let (started, receive_started) = mpsc::channel();
    let held = Held {
        gate: gate.clone(),
        entered,
        exited,
    };
    assert!(pool.next_arbiter().spawn(async move {
        let _held = held;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    }));
    receive_started.recv_timeout(WAIT).unwrap();
    let mut first = Box::pin(pool.stop_and_join());
    assert!(futures::poll!(&mut first).is_pending());
    drop(first);
    let entered = receive_entered.recv_timeout(WAIT);
    let early = pool.stop_and_join().now_or_never();
    gate.release();
    tokio::time::timeout(WAIT, pool.stop_and_join())
        .await
        .unwrap()
        .unwrap();
    assert!(entered.is_ok());
    assert!(
        early.is_none(),
        "second pool waiter bypassed an in-flight worker join"
    );
    receive_exited.try_recv().unwrap();
    pool.stop_and_join().await.unwrap();
}

#[actix::test]
async fn last_pool_reference_dropped_on_its_own_worker_keeps_external_retirement_receipt() {
    let pool = ArbiterPool::new(1);
    let worker = pool.next_arbiter();
    let receipt = pool.inner.retirement.receipt();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (entered, receive_entered) = mpsc::channel();
    let (exited, receive_exited) = mpsc::channel();
    let (dropped, receive_dropped) = mpsc::channel();
    let held = Held {
        gate: gate.clone(),
        entered,
        exited,
    };
    assert!(worker.spawn(async move {
        let _held = held;
        drop(pool);
        dropped.send(()).unwrap();
        std::future::pending::<()>().await;
    }));
    let returned = receive_dropped.recv_timeout(WAIT);
    let entered = receive_entered.recv_timeout(WAIT);
    let mut receipt = Box::pin(receipt);
    let early = futures::poll!(&mut receipt).is_ready();
    gate.release();
    worker.stop();
    tokio::time::timeout(WAIT, receipt).await.unwrap().unwrap();
    assert!(
        returned.is_ok(),
        "last pool reference attempted to join its own worker"
    );
    assert!(entered.is_ok());
    assert!(
        !early,
        "last-owner receipt preceded blocked worker disposal"
    );
    receive_exited.try_recv().unwrap();
    assert!(!worker.alive());
}

#[actix::test]
async fn native_status_counts_retirement_thread_until_explicit_shutdown_joins_it() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    assert_eq!(system.status().resources.thread_count, 2);
    system.clone().shutdown().await.unwrap();
    assert_eq!(system.status().resources.thread_count, 0);
    assert_eq!(system.status().state, SystemState::Stopped);
}
