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
        cleanup: Mutex::default(),
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

#[test]
fn helper_completion_waker_can_reenter_synchronous_join() {
    crate::test_support::isolated_with_timeout(
        "actix::pool_retirement::helper_contracts::helper_completion_waker_can_reenter_synchronous_join",
        WAIT,
        || {
            struct Reenter {
                helper: Arc<HelperJoin>,
                calls: AtomicUsize,
            }
            impl futures::task::ArcWake for Reenter {
                fn wake_by_ref(this: &Arc<Self>) {
                    this.helper.join_blocking();
                    assert!(matches!(*this.helper.finished.borrow(), Some(Ok(()))));
                    this.calls.fetch_add(1, Ordering::SeqCst);
                }
            }

            let fixture = actix::System::new().block_on(async { fixture() });
            let _release = Release(fixture.gate.clone());
            fixture.retirement.request();
            futures::executor::block_on(fixture.retirement.receipt()).unwrap();
            fixture.entered.recv_timeout(WAIT).unwrap();
            let helper = fixture.retirement.helper.clone();
            let reenter = Arc::new(Reenter {
                helper: helper.clone(),
                calls: AtomicUsize::new(0),
            });
            let waker = futures::task::waker(reenter.clone());
            let mut context = std::task::Context::from_waker(&waker);
            let mut receipt = Box::pin(wait(helper.finished.subscribe()));
            assert!(receipt.as_mut().poll(&mut context).is_pending());
            fixture.gate.release();
            fixture.retirement.join_blocking().unwrap();
            assert_eq!(reenter.calls.load(Ordering::SeqCst), 1);
            assert!(matches!(
                receipt.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(()))
            ));
            assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.retirement.owned_threads(1), 0);
        },
    );
}

fn check_completion_quota(takeover: bool) {
    struct Observe {
        exited: Arc<AtomicUsize>,
        snapshots: mpsc::Sender<(bool, usize, usize)>,
    }
    impl futures::task::ArcWake for Observe {
        fn wake_by_ref(this: &Arc<Self>) {
            let pending = crate::owned_payload::try_pending_cleanup_permit(helper_cleanup_slots());
            let _ = this.snapshots.send((
                pending.is_some(),
                helper_cleanup_slots().available_permits(),
                this.exited.load(Ordering::SeqCst),
            ));
        }
    }
    let fixture = actix::System::new().block_on(async { fixture() });
    let release = Release(fixture.gate.clone());
    fixture.retirement.request();
    futures::executor::block_on(fixture.retirement.receipt()).unwrap();
    fixture.entered.recv_timeout(WAIT).unwrap();
    let filler = Arc::new(Semaphore::new(8));
    let fillers: Vec<_> = (0..crate::owned_payload::MAX_PENDING_CLEANUPS_GLOBAL - 1)
        .map(|_| crate::owned_payload::try_pending_cleanup_permit(&filler).unwrap())
        .collect();
    let active: Vec<_> = (0..if takeover { HELPER_CLEANUP_SLOTS } else { 0 })
        .map(|_| helper_cleanup_slots().clone().try_acquire_owned().unwrap())
        .collect();
    let (snapshots, observed) = mpsc::channel();
    let observer = Arc::new(Observe {
        exited: fixture.helper_exited.clone(),
        snapshots,
    });
    let waker = futures::task::waker(observer);
    let mut receipt = Box::pin(wait(fixture.retirement.helper.finished.subscribe()));
    assert!(
        receipt
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    fixture
        .retirement
        .helper
        .dispatch_in_lifecycle(&fixture.retirement.runtime);
    let synchronous = takeover.then(|| {
        let deadline = std::time::Instant::now() + WAIT;
        while crate::owned_payload::try_pending_cleanup_permit(helper_cleanup_slots()).is_some()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            crate::owned_payload::try_pending_cleanup_permit(helper_cleanup_slots()).is_none(),
            "the queued asynchronous owner must hold the final pending quota"
        );
        let helper = fixture.retirement.helper.clone();
        std::thread::spawn(move || helper.join_blocking())
    });
    let deadline = std::time::Instant::now() + WAIT;
    while fixture.retirement.helper.thread.lock().unwrap().is_some()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    let claimed = fixture.retirement.helper.thread.lock().unwrap().is_none();
    let incomplete = fixture.retirement.helper.finished.borrow().is_none();
    drop(release);
    let snapshot = observed.recv_timeout(WAIT);
    futures::executor::block_on(receipt).unwrap();
    fixture.retirement.join_blocking().unwrap();
    if let Some(synchronous) = synchronous {
        synchronous.join().unwrap();
    }
    drop(active);
    drop(fillers);
    assert!(
        claimed && incomplete,
        "join must claim the real helper and wait for TLS exit"
    );
    assert_eq!(
        snapshot.unwrap(),
        (true, if takeover { 0 } else { HELPER_CLEANUP_SLOTS }, 1),
        "helper completion waker must be able to reuse its pending and active cleanup quotas"
    );
}

#[test]
fn helper_completion_waker_can_reuse_its_global_cleanup_quota() {
    crate::test_support::isolated(
        "actix::pool_retirement::helper_contracts::helper_completion_waker_can_reuse_its_global_cleanup_quota",
        || check_completion_quota(false),
    );
}

#[test]
fn helper_takeover_waker_can_reuse_admitted_quota_while_original_owner_waits() {
    crate::test_support::isolated(
        "actix::pool_retirement::helper_contracts::helper_takeover_waker_can_reuse_admitted_quota_while_original_owner_waits",
        || check_completion_quota(true),
    );
}

#[test]
fn helper_completion_waker_panic_preserves_join_receipt() {
    crate::test_support::isolated_with_timeout(
        "actix::pool_retirement::helper_contracts::helper_completion_waker_panic_preserves_join_receipt",
        WAIT,
        || {
            struct WakePanic(AtomicUsize);
            impl futures::task::ArcWake for WakePanic {
                fn wake_by_ref(this: &Arc<Self>) {
                    this.0.fetch_add(1, Ordering::SeqCst);
                    panic!("helper completion wake failure");
                }
            }

            let fixture = actix::System::new().block_on(async { fixture() });
            let _release = Release(fixture.gate.clone());
            fixture.retirement.request();
            futures::executor::block_on(fixture.retirement.receipt()).unwrap();
            fixture.entered.recv_timeout(WAIT).unwrap();
            let wake = Arc::new(WakePanic(AtomicUsize::new(0)));
            let waker = futures::task::waker(wake.clone());
            let mut context = std::task::Context::from_waker(&waker);
            let mut receipt = Box::pin(wait(fixture.retirement.helper.finished.subscribe()));
            assert!(receipt.as_mut().poll(&mut context).is_pending());
            fixture.gate.release();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fixture.retirement.join_blocking()
            }));
            assert!(result.is_ok(), "a receipt waker unwound the join owner");
            result.unwrap().unwrap();
            assert_eq!(wake.0.load(Ordering::SeqCst), 1);
            assert!(matches!(
                receipt.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(()))
            ));
            fixture.retirement.join_blocking().unwrap();
            assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.retirement.owned_threads(1), 0);
        },
    );
}

#[test]
fn worker_completion_waker_panic_preserves_join_receipt() {
    crate::test_support::isolated_with_timeout(
        "actix::pool_retirement::helper_contracts::worker_completion_waker_panic_preserves_join_receipt",
        WAIT,
        || {
            struct WakePanic(AtomicUsize);
            impl futures::task::ArcWake for WakePanic {
                fn wake_by_ref(this: &Arc<Self>) {
                    this.0.fetch_add(1, Ordering::SeqCst);
                    panic!("worker completion wake failure");
                }
            }

            let fixture = actix::System::new().block_on(async { fixture() });
            let wake = Arc::new(WakePanic(AtomicUsize::new(0)));
            let waker = futures::task::waker(wake.clone());
            let mut context = std::task::Context::from_waker(&waker);
            let mut receipt = Box::pin(wait(fixture.retirement.finished.clone()));
            assert!(receipt.as_mut().poll(&mut context).is_pending());
            fixture.retirement.request();
            fixture.gate.release();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fixture.retirement.join_blocking()
            }));
            assert!(result.is_ok(), "a worker receipt waker unwound retirement");
            result.unwrap().unwrap();
            assert_eq!(wake.0.load(Ordering::SeqCst), 1);
            assert!(matches!(
                receipt.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(()))
            ));
            assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.retirement.owned_threads(1), 0);
        },
    );
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

#[test]
fn failed_fallback_thread_creation_is_joined_by_a_non_owner() {
    let (finished, mut receipt) = watch::channel(None);
    let reservations = Arc::new(Mutex::new(Vec::new()));
    let (started, receive_started) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        started.send(()).unwrap();
    });
    receive_started.recv_timeout(WAIT).unwrap();
    let owner: FallbackOwner = Arc::new(Mutex::new(Some((worker, reservations, finished))));
    let weak = Arc::downgrade(&owner);

    // Simulate the Builder::spawn failure branch. The current test thread is
    // not the worker, so synchronous takeover is safe and must publish the
    // same terminal receipt instead of forgetting the OS handle.
    HelperJoin::finish_fallback(owner);

    assert!(weak.upgrade().is_none());
    assert_eq!(receipt.borrow_and_update().clone(), Some(Ok(())));
}

#[test]
fn failed_fallback_on_its_own_worker_eventually_returns_the_native_lease() {
    self_owned_fallback(false);
}

#[test]
fn self_owned_fallback_preserves_worker_panic_and_returns_the_native_lease() {
    self_owned_fallback(true);
}

fn self_owned_fallback(panics: bool) {
    let budget = Arc::new(crate::thread::native_budget::NativeWorkerBudget::with_capacity(1));
    let reservation = budget.try_reserve().unwrap();
    let (finished, receipt) = watch::channel(None);
    let (send, receive) = mpsc::sync_channel(1);
    let (started, observe_started) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        HelperJoin::finish_fallback(receive.recv().unwrap());
        started.send(()).unwrap();
        wait.recv_timeout(WAIT).unwrap();
        assert!(!panics, "self-owned helper panic");
    });
    let owner: FallbackOwner = Arc::new(Mutex::new(Some((
        worker,
        Arc::new(Mutex::new(vec![reservation])),
        finished,
    ))));
    send.send(owner).unwrap();
    observe_started.recv_timeout(WAIT).unwrap();
    let retained = budget.try_reserve().is_err();
    let premature = receipt.borrow().is_some();
    release.send(()).unwrap();
    let result = observer_wait(receipt);
    assert!(
        retained && !premature,
        "helper lease/receipt preceded actual exit"
    );
    let result = result.expect("self-owned helper join was stranded");
    if panics {
        assert_eq!(
            result,
            Err("Actix retirement thread failed: self-owned helper panic".into())
        );
    } else {
        assert_eq!(result, Ok(()));
    }
    let reclaimed = budget.try_reserve().unwrap();
    reclaimed.release_after_join();
}

fn receipt_diagnostic(dispatch_failure: bool, during_unwind: bool) {
    let fixture = actix::System::new().block_on(async { fixture() });
    let _release = Release(fixture.gate.clone());
    fixture.retirement.request();
    futures::executor::block_on(fixture.retirement.receipt()).unwrap();
    fixture.entered.recv_timeout(WAIT).unwrap();
    fixture.gate.release();
    crate::owned_payload::diagnostic_contracts::verify(
        &fixture.retirement.helper.finished,
        during_unwind,
        if dispatch_failure { 2 } else { 1 },
        || {
            if dispatch_failure {
                fixture
                    .retirement
                    .helper
                    .dispatch(|_job| panic!("original dispatch failure"));
            } else {
                fixture.retirement.join_blocking().unwrap();
            }
        },
        |value| assert!(value.is_ok()),
    );
    fixture.retirement.join_blocking().unwrap();
    assert_eq!(fixture.helper_exited.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.worker_disposed.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.retirement.owned_threads(1), 0);
}
use crate::owned_payload::diagnostic_contracts::contract;
contract!(
    receipt_diagnostic_helper_join,
    receipt_diagnostic(false, false)
);
contract!(
    receipt_diagnostic_helper_join_unwind,
    receipt_diagnostic(false, true)
);
contract!(
    receipt_diagnostic_helper_dispatch,
    receipt_diagnostic(true, false)
);
contract!(
    receipt_diagnostic_helper_dispatch_unwind,
    receipt_diagnostic(true, true)
);
