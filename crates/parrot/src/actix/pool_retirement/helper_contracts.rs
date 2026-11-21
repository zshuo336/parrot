use super::*;
use std::{
    cell::RefCell,
    sync::{
        Condvar,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

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
struct LocalExit {
    gate: Arc<Gate>,
    entered: mpsc::Sender<()>,
    exited: Arc<AtomicUsize>,
}
impl Drop for LocalExit {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        self.gate.wait();
        self.exited.fetch_add(1, Ordering::SeqCst);
    }
}
thread_local! {
    static LOCAL_EXIT: RefCell<Option<LocalExit>> = const { RefCell::new(None) };
}
struct Held(Arc<AtomicUsize>);
impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Fixture {
    worker: actix::ArbiterHandle,
    retirement: Retirement,
    gate: Arc<Gate>,
    entered: mpsc::Receiver<()>,
    helper_exited: Arc<AtomicUsize>,
    worker_disposed: Arc<AtomicUsize>,
}
fn fixture() -> Fixture {
    let owner = actix::Arbiter::try_new().unwrap();
    let worker = owner.handle();
    let worker_disposed = Arc::new(AtomicUsize::new(0));
    let held = Held(worker_disposed.clone());
    let (started, receive_started) = mpsc::channel();
    assert!(owner.spawn(async move {
        let _held = held;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    }));
    receive_started.recv_timeout(WAIT).unwrap();
    let gate = Arc::new(Gate::default());
    let helper_exited = Arc::new(AtomicUsize::new(0));
    let (entered, receive_entered) = mpsc::channel();
    let local = LocalExit {
        gate: gate.clone(),
        entered,
        exited: helper_exited.clone(),
    };
    let retirement = Retirement::with_spawner(vec![owner], move |job| {
        std::thread::Builder::new().spawn(move || {
            LOCAL_EXIT.with(|slot| *slot.borrow_mut() = Some(local));
            job();
        })
    })
    .unwrap();
    Fixture {
        worker,
        retirement,
        gate,
        entered: receive_entered,
        helper_exited,
        worker_disposed,
    }
}

#[actix::test]
async fn explicit_join_waits_for_helper_tls_exit_and_preserves_executor_progress() {
    let fixture = fixture();
    let _release = Release(fixture.gate.clone());
    assert_eq!(fixture.retirement.owned_threads(1), 2);
    fixture.retirement.request();
    fixture.retirement.receipt().await.unwrap();
    let entered = fixture.entered.recv_timeout(WAIT);
    assert_eq!(fixture.worker_disposed.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.retirement.owned_threads(1), 1);
    let mut joined = Box::pin(fixture.retirement.join());
    let early = futures::poll!(&mut joined);
    let returned_early = early.is_ready();
    let progress = tokio::time::timeout(WAIT, async {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    })
    .await;
    fixture.gate.release();
    match early {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => tokio::time::timeout(WAIT, &mut joined).await.unwrap(),
    }
    .unwrap();
    assert!(entered.is_ok());
    assert!(
        !returned_early,
        "worker receipt bypassed blocked helper TLS destruction"
    );
    assert!(
        progress.is_ok(),
        "helper OS join blocked the async executor"
    );
    assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.retirement.owned_threads(1), 0);
    fixture.retirement.join().await.unwrap();
}

#[actix::test]
async fn cancelled_waiter_and_queued_job_keep_one_real_helper_join() {
    let fixture = fixture();
    let _release = Release(fixture.gate.clone());
    fixture.retirement.request();
    fixture.retirement.receipt().await.unwrap();
    let entered = fixture.entered.recv_timeout(WAIT);
    let (send, receive) = mpsc::channel();
    let mut first = Box::pin(fixture.retirement.join_with(move |job| {
        send.send(job).unwrap();
    }));
    assert!(futures::poll!(&mut first).is_pending());
    drop(first);
    let queued = receive.recv_timeout(WAIT).unwrap();
    let mut later = Box::pin(
        fixture
            .retirement
            .join_with(|_| panic!("helper join dispatched twice")),
    );
    let early = futures::poll!(&mut later);
    let later_returned_early = early.is_ready();
    let (done, receive_done) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        drop(queued);
        done.send(()).unwrap();
    });
    let returned_early = receive_done.try_recv().is_ok();
    let count = fixture.retirement.owned_threads(1);
    fixture.gate.release();
    receive_done.recv_timeout(WAIT).unwrap();
    worker.join().unwrap();
    match early {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => tokio::time::timeout(WAIT, later).await.unwrap(),
    }
    .unwrap();
    assert!(entered.is_ok());
    assert!(
        !later_returned_early,
        "later waiter bypassed the claimed helper join"
    );
    assert!(
        !returned_early,
        "queued join guard discarded the helper handle"
    );
    assert_eq!(count, 1);
    assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.retirement.owned_threads(1), 0);
}

#[test]
fn dispatch_panic_still_joins_helper_before_returning_actual_result() {
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("dispatch payload destructor witness");
        }
    }
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (ready, receive_ready) = mpsc::channel();
    let (done, receive_done) = mpsc::channel();
    let (attempted, receive_attempted) = mpsc::channel();
    let moved_gate = gate.clone();
    let thread = std::thread::spawn(move || {
        actix::System::new().block_on(async move {
            let fixture = fixture();
            // Forward the fixture's actual TLS release to this external observer.
            let payloads = Arc::new(AtomicUsize::new(0));
            fixture.retirement.request();
            fixture.retirement.receipt().await.unwrap();
            fixture.entered.recv_timeout(WAIT).unwrap();
            ready
                .send((fixture.gate.clone(), fixture.helper_exited.clone()))
                .unwrap();
            moved_gate.wait();
            let result = fixture
                .retirement
                .join_with(|_job| {
                    attempted.send(()).unwrap();
                    std::panic::panic_any(Payload(payloads.clone()));
                })
                .await;
            done.send((
                result,
                payloads.load(Ordering::SeqCst),
                fixture.retirement.owned_threads(1),
            ))
            .unwrap();
        });
    });
    let (helper_gate, exited) = receive_ready.recv_timeout(WAIT).unwrap();
    let _helper_release = Release(helper_gate.clone());
    gate.release();
    receive_attempted.recv_timeout(WAIT).unwrap();
    let early = receive_done.try_recv().ok();
    let returned_early = early.is_some();
    helper_gate.release();
    let (result, payloads, count) =
        early.unwrap_or_else(|| receive_done.recv_timeout(WAIT).unwrap());
    thread.join().unwrap();
    assert!(
        !returned_early,
        "dispatch failure returned before helper join"
    );
    result.unwrap();
    assert_eq!(payloads, 1);
    assert_eq!(exited.load(Ordering::SeqCst), 1);
    assert_eq!(count, 0);
}

#[actix::test]
async fn helper_panic_and_worker_failure_are_both_retained_after_all_joins() {
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("helper panic payload disposal witness");
        }
    }
    let armed = Arc::new(AtomicBool::new(false));
    let parked = armed.clone();
    let (send, receive) = mpsc::channel();
    let owner = actix::Arbiter::try_with_tokio_rt(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .on_thread_park(move || {
                if parked.swap(false, Ordering::SeqCst) {
                    send.send(()).unwrap();
                    panic!("business worker failure witness");
                }
            })
            .build()
    })
    .unwrap();
    assert!(owner.spawn(async move {
        armed.store(true, Ordering::SeqCst);
    }));
    receive.recv_timeout(WAIT).unwrap();
    let payloads = Arc::new(AtomicUsize::new(0));
    let moved = payloads.clone();
    let retirement = Retirement::with_spawner(vec![owner], move |job| {
        std::thread::Builder::new().spawn(move || {
            job();
            std::panic::panic_any(Payload(moved));
        })
    })
    .unwrap();
    retirement.request();
    let first = tokio::time::timeout(WAIT, retirement.join())
        .await
        .unwrap()
        .unwrap_err();
    let second = retirement.join().await.unwrap_err();
    let message = first.to_string();
    assert_eq!(message, second.to_string());
    assert!(
        message.contains("business worker failure witness"),
        "{message}"
    );
    assert!(
        message.contains("Actix retirement thread failed"),
        "{message}"
    );
    assert!(
        message.contains("panic payload destructor panicked"),
        "{message}"
    );
    assert_eq!(payloads.load(Ordering::SeqCst), 1);
    assert_eq!(retirement.owned_threads(1), 0);
}

#[test]
fn explicit_join_without_runtime_preserves_actual_os_retirement() {
    let fixture = actix::System::new().block_on(async { fixture() });
    fixture.retirement.request();
    fixture.gate.release();
    futures::executor::block_on(fixture.retirement.join()).unwrap();
    assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.retirement.owned_threads(1), 0);
}

#[test]
fn last_retirement_owner_without_runtime_keeps_actual_helper_join() {
    let fixture = actix::System::new().block_on(async { fixture() });
    let _release = Release(fixture.gate.clone());
    let helper = fixture.retirement.helper.clone();
    drop(fixture.retirement);
    fixture.entered.recv_timeout(WAIT).unwrap();
    let pending = helper.finished.borrow().is_none();
    fixture.gate.release();
    let result = observer_wait(helper.finished.subscribe());
    // Reap the old implementation too, after recording its missing receipt.
    helper.join_blocking();
    assert!(pending, "helper completion preceded TLS disposal");
    assert!(
        matches!(&result, Ok(Ok(()))),
        "last owner abandoned the helper OS join: {result:?}"
    );
    assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.worker_disposed.load(Ordering::SeqCst), 1);
}

#[test]
fn orphaned_helper_owner_keeps_actual_join_and_native_lease() {
    let gate = Arc::new(Gate::default());
    let (joined, receive_joined) = mpsc::channel();
    let moved_gate = gate.clone();
    let thread = std::thread::Builder::new()
        .name("parrot-actix-orphaned-helper-test".into())
        .spawn(move || {
            moved_gate.wait();
            joined.send(()).unwrap();
        })
        .unwrap();
    let budget = Arc::new(crate::thread::native_budget::NativeWorkerBudget::with_capacity(1));
    let reservation = budget.try_reserve().unwrap();
    let reservations = Arc::new(Mutex::new(vec![reservation]));
    let (finished, receipt) = watch::channel(None);
    let helper = Arc::new(HelperJoin {
        thread: Mutex::new(Some(thread)),
        reservations,
        finished,
        scheduled: AtomicBool::new(false),
        changed: Condvar::new(),
    });

    drop(helper);
    assert!(receive_joined.try_recv().is_err());
    assert!(budget.try_reserve().is_err());
    gate.release();
    receive_joined.recv_timeout(WAIT).unwrap();
    let result = observer_wait(receipt).unwrap();
    assert!(result.is_ok(), "fallback join failed: {result:?}");
    let reclaimed = budget.try_reserve().unwrap();
    reclaimed.release_after_join();
}

fn observer_wait(
    receipt: watch::Receiver<Option<Exit>>,
) -> Result<Exit, tokio::time::error::Elapsed> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { tokio::time::timeout(WAIT, wait(receipt)).await })
}

#[actix::test]
async fn last_retirement_owner_on_worker_keeps_actual_helper_join_without_self_join() {
    let fixture = fixture();
    let _release = Release(fixture.gate.clone());
    let helper = fixture.retirement.helper.clone();
    let (done, receive_done) = mpsc::channel();
    let retirement = fixture.retirement;
    assert!(fixture.worker.spawn(async move {
        drop(retirement);
        let _ = done.send(());
    }));
    let returned = receive_done.recv_timeout(WAIT);
    let entered = fixture.entered.recv_timeout(WAIT);
    let pending = helper.finished.borrow().is_none();
    fixture.gate.release();
    let result = tokio::time::timeout(WAIT, wait(helper.finished.subscribe())).await;
    helper.join_blocking();
    assert!(
        returned.is_ok(),
        "last retirement owner joined its own worker"
    );
    assert!(entered.is_ok());
    assert!(pending, "helper completion preceded TLS disposal");
    assert!(
        matches!(&result, Ok(Ok(()))),
        "last owner abandoned actual helper join: {result:?}"
    );
    assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.worker_disposed.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn synchronous_takeover_joins_helper_while_cancelled_executor_still_holds_queued_job() {
    let fixture = fixture();
    let _release = Release(fixture.gate.clone());
    fixture.retirement.request();
    fixture.retirement.receipt().await.unwrap();
    fixture.entered.recv_timeout(WAIT).unwrap();
    let (send, receive) = mpsc::channel();
    let mut first = Box::pin(fixture.retirement.join_with(move |job| {
        send.send(job).unwrap();
    }));
    assert!(futures::poll!(&mut first).is_pending());
    drop(first);
    let queued = receive.recv_timeout(WAIT).unwrap();
    let retirement = Arc::new(fixture.retirement);
    let moved = retirement.clone();
    let (done, receive_done) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let result = moved.join_blocking();
        done.send(result).unwrap();
    });
    let early = receive_done.recv_timeout(Duration::from_millis(100)).ok();
    let count = retirement.owned_threads(1);
    fixture.gate.release();
    // The queued job is intentionally retained until the synchronous join ends.
    let returned_early = early.is_some();
    early
        .unwrap_or_else(|| receive_done.recv_timeout(WAIT).unwrap())
        .unwrap();
    thread.join().unwrap();
    drop(queued);
    retirement.join().await.unwrap();
    assert!(
        !returned_early,
        "synchronous takeover bypassed helper TLS destruction"
    );
    assert_eq!(count, 1);
    assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
    assert_eq!(retirement.owned_threads(1), 0);
}

#[actix::test]
async fn concurrent_synchronous_join_waits_for_the_in_flight_os_join() {
    let fixture = fixture();
    let _release = Release(fixture.gate.clone());
    fixture.retirement.request();
    fixture.retirement.receipt().await.unwrap();
    fixture.entered.recv_timeout(WAIT).unwrap();
    let retirement = Arc::new(fixture.retirement);
    let (done, receive_done) = mpsc::channel();
    let mut threads = Vec::new();
    for _ in 0..2 {
        let moved = retirement.clone();
        let done = done.clone();
        threads.push(std::thread::spawn(move || {
            done.send(moved.join_blocking()).unwrap();
        }));
    }
    let early = receive_done.recv_timeout(Duration::from_millis(100)).ok();
    let count = retirement.owned_threads(1);
    fixture.gate.release();
    let returned_early = early.is_some();
    early
        .unwrap_or_else(|| receive_done.recv_timeout(WAIT).unwrap())
        .unwrap();
    receive_done.recv_timeout(WAIT).unwrap().unwrap();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(
        !returned_early,
        "concurrent observer bypassed an in-flight OS join"
    );
    assert_eq!(count, 1);
    assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
    assert_eq!(retirement.owned_threads(1), 0);
}

#[test]
fn saturated_cancelled_waiter_releases_completed_helper_after_synchronous_takeover() {
    crate::test_support::isolated(
        "actix::pool_retirement::helper_contracts::saturated_cancelled_waiter_releases_completed_helper_after_synchronous_takeover",
        || {
            actix::System::new().block_on(async {
                let fixture = fixture();
                let _release = Release(fixture.gate.clone());
                fixture.retirement.request();
                fixture.retirement.receipt().await.unwrap();
                fixture.entered.recv_timeout(WAIT).unwrap();
                let filler = Arc::new(Semaphore::new(8));
                let permits: Vec<_> = (0..crate::owned_payload::MAX_PENDING_CLEANUPS_GLOBAL)
                    .map(|_| crate::owned_payload::try_pending_cleanup_permit(&filler).unwrap())
                    .collect();
                let weak = Arc::downgrade(&fixture.retirement.helper);
                let mut cancelled = Box::pin(fixture.retirement.join());
                assert!(futures::poll!(&mut cancelled).is_pending());
                drop(cancelled);
                fixture.gate.release();
                fixture.retirement.join_blocking().unwrap();
                drop(fixture.retirement);
                let reclaimed = tokio::time::timeout(WAIT, async {
                    while weak.upgrade().is_some() {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                // Return no capacity until after observing actual reclamation.
                drop(permits);
                assert!(
                    reclaimed.is_ok(),
                    "completed helper remained owned by a saturated admission waiter"
                );
                assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
            });
        },
    );
}
