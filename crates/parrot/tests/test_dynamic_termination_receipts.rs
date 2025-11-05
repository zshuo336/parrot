//! Termination receipts describe actual user-resource disposal, independently
//! of stop deadlines. Gates hold known execution phases; no timing guess starts a stop.
use parrot::thread::{
    config::{SchedulingMode, SupervisorStrategy, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    error::SystemError,
    system::{Terminated, ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    supervisor::DeathReason,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(3);
struct Gate {
    entered: tokio::sync::Notify,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    exited: AtomicBool,
}
impl Gate {
    fn new() -> (Arc<Self>, std::sync::mpsc::Sender<()>) {
        let (release, receiver) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                entered: Default::default(),
                release: Mutex::new(receiver),
                exited: AtomicBool::new(false),
            }),
            release,
        )
    }
    fn block(&self) {
        self.entered.notify_one();
        let _ = self.release.lock().unwrap().recv_timeout(LIMIT);
        self.exited.store(true, Ordering::SeqCst);
    }
}
struct Target {
    handler: Option<Arc<Gate>>,
    destructor: Option<Arc<Gate>>,
    disposed: Arc<AtomicBool>,
    panic_drop: bool,
}
impl Drop for Target {
    fn drop(&mut self) {
        if let Some(gate) = &self.destructor {
            gate.block();
        }
        self.disposed.store(true, Ordering::SeqCst);
        assert!(!self.panic_drop, "intentional destructor panic");
    }
}
impl Actor for Target {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<bool>() {
                panic!("intentional handler panic");
            }
            if msg.is::<u8>()
                && let Some(gate) = &self.handler
            {
                gate.block();
            }
            Ok(msg)
        })
    }
}
struct Watcher(
    Arc<AtomicBool>,
    tokio::sync::mpsc::UnboundedSender<(bool, DeathReason)>,
);
impl ParrotTypedDispatch for Watcher {
    type Msg = Terminated;
    type Reply = ();
    fn dispatch(&mut self, event: Terminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let _ = self.1.send((self.0.load(Ordering::SeqCst), event.reason));
            Ok(())
        })
    }
}
impl ParrotMsgVariant<Watcher> for Terminated {
    fn inject(x: Self) -> Self {
        x
    }
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
fn modes() -> [SchedulingMode; 3] {
    [
        SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "retirement".into(),
        },
    ]
}
fn system(mode: SchedulingMode, timeout: Duration) -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: mode,
        shared_pool_size: 2,
        shutdown_timeout: timeout,
        ..Default::default()
    })
}
async fn gone(system: &ThreadActorSystem) {
    tokio::time::timeout(LIMIT, async {
        while system.get_actor_ref("/target").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual disposal must eventually release registration");
}
async fn watcher(
    system: &Arc<ThreadActorSystem>,
    disposed: Arc<AtomicBool>,
) -> tokio::sync::mpsc::UnboundedReceiver<(bool, DeathReason)> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    system
        .spawn_typed::<Watcher, Terminated>(Watcher(disposed, sender), "/observer")
        .await
        .unwrap();
    system
        .watch("/observer".into(), "/target".into())
        .await
        .unwrap();
    receiver
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_drop_retains_path_and_delays_death_in_all_modes() {
    for mode in modes() {
        let system = system(mode, Duration::from_millis(60));
        let disposed = Arc::new(AtomicBool::new(false));
        let (gate, release) = Gate::new();
        let actor = system
            .spawn_at(
                Target {
                    handler: None,
                    destructor: Some(gate.clone()),
                    disposed: disposed.clone(),
                    panic_drop: false,
                },
                "/target",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        actor.send(Box::new(())).await.unwrap();
        let mut notices = watcher(&system, disposed.clone()).await;
        let stopping = system.clone();
        let stop = tokio::spawn(async move { stopping.stop_actor("/target").await });
        tokio::time::timeout(LIMIT, gate.entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(LIMIT, stop).await.unwrap().unwrap(),
            Err(SystemError::Timeout(_))
        ));
        assert!(!disposed.load(Ordering::SeqCst));
        assert!(system.get_actor_ref("/target").is_some());
        assert!(notices.try_recv().is_err());
        // A timed-out result is sticky for concurrent/later stop callers.
        assert!(matches!(
            system.stop_actor("/target").await,
            Err(SystemError::Timeout(_))
        ));
        assert!(
            system
                .spawn_at(
                    Target {
                        handler: None,
                        destructor: None,
                        disposed: Arc::default(),
                        panic_drop: false
                    },
                    "/target",
                    None,
                    Default::default()
                )
                .await
                .is_err()
        );
        release.send(()).unwrap();
        let (after_drop, reason) = tokio::time::timeout(LIMIT, notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(after_drop);
        assert!(
            matches!(reason, DeathReason::Normal),
            "successful hook plus slow Drop: {reason:?}"
        );
        gone(&system).await;
        let replacement = system
            .spawn_at(
                Target {
                    handler: None,
                    destructor: None,
                    disposed: Arc::default(),
                    panic_drop: false,
                },
                "/target",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        replacement.send(Box::new(())).await.unwrap();
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_handler_is_not_reported_dead_or_replaced_in_all_modes() {
    for mode in modes() {
        let system = system(mode, Duration::from_millis(60));
        let disposed = Arc::new(AtomicBool::new(false));
        let (gate, release) = Gate::new();
        let actor = system
            .spawn_at(
                Target {
                    handler: Some(gate.clone()),
                    destructor: None,
                    disposed: disposed.clone(),
                    panic_drop: false,
                },
                "/target",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        actor.send(Box::new(())).await.unwrap();
        let mut notices = watcher(&system, disposed.clone()).await;
        actor.deliver(Box::new(1u8)).await.unwrap();
        tokio::time::timeout(LIMIT, gate.entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            system.stop_actor("/target").await,
            Err(SystemError::Timeout(_))
        ));
        assert!(system.get_actor_ref("/target").is_some());
        assert!(!disposed.load(Ordering::SeqCst));
        assert!(notices.try_recv().is_err());
        release.send(()).unwrap();
        let (after_drop, reason) = tokio::time::timeout(LIMIT, notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(after_drop);
        assert!(matches!(reason, DeathReason::Killed), "{reason:?}");
        gone(&system).await;
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_stop_and_drop_panic_both_wait_for_disposal() {
    for mode in modes() {
        for panic_drop in [false, true] {
            let system = system(mode.clone(), Duration::from_secs(1));
            let disposed = Arc::new(AtomicBool::new(false));
            let actor = system
                .spawn_at(
                    Target {
                        handler: None,
                        destructor: None,
                        disposed: disposed.clone(),
                        panic_drop,
                    },
                    "/target",
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
            actor.send(Box::new(())).await.unwrap();
            let mut notices = watcher(&system, disposed.clone()).await;
            let result = system.stop_actor("/target").await;
            assert!(disposed.load(Ordering::SeqCst));
            assert!(system.get_actor_ref("/target").is_none());
            if panic_drop {
                assert!(matches!(result, Err(SystemError::Panic(_))), "{result:?}");
            } else {
                result.unwrap();
            }
            let (after_drop, reason) = tokio::time::timeout(LIMIT, notices.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(after_drop);
            assert_eq!(
                matches!(reason, DeathReason::Panic(_)),
                panic_drop,
                "{reason:?}"
            );
            system.shutdown_internal().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_timeout_keeps_cleanup_and_registry_alive() {
    for mode in modes() {
        let system = system(mode, Duration::from_millis(60));
        let disposed = Arc::new(AtomicBool::new(false));
        let (gate, release) = Gate::new();
        let actor = system
            .spawn_at(
                Target {
                    handler: None,
                    destructor: Some(gate.clone()),
                    disposed: disposed.clone(),
                    panic_drop: false,
                },
                "/target",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        actor.send(Box::new(())).await.unwrap();
        assert!(matches!(
            system.shutdown_internal().await,
            Err(SystemError::Timeout(_))
        ));
        tokio::time::timeout(LIMIT, gate.entered.notified())
            .await
            .unwrap();
        assert!(system.get_actor_ref("/target").is_some());
        assert!(!disposed.load(Ordering::SeqCst));
        release.send(()).unwrap();
        gone(&system).await;
        assert!(disposed.load(Ordering::SeqCst));
        assert!(matches!(
            system.shutdown_internal().await,
            Err(SystemError::Timeout(_))
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_drop_preserves_owned_disposal_in_all_modes() {
    for mode in modes() {
        let system = system(mode, Duration::from_millis(60));
        let disposed = Arc::new(AtomicBool::new(false));
        let (gate, release) = Gate::new();
        let actor = system
            .spawn_at(
                Target {
                    handler: None,
                    destructor: Some(gate.clone()),
                    disposed: disposed.clone(),
                    panic_drop: false,
                },
                "/target",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        actor.send(Box::new(())).await.unwrap();
        let weak = Arc::downgrade(&system);
        drop(system);
        assert!(weak.upgrade().is_none());
        tokio::time::timeout(LIMIT, gate.entered.notified())
            .await
            .unwrap();
        assert!(!disposed.load(Ordering::SeqCst));
        release.send(()).unwrap();
        tokio::time::timeout(LIMIT, async {
            while !disposed.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_restart_never_overlaps_a_failed_instances_slow_drop() {
    for mode in modes() {
        let system = system(mode.clone(), Duration::from_millis(60));
        let disposed = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let (gate, release) = Gate::new();
        let factory_count = count.clone();
        let factory_gate = gate.clone();
        let factory_disposed = disposed.clone();
        let actor = system
            .spawn_supervised_with_config(
                move || {
                    factory_count.fetch_add(1, Ordering::SeqCst);
                    Target {
                        handler: None,
                        destructor: Some(factory_gate.clone()),
                        disposed: factory_disposed.clone(),
                        panic_drop: false,
                    }
                },
                "/target",
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    supervisor_strategy: Some(SupervisorStrategy::Restart {
                        max_retries: 3,
                        within: Duration::from_secs(10),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        actor.send(Box::new(())).await.unwrap();
        let mut notices = watcher(&system, disposed.clone()).await;
        assert!(actor.send(Box::new(true)).await.is_err());
        tokio::time::timeout(LIMIT, gate.entered.notified())
            .await
            .unwrap();
        // Wait past the actual configured retirement budget without cancelling restart.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(system.get_actor_ref("/target").is_some());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(notices.try_recv().is_err());
        release.send(()).unwrap();
        let (after_drop, reason) = tokio::time::timeout(LIMIT, notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(after_drop);
        assert!(matches!(reason, DeathReason::Panic(_)));
        gone(&system).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "expired retirement must not restart later"
        );
        system.shutdown_internal().await.unwrap();
    }
}

struct SuspendedDrop {
    gate: Arc<Gate>,
    panic: bool,
}
impl Drop for SuspendedDrop {
    fn drop(&mut self) {
        self.gate.block();
        assert!(!self.panic, "intentional suspended future Drop panic");
    }
}
struct SuspendedActor {
    gate: Arc<Gate>,
    entered: Arc<tokio::sync::Notify>,
    hook: bool,
    panic: bool,
    disposed: Arc<AtomicBool>,
}
impl SuspendedActor {
    async fn suspend(&self) {
        let _guard = SuspendedDrop {
            gate: self.gate.clone(),
            panic: self.panic,
        };
        self.entered.notify_one();
        std::future::pending::<()>().await;
    }
}
impl Drop for SuspendedActor {
    fn drop(&mut self) {
        self.disposed.store(true, Ordering::SeqCst);
    }
}
impl Actor for SuspendedActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if !self.hook && msg.is::<u8>() {
                self.suspend().await;
            }
            Ok(msg)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.hook {
                self.suspend().await;
            }
            Ok(())
        })
    }
}
#[tokio::test(flavor = "current_thread")]
async fn suspended_future_disposal_is_owned_isolated_and_cannot_kill_a_neighbour() {
    for mode in modes() {
        for hook in [false, true] {
            for panic in [false, true] {
                let system = system(mode.clone(), Duration::from_millis(60));
                let disposed = Arc::new(AtomicBool::new(false));
                let entered = Arc::new(tokio::sync::Notify::new());
                let (gate, release) = Gate::new();
                let actor = system
                    .spawn_at(
                        SuspendedActor {
                            gate: gate.clone(),
                            entered: entered.clone(),
                            hook,
                            panic,
                            disposed: disposed.clone(),
                        },
                        "/target",
                        None,
                        Default::default(),
                    )
                    .await
                    .unwrap();
                let neighbour = system
                    .spawn_at(
                        Target {
                            handler: None,
                            destructor: None,
                            disposed: Arc::default(),
                            panic_drop: false,
                        },
                        "/neighbour",
                        None,
                        Default::default(),
                    )
                    .await
                    .unwrap();
                actor.send(Box::new(())).await.unwrap();
                let mut notices = watcher(&system, disposed.clone()).await;
                if !hook {
                    actor.deliver(Box::new(1u8)).await.unwrap();
                    tokio::time::timeout(LIMIT, entered.notified())
                        .await
                        .unwrap();
                }
                let stopping = system.clone();
                let started = tokio::time::Instant::now();
                let stop = tokio::spawn(async move { stopping.stop_actor("/target").await });
                tokio::time::timeout(LIMIT, gate.entered.notified())
                    .await
                    .unwrap();
                assert!(matches!(
                    tokio::time::timeout(LIMIT, stop).await.unwrap().unwrap(),
                    Err(SystemError::Timeout(_))
                ));
                // Current-thread test runtime must progress even during shared
                // future destruction; affinity modes legitimately hold their shard.
                assert!(started.elapsed() < Duration::from_secs(1));
                assert!(system.get_actor_ref("/target").is_some());
                assert!(!disposed.load(Ordering::SeqCst));
                assert!(notices.try_recv().is_err());
                release.send(()).unwrap();
                let (after_drop, reason) = tokio::time::timeout(LIMIT, notices.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(after_drop);
                assert_eq!(matches!(reason, DeathReason::Panic(_)), panic, "{reason:?}");
                gone(&system).await;
                neighbour
                    .send_with_timeout(Box::new(()), Some(LIMIT))
                    .await
                    .unwrap();
                system.shutdown_internal().await.unwrap();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_retains_a_fast_destructor_error_after_registry_removal() {
    for mode in modes() {
        let system = system(mode, Duration::from_secs(1));
        let disposed = Arc::new(AtomicBool::new(false));
        let actor = system
            .spawn_at(
                Target {
                    handler: None,
                    destructor: None,
                    disposed: disposed.clone(),
                    panic_drop: true,
                },
                "/target",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        actor.send(Box::new(())).await.unwrap();
        assert!(matches!(
            system.shutdown_internal().await,
            Err(SystemError::Panic(_))
        ));
        assert!(disposed.load(Ordering::SeqCst));
        assert_eq!(system.actor_count(), 0);
        assert!(matches!(
            system.shutdown_internal().await,
            Err(SystemError::Panic(_))
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_restart_factory_observes_old_instance_already_disposed() {
    for mode in modes() {
        let system = system(mode.clone(), Duration::from_secs(1));
        let count = Arc::new(AtomicUsize::new(0));
        let disposed = Arc::new(AtomicBool::new(true));
        let factory_count = count.clone();
        let factory_disposed = disposed.clone();
        let actor = system
            .spawn_supervised_with_config(
                move || {
                    assert!(
                        factory_disposed.swap(false, Ordering::SeqCst),
                        "factory ran before old instance Drop"
                    );
                    factory_count.fetch_add(1, Ordering::SeqCst);
                    Target {
                        handler: None,
                        destructor: None,
                        disposed: factory_disposed.clone(),
                        panic_drop: false,
                    }
                },
                "/target",
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    supervisor_strategy: Some(SupervisorStrategy::Restart {
                        max_retries: 1,
                        within: Duration::from_secs(10),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        actor.send(Box::new(())).await.unwrap();
        assert!(actor.send(Box::new(true)).await.is_err());
        tokio::time::timeout(LIMIT, async {
            loop {
                if count.load(Ordering::SeqCst) == 2 && actor.send(Box::new(())).await.is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        system.shutdown_internal().await.unwrap();
        assert!(disposed.load(Ordering::SeqCst));
    }
}

struct CleanupConcurrency {
    active: AtomicUsize,
    peak: AtomicUsize,
    released: Mutex<bool>,
    wake: std::sync::Condvar,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
}
struct CleanupActor(Arc<CleanupConcurrency>);
impl Actor for CleanupActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let active = self.0.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.0.peak.fetch_max(active, Ordering::SeqCst);
            let _ = self.0.entered.send(());
            let released = self.0.released.lock().unwrap();
            let _ = self
                .0
                .wake
                .wait_timeout_while(released, LIMIT, |released| !*released)
                .unwrap();
            self.0.active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
#[tokio::test(flavor = "current_thread")]
async fn mass_shutdown_bounds_simultaneous_synchronous_cleanup_polls() {
    let system = system(modes()[0].clone(), Duration::from_millis(100));
    let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
    let state = Arc::new(CleanupConcurrency {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        released: Mutex::new(false),
        wake: Default::default(),
        entered,
    });
    for i in 0..24 {
        let actor = system
            .spawn_at(
                CleanupActor(state.clone()),
                &format!("/cleanup-{i}"),
                None,
                Default::default(),
            )
            .await
            .unwrap();
        actor.send(Box::new(())).await.unwrap();
    }
    let stopping = system.clone();
    let shutdown = tokio::spawn(async move { stopping.shutdown_internal().await });
    for _ in 0..2 {
        tokio::time::timeout(LIMIT, entries.recv())
            .await
            .unwrap()
            .unwrap();
    }
    let result = tokio::time::timeout(LIMIT, shutdown)
        .await
        .unwrap()
        .unwrap();
    let peak = state.peak.load(Ordering::SeqCst);
    *state.released.lock().unwrap() = true;
    state.wake.notify_all();
    assert!(matches!(result, Err(SystemError::Timeout(_))));
    assert_eq!(
        peak, 2,
        "shared_pool_size bounds simultaneously blocking lifecycle polls"
    );
    tokio::time::timeout(LIMIT, async {
        while system.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
