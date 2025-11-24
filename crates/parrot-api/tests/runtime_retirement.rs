use parrot_api::{
    errors::ActorError,
    runtime::{
        ActorRuntime, LoadBalancingStrategy, RuntimeConfig, SchedulerConfig, TokioActorRuntime,
    },
};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};
use tokio::sync::oneshot;

const WAIT: Duration = Duration::from_secs(5);

struct TlsExit {
    entered: Option<oneshot::Sender<()>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    exits: Arc<AtomicUsize>,
}

impl Drop for TlsExit {
    fn drop(&mut self) {
        let _ = self.entered.take().unwrap().send(());
        let (lock, changed) = &*self.gate;
        let _open = changed
            .wait_while(lock.lock().unwrap(), |open| !*open)
            .unwrap();
        self.exits.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static EXIT: RefCell<Option<TlsExit>> = const { RefCell::new(None) };
}

struct Release(Arc<(Mutex<bool>, Condvar)>);

impl Drop for Release {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }
}

fn probe(exits: &Arc<AtomicUsize>) -> (TlsExit, oneshot::Receiver<()>, Release) {
    let (entered, observed) = oneshot::channel();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    (
        TlsExit {
            entered: Some(entered),
            gate: gate.clone(),
            exits: exits.clone(),
        },
        observed,
        Release(gate),
    )
}

#[derive(Default)]
struct CrossExitState {
    entered: Mutex<(usize, bool)>,
    changed: Condvar,
    exited: AtomicUsize,
}

struct CrossExit(Arc<CrossExitState>);

impl Drop for CrossExit {
    fn drop(&mut self) {
        let mut entered = self.0.entered.lock().unwrap();
        entered.0 += 1;
        self.0.changed.notify_all();
        let _ready = self
            .0
            .changed
            .wait_while(entered, |(count, force)| *count < 2 && !*force)
            .unwrap();
        self.0.exited.fetch_add(1, Ordering::SeqCst);
    }
}

struct ReleaseCrossExit(Arc<CrossExitState>);

impl Drop for ReleaseCrossExit {
    fn drop(&mut self) {
        self.0.entered.lock().unwrap().1 = true;
        self.0.changed.notify_all();
    }
}

thread_local! {
    static CROSS_EXIT: RefCell<Option<CrossExit>> = const { RefCell::new(None) };
}

#[tokio::test]
async fn placement_workers_begin_tls_shutdown_before_any_worker_join_is_awaited() {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        worker_threads: Some(2),
        scheduler_config: SchedulerConfig {
            load_balancing: LoadBalancingStrategy::LeastLoaded,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    let state = Arc::new(CrossExitState::default());
    let _release_on_failure = ReleaseCrossExit(state.clone());
    let mut tasks = Vec::new();
    let mut finishes = Vec::new();
    for _ in 0..2 {
        let state = state.clone();
        let executor = runtime.clone();
        let (entered, running) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        tasks.push(tokio::spawn(async move {
            executor
                .spawn(async move {
                    CROSS_EXIT.with(|slot| *slot.borrow_mut() = Some(CrossExit(state)));
                    entered.send(()).unwrap();
                    let _ = finished.await;
                    std::thread::current().id()
                })
                .await
                .unwrap()
        }));
        tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
        finishes.push(finish);
    }
    for finish in finishes {
        finish.send(()).unwrap();
    }
    let first = tokio::time::timeout(WAIT, tasks.remove(0))
        .await
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(WAIT, tasks.remove(0))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        first, second,
        "fixture did not install TLS on two actual placement workers"
    );
    tokio::time::timeout(WAIT, runtime.clone().shutdown())
        .await
        .expect("placement TLS shutdown serialized one join before stopping the other worker")
        .unwrap();
    assert_eq!(state.exited.load(Ordering::SeqCst), 2);
}

async fn install(worker: TlsExit, helper: TlsExit) {
    EXIT.with(|slot| *slot.borrow_mut() = Some(worker));
    tokio::task::spawn_blocking(move || {
        EXIT.with(|slot| *slot.borrow_mut() = Some(helper));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn shutdown_joins_both_lanes_and_blocking_helpers_with_live_clones() {
    for strategy in [
        LoadBalancingStrategy::RoundRobin,
        LoadBalancingStrategy::Random,
        LoadBalancingStrategy::LeastLoaded,
    ] {
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(1),
            io_threads: Some(1),
            scheduler_config: SchedulerConfig {
                load_balancing: strategy,
                ..Default::default()
            },
        })
        .await
        .unwrap();
        let exits = Arc::new(AtomicUsize::new(0));
        let mut observed = Vec::new();
        let mut releases = Vec::new();
        let mut probes = Vec::new();
        for _ in 0..4 {
            let (input, entered, release) = probe(&exits);
            probes.push(input);
            observed.push(entered);
            releases.push(release);
        }
        let helper = probes.pop().unwrap();
        let worker = probes.pop().unwrap();
        runtime.spawn(install(worker, helper)).await.unwrap();
        let helper = probes.pop().unwrap();
        let worker = probes.pop().unwrap();
        runtime.spawn_io(install(worker, helper)).await.unwrap();

        let mut first = Box::pin(runtime.clone().shutdown());
        let premature = futures::poll!(&mut first).is_ready();
        drop(first);
        assert!(
            !premature,
            "shutdown returned before owned TLS destruction with live clones"
        );
        // Retirement must continue after the only shutdown waiter is cancelled.
        for entered in observed {
            tokio::time::timeout(WAIT, entered).await.unwrap().unwrap();
        }
        assert_eq!(exits.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.metrics().active_actors, 0);
        let mut second = Box::pin(runtime.clone().shutdown());
        let mut third = Box::pin(runtime.clone().shutdown());
        assert!(futures::poll!(&mut second).is_pending());
        assert!(futures::poll!(&mut third).is_pending());
        assert!(matches!(
            runtime.spawn(async { 1 }).await,
            Err(ActorError::Stopped)
        ));
        assert!(matches!(
            runtime.spawn_io(async { 1 }).await,
            Err(ActorError::Stopped)
        ));

        while releases.len() > 1 {
            drop(releases.pop());
            assert!(futures::poll!(&mut second).is_pending());
        }
        drop(releases);
        tokio::time::timeout(WAIT, async {
            second.await.unwrap();
            third.await.unwrap();
            runtime.clone().shutdown().await.unwrap();
        })
        .await
        .unwrap();
        assert_eq!(exits.load(Ordering::SeqCst), 4);
    }
}

#[tokio::test]
async fn cancelled_shutdown_before_idle_still_retires_owned_executors() {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        worker_threads: Some(1),
        io_threads: Some(1),
        ..Default::default()
    })
    .await
    .unwrap();
    let exits = Arc::new(AtomicUsize::new(0));
    let (input, entered, release) = probe(&exits);
    let (started, running) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let mut task = Box::pin(runtime.spawn_io(async move {
        EXIT.with(|slot| *slot.borrow_mut() = Some(input));
        started.send(()).unwrap();
        let _ = finished.await;
    }));
    assert!(futures::poll!(&mut task).is_pending());
    tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    let mut closing = Box::pin(runtime.clone().shutdown());
    assert!(futures::poll!(&mut closing).is_pending());
    drop(closing);
    finish.send(()).unwrap();
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    tokio::time::timeout(WAIT, entered).await.unwrap().unwrap();
    let mut rejoin = Box::pin(runtime.clone().shutdown());
    assert!(futures::poll!(&mut rejoin).is_pending());
    drop(release);
    tokio::time::timeout(WAIT, rejoin).await.unwrap().unwrap();
    assert_eq!(exits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn own_executor_shutdown_fails_without_deadlocking_or_closing_admission() {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        worker_threads: Some(1),
        io_threads: Some(1),
        ..Default::default()
    })
    .await
    .unwrap();
    let actor = runtime.clone();
    let result = tokio::time::timeout(WAIT, runtime.spawn(async move { actor.shutdown().await }))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(ActorError::InternalError(message))
        if message.contains("outside its submitted tasks")));
    let io = runtime.clone();
    let result = tokio::time::timeout(WAIT, runtime.spawn_io(async move { io.shutdown().await }))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(ActorError::InternalError(message))
        if message.contains("outside its submitted tasks")));
    assert_eq!(runtime.spawn(async { 41 }).await.unwrap(), 41);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn nested_borrowed_submissions_cannot_wait_for_their_own_outer_shutdown() {
    let outer = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let inner = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let closing = outer.clone();
    let nested = inner.clone();
    let result = tokio::time::timeout(
        WAIT,
        outer.spawn(async move { nested.spawn(async move { closing.shutdown().await }).await }),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(matches!(result, Err(ActorError::InternalError(message))
        if message.contains("outside its submitted tasks")));
    assert_eq!(outer.spawn(async { 42 }).await.unwrap(), 42);
    inner.shutdown().await.unwrap();
    outer.shutdown().await.unwrap();
}
