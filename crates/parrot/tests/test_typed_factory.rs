//! Repeatable static roots must preserve concrete protocols and real ownership.
use parrot::thread::{ThreadActorSystem, ThreadActorSystemConfig};
use parrot_api::{
    errors::ActorError,
    message::Message,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Stats {
    built: AtomicUsize,
    init: AtomicUsize,
    drops: AtomicUsize,
    hooks: AtomicUsize,
    handled: Mutex<Vec<(usize, usize)>>,
}
struct Probe {
    generation: usize,
    value: usize,
    stats: Arc<Stats>,
    gate: Option<Arc<Gate>>,
    bad_drop: bool,
    bad_init: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.stats.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.bad_drop, "factory instance destructor failed");
    }
}
enum Op {
    Read,
    Add(usize),
    Error,
    Panic,
}
impl Message for Op {
    type Result = (usize, usize);
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = (usize, usize);
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.stats.init.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.bad_init {
                Err(ActorError::InitializationError("bad init".into()))
            } else {
                Ok(())
            }
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.stats.hooks.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<Self::Reply>> {
        Box::pin(async move {
            match op {
                Op::Read => {}
                Op::Add(value) => {
                    self.value += value;
                    self.stats
                        .handled
                        .lock()
                        .unwrap()
                        .push((self.generation, value));
                }
                Op::Error => {
                    self.value += 1;
                    return Err(ActorError::MessageHandlingError(
                        "failed typed message".into(),
                    ));
                }
                Op::Panic => {
                    self.value += 1;
                    panic!("failed typed handler");
                }
            }
            Ok((self.generation, self.value))
        })
    }
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(op: Self) -> Op {
        op
    }
    fn extract(reply: (usize, usize)) -> ActorResult<Self::Result> {
        Ok(reply)
    }
}
fn probe(stats: &Arc<Stats>) -> Probe {
    let generation = stats.built.fetch_add(1, Ordering::SeqCst);
    Probe {
        generation,
        value: 0,
        stats: stats.clone(),
        gate: None,
        bad_drop: false,
        bad_init: false,
    }
}
fn policy(n: u32, decision: SupervisionDecision, calls: Arc<AtomicUsize>) -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: n,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            decision
        }),
    }
}
fn system(timeout: Duration) -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shutdown_timeout: timeout,
        ..Default::default()
    })
}
async fn absent(system: &ThreadActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.get_actor_ref(path).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("registration must retire");
}
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (_guard, expired) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(5), |open| {
                !*open
            })
            .unwrap();
        assert!(!expired.timed_out(), "owned test gate not released");
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.entered.notified())
            .await
            .unwrap();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test]
async fn typed_factory_restart_preserves_reference_queue_and_counts_budget_once() {
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let actor = s
        .spawn_typed_factory_with_strategy::<_, Op, _>(
            move || probe(&made),
            "/factory",
            policy(2, SupervisionDecision::Restart, calls.clone()),
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (0, 0));
    let bridge = actor.clone().into_dyn();
    assert!(
        matches!(actor.ask(Op::Error).await,Err(ActorError::MessageHandlingError(text)) if text=="failed typed message")
    );
    actor.tell(Op::Add(2)).await.unwrap();
    actor.tell(Op::Add(3)).await.unwrap();
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (1, 5));
    assert!(matches!(
        actor.ask(Op::Panic).await,
        Err(ActorError::Panic(_))
    ));
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (2, 0));
    let bridged = bridge.send(Box::new(Op::Read)).await.unwrap();
    assert_eq!(*bridged.downcast::<(usize, usize)>().unwrap(), (2, 0));
    actor.tell(Op::Error).await.unwrap();
    absent(&s, "/factory").await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(stats.built.load(Ordering::SeqCst), 3);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
    assert_eq!(*stats.handled.lock().unwrap(), [(1, 2), (1, 3)]);
    assert!(!actor.is_alive());
    assert!(actor.stop().await.is_err());
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_resume_preserves_mutation_with_zero_restart_budget() {
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let actor = s
        .spawn_typed_factory_with_strategy::<_, Op, _>(
            move || probe(&made),
            "/resume",
            policy(0, SupervisionDecision::Resume, calls.clone()),
        )
        .await
        .unwrap();
    for expected in 1..=3 {
        assert!(actor.ask(Op::Error).await.is_err());
        assert_eq!(actor.ask(Op::Read).await.unwrap(), (0, expected));
    }
    actor.stop().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_initialization_failure_reconstructs_before_processing() {
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let actor = s
        .spawn_typed_factory_with_strategy::<_, Op, _>(
            move || {
                let mut p = probe(&made);
                p.bad_init = p.generation == 0;
                p
            },
            "/init",
            policy(1, SupervisionDecision::Restart, calls.clone()),
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Op::Add(9)).await.unwrap(), (1, 9));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    actor.stop().await.unwrap();
    assert_eq!(stats.init.load(Ordering::SeqCst), 2);
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_initialization_resume_does_not_activate_failed_state() {
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let actor = s
        .spawn_typed_factory_with_strategy::<_, Op, _>(
            move || {
                let mut p = probe(&made);
                p.bad_init = true;
                p
            },
            "/init-resume",
            policy(3, SupervisionDecision::Resume, calls.clone()),
        )
        .await
        .unwrap();
    assert!(actor.ask(Op::Add(9)).await.is_err());
    absent(&s, "/init-resume").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert!(stats.handled.lock().unwrap().is_empty());
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_waits_for_old_instance_disposal_before_constructing() {
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                let mut p = probe(&made);
                if p.generation == 0 {
                    p.gate = Some(blocked.clone());
                }
                p
            },
            "/disposal",
        )
        .await
        .unwrap();
    assert!(actor.ask(Op::Error).await.is_err());
    gate.entered().await;
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 0);
    actor.tell(Op::Add(4)).await.unwrap();
    gate.release();
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (1, 4));
    actor.stop().await.unwrap();
    assert_eq!(stats.drops.load(Ordering::SeqCst), 2);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_disposal_panic_prevents_reconstruction_and_preserves_error() {
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                let mut p = probe(&made);
                p.bad_drop = true;
                p
            },
            "/drop-error",
        )
        .await
        .unwrap();
    assert!(actor.ask(Op::Error).await.is_err());
    absent(&s, "/drop-error").await;
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert!(
        actor
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("factory instance destructor failed")
    );
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_stop_owns_late_constructor_output_and_prevents_initialization() {
    let s = system(Duration::from_millis(80));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                blocked.block();
                probe(&made)
            },
            "/late",
        )
        .await
        .unwrap();
    gate.entered().await;
    assert!(actor.stop().await.is_err());
    assert!(s.get_actor_ref("/late").is_some());
    assert_eq!(stats.init.load(Ordering::SeqCst), 0);
    gate.release();
    absent(&s, "/late").await;
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    assert_eq!(stats.init.load(Ordering::SeqCst), 0);
    assert!(!actor.is_alive());
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_duplicate_rejects_before_invocation_and_retires_captures() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(move || probe(&made), "/duplicate")
        .await
        .unwrap();
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (0, 0));
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(drops.clone());
    let invoked = Arc::new(AtomicUsize::new(0));
    let calls = invoked.clone();
    let made = stats.clone();
    let result = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                let _ = &capture;
                calls.fetch_add(1, Ordering::SeqCst);
                probe(&made)
            },
            "/duplicate",
        )
        .await;
    assert!(result.is_err());
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (0, 0));
    actor.stop().await.unwrap();
    s.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_factory_recovery_deadline_seals_admission_during_blocked_disposal() {
    let s = system(Duration::from_millis(80));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                let mut p = probe(&made);
                p.gate = Some(blocked.clone());
                p
            },
            "/expired",
        )
        .await
        .unwrap();
    assert!(actor.ask(Op::Error).await.is_err());
    gate.entered().await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        !actor.is_alive(),
        "the recovery deadline must seal admission without waiting for Drop"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), actor.stop())
            .await
            .unwrap()
            .is_err()
    );
    assert!(s.get_actor_ref("/expired").is_some());
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    gate.release();
    absent(&s, "/expired").await;
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    s.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_factory_constructor_capacity_is_shared_and_cancelled_waiter_never_invokes() {
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let first = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                blocked.block();
                probe(&made)
            },
            "/first",
        )
        .await
        .unwrap();
    gate.entered().await;
    let second_calls = Arc::new(AtomicUsize::new(0));
    let calls = second_calls.clone();
    let made = stats.clone();
    let second = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                probe(&made)
            },
            "/second",
        )
        .await
        .unwrap();
    // Drive an actual ASK while the first constructor owns the sole permit.
    // An unbounded implementation would construct and reply instead of waiting.
    let pending = second.ask(Op::Read);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut pending)
            .await
            .is_err()
    );
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    // With one constructor worker, cancellation must retire the waiting factory
    // without releasing the unrelated in-flight first constructor.
    tokio::time::timeout(Duration::from_millis(300), second.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    absent(&s, "/second").await;
    gate.release();
    assert_eq!(first.ask(Op::Read).await.unwrap(), (0, 0));
    first.stop().await.unwrap();
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_capture_disposal_is_part_of_stop_even_after_successful_instance_drop() {
    struct Capture(Arc<Gate>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.block();
        }
    }
    let s = system(Duration::from_millis(80));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let capture = Capture(gate.clone());
    let actor = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                let _ = &capture;
                probe(&made)
            },
            "/capture",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (0, 0));
    let stopped = actor.stop();
    tokio::pin!(stopped);
    tokio::select! { _ = gate.entered() => {}, result = &mut stopped => panic!("stop ended before capture disposal {result:?}") }
    assert!(stopped.await.is_err());
    assert!(s.get_actor_ref("/capture").is_some());
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    gate.release();
    absent(&s, "/capture").await;
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_constructor_panic_retires_capture_and_reports_diagnostic() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let s = system(Duration::from_secs(2));
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(drops.clone());
    let actor = s
        .spawn_typed_factory::<Probe, Op, _>(
            move || {
                let _ = &capture;
                panic!("typed constructor failed")
            },
            "/ctor-panic",
        )
        .await
        .unwrap();
    absent(&s, "/ctor-panic").await;
    assert!(
        actor
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("typed constructor failed")
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_restart_clears_recovery_timer_without_delayed_termination() {
    let s = system(Duration::from_millis(80));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(move || probe(&made), "/stable")
        .await
        .unwrap();
    assert!(actor.ask(Op::Error).await.is_err());
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (1, 0));
    tokio::time::sleep(Duration::from_millis(160)).await;
    assert_eq!(actor.ask(Op::Add(3)).await.unwrap(), (1, 3));
    actor.stop().await.unwrap();
    s.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_factory_late_restart_decision_cannot_revive_stopped_root() {
    let s = system(Duration::from_millis(80));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let actor = s
        .spawn_typed_factory_with_strategy::<_, Op, _>(
            move || probe(&made),
            "/decision",
            OneForOneStrategy {
                max_restarts: 3,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(move |_| {
                    blocked.block();
                    SupervisionDecision::Restart
                }),
            },
        )
        .await
        .unwrap();
    actor.tell(Op::Error).await.unwrap();
    gate.entered().await;
    assert!(actor.stop().await.is_err());
    assert!(s.get_actor_ref("/decision").is_some());
    assert!(!actor.is_alive());
    gate.release();
    absent(&s, "/decision").await;
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_recovery_shares_deadline_between_disposal_and_reconstruction() {
    let s = system(Duration::from_millis(160));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let drops = Arc::new(Gate::default());
    let _release_drop = Release(drops.clone());
    let blocked_drop = drops.clone();
    let builds = Arc::new(Gate::default());
    let _release_build = Release(builds.clone());
    let blocked_build = builds.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                let mut p = probe(&made);
                if p.generation == 0 {
                    p.gate = Some(blocked_drop.clone());
                } else {
                    blocked_build.block();
                }
                p
            },
            "/total",
        )
        .await
        .unwrap();
    assert!(actor.ask(Op::Error).await.is_err());
    drops.entered().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    drops.release();
    builds.entered().await;
    tokio::time::timeout(Duration::from_millis(100), async {
        while actor.is_alive() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconstruction must inherit original failure deadline");
    assert!(s.get_actor_ref("/total").is_some());
    builds.release();
    absent(&s, "/total").await;
    assert_eq!(stats.init.load(Ordering::SeqCst), 1);
    assert_eq!(stats.built.load(Ordering::SeqCst), 2);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 2);
    s.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_factory_rejection_isolates_factory_and_policy_capture_panics() {
    struct Bomb(Arc<AtomicUsize>);
    impl Drop for Bomb {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("rejected factory capture");
        }
    }
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(move || probe(&made), "/collision")
        .await
        .unwrap();
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (0, 0));
    let drops = Arc::new(AtomicUsize::new(0));
    let factory = Bomb(drops.clone());
    let decider = Bomb(drops.clone());
    let made = stats.clone();
    let result = s
        .spawn_typed_factory_with_strategy::<_, Op, _>(
            move || {
                let _ = &factory;
                probe(&made)
            },
            "/collision",
            OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(2),
                decider: BasicDecisionFn::new(move |_| {
                    let _ = &decider;
                    SupervisionDecision::Restart
                }),
            },
        )
        .await;
    assert!(result.is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(actor.ask(Op::Read).await.unwrap(), (0, 0));
    actor.stop().await.unwrap();
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_factory_death_watch_survives_restarts_and_emits_one_final_event() {
    use parrot_api::{context::ActorTerminated, typed::TypedReceive};
    #[derive(parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(ActorTerminated))]
    struct Observer(tokio::sync::mpsc::UnboundedSender<ActorTerminated>);
    impl TypedReceive<ActorTerminated> for Observer {
        fn receive_typed(&mut self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                self.0.send(event).unwrap();
                Ok(())
            })
        }
    }
    let s = system(Duration::from_secs(2));
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let actor = s
        .spawn_typed_factory::<_, Op, _>(move || probe(&made), "/watched")
        .await
        .unwrap();
    let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
    let watcher = s
        .spawn_typed::<_, ActorTerminated>(Observer(send), "/observer")
        .await
        .unwrap();
    let original = actor.clone().into_dyn();
    let identity = original.death_watch().unwrap().id();
    original.watch(watcher.clone().into_dyn()).await.unwrap();
    for generation in 1..=2 {
        assert!(actor.ask(Op::Error).await.is_err());
        assert_eq!(actor.ask(Op::Read).await.unwrap(), (generation, 0));
        assert_eq!(
            actor.clone().into_dyn().death_watch().unwrap().id(),
            identity
        );
        assert!(events.try_recv().is_err());
    }
    actor.stop().await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.generation, identity);
    assert_eq!(event.path, "/watched");
    assert_eq!(event.reason, parrot_api::supervisor::DeathReason::Normal);
    assert!(events.try_recv().is_err());
    watcher.stop().await.unwrap();
    s.shutdown_internal().await.unwrap();
}

#[test]
fn typed_factory_host_cancellation_isolates_late_output_and_capture_panics() {
    const NAME: &str = "typed_factory_host_cancellation_isolates_late_output_and_capture_panics";
    if std::env::var("PARROT_TYPED_FACTORY_CANCEL_CHILD").as_deref() == Ok(NAME) {
        struct WorkerExit(Arc<AtomicUsize>);
        impl Drop for WorkerExit {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        thread_local! {
            static WORKER_EXIT: std::cell::RefCell<Option<WorkerExit>> = const { std::cell::RefCell::new(None) };
        }
        struct Capture(Arc<AtomicUsize>);
        impl Drop for Capture {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("host cancelled factory capture");
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stats = Arc::new(Stats::default());
        let made = stats.clone();
        let captures = Arc::new(AtomicUsize::new(0));
        let capture = Capture(captures.clone());
        let exited = Arc::new(AtomicUsize::new(0));
        let worker_exit = exited.clone();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let (system, actor) = runtime.block_on(async {
            let s = system(Duration::from_secs(2));
            let actor = s
                .spawn_typed_factory::<_, Op, _>(
                    move || {
                        let _ = &capture;
                        WORKER_EXIT.with(|slot| {
                            *slot.borrow_mut() = Some(WorkerExit(worker_exit.clone()))
                        });
                        blocked.block();
                        let mut p = probe(&made);
                        p.bad_drop = true;
                        p
                    },
                    "/host",
                )
                .await
                .unwrap();
            gate.entered().await;
            (s, actor)
        });
        runtime.shutdown_timeout(Duration::from_millis(100));
        println!("FACTORY_HOST_STOPPED");
        drop(system);
        gate.release();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        // Counters inside panicking Drop bodies are not completion receipts.
        // Wait for the actual constructor worker to exit after late disposal;
        // otherwise the child may return just before a detached worker aborts.
        while exited.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            exited.load(Ordering::SeqCst),
            1,
            "constructor worker must actually exit"
        );
        assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(stats.init.load(Ordering::SeqCst), 0);
        assert!(!actor.is_alive());
        println!("FACTORY_HOST_RETIRED");
        return;
    }
    use std::process::{Command, Stdio};
    struct Logs(std::path::PathBuf);
    impl Drop for Logs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let logs =
        Logs(std::env::temp_dir().join(format!("parrot-factory-host-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&logs.0).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture"])
        .env("PARROT_TYPED_FACTORY_CANCEL_CHILD", NAME)
        .stdout(Stdio::from(
            std::fs::File::create(logs.0.join("stdout")).unwrap(),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(logs.0.join("stderr")).unwrap(),
        ))
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let read = |name| {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(logs.0.join(name))
            .unwrap()
            .take(256 * 1024)
            .read_to_end(&mut bytes)
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let stdout = read("stdout");
    let stderr = read("stderr");
    assert!(!timed_out, "owned child timed out: {stdout}\n{stderr}");
    assert!(
        stdout.contains("FACTORY_HOST_STOPPED"),
        "{stdout}\n{stderr}"
    );
    assert!(
        status.success(),
        "owned child failed {status}: {stdout}\n{stderr}"
    );
    assert!(stdout.contains("FACTORY_HOST_RETIRED"));
}

#[tokio::test]
async fn typed_factory_stop_and_parentless_escalation_leave_other_roots_running() {
    let s = system(Duration::from_secs(2));
    let healthy_stats = Arc::new(Stats::default());
    let made = healthy_stats.clone();
    let healthy = s
        .spawn_typed_factory::<_, Op, _>(move || probe(&made), "/healthy-root")
        .await
        .unwrap();
    assert_eq!(healthy.ask(Op::Add(7)).await.unwrap(), (0, 7));
    for decision in [SupervisionDecision::Stop, SupervisionDecision::Escalate] {
        let stats = Arc::new(Stats::default());
        let made = stats.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let actor = s
            .spawn_typed_factory_with_strategy::<_, Op, _>(
                move || probe(&made),
                "/terminal-root",
                policy(3, decision, calls.clone()),
            )
            .await
            .unwrap();
        assert!(actor.ask(Op::Error).await.is_err());
        absent(&s, "/terminal-root").await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(stats.built.load(Ordering::SeqCst), 1);
        assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
        assert_eq!(healthy.ask(Op::Read).await.unwrap(), (0, 7));
    }
    healthy.stop().await.unwrap();
    s.shutdown_internal().await.unwrap();
}
