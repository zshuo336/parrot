use super::*;
use std::sync::{Condvar, mpsc};

const WAIT: Duration = Duration::from_secs(8);

struct CancelledActor(Arc<AtomicUsize>);
impl Drop for CancelledActor {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotActor for CancelledActor {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(std::future::pending())
    }
}

fn observer() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn terminal_failure(system: &ActixActorSystem) -> String {
    let observer = observer();
    let result =
        observer.block_on(async { tokio::time::timeout(WAIT, system.clone().shutdown()).await });
    let status = system.status();
    // Always retire the old candidate before checking the observed contract.
    observer.block_on(system.arbiters.stop_and_join()).unwrap();
    let first = result
        .expect("host exit left a pending shutdown receipt")
        .expect_err("host cancellation was reported as success")
        .to_string();
    let repeated = observer
        .block_on(system.clone().shutdown())
        .unwrap_err()
        .to_string();
    assert_eq!(first, repeated);
    assert_eq!(status.state, SystemState::Stopped);
    assert_eq!(status.active_actors, 0);
    assert_eq!(status.resources.thread_count, 0);
    first
}

#[test]
fn shutdown_cancelled_before_first_poll_by_host_exit_retires_owned_workers() {
    let host = actix::System::new();
    let disposed = Arc::new(AtomicUsize::new(0));
    let (system, actor) = host.block_on(async {
        let mut system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        // Exercise cancellation of the lifecycle executor itself.
        Arc::get_mut(&mut system.root_cohorts)
            .unwrap()
            .set_test_retirement_runtime(tokio::runtime::Handle::current());
        let actor = system
            .spawn_root_boxed(Box::new(CancelledActor(disposed.clone())), Box::new(()))
            .await
            .unwrap();
        actor.send(Box::new(7_u8)).await.unwrap();
        assert_eq!(system.status().active_actors, 1);
        let mut shutdown = Box::pin(system.clone().shutdown());
        assert!(futures::poll!(&mut shutdown).is_pending());
        drop(shutdown);
        (system, actor)
    });
    drop(host);
    assert!(terminal_failure(&system).contains("without graceful completion"));
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    let observer = observer();
    assert!(observer.block_on(actor.send(Box::new(9_u8))).is_err());
    assert!(!observer.block_on(actor.is_alive()));
    let rejected = Arc::new(AtomicUsize::new(0));
    assert!(
        observer
            .block_on(
                system.spawn_root_boxed(Box::new(CancelledActor(rejected.clone())), Box::new(()))
            )
            .is_err()
    );
    assert_eq!(rejected.load(Ordering::SeqCst), 1);
}

#[test]
fn shutdown_first_requested_after_host_exit_retires_without_registry_lock_deadlock() {
    let host = actix::System::new();
    let mut system = host
        .block_on(ActixActorSystem::with_arbiter_count(1))
        .unwrap();
    host.block_on(async {
        Arc::get_mut(&mut system.root_cohorts)
            .unwrap()
            .set_test_retirement_runtime(tokio::runtime::Handle::current());
    });
    drop(host);
    assert!(terminal_failure(&system).contains("without graceful completion"));
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
struct BlockedDisposal {
    gate: Arc<Gate>,
    disposing: Option<tokio::sync::oneshot::Sender<()>>,
    disposed: Arc<AtomicUsize>,
}
impl Drop for BlockedDisposal {
    fn drop(&mut self) {
        let _ = self.disposing.take().unwrap().send(());
        self.gate.wait();
        self.disposed.fetch_add(1, Ordering::SeqCst);
    }
}

fn running_host_exit(expired: bool) {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let disposed = Arc::new(AtomicUsize::new(0));
    let moved_disposed = disposed.clone();
    let moved_gate = gate.clone();
    let (ready, receive_ready) = mpsc::channel();
    let (exit, receive_exit) = mpsc::channel();
    let (dropping, receive_dropping) = mpsc::channel();
    let (done, receive_done) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let host = actix::System::new();
        let (system, timeout) = host.block_on(async {
            let mut system = ActixActorSystem::with_settings(ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: if expired {
                    Duration::from_millis(10)
                } else {
                    Duration::from_secs(60)
                },
                ..Default::default()
            })
            .await
            .unwrap();
            Arc::get_mut(&mut system.root_cohorts)
                .unwrap()
                .set_test_retirement_runtime(tokio::runtime::Handle::current());
            let (started, receive_started) = tokio::sync::oneshot::channel();
            let (disposing, receive_disposing) = tokio::sync::oneshot::channel();
            let held = BlockedDisposal {
                gate: moved_gate,
                disposing: Some(disposing),
                disposed: moved_disposed,
            };
            assert!(system.arbiters.inner.workers[0].spawn(async move {
                let _held = held;
                let _ = started.send(());
                std::future::pending::<()>().await;
            }));
            receive_started.await.unwrap();
            let mut shutdown = Box::pin(system.clone().shutdown());
            assert!(futures::poll!(&mut shutdown).is_pending());
            // The real cleanup has polled and requested actual worker teardown.
            receive_disposing.await.unwrap();
            let timeout = if expired {
                tokio::time::pause();
                tokio::time::advance(Duration::from_millis(11)).await;
                let result = shutdown.await.unwrap_err().to_string();
                tokio::time::resume();
                Some(result)
            } else {
                drop(shutdown);
                None
            };
            (system, timeout)
        });
        ready.send((system, timeout)).unwrap();
        receive_exit.recv_timeout(WAIT).unwrap();
        dropping.send(()).unwrap();
        drop(host);
        done.send(()).unwrap();
    });
    let (system, timeout) = receive_ready.recv_timeout(WAIT).unwrap();
    exit.send(()).unwrap();
    receive_dropping.recv_timeout(WAIT).unwrap();
    let early_exit = receive_done
        .recv_timeout(Duration::from_millis(100))
        .is_ok();
    let status = system.status();
    let early_disposed = disposed.load(Ordering::SeqCst);
    gate.release();
    if !early_exit {
        receive_done.recv_timeout(WAIT).unwrap();
    }
    thread.join().unwrap();
    let failure = terminal_failure(&system);
    assert!(
        !early_exit,
        "host cancellation abandoned an actual worker join"
    );
    assert_eq!(status.state, SystemState::ShuttingDown);
    assert_eq!(status.resources.thread_count, 2);
    assert_eq!(early_disposed, 0);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    if let Some(timeout) = timeout {
        assert_eq!(
            failure, timeout,
            "host cancellation overwrote the published timeout"
        );
        assert!(failure.contains("deadline"), "{failure}");
    } else {
        assert!(failure.contains("without graceful completion"), "{failure}");
    }
}

#[test]
fn host_cancellation_during_cleanup_waits_for_actual_worker_disposal() {
    running_host_exit(false);
}

#[test]
fn published_timeout_survives_host_cancellation_and_actual_worker_retirement() {
    running_host_exit(true);
}
