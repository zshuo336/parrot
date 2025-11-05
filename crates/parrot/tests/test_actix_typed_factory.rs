//! Repeatable static roots must preserve concrete protocols and real ownership.
use parrot::actix::{ActixActorSystem, ActixSystemSettings};
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
async fn system(timeout: Duration) -> Arc<ActixActorSystem> {
    Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: timeout,
            ..Default::default()
        })
        .await
        .unwrap(),
    )
}
async fn absent(system: &ActixActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.get_actor(&path.to_owned()).await.is_some() {
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

#[actix::test]
async fn typed_factory_restart_preserves_reference_queue_and_counts_budget_once() {
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_resume_preserves_mutation_with_zero_restart_budget() {
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_initialization_failure_reconstructs_before_processing() {
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_initialization_resume_does_not_activate_failed_state() {
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_waits_for_old_instance_disposal_before_constructing() {
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_disposal_panic_prevents_reconstruction_and_preserves_error() {
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_stop_owns_late_constructor_output_and_prevents_initialization() {
    let s = system(Duration::from_millis(80)).await;
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
    assert!(s.get_actor(&"/late".to_owned()).await.is_some());
    assert_eq!(stats.init.load(Ordering::SeqCst), 0);
    gate.release();
    absent(&s, "/late").await;
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    assert_eq!(stats.init.load(Ordering::SeqCst), 0);
    assert!(!actor.is_alive());
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_duplicate_rejects_before_invocation_and_retires_captures() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_factory_recovery_deadline_seals_admission_during_blocked_disposal() {
    let s = system(Duration::from_millis(80)).await;
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
    assert!(s.get_actor(&"/expired".to_owned()).await.is_some());
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    gate.release();
    absent(&s, "/expired").await;
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_factory_constructor_capacity_is_shared_and_cancelled_waiter_never_invokes() {
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_capture_disposal_is_part_of_stop_even_after_successful_instance_drop() {
    struct Capture(Arc<Gate>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.block();
        }
    }
    let s = system(Duration::from_millis(80)).await;
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
    assert!(s.get_actor(&"/capture".to_owned()).await.is_some());
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    gate.release();
    absent(&s, "/capture").await;
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_constructor_panic_retires_capture_and_reports_diagnostic() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_restart_clears_recovery_timer_without_delayed_termination() {
    let s = system(Duration::from_millis(80)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_factory_late_restart_decision_cannot_revive_stopped_root() {
    let s = system(Duration::from_millis(80)).await;
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
    assert!(s.get_actor(&"/decision".to_owned()).await.is_some());
    assert!(!actor.is_alive());
    gate.release();
    absent(&s, "/decision").await;
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_factory_recovery_shares_deadline_between_disposal_and_reconstruction() {
    let s = system(Duration::from_millis(160)).await;
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
    assert!(s.get_actor(&"/total".to_owned()).await.is_some());
    builds.release();
    absent(&s, "/total").await;
    assert_eq!(stats.init.load(Ordering::SeqCst), 1);
    assert_eq!(stats.built.load(Ordering::SeqCst), 2);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 2);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_factory_rejection_isolates_factory_and_policy_capture_panics() {
    struct Bomb(Arc<AtomicUsize>);
    impl Drop for Bomb {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("rejected factory capture");
        }
    }
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
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
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn factory_instances_keep_arbiter_affinity_across_reconstruction() {
    type Events = Arc<Mutex<Vec<(usize, &'static str, std::thread::ThreadId)>>>;
    struct Affine {
        generation: usize,
        events: Events,
    }
    impl Affine {
        fn record(&self, phase: &'static str) {
            self.events
                .lock()
                .unwrap()
                .push((self.generation, phase, std::thread::current().id()));
        }
    }
    impl Drop for Affine {
        fn drop(&mut self) {
            self.record("drop");
        }
    }
    impl ParrotTypedDispatch for Affine {
        type Msg = bool;
        type Reply = usize;
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.record("init");
            Box::pin(async { Ok(()) })
        }
        fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.record("hook");
            Box::pin(async { Ok(()) })
        }
        fn dispatch(&mut self, fail: bool) -> BoxedFuture<'_, ActorResult<usize>> {
            self.record("dispatch");
            Box::pin(async move {
                if fail {
                    Err(ActorError::MessageHandlingError("affine failure".into()))
                } else {
                    Ok(self.generation)
                }
            })
        }
    }
    struct Get(bool);
    impl Message for Get {
        type Result = usize;
    }
    impl ParrotMsgVariant<Affine> for Get {
        fn inject(op: Self) -> bool {
            op.0
        }
        fn extract(reply: usize) -> ActorResult<usize> {
            Ok(reply)
        }
    }
    let s = system(Duration::from_secs(2)).await;
    let events: Events = Arc::default();
    let output = events.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let constructed = calls.clone();
    let actor = s
        .spawn_typed_factory::<_, Get, _>(
            move || {
                let generation = constructed.fetch_add(1, Ordering::SeqCst);
                let a = Affine {
                    generation,
                    events: output.clone(),
                };
                a.record("construct");
                a
            },
            "/affinity",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Get(false)).await.unwrap(), 0);
    assert!(actor.ask(Get(true)).await.is_err());
    assert_eq!(actor.ask(Get(false)).await.unwrap(), 1);
    actor.stop().await.unwrap();
    let snapshot = events.lock().unwrap().clone();
    let arbiter = snapshot
        .iter()
        .find(|(_, phase, _)| *phase == "init")
        .unwrap()
        .2;
    for (_, phase, thread) in &snapshot {
        if *phase == "construct" {
            assert_ne!(*thread, arbiter)
        } else {
            assert_eq!(*thread, arbiter, "{phase}")
        }
    }
    assert_eq!(
        snapshot
            .iter()
            .filter(|(_, phase, _)| *phase == "drop")
            .count(),
        2
    );
    assert_eq!(
        snapshot
            .iter()
            .filter(|(_, phase, _)| *phase == "hook")
            .map(|(generation, _, _)| *generation)
            .collect::<Vec<_>>(),
        [1]
    );
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_factory_stop_and_parentless_escalation_leave_other_roots_running() {
    let s = system(Duration::from_secs(2)).await;
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
    s.as_ref().clone().shutdown().await.unwrap();
}
