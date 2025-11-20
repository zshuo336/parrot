use super::*;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Condvar, Mutex, mpsc},
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
    started: Option<mpsc::Sender<()>>,
    disposing: mpsc::Sender<usize>,
    disposed: mpsc::Sender<usize>,
    peer: Option<Arc<Gate>>,
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

fn failing_runtime() -> std::io::Result<actix::Arbiter> {
    actix::Arbiter::try_with_tokio_rt(|| {
        Err::<tokio::runtime::Runtime, _>(std::io::Error::other("runtime construction witness"))
    })
}

#[test]
fn first_worker_runtime_error_is_returned_without_unwinding() {
    actix::System::new().block_on(async {
        let result = std::panic::catch_unwind(|| build(1, |_| failing_runtime()));
        let error = result
            .expect("worker setup error escaped as a panic")
            .unwrap_err();
        assert!(
            matches!(error, SystemError::InitializationError(_)),
            "{error}"
        );
        assert!(error.to_string().contains("worker 0"), "{error}");
        assert!(
            error.to_string().contains("runtime construction witness"),
            "{error}"
        );
    });
}

fn partial_failure(created: usize, panic_factory: bool, peers: bool) {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let peer = peers.then(|| Arc::new(Gate::default()));
    let _peer_release = peer.clone().map(Release);
    let handles = Arc::new(Mutex::new(Vec::<actix::ArbiterHandle>::new()));
    let (attempt_send, attempt_receive) = mpsc::channel();
    let (disposing_send, disposing_receive) = mpsc::channel();
    let (disposed_send, disposed_receive) = mpsc::channel();
    let (result_send, result_receive) = mpsc::channel();
    let handles_thread = handles.clone();
    let gate_thread = gate.clone();
    let worker = std::thread::spawn(move || {
        actix::System::new().block_on(async move {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                build(created + 1, |index| {
                    if index == created {
                        attempt_send.send(()).unwrap();
                        if panic_factory {
                            panic!("factory setup witness");
                        }
                        return failing_runtime();
                    }
                    let owner = actix::Arbiter::try_new()?;
                    handles_thread.lock().unwrap().push(owner.handle());
                    let (started_send, started_receive) = mpsc::channel();
                    assert!(owner.spawn(Work {
                        index,
                        gate: gate_thread.clone(),
                        started: Some(started_send),
                        disposing: disposing_send.clone(),
                        disposed: disposed_send.clone(),
                        peer: peer.clone(),
                    }));
                    started_receive.recv_timeout(WAIT).unwrap();
                    Ok(owner)
                })
            }));
            let _ = result_send.send(result.map_err(crate::owned_payload::panic_message));
        });
    });
    attempt_receive.recv_timeout(WAIT).unwrap();
    let entered = disposing_receive.recv_timeout(WAIT);
    let peer_entered = !peers || disposing_receive.recv_timeout(WAIT).is_ok();
    let early = result_receive.try_recv().ok();
    let returned_early = early.is_some();
    // Always clean up even when comparing against the broken implementation.
    // Retained handles keep old detached workers observable until they stop.
    for handle in handles.lock().unwrap().iter() {
        handle.stop();
    }
    gate.release();
    if let Some(peer) = &_peer_release {
        peer.0.release();
    }
    let result = early.unwrap_or_else(|| result_receive.recv_timeout(WAIT).unwrap());
    worker.join().unwrap();
    let mut disposed: Vec<_> = (0..created)
        .map(|_| disposed_receive.recv_timeout(WAIT).unwrap())
        .collect();
    disposed.sort_unstable();
    assert!(
        entered.is_ok(),
        "constructor did not stop existing worker tasks before returning"
    );
    assert!(
        !returned_early,
        "constructor returned before blocked task disposal completed"
    );
    assert!(
        peer_entered,
        "rollback joined one worker before stopping its peer"
    );
    let error = result
        .expect("constructor unwound instead of returning an initialization error")
        .unwrap_err();
    assert!(
        matches!(error, SystemError::InitializationError(_)),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains(&format!("worker {created}")), "{message}");
    assert!(
        message.contains(if panic_factory {
            "factory setup witness"
        } else {
            "runtime construction witness"
        }),
        "{message}"
    );
    assert_eq!(disposed, (0..created).collect::<Vec<_>>());
    assert!(handles.lock().unwrap().iter().all(|handle| !handle.alive()));
}

#[test]
fn second_worker_failure_joins_existing_future_disposal() {
    partial_failure(1, false, false);
}
#[test]
fn later_worker_failure_joins_every_existing_future() {
    partial_failure(3, false, false);
}
#[test]
fn factory_panic_is_reported_after_existing_workers_retire() {
    partial_failure(2, true, false);
}
#[test]
fn rollback_stops_every_worker_before_joining_the_first() {
    partial_failure(2, false, true);
}

#[test]
fn successful_build_returns_live_workers_until_explicit_stop_and_join() {
    actix::System::new().block_on(async {
        let owners = build(2, |_| actix::Arbiter::try_new()).unwrap();
        let handles: Vec<_> = owners.iter().map(actix::Arbiter::handle).collect();
        assert!(handles.iter().all(actix::ArbiterHandle::alive));
        for handle in &handles {
            handle.stop();
        }
        for owner in owners {
            owner.join().unwrap();
        }
        assert!(handles.iter().all(|handle| !handle.alive()));
    });
}

#[test]
fn construction_guard_retires_workers_during_unwinding() {
    actix::System::new().block_on(async {
        let gate = Arc::new(Gate::default());
        gate.release();
        let (started_send, started_receive) = mpsc::channel();
        let (disposing_send, _) = mpsc::channel();
        let (disposed_send, disposed_receive) = mpsc::channel();
        let owner = actix::Arbiter::try_new().unwrap();
        let handle = owner.handle();
        assert!(owner.spawn(Work {
            index: 0,
            gate,
            started: Some(started_send),
            disposing: disposing_send,
            disposed: disposed_send,
            peer: None,
        }));
        started_receive.recv_timeout(WAIT).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _workers = StartingWorkers(vec![owner]);
            panic!("construction guard unwind witness");
        }));
        assert!(result.is_err());
        assert_eq!(disposed_receive.try_recv().unwrap(), 0);
        assert!(!handle.alive());
    });
}

#[test]
fn primary_setup_error_and_failed_joins_are_preserved_without_skipping_later_workers() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("worker panic payload disposal witness");
        }
    }
    struct Disposed(Arc<AtomicUsize>);
    impl Drop for Disposed {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    actix::System::new().block_on(async {
        let payloads = Arc::new(AtomicUsize::new(0));
        let healthy = Arc::new(AtomicUsize::new(0));
        let result = build(4, |index| {
            if index == 3 {
                return failing_runtime();
            }
            if index == 2 {
                let owner = actix::Arbiter::try_new()?;
                let disposed = Disposed(healthy.clone());
                let (send, receive) = mpsc::channel();
                assert!(owner.spawn(async move {
                    let _disposed = disposed;
                    send.send(()).unwrap();
                    std::future::pending::<()>().await;
                }));
                receive.recv_timeout(WAIT).unwrap();
                return Ok(owner);
            }
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
            })?;
            assert!(owner.spawn(async move {
                armed.store(true, Ordering::SeqCst);
            }));
            receive.recv_timeout(WAIT).unwrap();
            Ok(owner)
        });
        let error = result.unwrap_err();
        assert!(
            matches!(error, SystemError::InitializationError(_)),
            "{error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("worker 3 initialization failed: runtime construction witness"),
            "{message}"
        );
        assert!(
            message.contains("startup cleanup failed (2 worker joins): worker 0"),
            "{message}"
        );
        assert!(
            message.contains("panic payload destructor panicked"),
            "{message}"
        );
        assert_eq!(payloads.load(Ordering::SeqCst), 2);
        assert_eq!(
            healthy.load(Ordering::SeqCst),
            1,
            "later healthy worker was not joined"
        );
    });
}
