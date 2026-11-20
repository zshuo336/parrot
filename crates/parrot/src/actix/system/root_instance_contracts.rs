//! Real initialization and disposal distinguish creation slots from worker slots.
use super::*;
use parrot_api::{
    actor::ActorConfig,
    engine::{AsyncEngineRuntime, SpawnOptions},
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
};
use std::sync::Condvar;
use tokio::sync::{Notify, Semaphore};

#[derive(Default)]
struct Counts {
    initialized: AtomicUsize,
    actors: AtomicUsize,
    configs: AtomicUsize,
}
#[derive(Default)]
struct Gate {
    entered: Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| {
                !*open
            })
            .unwrap();
        assert!(*open, "test must release disposal gate");
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Config {
    counts: Arc<Counts>,
    gate: Option<Arc<Gate>>,
}
impl ActorConfig for Config {}
impl Drop for Config {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.counts.configs.fetch_add(1, Ordering::SeqCst);
    }
}
struct State {
    counts: Arc<Counts>,
    init: Option<Arc<Semaphore>>,
    fail_init: bool,
    retirement: Option<Arc<Gate>>,
    panic_drop: bool,
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(gate) = &self.retirement {
            gate.block();
        }
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "root instance destructor receipt");
    }
}
struct Probe(State);
struct ErasedProbe(State);
macro_rules! actor {
    ($actor:ty, $context:ty, $config:ty) => {
        impl ParrotActor for $actor {
            type Config = $config;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.counts.initialized.fetch_add(1, Ordering::SeqCst);
                    if let Some(gate) = &self.0.init {
                        gate.acquire().await.unwrap().forget();
                    }
                    if self.0.fail_init {
                        Err(ActorError::InitializationError(
                            "root instance init receipt".into(),
                        ))
                    } else {
                        Ok(())
                    }
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(message) })
            }
        }
    };
}
actor!(Probe, ActixContext<ActixActor<Probe>>, Config);
actor!(ErasedProbe, dyn ActorContext, Box<dyn Any + Send>);
impl ActixEngineExt for Probe {}

fn inputs(counts: &Arc<Counts>, init: Option<Arc<Semaphore>>) -> (Probe, Config) {
    (
        Probe(State {
            counts: counts.clone(),
            init,
            fail_init: false,
            retirement: None,
            panic_drop: false,
        }),
        Config {
            counts: counts.clone(),
            gate: None,
        },
    )
}
fn strategy() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(5),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
    })
}
async fn system(capacity: usize) -> ActixActorSystem {
    let mut system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 2,
        shutdown_timeout: Duration::from_secs(3),
        ..Default::default()
    })
    .await
    .unwrap();
    system.instance_admission = Arc::new(Semaphore::new(capacity));
    system
}
async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn ready(reference: &BoxedActorRef) {
    reference.send(Box::new(())).await.unwrap();
}

#[actix::test]
async fn root_entries_share_creation_capacity_through_clone_and_weak_upgrade() {
    for entry in 0..4 {
        let system = system(1).await;
        let clone = system.clone();
        let upgraded = system.downgrade().upgrade().unwrap();
        let counts = Arc::new(Counts::default());
        let init = Arc::new(Semaphore::new(0));
        let (actor, config) = inputs(&counts, Some(init.clone()));
        let first = system.spawn_root_typed(actor, config).await.unwrap();
        until(|| counts.initialized.load(Ordering::SeqCst) == 1).await;
        let second_counts = Arc::new(Counts::default());
        let (actor, config) = inputs(&second_counts, None);
        let cohort = system.root_cohort(strategy()).await.unwrap();
        let mut pending: BoxedFuture<'_, ActorResult<BoxedActorRef>> = match entry {
            0 => Box::pin(async {
                clone
                    .spawn_root_typed(actor, config)
                    .await
                    .map_err(|e| ActorError::InternalError(e.to_string()))
            }),
            1 => Box::pin(async {
                upgraded
                    .spawn_root_typed_with_strategy(actor, config, strategy())
                    .await
                    .map_err(|e| ActorError::InternalError(e.to_string()))
            }),
            2 => Box::pin(system.spawn_root_in_cohort(actor, config, "/creation-cohort", &cohort)),
            _ => Box::pin(async {
                let Probe(state) = actor;
                AsyncEngineRuntime::spawn(
                    &clone,
                    Box::new(ErasedProbe(state)),
                    Box::new(config),
                    SpawnOptions::at("/creation-erased"),
                )
                .await
                .map_err(|e| ActorError::InternalError(e.to_string()))
            }),
        };
        assert!(
            futures::poll!(&mut pending).is_pending(),
            "entry {entry} bypassed shared quota"
        );
        assert_eq!(system.registry.read().unwrap().actors.len(), 1);
        assert_eq!(second_counts.initialized.load(Ordering::SeqCst), 0);
        init.add_permits(1);
        let second = pending.await.unwrap();
        ready(&first).await;
        ready(&second).await;
        assert_eq!(
            system.instance_admission.available_permits(),
            1,
            "active actors retained creation quota"
        );
        first.stop().await.unwrap();
        second.stop().await.unwrap();
        drop(cohort);
        clone.shutdown().await.unwrap();
        assert_eq!(second_counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(second_counts.configs.load(Ordering::SeqCst), 1);
    }
}

#[actix::test]
async fn failed_init_retains_slot_through_real_actor_disposal_and_panic_receipt() {
    for panic_drop in [false, true] {
        let system = system(1).await;
        let counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let (mut actor, config) = inputs(&counts, None);
        actor.0.fail_init = true;
        actor.0.retirement = Some(gate.clone());
        actor.0.panic_drop = panic_drop;
        let first = system.spawn_root_typed(actor, config).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        let (actor, config) = inputs(&counts, None);
        let pending = system.spawn_root_typed(actor, config);
        tokio::pin!(pending);
        assert!(
            futures::poll!(&mut pending).is_pending(),
            "failed init released quota before destructor returned"
        );
        assert_eq!(system.instance_admission.available_permits(), 0);
        drop(release);
        let second = pending.await.unwrap();
        ready(&second).await;
        let failure = first.stop().await.unwrap_err().to_string();
        assert!(
            failure.contains(if panic_drop {
                "root instance destructor receipt"
            } else {
                "root instance init receipt"
            }),
            "{failure}"
        );
        second.stop().await.unwrap();
        system.clone().shutdown().await.unwrap();
        assert_eq!(counts.actors.load(Ordering::SeqCst), 2);
    }
}

#[actix::test]
async fn shutdown_wakes_root_admission_without_late_initialization() {
    let system = system(1).await;
    let counts = Arc::new(Counts::default());
    let (actor, config) = inputs(&counts, Some(Arc::new(Semaphore::new(0))));
    let _first = system.spawn_root_typed(actor, config).await.unwrap();
    until(|| counts.initialized.load(Ordering::SeqCst) == 1).await;
    let (actor, config) = inputs(&counts, None);
    let pending = system.spawn_root_typed(actor, config);
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    let shutdown = system.clone().shutdown();
    tokio::pin!(shutdown);
    let _ = futures::poll!(&mut shutdown);
    assert!(matches!(pending.await, Err(SystemError::ShuttingDown)));
    shutdown.await.unwrap();
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 2);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 2);
}

#[actix::test]
async fn cancelled_waiting_root_never_initializes_after_capacity_returns() {
    let system = system(1).await;
    let first_counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let (actor, config) = inputs(&first_counts, Some(init.clone()));
    let first = system.spawn_root_typed(actor, config).await.unwrap();
    let counts = Arc::new(Counts::default());
    let (actor, config) = inputs(&counts, None);
    let mut pending = Box::pin(system.spawn_root_typed(actor, config));
    assert!(futures::poll!(&mut pending).is_pending());
    drop(pending);
    until(|| {
        counts.actors.load(Ordering::SeqCst) == 1 && counts.configs.load(Ordering::SeqCst) == 1
    })
    .await;
    init.add_permits(1);
    ready(&first).await;
    assert_eq!(system.registry.read().unwrap().actors.len(), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    first.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
}

#[actix::test]
async fn queueing_and_configuration_disposal_spend_one_creation_deadline() {
    let mut system = system(1).await;
    system.operation_timeouts.actor_creation = Duration::from_millis(300);
    let counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let (actor, config) = inputs(&counts, Some(init.clone()));
    let first = system.spawn_root_typed(actor, config).await.unwrap();
    until(|| counts.initialized.load(Ordering::SeqCst) == 1).await;
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let second_counts = Arc::new(Counts::default());
    let (actor, mut config) = inputs(&second_counts, None);
    config.gate = Some(gate.clone());
    let pending = system.spawn_root_typed(actor, config);
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    tokio::time::sleep(Duration::from_millis(180)).await;
    init.add_permits(1);
    let result = tokio::time::timeout(Duration::from_millis(180), &mut pending)
        .await
        .expect("admission reset the original creation deadline");
    assert!(result.is_err());
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(second_counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(
        system.instance_admission.available_permits(),
        0,
        "timed-out config disposal released quota early"
    );
    drop(release);
    until(|| system.instance_admission.available_permits() == 1).await;
    first.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
    assert_eq!(second_counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(second_counts.configs.load(Ordering::SeqCst), 1);
}

struct PolicyDrop {
    gate: Arc<Gate>,
    disposed: Arc<AtomicUsize>,
    panic: bool,
}
impl Drop for PolicyDrop {
    fn drop(&mut self) {
        self.gate.block();
        self.disposed.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "ignored policy disposal receipt");
    }
}
fn blocked_strategy(
    gate: &Arc<Gate>,
    disposed: &Arc<AtomicUsize>,
    panic: bool,
) -> SupervisorStrategyType {
    let capture = PolicyDrop {
        gate: gate.clone(),
        disposed: disposed.clone(),
        panic,
    };
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(1),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    })
}

#[actix::test]
async fn ignored_cohort_policy_retains_creation_capacity_after_timeout_or_cancellation() {
    for cancel in [false, true] {
        let mut system = system(1).await;
        system.operation_timeouts.actor_creation = Duration::from_millis(150);
        let cohort = system.root_cohort(strategy()).await.unwrap();
        let counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let disposed = Arc::new(AtomicUsize::new(0));
        let (actor, config) = inputs(&counts, None);
        let mut creation = Box::pin(system.spawn_root_instance(
            actor,
            config,
            Some(blocked_strategy(&gate, &disposed, false)),
            None,
            Some(&cohort),
        ));
        assert!(futures::poll!(&mut creation).is_pending());
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        if !cancel {
            let result = tokio::time::timeout(Duration::from_millis(800), &mut creation)
                .await
                .expect("ignored policy extended creation deadline");
            assert!(matches!(
                result,
                Err(SystemError::ActorError(ActorError::TimeoutDetail(_)))
            ));
        }
        drop(creation);
        until(|| {
            counts.actors.load(Ordering::SeqCst) == 1 && counts.configs.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(disposed.load(Ordering::SeqCst), 0);
        assert_eq!(system.instance_admission.available_permits(), 0);
        let next_counts = Arc::new(Counts::default());
        let (actor, config) = inputs(&next_counts, None);
        let mut next = Box::pin(system.spawn_root_typed(actor, config));
        assert!(futures::poll!(&mut next).is_pending());
        assert!(system.registry.read().unwrap().actors.is_empty());
        drop(release);
        let next = next.await.unwrap();
        ready(&next).await;
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(system.instance_admission.available_permits(), 1);
        next.stop().await.unwrap();
        drop(cohort);
        system.clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn ignored_cohort_policy_disposal_failure_preserves_original_receipt() {
    let system = system(1).await;
    let cohort = system.root_cohort(strategy()).await.unwrap();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let disposed = Arc::new(AtomicUsize::new(0));
    let (actor, config) = inputs(&counts, None);
    let mut creation = Box::pin(system.spawn_root_instance(
        actor,
        config,
        Some(blocked_strategy(&gate, &disposed, true)),
        None,
        Some(&cohort),
    ));
    assert!(futures::poll!(&mut creation).is_pending());
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    drop(release);
    let error = creation.await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ignored policy disposal receipt")
    );
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    assert!(system.registry.read().unwrap().actors.is_empty());
    drop(cohort);
    system.clone().shutdown().await.unwrap();
}

#[actix::test]
async fn timed_out_admission_cannot_wait_for_unadmitted_input_destructor() {
    let mut system = system(1).await;
    let counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let (actor, config) = inputs(&counts, Some(init.clone()));
    let first = system.spawn_root_typed(actor, config).await.unwrap();
    until(|| counts.initialized.load(Ordering::SeqCst) == 1).await;
    system.operation_timeouts.actor_creation = Duration::from_millis(150);
    let rejected_counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let (actor, mut config) = inputs(&rejected_counts, None);
    config.gate = Some(gate.clone());
    let creation = system.spawn_root_typed(actor, config);
    let result = tokio::time::timeout(Duration::from_millis(800), creation)
        .await
        .expect("admission timeout waited for rejected input destruction");
    assert!(matches!(
        result,
        Err(SystemError::ActorError(ActorError::TimeoutDetail(_)))
    ));
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(rejected_counts.configs.load(Ordering::SeqCst), 0);
    assert_eq!(rejected_counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(system.registry.read().unwrap().actors.len(), 1);
    init.add_permits(1);
    ready(&first).await;
    assert_eq!(system.instance_admission.available_permits(), 1);
    drop(release);
    until(|| {
        rejected_counts.actors.load(Ordering::SeqCst) == 1
            && rejected_counts.configs.load(Ordering::SeqCst) == 1
    })
    .await;
    first.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
}
