use super::*;
use futures::FutureExt;
use std::{
    pin::Pin,
    sync::{
        Arc, Condvar,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Work {
    index: usize,
    gate: Arc<Gate>,
    peer: Option<Arc<Gate>>,
    started: Option<mpsc::Sender<()>>,
    disposing: mpsc::Sender<usize>,
    disposed: mpsc::Sender<usize>,
}
impl Future for Work {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
        }
        Poll::Pending
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        let _ = self.disposing.send(self.index);
        if self.index == 1
            && let Some(peer) = &self.peer
        {
            peer.release();
        }
        self.gate.wait();
        if self.index == 0
            && let Some(peer) = &self.peer
        {
            peer.wait();
        }
        let _ = self.disposed.send(self.index);
    }
}
struct Workers {
    owners: Vec<actix::Arbiter>,
    handles: Vec<actix::ArbiterHandle>,
    disposing: mpsc::Receiver<usize>,
    disposed: mpsc::Receiver<usize>,
}
fn live_workers(count: usize, gate: &Arc<Gate>, peer: Option<&Arc<Gate>>) -> Workers {
    let (disposing, receive_disposing) = mpsc::channel();
    let (disposed, receive_disposed) = mpsc::channel();
    let mut owners = Vec::new();
    let mut handles = Vec::new();
    for index in 0..count {
        let owner = actix::Arbiter::try_new().unwrap();
        let (started, receive) = mpsc::channel();
        assert!(owner.spawn(Work {
            index,
            gate: gate.clone(),
            peer: peer.cloned(),
            started: Some(started),
            disposing: disposing.clone(),
            disposed: disposed.clone(),
        }));
        receive.recv_timeout(WAIT).unwrap();
        handles.push(owner.handle());
        owners.push(owner);
    }
    Workers {
        owners,
        handles,
        disposing: receive_disposing,
        disposed: receive_disposed,
    }
}

fn failed_dispatch(panics: bool) {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (ready, receive_ready) = mpsc::channel();
    let (result, receive_result) = mpsc::channel();
    let moved_gate = gate.clone();
    let thread = std::thread::spawn(move || {
        actix::System::new().block_on(async move {
            let workers = live_workers(2, &moved_gate, None);
            ready
                .send((workers.handles, workers.disposing, workers.disposed))
                .unwrap();
            let retired = Retirement::with_spawner(workers.owners, move |_job| {
                if panics {
                    panic!("retirement dispatch panic witness");
                }
                Err(std::io::Error::other("retirement dispatch error witness"))
            });
            let _ = result.send(retired.map(drop));
        });
    });
    let (handles, disposing, disposed) = receive_ready.recv_timeout(WAIT).unwrap();
    let entered = disposing.recv_timeout(WAIT);
    let early = receive_result.try_recv().ok();
    let was_early = early.is_some();
    for handle in &handles {
        handle.stop();
    }
    gate.release();
    let result = early.unwrap_or_else(|| receive_result.recv_timeout(WAIT).unwrap());
    thread.join().unwrap();
    let mut actual = [
        disposed.recv_timeout(WAIT).unwrap(),
        disposed.recv_timeout(WAIT).unwrap(),
    ];
    actual.sort_unstable();
    assert!(entered.is_ok(), "failed dispatch did not stop worker tasks");
    assert!(
        !was_early,
        "failed dispatch returned before actual worker retirement"
    );
    let error = result.unwrap_err();
    assert!(
        matches!(error, SystemError::InitializationError(_)),
        "{error}"
    );
    assert!(
        error.to_string().contains(if panics {
            "retirement dispatch panic witness"
        } else {
            "retirement dispatch error witness"
        }),
        "{error}"
    );
    assert_eq!(actual, [0, 1]);
    assert!(handles.iter().all(|handle| !handle.alive()));
}

#[test]
fn retirement_thread_creation_error_joins_every_worker_before_returning() {
    failed_dispatch(false);
}
#[test]
fn retirement_thread_factory_panic_joins_every_worker_before_returning() {
    failed_dispatch(true);
}

#[actix::test]
async fn healthy_prestarted_owner_waits_for_request_and_actual_disposal() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let workers = live_workers(2, &gate, None);
    let retirement = Retirement::new(workers.owners).unwrap();
    assert!(workers.handles.iter().all(actix::ArbiterHandle::alive));
    assert!(retirement.receipt().now_or_never().is_none());
    retirement.request();
    let entered = workers.disposing.recv_timeout(WAIT);
    let early = retirement.receipt().now_or_never();
    gate.release();
    tokio::time::timeout(WAIT, retirement.receipt())
        .await
        .unwrap()
        .unwrap();
    assert!(entered.is_ok());
    assert!(
        early.is_none(),
        "stop signal was mistaken for actual retirement"
    );
    let mut actual = [
        workers.disposed.try_recv().unwrap(),
        workers.disposed.try_recv().unwrap(),
    ];
    actual.sort_unstable();
    assert_eq!(actual, [0, 1]);
    assert!(workers.handles.iter().all(|handle| !handle.alive()));
    retirement.receipt().await.unwrap();
}

#[actix::test]
async fn cancelled_receipt_does_not_cancel_retirement_or_let_later_waiters_finish_early() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let workers = live_workers(1, &gate, None);
    let retirement = Retirement::new(workers.owners).unwrap();
    retirement.request();
    let mut first = Box::pin(retirement.receipt());
    assert!(futures::poll!(&mut first).is_pending());
    drop(first);
    let entered = workers.disposing.recv_timeout(WAIT);
    retirement.request();
    let later = retirement.receipt().now_or_never();
    gate.release();
    tokio::time::timeout(WAIT, retirement.receipt())
        .await
        .unwrap()
        .unwrap();
    assert!(entered.is_ok());
    assert!(
        later.is_none(),
        "later observer returned before the shared join completed"
    );
    assert_eq!(workers.disposed.try_recv().unwrap(), 0);
    retirement.receipt().await.unwrap();
}

#[actix::test]
async fn retirement_stops_all_workers_before_joining_interdependent_disposal() {
    let gate = Arc::new(Gate::default());
    let peer = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let _peer_release = Release(peer.clone());
    let workers = live_workers(2, &gate, Some(&peer));
    let retirement = Retirement::new(workers.owners).unwrap();
    retirement.request();
    let first = workers.disposing.recv_timeout(WAIT);
    let second = workers.disposing.recv_timeout(WAIT);
    for handle in &workers.handles {
        handle.stop();
    }
    gate.release();
    peer.release();
    tokio::time::timeout(WAIT, retirement.receipt())
        .await
        .unwrap()
        .unwrap();
    assert!(
        first.is_ok() && second.is_ok(),
        "retirement joined a worker before stopping its peer"
    );
    assert_eq!(workers.disposed.try_iter().count(), 2);
}

fn failed_workers() -> (Vec<actix::Arbiter>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("retirement payload destructor witness");
        }
    }
    struct Healthy(Arc<AtomicUsize>);
    impl Drop for Healthy {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let payloads = Arc::new(AtomicUsize::new(0));
    let healthy = Arc::new(AtomicUsize::new(0));
    let mut owners = Vec::new();
    for _ in 0..2 {
        let armed = Arc::new(AtomicBool::new(false));
        let parked = armed.clone();
        let payloads = payloads.clone();
        let (send, receive) = mpsc::channel();
        let owner = actix::Arbiter::try_with_tokio_rt(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .on_thread_park(move || {
                    if parked.swap(false, Ordering::SeqCst) {
                        send.send(()).unwrap();
                        std::panic::panic_any(Payload(payloads.clone()));
                    }
                })
                .build()
        })
        .unwrap();
        assert!(owner.spawn(async move {
            armed.store(true, Ordering::SeqCst);
        }));
        receive.recv_timeout(WAIT).unwrap();
        owners.push(owner);
    }
    let owner = actix::Arbiter::try_new().unwrap();
    let held = Healthy(healthy.clone());
    let (send, receive) = mpsc::channel();
    assert!(owner.spawn(async move {
        let _held = held;
        send.send(()).unwrap();
        std::future::pending::<()>().await;
    }));
    receive.recv_timeout(WAIT).unwrap();
    owners.push(owner);
    (owners, payloads, healthy)
}

#[actix::test]
async fn worker_failures_retire_hostile_payloads_and_preserve_one_stable_shared_result() {
    let (owners, payloads, healthy) = failed_workers();
    let retirement = Retirement::new(owners).unwrap();
    retirement.request();
    let first = tokio::time::timeout(WAIT, retirement.receipt())
        .await
        .unwrap()
        .unwrap_err();
    let second = retirement.receipt().await.unwrap_err();
    assert!(matches!(first, ActorError::InternalError(_)), "{first}");
    assert_eq!(first.to_string(), second.to_string());
    assert!(first.to_string().contains("2 worker joins"), "{first}");
    assert!(first.to_string().contains("worker 0"), "{first}");
    assert!(
        first
            .to_string()
            .contains("panic payload destructor panicked"),
        "{first}"
    );
    assert_eq!(payloads.load(Ordering::SeqCst), 2);
    assert_eq!(
        healthy.load(Ordering::SeqCst),
        1,
        "later healthy worker was skipped"
    );
}

#[actix::test]
async fn failed_retirement_dispatch_preserves_setup_and_worker_cleanup_failures() {
    let (owners, payloads, healthy) = failed_workers();
    let result = Retirement::with_spawner(owners, |_job| {
        Err(std::io::Error::other("retirement setup primary witness"))
    });
    let error = match result {
        Ok(_) => panic!("failed dispatch unexpectedly succeeded"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        matches!(error, SystemError::InitializationError(_)),
        "{error}"
    );
    assert!(
        message.contains("retirement setup primary witness"),
        "{message}"
    );
    assert!(message.contains("2 worker joins"), "{message}");
    assert!(
        message.contains("panic payload destructor panicked"),
        "{message}"
    );
    assert_eq!(payloads.load(Ordering::SeqCst), 2);
    assert_eq!(healthy.load(Ordering::SeqCst), 1);
}

#[test]
fn last_retirement_owner_can_drop_without_an_async_runtime() {
    let (retirement, disposed) = actix::System::new().block_on(async {
        let gate = Arc::new(Gate::default());
        gate.release();
        let workers = live_workers(1, &gate, None);
        (Retirement::new(workers.owners).unwrap(), workers.disposed)
    });
    let receipt = retirement.receipt();
    drop(retirement);
    futures::executor::block_on(receipt).unwrap();
    assert_eq!(disposed.try_recv().unwrap(), 0);
}
