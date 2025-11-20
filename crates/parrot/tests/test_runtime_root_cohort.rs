//! Engine-neutral groups must exercise native supervision and ownership.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    system::ParrotActorSystem,
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, AsyncRootCohort, LocalActor, RuntimeFactory, SpawnOptions},
    errors::ActorError,
    local_typed::LocalBoxedFuture,
    supervisor::{
        BasicDecisionFn, DefaultStrategy, OneForAllStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    system::{ActorSystemConfig, SystemError, SystemStatus},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}
impl Gate {
    async fn wait(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

#[derive(Default)]
struct ConstructorGate {
    entered: Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl ConstructorGate {
    fn wait(&self) {
        self.entered.notify_one();
        let (guard, result) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(
            *guard && !result.timed_out(),
            "constructor was never released"
        );
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

#[derive(Default)]
struct Plan {
    builds: AtomicUsize,
    inits: AtomicUsize,
    drops: AtomicUsize,
    wrong_owner: AtomicUsize,
    first_retirement: Option<Arc<Gate>>,
    first_drop: Option<Arc<ConstructorGate>>,
    constructor: Option<Arc<ConstructorGate>>,
}
impl Plan {
    fn make(self: &Arc<Self>) -> Probe {
        if let Some(gate) = &self.constructor {
            gate.wait();
        }
        Probe {
            generation: self.builds.fetch_add(1, Ordering::SeqCst),
            failures: 0,
            plan: self.clone(),
        }
    }
    fn factory(self: &Arc<Self>) -> RuntimeFactory {
        let plan = self.clone();
        Box::new(move || Box::new(plan.make()))
    }
}
struct Probe {
    plan: Arc<Plan>,
    generation: usize,
    failures: usize,
}
impl Drop for Probe {
    fn drop(&mut self) {
        if self.generation == 0
            && let Some(gate) = &self.plan.first_drop
        {
            gate.wait();
        }
        self.plan.drops.fetch_add(1, Ordering::SeqCst);
    }
}
enum Command {
    Read,
    Fail,
    Failures,
}
impl Actor for Probe {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, context: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(
            context.parent().is_none(),
            "group introduced a synthetic parent"
        );
        self.plan.inits.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.generation == 0
                && let Some(gate) = &self.plan.first_retirement
            {
                gate.wait().await;
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = match *message.downcast::<Command>().unwrap() {
                Command::Read => self.generation,
                Command::Failures => self.failures,
                Command::Fail => {
                    self.failures += 1;
                    return Err(ActorError::MessageHandlingError("cohort witness".into()));
                }
            };
            Ok(Box::new(value) as BoxedMessage)
        })
    }
}

async fn runtime(actix: bool) -> Arc<dyn AsyncEngineRuntime> {
    if actix {
        Arc::new(
            ActixActorSystem::with_settings(ActixSystemSettings {
                arbiter_count: 2,
                shutdown_timeout: WAIT,
                ..Default::default()
            })
            .await
            .unwrap(),
        )
    } else {
        ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 2,
            shared_burst_workers_max: 0,
            shutdown_timeout: WAIT,
            ..Default::default()
        })
    }
}
fn all() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForAll(OneForAllStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    })
}
fn resume() -> SupervisorStrategyType {
    SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure)
}
async fn group(
    runtime: &dyn AsyncEngineRuntime,
    strategy: SupervisorStrategyType,
) -> Arc<dyn AsyncRootCohort> {
    runtime
        .cohort_runtime()
        .expect("missing root cohort capability")
        .root_cohort(strategy)
        .await
        .unwrap()
}
async fn ask(reference: &BoxedActorRef, command: Command) -> ActorResult<usize> {
    reference
        .send(Box::new(command))
        .await
        .map(|v| *v.downcast::<usize>().unwrap())
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observable condition never became true");
}
async fn notified(notify: &Notify) {
    tokio::time::timeout(WAIT, notify.notified()).await.unwrap();
}
async fn recovered(reference: &BoxedActorRef) {
    tokio::time::timeout(WAIT, async {
        loop {
            match ask(reference, Command::Read).await {
                Ok(value) => {
                    assert_eq!(value, 1);
                    break;
                }
                Err(ActorError::Stopped) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected recovery error: {error}"),
            }
        }
    })
    .await
    .expect("replacement never became available");
}
async fn shutdown(runtime: &dyn AsyncEngineRuntime) {
    if let Err(error) = runtime.shutdown().await {
        assert!(error.to_string().contains("cohort witness"), "{error}");
    }
}
macro_rules! both {
    ($name:ident, $body:ident) => {
        mod $name {
            use super::*;
            #[actix::test]
            async fn thread() {
                $body(false).await;
            }
            #[actix::test]
            async fn actix() {
                $body(true).await;
            }
        }
    };
}

async fn shared_barriers(actix: bool) {
    let runtime = runtime(actix).await;
    let cohort = group(runtime.as_ref(), all()).await;
    let other = group(runtime.as_ref(), all()).await;
    let gate = Arc::new(Gate::default());
    let plans = [
        Arc::new(Plan::default()),
        Arc::new(Plan {
            first_retirement: Some(gate.clone()),
            ..Default::default()
        }),
    ];
    let outsider = Arc::new(Plan::default());
    let a = cohort
        .spawn_factory(plans[0].factory(), SpawnOptions::at("/a"))
        .await
        .unwrap();
    let b = cohort
        .spawn_factory(plans[1].factory(), SpawnOptions::at("/b"))
        .await
        .unwrap();
    let c = other
        .spawn_factory(outsider.factory(), SpawnOptions::at("/c"))
        .await
        .unwrap();
    assert_eq!(ask(&a, Command::Read).await.unwrap(), 0);
    assert_eq!(ask(&b, Command::Read).await.unwrap(), 0);
    assert_eq!(ask(&c, Command::Read).await.unwrap(), 0);
    assert_eq!(cohort.members().len(), 2);
    assert_eq!(other.members().len(), 1);
    let identity = a.death_watch().unwrap();
    assert!(matches!(ask(&a, Command::Fail).await,
        Err(ActorError::MessageHandlingError(message)) if message == "cohort witness"));
    notified(&gate.entered).await;
    assert_eq!(
        plans[0].builds.load(Ordering::SeqCst),
        1,
        "recreated before sibling retirement"
    );
    assert_eq!(plans[1].builds.load(Ordering::SeqCst), 1);
    assert_eq!(ask(&c, Command::Read).await.unwrap(), 0);
    gate.release.notify_one();
    until(|| plans.iter().all(|p| p.builds.load(Ordering::SeqCst) == 2)).await;
    recovered(&a).await;
    recovered(&b).await;
    let looked_up = runtime.get_actor("/a").await.unwrap();
    assert_eq!(looked_up.path(), "/a");
    assert_eq!(ask(&looked_up, Command::Read).await.unwrap(), 1);
    assert_eq!(looked_up.death_watch().unwrap().id(), identity.id());
    assert!(ask(&b, Command::Fail).await.is_err());
    until(|| plans.iter().all(|p| p.drops.load(Ordering::SeqCst) == 2)).await;
    assert!(
        ask(&a, Command::Read).await.is_err(),
        "shared restart budget was ignored"
    );
    assert_eq!(ask(&c, Command::Read).await.unwrap(), 0);
    assert_eq!(outsider.builds.load(Ordering::SeqCst), 1);
    let _ = cohort.stop().await;
    other.stop().await.unwrap();
    shutdown(runtime.as_ref()).await;
}
both!(shared_budget_barriers_and_group_isolation, shared_barriers);

async fn instances(actix: bool) {
    let runtime = runtime(actix).await;
    let cohort = group(runtime.as_ref(), resume()).await;
    let plan = Arc::new(Plan::default());
    let config_drops = Arc::new(AtomicUsize::new(0));
    let actor = cohort
        .spawn(
            Box::new(plan.make()),
            Box::new(DropInput(config_drops.clone(), false)),
            SpawnOptions::at("/instance"),
        )
        .await
        .unwrap();
    assert_eq!(ask(&actor, Command::Read).await.unwrap(), 0);
    until(|| config_drops.load(Ordering::SeqCst) == 1).await;
    assert!(ask(&actor, Command::Fail).await.is_err());
    assert_eq!(ask(&actor, Command::Failures).await.unwrap(), 1);
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    assert_eq!(plan.drops.load(Ordering::SeqCst), 0);
    assert_eq!(cohort.members().len(), 1);
    cohort.stop().await.unwrap();
    assert_eq!(plan.drops.load(Ordering::SeqCst), 1);
    shutdown(runtime.as_ref()).await;
}
both!(
    instance_resume_preserves_state_and_retires_config,
    instances
);

struct DropInput(Arc<AtomicUsize>, bool);
impl Drop for DropInput {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "input retirement witness");
    }
}
async fn inputs(actix: bool) {
    let runtime = runtime(actix).await;
    let cohort = group(runtime.as_ref(), resume()).await;
    for (path, poll) in [("/unpolled", false), ("", true)] {
        let plan = Arc::new(Plan::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let pending = cohort.spawn(
            Box::new(plan.make()),
            Box::new(DropInput(drops.clone(), true)),
            SpawnOptions::at(path),
        );
        if poll {
            assert!(pending.await.is_err());
        } else {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending))).is_ok()
            );
        }
        until(|| plan.drops.load(Ordering::SeqCst) == 1 && drops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(plan.inits.load(Ordering::SeqCst), 0);
        assert!(runtime.get_actor(path).await.is_none());
    }
    for (path, poll) in [("/unpolled-factory", false), ("", true)] {
        let drops = Arc::new(AtomicUsize::new(0));
        let owned = DropInput(drops.clone(), true);
        let builds = Arc::new(AtomicUsize::new(0));
        let count = builds.clone();
        let pending = cohort.spawn_factory(
            Box::new(move || {
                let _ = &owned;
                count.fetch_add(1, Ordering::SeqCst);
                Box::new(Arc::new(Plan::default()).make())
            }),
            SpawnOptions::at(path),
        );
        if poll {
            assert!(pending.await.is_err());
        } else {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending))).is_ok()
            );
        }
        until(|| drops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(builds.load(Ordering::SeqCst), 0);
    }
    assert!(cohort.members().is_empty());
    let stopped = cohort.stop();
    // Admission closes even when its first cleanup waiter is never polled.
    drop(stopped);
    let plan = Arc::new(Plan::default());
    assert!(
        cohort
            .spawn_factory(plan.factory(), SpawnOptions::at("/closed"))
            .await
            .is_err()
    );
    assert_eq!(plan.builds.load(Ordering::SeqCst), 0);
    cohort.stop().await.unwrap();
    shutdown(runtime.as_ref()).await;
}
both!(unpolled_and_rejected_inputs_are_owned, inputs);

async fn pending_constructor(actix: bool) {
    let runtime = runtime(actix).await;
    let cohort = group(runtime.as_ref(), all()).await;
    let gate = Arc::new(ConstructorGate::default());
    let plan = Arc::new(Plan {
        constructor: Some(gate.clone()),
        ..Default::default()
    });
    let worker = cohort.clone();
    let factory = plan.factory();
    let pending = tokio::spawn(async move {
        worker
            .spawn_factory(factory, SpawnOptions::at("/pending"))
            .await
    });
    notified(&gate.entered).await;
    assert_eq!(
        cohort.members().len(),
        1,
        "pending constructor escaped group ownership"
    );
    assert_eq!(cohort.members().get(0).unwrap().path(), "/pending");
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let receipt = cohort.stop();
    gate.release();
    tokio::time::timeout(WAIT, receipt).await.unwrap().unwrap();
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    assert_eq!(plan.drops.load(Ordering::SeqCst), 1);
    assert!(runtime.get_actor("/pending").await.is_none());
    shutdown(runtime.as_ref()).await;
}
both!(
    cancelled_constructor_stays_owned_until_retirement,
    pending_constructor
);

async fn stop_cancellation(actix: bool) {
    let runtime = runtime(actix).await;
    let cohort = group(runtime.as_ref(), all()).await;
    let gate = Arc::new(Gate::default());
    let plan = Arc::new(Plan {
        first_retirement: Some(gate.clone()),
        ..Default::default()
    });
    let actor = cohort
        .spawn_factory(plan.factory(), SpawnOptions::at("/retiring"))
        .await
        .unwrap();
    assert_eq!(ask(&actor, Command::Read).await.unwrap(), 0);
    drop(cohort.stop());
    notified(&gate.entered).await;
    let repeated = cohort.stop();
    tokio::pin!(repeated);
    assert!(futures::poll!(&mut repeated).is_pending());
    gate.release.notify_one();
    repeated.await.unwrap();
    assert_eq!(plan.drops.load(Ordering::SeqCst), 1);
    assert!(!actor.is_alive().await);
    shutdown(runtime.as_ref()).await;
}
both!(
    cancelled_stop_waiter_does_not_cancel_retirement,
    stop_cancellation
);

async fn handle_lifetime(actix: bool) {
    let runtime = runtime(actix).await;
    let cohort = group(runtime.as_ref(), resume()).await;
    let plan = Arc::new(Plan::default());
    let actor = cohort
        .spawn_factory(plan.factory(), SpawnOptions::default())
        .await
        .unwrap();
    assert!(!actor.path().is_empty());
    let duplicate = Arc::new(Plan::default());
    assert!(
        cohort
            .spawn_factory(duplicate.factory(), SpawnOptions::at(actor.path()))
            .await
            .is_err()
    );
    assert_eq!(duplicate.builds.load(Ordering::SeqCst), 0);
    assert_eq!(cohort.members().len(), 1);
    drop(cohort);
    assert_eq!(ask(&actor, Command::Read).await.unwrap(), 0);
    assert!(runtime.get_actor(&actor.path()).await.is_some());
    shutdown(runtime.as_ref()).await;
    assert_eq!(plan.drops.load(Ordering::SeqCst), 1);
    assert!(!actor.is_alive().await);
}
both!(
    dropped_group_handle_preserves_members_until_system_shutdown,
    handle_lifetime
);

async fn policy_inputs(actix: bool) {
    let runtime = runtime(actix).await;
    for poll in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let owned = DropInput(drops.clone(), !poll);
        let strategy = SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 1,
            within: WAIT,
            decider: BasicDecisionFn::new(move |_| {
                let _ = &owned;
                SupervisionDecision::Restart
            }),
        });
        let pending = runtime.cohort_runtime().unwrap().root_cohort(strategy);
        if poll {
            let cohort = pending.await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            cohort.stop().await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        } else {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending))).is_ok()
            );
            until(|| drops.load(Ordering::SeqCst) == 1).await;
        }
    }
    shutdown(runtime.as_ref()).await;
    assert!(
        runtime
            .cohort_runtime()
            .unwrap()
            .root_cohort(resume())
            .await
            .is_err()
    );
}
both!(
    root_policy_captures_follow_creation_and_retirement,
    policy_inputs
);

struct LocalProbe {
    shared: Probe,
    owner: ThreadId,
    value: Rc<RefCell<usize>>,
}
impl LocalProbe {
    fn check_owner(&self) {
        if self.owner != std::thread::current().id() {
            self.shared.plan.wrong_owner.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(self.owner, std::thread::current().id());
    }
}
impl Drop for LocalProbe {
    fn drop(&mut self) {
        self.check_owner();
    }
}
impl LocalActor for LocalProbe {
    fn init<'a>(&'a mut self, context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.check_owner();
            assert!(context.parent().is_none());
            tokio::task::yield_now().await;
            self.check_owner();
            self.shared.plan.inits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.check_owner();
            if self.shared.generation == 0
                && let Some(gate) = &self.shared.plan.first_retirement
            {
                gate.wait().await;
            }
            self.check_owner();
            Ok(())
        })
    }
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.check_owner();
            let command = *message.downcast::<Command>().unwrap();
            tokio::task::yield_now().await;
            self.check_owner();
            let value = match command {
                Command::Read => self.shared.generation,
                Command::Failures => *self.value.borrow(),
                Command::Fail => {
                    *self.value.borrow_mut() += 1;
                    return Err(ActorError::MessageHandlingError("cohort witness".into()));
                }
            };
            Ok(Box::new(value) as BoxedMessage)
        })
    }
}

#[actix::test]
async fn local_and_send_factories_share_the_same_recovery_barrier() {
    let runtime: Arc<dyn AsyncEngineRuntime> = Arc::new(
        ActixActorSystem::with_guardian(
            ActixSystemSettings {
                arbiter_count: 2,
                shutdown_timeout: WAIT,
                ..Default::default()
            },
            parrot_api::system::GuardianConfig::default(),
        )
        .await
        .unwrap(),
    );
    let cohort = group(runtime.as_ref(), all()).await;
    let gate = Arc::new(ConstructorGate::default());
    let local = Arc::new(Plan {
        first_drop: Some(gate.clone()),
        ..Default::default()
    });
    let dynamic = Arc::new(Plan::default());
    let source = local.clone();
    let a = cohort
        .local_runtime()
        .unwrap()
        .spawn_local_factory(
            Box::new(move || {
                Box::new(LocalProbe {
                    shared: source.make(),
                    owner: std::thread::current().id(),
                    value: Rc::new(RefCell::new(0)),
                })
            }),
            SpawnOptions::at("/local"),
        )
        .await
        .unwrap();
    let b = cohort
        .spawn_factory(dynamic.factory(), SpawnOptions::at("/send"))
        .await
        .unwrap();
    assert_eq!(ask(&a, Command::Read).await.unwrap(), 0);
    assert_eq!(ask(&b, Command::Read).await.unwrap(), 0);
    assert!(ask(&b, Command::Fail).await.is_err());
    notified(&gate.entered).await;
    assert_eq!(
        dynamic.builds.load(Ordering::SeqCst),
        1,
        "Send member bypassed Local retirement"
    );
    assert_eq!(local.builds.load(Ordering::SeqCst), 1);
    assert_eq!(cohort.members().len(), 2);
    gate.release();
    until(|| {
        local.builds.load(Ordering::SeqCst) == 2 && dynamic.builds.load(Ordering::SeqCst) == 2
    })
    .await;
    recovered(&a).await;
    recovered(&b).await;
    assert_eq!(
        ask(&runtime.get_actor("/local").await.unwrap(), Command::Read)
            .await
            .unwrap(),
        1
    );
    assert!(ask(&a, Command::Fail).await.is_err());
    until(|| local.drops.load(Ordering::SeqCst) == 2 && dynamic.drops.load(Ordering::SeqCst) == 2)
        .await;
    assert!(
        ask(&b, Command::Read).await.is_err(),
        "Local failure bypassed the shared budget"
    );
    let _ = cohort.stop().await;
    assert_eq!(local.drops.load(Ordering::SeqCst), 2);
    assert_eq!(dynamic.drops.load(Ordering::SeqCst), 2);
    assert_eq!(local.wrong_owner.load(Ordering::SeqCst), 0);
    shutdown(runtime.as_ref()).await;
}

#[actix::test]
async fn local_resume_keeps_owner_state_and_closed_admission_retires_captures() {
    let runtime = runtime(true).await;
    let cohort = group(runtime.as_ref(), resume()).await;
    let plan = Arc::new(Plan::default());
    let source = plan.clone();
    let actor = cohort
        .local_runtime()
        .unwrap()
        .spawn_local_factory(
            Box::new(move || {
                Box::new(LocalProbe {
                    shared: source.make(),
                    owner: std::thread::current().id(),
                    value: Rc::new(RefCell::new(0)),
                })
            }),
            SpawnOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(ask(&actor, Command::Read).await.unwrap(), 0);
    assert!(ask(&actor, Command::Fail).await.is_err());
    assert_eq!(ask(&actor, Command::Failures).await.unwrap(), 1);
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    assert_eq!(plan.drops.load(Ordering::SeqCst), 0);
    cohort.stop().await.unwrap();
    assert_eq!(plan.drops.load(Ordering::SeqCst), 1);
    assert_eq!(plan.wrong_owner.load(Ordering::SeqCst), 0);
    for (path, poll) in [
        ("/unpolled-local", false),
        ("", true),
        ("/closed-local", true),
    ] {
        let drops = Arc::new(AtomicUsize::new(0));
        let owned = DropInput(drops.clone(), true);
        let builds = Arc::new(AtomicUsize::new(0));
        let count = builds.clone();
        let pending = cohort.local_runtime().unwrap().spawn_local_factory(
            Box::new(move || {
                let _ = &owned;
                count.fetch_add(1, Ordering::SeqCst);
                Box::new(LocalProbe {
                    shared: Arc::new(Plan::default()).make(),
                    owner: std::thread::current().id(),
                    value: Rc::new(RefCell::new(0)),
                })
            }),
            SpawnOptions::at(path),
        );
        if poll {
            assert!(pending.await.is_err());
        } else {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending))).is_ok()
            );
        }
        until(|| drops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        assert!(runtime.get_actor(path).await.is_none());
    }
    shutdown(runtime.as_ref()).await;
}

#[actix::test]
async fn local_pending_constructor_stop_joins_late_owner_output() {
    let runtime = runtime(true).await;
    let cohort = group(runtime.as_ref(), resume()).await;
    let gate = Arc::new(ConstructorGate::default());
    let plan = Arc::new(Plan {
        constructor: Some(gate.clone()),
        ..Default::default()
    });
    let source = plan.clone();
    let actor = cohort
        .local_runtime()
        .unwrap()
        .spawn_local_factory(
            Box::new(move || {
                Box::new(LocalProbe {
                    shared: source.make(),
                    owner: std::thread::current().id(),
                    value: Rc::new(RefCell::new(0)),
                })
            }),
            SpawnOptions::at("/pending-local"),
        )
        .await
        .unwrap();
    notified(&gate.entered).await;
    assert_eq!(cohort.members().len(), 1);
    drop(cohort.stop());
    tokio::time::timeout(WAIT, async {
        while actor.is_alive().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("group stop never sealed the pending Local member");
    let repeated = cohort.stop();
    tokio::pin!(repeated);
    assert!(futures::poll!(&mut repeated).is_pending());
    assert_eq!(plan.drops.load(Ordering::SeqCst), 0);
    gate.release();
    tokio::time::timeout(WAIT, repeated).await.unwrap().unwrap();
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    assert_eq!(plan.inits.load(Ordering::SeqCst), 0);
    assert_eq!(plan.drops.load(Ordering::SeqCst), 1);
    assert_eq!(plan.wrong_owner.load(Ordering::SeqCst), 0);
    assert!(!actor.is_alive().await);
    shutdown(runtime.as_ref()).await;
}

#[actix::test]
async fn facade_groups_capture_named_and_default_engines_before_poll() {
    let a = runtime(false).await;
    let b = runtime(true).await;
    let facade = ParrotActorSystem::new(ActorSystemConfig::default())
        .await
        .unwrap();
    facade
        .register_runtime("a".into(), a.clone(), true)
        .unwrap();
    facade
        .register_runtime("b".into(), b.clone(), false)
        .unwrap();
    let pending = facade.cohort_runtime().unwrap().root_cohort(resume());
    let named = facade.root_cohort_on("a", resume());
    facade.set_default_system("b").unwrap();
    facade
        .register_runtime("a".into(), b.clone(), false)
        .unwrap();
    let default_group = pending.await.unwrap();
    let named_group = named.await.unwrap();
    for (cohort, path) in [(&default_group, "/default"), (&named_group, "/named")] {
        assert!(
            cohort.local_runtime().is_none(),
            "group was redirected to new default"
        );
        let plan = Arc::new(Plan::default());
        let actor = cohort
            .spawn_factory(plan.factory(), SpawnOptions::at(path))
            .await
            .unwrap();
        assert_eq!(ask(&actor, Command::Read).await.unwrap(), 0);
        assert!(a.get_actor(path).await.is_some());
        assert!(b.get_actor(path).await.is_none());
    }
    let local_group = facade.root_cohort_on("b", resume()).await.unwrap();
    assert!(local_group.local_runtime().is_some());
    default_group.stop().await.unwrap();
    named_group.stop().await.unwrap();
    local_group.stop().await.unwrap();
    facade.shutdown_runtime().await.unwrap();
    shutdown(a.as_ref()).await;
}

struct WithoutCohorts;

#[actix::test]
async fn nested_facade_group_creation_captures_physical_engine_before_poll() {
    let a = runtime(false).await;
    let b = runtime(true).await;
    let inner = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    inner.register_runtime("a".into(), a.clone(), true).unwrap();
    inner
        .register_runtime("b".into(), b.clone(), false)
        .unwrap();
    let outer = ParrotActorSystem::new(ActorSystemConfig::default())
        .await
        .unwrap();
    outer
        .register_runtime("nested".into(), inner.clone(), true)
        .unwrap();
    let pending_default = outer.cohort_runtime().unwrap().root_cohort(resume());
    let pending_named = outer.root_cohort_on("nested", resume());
    inner.set_default_system("b").unwrap();
    let default = pending_default.await.unwrap();
    let named = pending_named.await.unwrap();
    let redirected_default = default.local_runtime().is_some();
    let redirected_named = named.local_runtime().is_some();
    default.stop().await.unwrap();
    named.stop().await.unwrap();
    outer.shutdown_runtime().await.unwrap();
    assert!(
        !redirected_default,
        "nested default creation changed physical engine before first poll"
    );
    assert!(
        !redirected_named,
        "nested named creation changed physical engine before first poll"
    );
}

impl AsyncEngineRuntime for WithoutCohorts {
    fn spawn<'a>(
        &'a self,
        _: parrot_api::engine::RuntimeActor,
        _: parrot_api::engine::InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        unreachable!()
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async { None })
    }
    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: parrot_api::system::SystemState::Running,
            active_actors: 0,
            uptime: Duration::ZERO,
            resources: parrot_api::system::SystemResources {
                memory_usage: 0,
                cpu_usage: 0.0,
                thread_count: 0,
            },
        }
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
}
#[actix::test]
async fn facade_rejection_and_unpolled_creation_retire_policy_captures() {
    let facade = ParrotActorSystem::new(ActorSystemConfig::default())
        .await
        .unwrap();
    facade
        .register_runtime("external".into(), Arc::new(WithoutCohorts), true)
        .unwrap();
    assert!(facade.cohort_runtime().is_none());
    for (name, poll) in [("external", true), ("missing", true), ("external", false)] {
        let drops = Arc::new(AtomicUsize::new(0));
        let owned = DropInput(drops.clone(), true);
        let strategy = SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 1,
            within: WAIT,
            decider: BasicDecisionFn::new(move |_| {
                let _ = &owned;
                SupervisionDecision::Restart
            }),
        });
        let pending = facade.root_cohort_on(name, strategy);
        if poll {
            assert!(pending.await.is_err());
        } else {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending))).is_ok()
            );
        }
        until(|| drops.load(Ordering::SeqCst) == 1).await;
    }
    facade.shutdown_runtime().await.unwrap();
}
