use super::*;
use std::{
    cell::RefCell,
    sync::{Condvar, mpsc},
};

const CASE: &str = "actix::system::worker_shutdown_contracts::owned_worker_can_initiate_shutdown_after_host_exit_without_self_join";
const CHILD: &str = "PARROT_ACTIX_WORKER_SHUTDOWN_CHILD";

struct Disposed(Arc<AtomicUsize>);
impl Drop for Disposed {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

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
struct ThreadExit {
    gate: Arc<Gate>,
    entered: mpsc::Sender<()>,
    exited: Arc<AtomicUsize>,
}
impl Drop for ThreadExit {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        self.gate.wait();
        self.exited.fetch_add(1, Ordering::SeqCst);
    }
}
thread_local! { static THREAD_EXIT: RefCell<Option<ThreadExit>> = const { RefCell::new(None) }; }

fn owned_worker_shutdown() {
    let host = actix::System::new();
    let mut system = host
        .block_on(ActixActorSystem::with_arbiter_count(1))
        .unwrap();
    // Preserve the rejected-lifecycle-executor self-join regression.
    host.block_on(async {
        Arc::get_mut(&mut system.root_cohorts)
            .unwrap()
            .set_test_retirement_runtime(tokio::runtime::Handle::current());
    });
    let moved = system.clone();
    let disposed = Arc::new(AtomicUsize::new(0));
    let held = Disposed(disposed.clone());
    let gate = Arc::new(Gate::default());
    let _release_gate = Release(gate.clone());
    let exited = Arc::new(AtomicUsize::new(0));
    let (entered, receive_entered) = mpsc::channel();
    let local = ThreadExit {
        gate: gate.clone(),
        entered,
        exited: exited.clone(),
    };
    let (release, released) = tokio::sync::oneshot::channel();
    let (started, receive_started) = mpsc::channel();
    let (polled, receive_polled) = mpsc::channel();
    assert!(system.arbiters.inner.workers[0].spawn(async move {
        let _held = held;
        THREAD_EXIT.with(|slot| *slot.borrow_mut() = Some(local));
        started.send(()).unwrap();
        released.await.unwrap();
        let mut shutdown = Box::pin(moved.shutdown());
        let first = futures::poll!(&mut shutdown);
        let _ = polled.send(first.is_pending());
        drop(shutdown);
    }));
    receive_started
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    drop(host);
    release.send(()).unwrap();
    let reached = receive_polled.recv_timeout(Duration::from_secs(2));
    let disposing = receive_entered.recv_timeout(Duration::from_secs(2));
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let early = observer.block_on(async {
        tokio::time::timeout(Duration::from_millis(100), system.clone().shutdown()).await
    });
    let status_before_release = system.status();
    let exited_before_release = exited.load(Ordering::SeqCst);
    gate.release();
    let result = observer
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(5), system.clone().shutdown()).await
        })
        .expect("owned-worker shutdown lost its independent completion owner");
    let error = result
        .expect_err("destroyed-host cancellation claimed graceful success")
        .to_string();
    assert!(
        !error.contains("worker failed during shutdown"),
        "owned-worker shutdown lost an actual OS join: {error}"
    );
    assert!(error.contains("without graceful completion"), "{error}");
    assert!(
        reached.unwrap(),
        "owned-worker first poll must yield before its own OS join"
    );
    assert!(disposing.is_ok(), "worker never reached TLS destruction");
    assert!(
        early.is_err(),
        "terminal result preceded actual worker TLS exit"
    );
    assert_eq!(status_before_release.state, SystemState::ShuttingDown);
    assert_eq!(status_before_release.resources.thread_count, 2);
    assert_eq!(exited_before_release, 0);
    assert_eq!(exited.load(Ordering::SeqCst), 1);
    let status = system.status();
    assert_eq!(status.state, SystemState::Stopped);
    assert_eq!(status.resources.thread_count, 0);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    println!("PARROT_ACTIX_WORKER_SHUTDOWN_RETIRED=1");
}

#[test]
fn last_system_reference_on_owned_worker_retains_external_terminal_receipt() {
    let host = actix::System::new();
    let mut system = host
        .block_on(ActixActorSystem::with_arbiter_count(1))
        .unwrap();
    host.block_on(async {
        Arc::get_mut(&mut system.root_cohorts)
            .unwrap()
            .set_test_retirement_runtime(tokio::runtime::Handle::current());
    });
    let registry = system.registry.clone();
    let pool = system.arbiters.clone();
    let worker = pool.inner.workers[0].clone();
    let (release, receive_release) = tokio::sync::oneshot::channel();
    let (started, receive_started) = mpsc::channel();
    let (polled, receive_polled) = mpsc::channel();
    assert!(worker.spawn(async move {
        started.send(()).unwrap();
        receive_release.await.unwrap();
        let mut shutdown = Box::pin(system.shutdown());
        let first = futures::poll!(&mut shutdown);
        drop(shutdown);
        let _ = polled.send(first.is_pending());
    }));
    receive_started
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    drop(host);
    release.send(()).unwrap();
    let polled = receive_polled.recv_timeout(Duration::from_secs(2));
    let receipt = registry.read().unwrap().shutdown.clone();
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let observed = observer
        .block_on(async {
            let receipt = receipt.expect("shutdown did not install a shared terminal receipt");
            tokio::time::timeout(Duration::from_secs(5), await_completion(receipt)).await
        })
        .expect("last system reference abandoned terminal cleanup");
    observer.block_on(pool.stop_and_join()).unwrap();
    let error = observed.unwrap_err().to_string();
    assert!(
        polled.unwrap(),
        "last system reference synchronously self-joined"
    );
    assert!(error.contains("without graceful completion"), "{error}");
    assert!(!error.contains("worker failed during shutdown"), "{error}");
    assert!(registry.read().unwrap().stopped);
    assert_eq!(pool.inner.retirement.owned_threads(1), 0);
}

#[actix::test]
async fn owned_worker_shutdown_with_live_host_keeps_normal_success() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let moved = system.clone();
    let (polled, received) = tokio::sync::oneshot::channel();
    assert!(system.arbiters.inner.workers[0].spawn(async move {
        let mut shutdown = Box::pin(moved.shutdown());
        let first = futures::poll!(&mut shutdown);
        drop(shutdown);
        let _ = polled.send(first.is_pending());
    }));
    assert!(received.await.unwrap());
    tokio::time::timeout(Duration::from_secs(5), system.clone().shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(system.status().state, SystemState::Stopped);
    assert_eq!(system.status().resources.thread_count, 0);
}

#[test]
fn owned_worker_can_initiate_shutdown_after_host_exit_without_self_join() {
    if std::env::var_os(CHILD).is_some() {
        owned_worker_shutdown();
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", CASE, "--nocapture"])
                    .env(CHILD, "1")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
        })
        .expect("owned-worker regression child did not terminate")
        .unwrap();
    assert!(
        output.status.success(),
        "owned-worker child failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("PARROT_ACTIX_WORKER_SHUTDOWN_RETIRED=1")
    );
}
