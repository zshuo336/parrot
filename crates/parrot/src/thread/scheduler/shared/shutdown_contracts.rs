//! Fault injection uses real owned Tokio tasks and the production shutdown path.
use super::*;
use futures::FutureExt;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;

fn pool() -> Arc<SharedThreadPool> {
    Arc::new(SharedThreadPool::new_unchecked_for_test(
        Some(SharedThreadPoolConfig {
            pool_size: 0,
            burst_workers_max: 0,
            ..Default::default()
        }),
        Handle::current(),
    ))
}
async fn failed_task(message: &'static str) -> JoinHandle<()> {
    let handle = tokio::spawn(async move { panic!("{message}") });
    while !handle.is_finished() {
        tokio::task::yield_now().await;
    }
    handle
}
fn error(result: Result<(), SystemError>) -> String {
    match result {
        Err(SystemError::ShutdownError(message)) => message,
        other => panic!("expected a preserved shutdown error, got {other:?}"),
    }
}

#[tokio::test]
async fn worker_panic_is_preserved_for_every_shutdown_waiter() {
    let pool = pool();
    let failed = failed_task("owned worker failure").await;
    pool.workers.lock().unwrap().push(failed);
    let first = pool.shutdown(1000).await;
    let second = pool.shutdown(0).await;
    assert_eq!(pool.status(), SchedulerStatus::Shutdown);
    let first = error(first);
    assert!(first.contains("owned worker failure"));
    assert_eq!(error(second), first);
}

#[tokio::test]
async fn unexpected_cancellation_is_not_success() {
    let pool = pool();
    let cancelled = tokio::spawn(std::future::pending::<()>());
    cancelled.abort();
    while !cancelled.is_finished() {
        tokio::task::yield_now().await;
    }
    pool.workers.lock().unwrap().push(cancelled);
    assert!(error(pool.shutdown(1000).await).contains("cancel"));
}

async fn wait_taken(pool: &SharedThreadPool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !pool.workers.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn overlapping_waiters(cancel_first: bool) {
    let pool = pool();
    let release = Arc::new(Notify::new());
    let held = release.clone();
    let task = tokio::spawn(async move {
        held.notified().await;
        panic!("late worker failure");
    });
    pool.workers.lock().unwrap().push(task);
    let owner = pool.clone();
    let first = tokio::spawn(async move { owner.shutdown(10_000).await });
    wait_taken(&pool).await;
    if cancel_first {
        first.abort();
    }
    let second = pool.shutdown(0);
    tokio::pin!(second);
    let early = tokio::select! {
        biased;
        result = &mut second => Some(result),
        _ = tokio::task::yield_now() => None,
    };
    let finished_early = early.is_some();
    let state_before_release = pool.status();
    release.notify_one();
    let second = match early {
        Some(result) => result,
        None => tokio::time::timeout(Duration::from_secs(3), second)
            .await
            .unwrap(),
    };
    let first = first.await;
    assert!(!finished_early, "a second caller escaped an in-flight join");
    assert_eq!(state_before_release, SchedulerStatus::ShuttingDown);
    let second = error(second);
    assert!(second.contains("late worker failure"));
    if cancel_first {
        assert!(first.unwrap_err().is_cancelled());
    } else {
        assert_eq!(error(first.unwrap()), second);
    }
}
#[tokio::test]
async fn concurrent_shutdowns_share_one_join_and_error() {
    overlapping_waiters(false).await;
}
#[tokio::test]
async fn cancelled_waiter_does_not_detach_owned_workers() {
    overlapping_waiters(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedule_reports_shutdown_when_queue_closes_during_hook_install() {
    let pool = pool();
    let (release, held) = std::sync::mpsc::channel();
    let gate = Arc::new(HookGate {
        entered: Notify::new(),
        release: std::sync::Mutex::new(held),
    });
    let mailbox = Arc::new(HostileMailbox {
        inner: crate::thread::mailbox::mpsc::MpscMailbox::new(
            1,
            parrot_api::address::ActorPath::placeholder("/hook-shutdown-race"),
        ),
        pool: Arc::downgrade(&pool),
        releases: Arc::default(),
        disposed: Arc::default(),
        release_panics: false,
        drop_panics: false,
        close_dependency: None,
        hook_gate: Some(gate.clone()),
    });
    let owner = pool.clone();
    let schedule = tokio::task::spawn_blocking(move || {
        owner.schedule_now("/hook-shutdown-race", mailbox, None)
    });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .expect("schedule must enter hook installation");

    // Let the real shutdown path close the queue and remove its registration
    // while schedule_now is still inside the mailbox hook.
    pool.shutdown(0).await.unwrap();
    release.send(()).unwrap();
    let result = schedule.await.unwrap();
    assert!(
        matches!(result, Err(SystemError::ShuttingDown)),
        "{result:?}"
    );
    assert!(!pool.is_scheduled("/hook-shutdown-race"));
}

#[tokio::test]
async fn elastic_probe_preserves_large_backlog_thresholds() {
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;
    let cases = [
        ("zero", Duration::ZERO, 1),
        ("submicrosecond", Duration::from_nanos(999), 1),
        ("ordinary", Duration::from_secs(1), 0),
        ("largest u64 micros", Duration::from_micros(u64::MAX), 0),
        (
            "one more microsecond",
            Duration::new(18_446_744_073_709, 551_616_000),
            0,
        ),
        ("millisecond config", Duration::from_millis(1 << 61), 0),
        ("Duration::MAX", Duration::MAX, 0),
    ];
    let mut observed = Vec::new();
    let mut expected = Vec::new();
    for (name, threshold, count) in cases {
        let pool = SharedThreadPool::new_unchecked_for_test(
            Some(SharedThreadPoolConfig {
                pool_size: 0,
                burst_workers_max: 1,
                burst_backlog_threshold: threshold,
                ..Default::default()
            }),
            Handle::current(),
        );
        let mailbox = Arc::new(MpscMailbox::new(1, ActorPath::placeholder("/threshold")));
        assert!(pool.scheduling_queue.push(mailbox).is_ok());
        pool.elastic_controller().probe();
        observed.push((name, pool.metrics().burst_workers_alive));
        expected.push((name, count));
        pool.shutdown(0).await.unwrap();
        assert_eq!(pool.metrics().burst_workers_alive, 0);
    }
    assert_eq!(
        observed, expected,
        "first pressure probe expanded prematurely"
    );
}

#[tokio::test]
async fn elastic_retirement_keeps_completed_worker_errors() {
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;
    let pool = SharedThreadPool::new_unchecked_for_test(
        Some(SharedThreadPoolConfig {
            pool_size: 0,
            burst_workers_max: 1,
            burst_backlog_threshold: Duration::ZERO,
            ..Default::default()
        }),
        Handle::current(),
    );
    let failed = failed_task("retired worker failure").await;
    pool.workers.lock().unwrap().push(failed);
    let mailbox = Arc::new(MpscMailbox::new(1, ActorPath::placeholder("/probe")));
    assert!(pool.scheduling_queue.push(mailbox).is_ok());
    let controller = pool.elastic_controller();
    controller.probe();
    controller.probe();
    let result = pool.shutdown(0).await;
    assert!(error(result).contains("retired worker failure"));
}

#[tokio::test]
async fn hostile_panic_payload_cannot_interrupt_other_worker_joins() {
    struct Hostile(Arc<AtomicUsize>);
    impl Drop for Hostile {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("payload destructor failed");
        }
    }
    struct Retired(Arc<AtomicUsize>);
    impl Drop for Retired {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let pool = pool();
    let disposed = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let payload = Hostile(dropped.clone());
    let failed = tokio::spawn(async move { std::panic::panic_any(payload) });
    while !failed.is_finished() {
        tokio::task::yield_now().await;
    }
    let (ready, started) = tokio::sync::oneshot::channel();
    let count = disposed.clone();
    let pending = tokio::spawn(async move {
        let _guard = Retired(count);
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    started.await.unwrap();
    pool.workers.lock().unwrap().extend([pending, failed]);
    let result = std::panic::AssertUnwindSafe(pool.shutdown(0))
        .catch_unwind()
        .await;
    let returned = match result {
        Ok(result) => Some(result),
        Err(payload) => {
            std::mem::forget(payload);
            None
        }
    };
    let disposed_before_return = disposed.load(Ordering::SeqCst);
    // Clean the original broken implementation's leftovers before asserting.
    for handle in pool.workers.lock().unwrap().drain(..) {
        handle.abort();
    }
    tokio::task::yield_now().await;
    assert!(returned.is_some(), "payload Drop escaped the join boundary");
    let message = error(returned.unwrap());
    assert!(message.contains("payload destructor"), "{message}");
    assert_eq!(disposed_before_return, 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_abort_is_success_only_after_future_destruction() {
    let pool = pool();
    let (dropping, entered) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let disposed = Arc::new(AtomicBool::new(false));
    struct Retired {
        dropping: Option<tokio::sync::oneshot::Sender<()>>,
        held: std::sync::mpsc::Receiver<()>,
        disposed: Arc<AtomicBool>,
    }
    impl Drop for Retired {
        fn drop(&mut self) {
            let _ = self.dropping.take().unwrap().send(());
            let _ = self.held.recv_timeout(Duration::from_secs(5));
            self.disposed.store(true, Ordering::Release);
        }
    }
    let (ready, started) = tokio::sync::oneshot::channel();
    let signal = disposed.clone();
    let worker = tokio::spawn(async move {
        let _guard = Retired {
            dropping: Some(dropping),
            held,
            disposed: signal,
        };
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    started.await.unwrap();
    pool.workers.lock().unwrap().push(worker);
    let owner = pool.clone();
    let first = tokio::spawn(async move { owner.shutdown(0).await });
    tokio::time::timeout(Duration::from_secs(3), entered)
        .await
        .unwrap()
        .unwrap();
    // The destructor positively entered and cannot finish before release. Give
    // an incorrectly detached operation a chance to publish its premature result.
    // The blocked destructor may occupy Tokio's time-driver thread. An external
    // fence makes the observation independent of that runtime's timer progress.
    let (fence, reached) = tokio::sync::oneshot::channel();
    let observer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let _ = fence.send(());
    });
    let early = tokio::select! {
        result = pool.shutdown(0) => Some(result),
        _ = reached => None,
    };
    observer.join().unwrap();
    let state = pool.status();
    let premature_disposal = disposed.load(Ordering::Acquire);
    release.send(()).unwrap();
    first.await.unwrap().unwrap();
    pool.shutdown(1).await.unwrap();
    assert!(
        early.is_none(),
        "shutdown completed while worker Drop was held"
    );
    assert_eq!(state, SchedulerStatus::ShuttingDown);
    assert!(!premature_disposal);
    assert!(disposed.load(Ordering::Acquire));
}

#[tokio::test]
async fn shutdown_retires_long_sleeping_patrol() {
    let pool = SharedThreadPool::new_unchecked_for_test(
        Some(SharedThreadPoolConfig {
            pool_size: 0,
            burst_workers_max: 1,
            burst_backlog_threshold: Duration::from_secs(600),
            ..Default::default()
        }),
        Handle::current(),
    );
    tokio::task::yield_now().await;
    pool.shutdown(0).await.unwrap();
    assert_eq!(
        Arc::strong_count(&pool.is_shutting_down),
        1,
        "patrol still owns shutdown state"
    );
}

#[tokio::test]
async fn dropping_pool_owns_worker_retirement_until_terminal_state() {
    let pool = pool();
    let status = pool.status.clone();
    let (retired, mut receipt) = tokio::sync::oneshot::channel();
    struct Retired(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Retired {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    let (ready, started) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let _guard = Retired(Some(retired));
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    started.await.unwrap();
    pool.workers.lock().unwrap().push(worker);
    drop(pool);
    tokio::time::timeout(Duration::from_secs(3), async {
        while SchedulerStatus::from_usize(status.load(Ordering::Acquire))
            != SchedulerStatus::Shutdown
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(receipt.try_recv().is_ok());
}

#[tokio::test]
async fn all_failed_workers_are_consumed_but_diagnostics_stay_bounded() {
    let pool = pool();
    let disposed = Arc::new(AtomicUsize::new(0));
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for _ in 0..32 {
        let payload = Payload(disposed.clone());
        let worker = tokio::spawn(async move { std::panic::panic_any(payload) });
        while !worker.is_finished() {
            tokio::task::yield_now().await;
        }
        pool.workers.lock().unwrap().push(worker);
    }
    let message = error(pool.shutdown(0).await);
    assert!(
        message.starts_with("32 shared cleanup failure(s):"),
        "{message}"
    );
    assert!(message.len() < 12_000);
    assert_eq!(disposed.load(Ordering::SeqCst), 32);
    assert!(pool.workers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn repeated_elastic_retirement_keeps_ownership_and_bounded_history() {
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;
    let pool = SharedThreadPool::new_unchecked_for_test(
        Some(SharedThreadPoolConfig {
            pool_size: 0,
            burst_workers_max: 1,
            burst_backlog_threshold: Duration::ZERO,
            burst_idle_timeout: Duration::ZERO,
            ..Default::default()
        }),
        Handle::current(),
    );
    let controller = pool.elastic_controller();
    for round in 0..32 {
        let failed = failed_task("retirement history").await;
        pool.workers.lock().unwrap().push(failed);
        let mailbox = Arc::new(MpscMailbox::new(
            1,
            ActorPath::placeholder(format!("/burst/{round}")),
        ));
        assert!(pool.scheduling_queue.push(mailbox).is_ok());
        controller.probe();
        tokio::time::timeout(Duration::from_secs(3), async {
            while pool
                .workers
                .lock()
                .unwrap()
                .iter()
                .any(|h| !h.is_finished())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Yield to the owned retirement observer, without prescribing its fields.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(
            pool.workers.lock().unwrap().len() <= 2,
            "finished workers accumulated"
        );
    }
    let result = error(pool.shutdown(0).await);
    assert!(
        result.starts_with("32 shared cleanup failure(s):"),
        "{result}"
    );
    assert_eq!(result, error(pool.shutdown(0).await));
}

#[tokio::test]
async fn shared_failure_propagates_through_real_system_after_retirement() {
    use crate::thread::config::ThreadActorSystemConfig;
    use crate::thread::system::ThreadActorSystem;
    use parrot_api::system::{ActorSystem, SystemState};
    let system = ThreadActorSystem::new(ThreadActorSystemConfig::default());
    let failed = failed_task("system worker failure").await;
    system
        .scheduler_group()
        .shared_scheduler
        .workers
        .lock()
        .unwrap()
        .push(failed);
    let first = error(system.shutdown_internal().await);
    assert!(first.contains("system worker failure"), "{first}");
    assert_eq!(system.status().state, SystemState::Stopped);
    assert_eq!(first, error(system.shutdown_internal().await));
}

#[tokio::test]
async fn secondary_hostile_panic_payload_cannot_unwind_cleanup() {
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("recursive destructor");
        }
    }
    struct Primary;
    impl Drop for Primary {
        fn drop(&mut self) {
            std::panic::panic_any(Secondary);
        }
    }
    let pool = pool();
    let worker = tokio::spawn(async {
        std::panic::panic_any(Primary);
    });
    while !worker.is_finished() {
        tokio::task::yield_now().await;
    }
    pool.workers.lock().unwrap().push(worker);
    let result = error(pool.shutdown(0).await);
    assert!(result.contains("payload destructor"), "{result}");
    assert_eq!(pool.status(), SchedulerStatus::Shutdown);
}

#[derive(Debug)]
struct HostileMailbox {
    inner: crate::thread::mailbox::mpsc::MpscMailbox,
    pool: std::sync::Weak<SharedThreadPool>,
    releases: Arc<AtomicUsize>,
    disposed: Arc<AtomicUsize>,
    release_panics: bool,
    drop_panics: bool,
    close_dependency: Option<Arc<CloseDependency>>,
    hook_gate: Option<Arc<HookGate>>,
}
#[derive(Debug)]
struct HookGate {
    entered: Notify,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
#[derive(Debug)]
struct CloseDependency {
    entered: Notify,
    retired: tokio::sync::watch::Receiver<bool>,
    calls: AtomicUsize,
    cancelled: AtomicUsize,
}
struct CloseAttempt<'a> {
    dependency: &'a CloseDependency,
    complete: bool,
}
impl Drop for CloseAttempt<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.dependency.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}
impl Drop for HostileMailbox {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            assert!(
                pool.scheduled_paths.try_lock().is_ok(),
                "path lock held during Drop"
            );
            assert!(
                pool.workers.try_lock().is_ok(),
                "worker lock held during Drop"
            );
            assert!(
                pool.burst_spawn_gate.try_lock().is_ok(),
                "spawn gate held during Drop"
            );
            // Re-entry must observe the closed queue, without deadlocking it.
            assert!(pool.scheduling_queue.is_empty());
        }
        self.disposed.fetch_add(1, Ordering::SeqCst);
        assert!(!self.drop_panics, "mailbox destructor failure");
    }
}
#[async_trait::async_trait]
impl Mailbox for HostileMailbox {
    async fn push(
        &self,
        msg: parrot_api::types::BoxedMessage,
        strategy: crate::thread::config::BackpressureStrategy,
    ) -> Result<(), crate::thread::error::MailboxError> {
        self.inner.push(msg, strategy).await
    }
    async fn pop(&self) -> Option<parrot_api::types::BoxedMessage> {
        self.inner.pop().await
    }
    async fn is_empty(&self) -> bool {
        self.inner.is_empty().await
    }
    async fn signal_ready(&self) {
        self.inner.signal_ready().await;
    }
    fn path(&self) -> &parrot_api::address::ActorPath {
        self.inner.path()
    }
    fn capacity(&self) -> usize {
        self.inner.capacity()
    }
    async fn len(&self) -> usize {
        self.inner.len().await
    }
    fn seal(&self) {
        self.inner.seal();
    }
    async fn close(&self) {
        if let Some(dependency) = &self.close_dependency {
            let mut attempt = CloseAttempt {
                dependency,
                complete: false,
            };
            dependency.calls.fetch_add(1, Ordering::SeqCst);
            dependency.entered.notify_one();
            let mut retired = dependency.retired.clone();
            while !*retired.borrow_and_update() {
                retired.changed().await.unwrap();
            }
            attempt.complete = true;
        }
        self.inner.close().await;
    }
    fn set_processor(&self, processor: Arc<dyn crate::thread::processor::ProcessorInterface>) {
        self.inner.set_processor(processor);
    }
    fn get_processor(&self) -> Option<Arc<dyn crate::thread::processor::ProcessorInterface>> {
        self.inner.get_processor()
    }
    // No worker executes this fixture; use the real pool admission/close path.
    fn has_processor(&self) -> bool {
        true
    }
    fn set_wake_hook(&self, hook: crate::thread::mailbox::WakeHook) {
        if let Some(gate) = &self.hook_gate {
            gate.entered.notify_one();
            gate.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .expect("scheduler hook release");
        }
        self.inner.set_wake_hook(hook);
    }
    fn fire_wake_hook(&self) {
        self.inner.fire_wake_hook();
    }
    fn schedule_state(&self) -> &crate::thread::mailbox::ScheduleState {
        if let Some(pool) = self.pool.upgrade()
            && pool.is_shutting_down.load(Ordering::Acquire)
        {
            assert!(pool.scheduling_queue.is_empty());
            assert!(pool.scheduled_paths.try_lock().is_ok());
            self.releases.fetch_add(1, Ordering::SeqCst);
            assert!(!self.release_panics, "mailbox release failure");
        }
        self.inner.schedule_state()
    }
}

#[tokio::test]
async fn mailbox_release_and_destructor_failures_do_not_skip_other_resources() {
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;
    let pool = pool();
    let releases = Arc::new(AtomicUsize::new(0));
    let disposed = Arc::new(AtomicUsize::new(0));
    for (path, release_panics, drop_panics) in [
        ("/bad-release", true, false),
        ("/bad-drop", false, true),
        ("/healthy", false, false),
    ] {
        pool.schedule_now(
            path,
            Arc::new(HostileMailbox {
                inner: MpscMailbox::new(1, ActorPath::placeholder(path)),
                pool: Arc::downgrade(&pool),
                releases: releases.clone(),
                disposed: disposed.clone(),
                release_panics,
                drop_panics,
                close_dependency: None,
                hook_gate: None,
            }),
            None,
        )
        .unwrap();
    }
    let failed = failed_task("worker after mailbox failure").await;
    pool.workers.lock().unwrap().push(failed);
    let result = error(pool.shutdown(0).await);
    assert!(result.contains("mailbox release failure"), "{result}");
    assert!(result.contains("mailbox destructor failure"), "{result}");
    assert!(result.contains("worker after mailbox failure"), "{result}");
    assert_eq!(disposed.load(Ordering::SeqCst), 3);
    assert_eq!(releases.load(Ordering::SeqCst), 3);
    assert_eq!(pool.status(), SchedulerStatus::Shutdown);
    assert_eq!(result, error(pool.shutdown(0).await));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_shutdown_closes_queued_mailbox_before_releasing_its_arc() {
    use crate::thread::config::BackpressureStrategy;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;

    struct Held {
        entered: Arc<Notify>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Held {
        fn drop(&mut self) {
            self.entered.notify_one();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .expect("host shutdown release");
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    let pool = pool();
    let mailbox = Arc::new(MpscMailbox::new(1, ActorPath::placeholder("/host-drain")));
    let entered = Arc::new(Notify::new());
    let (release, held) = std::sync::mpsc::channel();
    let drops = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(Held {
                entered: entered.clone(),
                release: std::sync::Mutex::new(held),
                drops: drops.clone(),
            }),
            BackpressureStrategy::Error,
        )
        .await
        .unwrap();
    assert!(pool.scheduling_queue.push(mailbox.clone()).is_ok());

    let owner = pool.clone();
    let shutdown = tokio::spawn(async move { owner.shutdown(5_000).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("host shutdown must use mailbox close cleanup");
    assert!(!shutdown.is_finished());
    release.send(()).unwrap();
    shutdown.await.unwrap().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(mailbox.is_closed().await);
    assert_eq!(mailbox.len().await, 0);
}

async fn expired_grace_keeps_mailbox_drain(kind: usize, hostile: bool) {
    use crate::thread::{
        BackpressureStrategy,
        mailbox::{mpsc::MpscMailbox, spsc::SpscMailbox, spsc_ringbuf::SpscRingbufMailbox},
        scheduler::queue::Lane,
    };
    use parrot_api::address::ActorPath;

    struct Held {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        drops: Arc<AtomicUsize>,
        hostile: bool,
    }
    impl Drop for Held {
        fn drop(&mut self) {
            let _ = self.entered.take().unwrap().send(());
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            self.drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.hostile, "late mailbox disposal failure");
        }
    }
    struct Release(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(release) = self.0.take() {
                let _ = release.send(());
            }
        }
    }
    for source in 0..4 {
        for cancel in [false, true] {
            let mut owned = SharedThreadPool::new_unchecked_for_test(
                Some(SharedThreadPoolConfig {
                    pool_size: 0,
                    burst_workers_max: 0,
                    ..Default::default()
                }),
                Handle::current(),
            );
            owned.scheduling_queue = Arc::new(SchedulingQueue::with_workers(1, 1));
            let pool = Arc::new(owned);
            let path = ActorPath::placeholder("/expired-host-drain");
            let mailbox: Arc<dyn Mailbox> = match kind {
                0 => Arc::new(MpscMailbox::new(1, path)),
                1 => Arc::new(SpscMailbox::new(1, path)),
                _ => Arc::new(SpscRingbufMailbox::new(1, path)),
            };
            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let release = Release(Some(release));
            let drops = Arc::new(AtomicUsize::new(0));
            mailbox
                .push(
                    Box::new(Held {
                        entered: Some(entered),
                        release: held,
                        drops: drops.clone(),
                        hostile,
                    }),
                    BackpressureStrategy::Error,
                )
                .await
                .unwrap();
            match source {
                0 => pool.scheduling_queue.push(mailbox.clone()).unwrap(),
                1 | 2 => {
                    assert!(pool.scheduling_queue.register(&mailbox));
                    assert!(mailbox.schedule_state().try_enqueue());
                    if source == 2 {
                        pool.scheduling_queue
                            .push(Arc::new(MpscMailbox::new(
                                1,
                                ActorPath::placeholder("/resident-filler"),
                            )))
                            .unwrap();
                    }
                    assert!(pool.scheduling_queue.enqueue_on_worker(
                        mailbox.clone(),
                        Lane::Normal,
                        Some(0),
                    ));
                    assert_eq!(pool.scheduling_queue.local_len(), usize::from(source == 1));
                    assert_eq!(
                        pool.scheduling_queue.deferred_len(),
                        usize::from(source == 2)
                    );
                }
                _ => {
                    // No resident entry: the host path index must also be drained.
                    pool.scheduled_paths
                        .lock()
                        .unwrap()
                        .insert("/registered".into(), mailbox.clone());
                }
            }
            let owner = pool.clone();
            let first = tokio::spawn(async move { owner.shutdown(0).await });
            tokio::time::timeout(Duration::from_secs(3), started)
                .await
                .unwrap()
                .unwrap();
            if cancel {
                first.abort();
            }
            // An external fence remains runnable while the destructor is held.
            let (fence, observed) = tokio::sync::oneshot::channel();
            let observer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                let _ = fence.send(());
            });
            let early = tokio::select! {
                result = pool.shutdown(0) => Some(result),
                _ = observed => None,
            };
            let status = pool.status();
            let before_release = drops.load(Ordering::SeqCst);
            drop(release);
            observer.join().unwrap();
            let first = first.await;
            let terminal = tokio::time::timeout(Duration::from_secs(3), pool.shutdown(0))
                .await
                .unwrap();
            let retired_before_terminal = drops.load(Ordering::SeqCst);
            // This extra close cannot conceal a premature pool completion.
            let closed = std::panic::AssertUnwindSafe(mailbox.close())
                .catch_unwind()
                .await;
            assert!(
                early.is_none(),
                "premature host receipt: kind={kind} source={source} cancel={cancel}"
            );
            assert_eq!(status, SchedulerStatus::ShuttingDown);
            assert_eq!(before_release, 0);
            assert_eq!(retired_before_terminal, 1);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(mailbox.is_closed().await);
            assert_eq!(mailbox.len().await, 0);
            let message = if hostile {
                let message = error(terminal);
                assert!(
                    message.contains("late mailbox disposal failure"),
                    "{message}"
                );
                assert_eq!(message, error(pool.shutdown(0).await));
                assert!(closed.is_err());
                Some(message)
            } else {
                terminal.unwrap();
                closed.unwrap();
                None
            };
            if cancel {
                assert!(first.unwrap_err().is_cancelled());
            } else if let Some(message) = message {
                assert_eq!(error(first.unwrap()), message);
            } else {
                first.unwrap().unwrap();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_grace_keeps_mpsc_drain_owned_through_cancel_and_repeated_shutdown() {
    expired_grace_keeps_mailbox_drain(0, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_grace_keeps_spsc_drain_owned_through_cancel_and_repeated_shutdown() {
    expired_grace_keeps_mailbox_drain(1, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_grace_keeps_ring_drain_owned_through_cancel_and_repeated_shutdown() {
    expired_grace_keeps_mailbox_drain(2, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_grace_preserves_late_disposal_errors_for_all_shutdown_waiters() {
    for kind in 0..3 {
        expired_grace_keeps_mailbox_drain(kind, true).await;
    }
}

#[tokio::test]
async fn grace_expiry_joins_workers_before_finishing_the_original_mailbox_close() {
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;
    struct WorkerExit(tokio::sync::watch::Sender<bool>);
    impl Drop for WorkerExit {
        fn drop(&mut self) {
            self.0.send_replace(true);
        }
    }
    let pool = pool();
    let (retire, retired) = tokio::sync::watch::channel(false);
    let dependency = Arc::new(CloseDependency {
        entered: Notify::new(),
        retired,
        calls: AtomicUsize::new(0),
        cancelled: AtomicUsize::new(0),
    });
    pool.schedule_now(
        "/worker-dependent-close",
        Arc::new(HostileMailbox {
            inner: MpscMailbox::new(1, ActorPath::placeholder("/worker-dependent-close")),
            pool: Arc::downgrade(&pool),
            releases: Arc::default(),
            disposed: Arc::default(),
            release_panics: false,
            drop_panics: false,
            close_dependency: Some(dependency.clone()),
            hook_gate: None,
        }),
        None,
    )
    .unwrap();
    let (started, entered) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let _exit = WorkerExit(retire);
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    entered.await.unwrap();
    pool.workers.lock().unwrap().push(worker);
    tokio::time::timeout(Duration::from_secs(3), pool.shutdown(0))
        .await
        .unwrap()
        .unwrap();
    assert!(*dependency.retired.borrow());
    assert_eq!(dependency.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        dependency.cancelled.load(Ordering::SeqCst),
        0,
        "grace expiry must retain the original close futures, not cancel or restart them"
    );
    pool.shutdown(0).await.unwrap();
}

#[tokio::test]
async fn huge_unicode_panic_messages_have_bounded_retained_diagnostics() {
    let pool = pool();
    for _ in 0..16 {
        let worker = tokio::spawn(async {
            panic!("{}", "🦜".repeat(100_000));
        });
        while !worker.is_finished() {
            tokio::task::yield_now().await;
        }
        pool.workers.lock().unwrap().push(worker);
    }
    let result = error(pool.shutdown(0).await);
    assert!(result.starts_with("16 shared cleanup failure(s):"));
    assert!(
        result.len() < 34_000,
        "diagnostics grew to {} bytes",
        result.len()
    );
    assert!(result.contains('…'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_retirement_does_not_allow_unbounded_burst_handle_accumulation() {
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;
    struct Payload {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            let _ = self.started.take().unwrap().send(());
            let _ = self.release.recv_timeout(Duration::from_secs(5));
        }
    }
    let pool = SharedThreadPool::new_unchecked_for_test(
        Some(SharedThreadPoolConfig {
            pool_size: 0,
            burst_workers_max: 1,
            burst_backlog_threshold: Duration::ZERO,
            burst_idle_timeout: Duration::ZERO,
            ..Default::default()
        }),
        Handle::current(),
    );
    let (started, receipt) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let payload = Payload {
        started: Some(started),
        release: held,
    };
    let worker = tokio::spawn(async move {
        std::panic::panic_any(payload);
    });
    while !worker.is_finished() {
        tokio::task::yield_now().await;
    }
    pool.workers.lock().unwrap().push(worker);
    let controller = pool.elastic_controller();
    assert!(
        pool.scheduling_queue
            .push(Arc::new(MpscMailbox::new(
                1,
                ActorPath::placeholder("/start")
            )))
            .is_ok()
    );
    controller.probe();
    tokio::time::timeout(Duration::from_secs(3), receipt)
        .await
        .unwrap()
        .unwrap();
    for round in 0..64 {
        let mailbox = Arc::new(MpscMailbox::new(
            1,
            ActorPath::placeholder(format!("/held/{round}")),
        ));
        assert!(pool.scheduling_queue.push(mailbox).is_ok());
        controller.probe();
        tokio::task::yield_now().await;
    }
    let retained = pool.workers.lock().unwrap().len();
    release.send(()).unwrap();
    let result = error(pool.shutdown(0).await);
    assert!(result.contains("non-string panic payload"));
    assert!(
        retained <= 2,
        "{retained} handles accumulated while retirement was blocked"
    );
}
