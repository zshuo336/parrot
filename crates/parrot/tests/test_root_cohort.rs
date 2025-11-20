//! Public root groups must preserve root identity and execute real recovery barriers.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    root_cohort::RootCohort,
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
    typed_context::TypedContext,
};
use parrot_api::{
    Message,
    actor::{Actor, ActorState, EmptyConfig},
    context::ActorContext,
    errors::ActorError,
    supervisor::{
        BasicDecisionFn, OneForAllStrategy, OneForOneStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Barrier, Notify};

const WAIT: Duration = Duration::from_secs(8);
#[derive(Clone)]
enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
impl Engine {
    async fn new(actix: bool) -> Self {
        Self::with_timeout(actix, WAIT).await
    }
    async fn with_timeout(actix: bool, timeout: Duration) -> Self {
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_settings(ActixSystemSettings {
                    arbiter_count: 4,
                    shutdown_timeout: timeout,
                    ..Default::default()
                })
                .await
                .unwrap(),
            ))
        } else {
            Self::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
                shared_pool_size: 4,
                shared_burst_workers_max: 0,
                shutdown_timeout: timeout,
                ..Default::default()
            }))
        }
    }
    async fn cohort(&self, policy: SupervisorStrategyType) -> RootCohort {
        self.try_cohort(policy).await.unwrap()
    }
    async fn try_cohort(&self, policy: SupervisorStrategyType) -> ActorResult<RootCohort> {
        match self {
            Self::Thread(system) => system.root_cohort(policy).await,
            Self::Actix(system) => system.root_cohort(policy).await,
        }
    }
    async fn spawn(
        &self,
        plan: &Arc<Plan>,
        cohort: &RootCohort,
        typed: bool,
        factory: bool,
    ) -> ActorResult<Reference> {
        let path = format!("/root-{}", plan.id);
        let plan = plan.clone();
        match (self, typed, factory) {
            (Self::Thread(s), true, true) => s
                .spawn_typed_factory_in_cohort(move || plan.make(), &path, cohort)
                .await
                .map(Reference::Typed),
            (Self::Actix(s), true, true) => s
                .spawn_typed_factory_in_cohort(move || plan.make(), &path, cohort)
                .await
                .map(Reference::Typed),
            (Self::Thread(s), true, false) => s
                .spawn_typed_in_cohort(plan.make(), &path, cohort)
                .await
                .map(Reference::Typed),
            (Self::Actix(s), true, false) => s
                .spawn_typed_in_cohort(plan.make(), &path, cohort)
                .await
                .map(Reference::Typed),
            (Self::Thread(s), false, true) => s
                .spawn_supervised_in_cohort(
                    move || ThreadProbe(plan.make()),
                    &path,
                    Default::default(),
                    cohort,
                )
                .await
                .map(|r| Reference::Dynamic(Box::new(r)))
                .map_err(ActorError::from),
            (Self::Actix(s), false, true) => s
                .spawn_root_factory_in_cohort(move || ActixProbe(plan.make()), &path, cohort)
                .await
                .map(Reference::Dynamic),
            (Self::Thread(s), false, false) => s
                .spawn_in_cohort(
                    ThreadProbe(plan.make()),
                    EmptyConfig,
                    Default::default(),
                    path,
                    cohort,
                )
                .await
                .map(|r| Reference::Dynamic(Box::new(r)))
                .map_err(ActorError::from),
            (Self::Actix(s), false, false) => s
                .spawn_root_in_cohort(ActixProbe(plan.make()), EmptyConfig, &path, cohort)
                .await
                .map(Reference::Dynamic),
        }
    }
    async fn shutdown(&self) {
        self.shutdown_result().await.unwrap();
    }
    async fn shutdown_result(&self) -> Result<(), String> {
        match self {
            Self::Thread(s) => s
                .shutdown_internal()
                .await
                .map_err(|error| error.to_string()),
            Self::Actix(s) => s
                .as_ref()
                .clone()
                .shutdown()
                .await
                .map_err(|error| error.to_string()),
        }
    }
}
enum Reference {
    Typed(TypedActorRef<Probe, Op>),
    Dynamic(BoxedActorRef),
}
impl Reference {
    async fn ask(&self, op: Op) -> ActorResult<usize> {
        match self {
            Self::Typed(reference) => reference.ask(op).await,
            Self::Dynamic(reference) => reference
                .send(Box::new(op))
                .await
                .map(|reply| *reply.downcast::<usize>().unwrap()),
        }
    }
    async fn stop(&self) {
        self.stop_result().await.unwrap();
    }
    async fn stop_result(&self) -> ActorResult<()> {
        match self {
            Self::Typed(reference) => reference.stop().await,
            Self::Dynamic(reference) => reference.stop().await,
        }
    }
}
enum Op {
    Read,
    Fail,
    FailTogether(Arc<Barrier>),
}
impl Message for Op {
    type Result = usize;
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
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
        let (open, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(*open || !timeout.timed_out(), "test gate was not released");
    }
    async fn entered(&self) {
        tokio::time::timeout(WAIT, self.entered.notified())
            .await
            .unwrap();
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
type Events = Arc<Mutex<Vec<(usize, usize, &'static str)>>>;
struct Plan {
    id: usize,
    builds: AtomicUsize,
    inits: AtomicUsize,
    drops: AtomicUsize,
    events: Events,
    construct_gate: Mutex<Option<(usize, Arc<Gate>)>>,
    drop_gate: Mutex<Option<(usize, Arc<Gate>)>>,
    panic_at: AtomicUsize,
    panic_drop_at: AtomicUsize,
}
impl Plan {
    fn new(id: usize, events: &Events) -> Arc<Self> {
        Arc::new(Self {
            id,
            builds: AtomicUsize::new(0),
            inits: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            events: events.clone(),
            construct_gate: Mutex::default(),
            drop_gate: Mutex::default(),
            panic_at: AtomicUsize::new(usize::MAX),
            panic_drop_at: AtomicUsize::new(usize::MAX),
        })
    }
    fn note(&self, generation: usize, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((self.id, generation, phase));
    }
    fn make(self: &Arc<Self>) -> Probe {
        let generation = self.builds.fetch_add(1, Ordering::SeqCst);
        self.note(generation, "construct");
        let gate = self.construct_gate.lock().unwrap().clone();
        if let Some((selected, gate)) = gate
            && selected == generation
        {
            gate.block();
        }
        assert_ne!(
            self.panic_at.load(Ordering::SeqCst),
            generation,
            "deliberate constructor failure"
        );
        Probe {
            plan: self.clone(),
            generation,
        }
    }
}
struct Probe {
    plan: Arc<Plan>,
    generation: usize,
}
impl Probe {
    fn initialized(&self) {
        self.plan.note(self.generation, "init");
        self.plan.inits.fetch_add(1, Ordering::SeqCst);
    }
    async fn receive(&mut self, op: Op) -> ActorResult<usize> {
        match op {
            Op::Read => Ok(self.generation),
            Op::Fail | Op::FailTogether(_) => {
                if let Op::FailTogether(barrier) = op {
                    barrier.wait().await;
                }
                Err(ActorError::MessageHandlingError(
                    "deliberate root failure".into(),
                ))
            }
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        let gate = self.plan.drop_gate.lock().unwrap().clone();
        if let Some((selected, gate)) = gate
            && selected == self.generation
        {
            gate.block();
        }
        self.plan.note(self.generation, "drop");
        self.plan.drops.fetch_add(1, Ordering::SeqCst);
        assert_ne!(
            self.plan.panic_drop_at.load(Ordering::SeqCst),
            self.generation,
            "deliberate root destructor failure"
        );
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = usize;
    fn init_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(
            context.parent().is_none(),
            "group membership must not manufacture a parent actor"
        );
        self.initialized();
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(self.receive(op))
    }
}
macro_rules! dynamic_probe {
    ($name:ident, $context:ty) => {
        struct $name(Probe);
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                context: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                assert!(context.parent().is_none());
                self.0.initialized();
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    self.0
                        .receive(*message.downcast::<Op>().unwrap())
                        .await
                        .map(|generation| Box::new(generation) as BoxedMessage)
                })
            }
        }
    };
}
dynamic_probe!(ThreadProbe, parrot::thread::ThreadContext<Self>);
dynamic_probe!(
    ActixProbe,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
impl parrot_api::actor::ActixEngineExt for ActixProbe {}

fn policy(all: bool, maximum: u32, decision: SupervisionDecision) -> SupervisorStrategyType {
    let decider = BasicDecisionFn::new(move |_| decision);
    if all {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: maximum,
            within: Duration::from_secs(60),
            decider,
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: maximum,
            within: Duration::from_secs(60),
            decider,
        })
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn ready(plans: &[Arc<Plan>], count: usize) {
    until(|| {
        plans
            .iter()
            .all(|plan| plan.inits.load(Ordering::SeqCst) == count)
    })
    .await;
}
fn barrier_order(events: &Events, members: &[usize], generation: usize) {
    let events = events.lock().unwrap();
    let last_drop = members
        .iter()
        .map(|id| {
            events
                .iter()
                .position(|event| *event == (*id, generation - 1, "drop"))
                .unwrap()
        })
        .max()
        .unwrap();
    let first_build = members
        .iter()
        .map(|id| {
            events
                .iter()
                .position(|event| *event == (*id, generation, "construct"))
                .unwrap()
        })
        .min()
        .unwrap();
    let last_build = members
        .iter()
        .map(|id| {
            events
                .iter()
                .position(|event| *event == (*id, generation, "construct"))
                .unwrap()
        })
        .max()
        .unwrap();
    let first_init = members
        .iter()
        .map(|id| {
            events
                .iter()
                .position(|event| *event == (*id, generation, "init"))
                .unwrap()
        })
        .min()
        .unwrap();
    assert!(
        last_drop < first_build,
        "replacement preceded actual retirement: {events:?}"
    );
    assert!(
        last_build < first_init,
        "partial group initialized: {events:?}"
    );
}
macro_rules! both {
    ($name:ident, $engine:ident, $body:block) => {
        mod $name {
            use super::*;
            async fn run($engine: Engine) $body
            #[actix::test] async fn thread() { run(Engine::new(false).await).await; }
            #[actix::test] async fn actix() { run(Engine::new(true).await).await; }
        }
    };
}

both!(stable_roots_shared_budget_and_isolation, engine, {
    for (layout, typed) in [[false, false], [true, true], [false, true]]
        .into_iter()
        .enumerate()
    {
        let events = Events::default();
        let plans = [
            Plan::new(layout * 3, &events),
            Plan::new(layout * 3 + 1, &events),
        ];
        let other = Plan::new(layout * 3 + 2, &events);
        let cohort = engine
            .cohort(policy(true, 2, SupervisionDecision::Restart))
            .await;
        let isolated = engine
            .cohort(policy(true, 2, SupervisionDecision::Restart))
            .await;
        let a = engine
            .spawn(&plans[0], &cohort, typed[0], true)
            .await
            .unwrap();
        let b = engine
            .spawn(&plans[1], &cohort, typed[1], true)
            .await
            .unwrap();
        let unaffected = engine.spawn(&other, &isolated, true, true).await.unwrap();
        // Static publication precedes initialization. This budget test starts
        // from two live generations; pending first construction is tested below.
        assert_eq!(a.ask(Op::Read).await.unwrap(), 0);
        assert_eq!(b.ask(Op::Read).await.unwrap(), 0);
        assert_eq!(unaffected.ask(Op::Read).await.unwrap(), 0);
        for (epoch, failing) in [&a, &b].into_iter().enumerate() {
            assert!(failing.ask(Op::Fail).await.is_err());
            ready(&plans, epoch + 2).await;
            assert_eq!(a.ask(Op::Read).await.unwrap(), epoch + 1);
            assert_eq!(b.ask(Op::Read).await.unwrap(), epoch + 1);
            barrier_order(&events, &[plans[0].id, plans[1].id], epoch + 1);
            assert_eq!(unaffected.ask(Op::Read).await.unwrap(), 0);
        }
        assert!(a.ask(Op::Fail).await.is_err());
        until(|| {
            plans
                .iter()
                .all(|plan| plan.drops.load(Ordering::SeqCst) == 3)
        })
        .await;
        assert!(a.ask(Op::Read).await.is_err());
        assert!(b.ask(Op::Read).await.is_err());
        assert_eq!(unaffected.ask(Op::Read).await.unwrap(), 0);
        assert!(
            plans
                .iter()
                .all(|plan| plan.builds.load(Ordering::SeqCst) == 3)
        );
        cohort.stop().await.unwrap();
        isolated.stop().await.unwrap();
    }
    engine.shutdown().await;
});

both!(retirement_and_preparation_are_group_barriers, engine, {
    let events = Events::default();
    let plans = [Plan::new(0, &events), Plan::new(1, &events)];
    let retired = Arc::new(Gate::default());
    let prepared = Arc::new(Gate::default());
    let _release = (Release(retired.clone()), Release(prepared.clone()));
    *plans[1].drop_gate.lock().unwrap() = Some((0, retired.clone()));
    *plans[0].construct_gate.lock().unwrap() = Some((1, prepared.clone()));
    let cohort = engine
        .cohort(policy(true, 2, SupervisionDecision::Restart))
        .await;
    let a = engine.spawn(&plans[0], &cohort, false, true).await.unwrap();
    let b = engine.spawn(&plans[1], &cohort, true, true).await.unwrap();
    ready(&plans, 1).await;
    assert!(a.ask(Op::Fail).await.is_err());
    retired.entered().await;
    assert!(
        plans
            .iter()
            .all(|plan| plan.builds.load(Ordering::SeqCst) == 1)
    );
    retired.release();
    prepared.entered().await;
    assert!(
        plans
            .iter()
            .all(|plan| plan.inits.load(Ordering::SeqCst) == 1)
    );
    prepared.release();
    ready(&plans, 2).await;
    barrier_order(&events, &[0, 1], 1);
    assert_eq!(a.ask(Op::Read).await.unwrap(), 1);
    assert_eq!(b.ask(Op::Read).await.unwrap(), 1);
    cohort.stop().await.unwrap();
    engine.shutdown().await;
});

both!(concurrent_failures_join_one_epoch, engine, {
    let events = Events::default();
    let plans = [Plan::new(0, &events), Plan::new(1, &events)];
    let cohort = engine
        .cohort(policy(true, 1, SupervisionDecision::Restart))
        .await;
    let a = engine.spawn(&plans[0], &cohort, false, true).await.unwrap();
    let b = engine.spawn(&plans[1], &cohort, true, true).await.unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let (first, second) = tokio::join!(
        a.ask(Op::FailTogether(barrier.clone())),
        b.ask(Op::FailTogether(barrier))
    );
    assert!(first.is_err() && second.is_err());
    ready(&plans, 2).await;
    assert_eq!(a.ask(Op::Read).await.unwrap(), 1);
    assert_eq!(b.ask(Op::Read).await.unwrap(), 1);
    assert!(
        plans
            .iter()
            .all(|plan| plan.builds.load(Ordering::SeqCst) == 2)
    );
    barrier_order(&events, &[0, 1], 1);
    cohort.stop().await.unwrap();
    engine.shutdown().await;
});

both!(constructor_failure_never_partially_activates, engine, {
    let events = Events::default();
    let plans = [Plan::new(0, &events), Plan::new(1, &events)];
    plans[1].panic_at.store(1, Ordering::SeqCst);
    let gate = Arc::new(Gate::default());
    let failure_gate = Arc::new(Gate::default());
    let _release = (Release(gate.clone()), Release(failure_gate.clone()));
    *plans[0].construct_gate.lock().unwrap() = Some((1, gate.clone()));
    *plans[1].construct_gate.lock().unwrap() = Some((1, failure_gate.clone()));
    let cohort = engine
        .cohort(policy(true, 2, SupervisionDecision::Restart))
        .await;
    let a = engine.spawn(&plans[0], &cohort, true, true).await.unwrap();
    let b = engine.spawn(&plans[1], &cohort, false, true).await.unwrap();
    ready(&plans, 1).await;
    assert!(a.ask(Op::Fail).await.is_err());
    gate.entered().await;
    failure_gate.entered().await;
    failure_gate.release();
    until(|| plans[1].builds.load(Ordering::SeqCst) == 2).await;
    until(|| cohort.members().len() == 1).await;
    gate.release();
    until(|| cohort.members().is_empty()).await;
    let error = a.stop_result().await.unwrap_err();
    assert!(
        !matches!(error, ActorError::Timeout | ActorError::TimeoutDetail(_)),
        "sibling construction failure became a timeout: {error:?}"
    );
    assert!(
        error.to_string().contains("deliberate constructor failure"),
        "lost cohort failure cause: {error}"
    );
    cohort.stop().await.unwrap();
    assert_eq!(plans[0].inits.load(Ordering::SeqCst), 1);
    assert_eq!(plans[1].inits.load(Ordering::SeqCst), 1);
    assert!(a.ask(Op::Read).await.is_err());
    assert!(b.ask(Op::Read).await.is_err());
    engine.shutdown().await;
});

both!(mixed_instance_group_stops_without_reconstruction, engine, {
    for instance_typed in [false, true] {
        let events = Events::default();
        let plans = [
            Plan::new(usize::from(instance_typed) * 2, &events),
            Plan::new(usize::from(instance_typed) * 2 + 1, &events),
        ];
        let cohort = engine
            .cohort(policy(true, 3, SupervisionDecision::Restart))
            .await;
        let a = engine
            .spawn(&plans[0], &cohort, !instance_typed, true)
            .await
            .unwrap();
        let b = engine
            .spawn(&plans[1], &cohort, instance_typed, false)
            .await
            .unwrap();
        assert!(a.ask(Op::Fail).await.is_err());
        until(|| {
            plans
                .iter()
                .all(|plan| plan.drops.load(Ordering::SeqCst) == 1)
        })
        .await;
        assert!(
            plans
                .iter()
                .all(|plan| plan.builds.load(Ordering::SeqCst) == 1)
        );
        assert!(b.ask(Op::Read).await.is_err());
        cohort.stop().await.unwrap();
    }
    engine.shutdown().await;
});

both!(resume_and_one_for_one_preserve_siblings, engine, {
    for (id, decision) in [SupervisionDecision::Resume, SupervisionDecision::Restart]
        .into_iter()
        .enumerate()
    {
        let events = Events::default();
        let plans = [Plan::new(id * 2, &events), Plan::new(id * 2 + 1, &events)];
        let cohort = engine.cohort(policy(false, 1, decision)).await;
        let a = engine.spawn(&plans[0], &cohort, true, true).await.unwrap();
        let b = engine.spawn(&plans[1], &cohort, false, true).await.unwrap();
        assert!(a.ask(Op::Fail).await.is_err());
        let generation = usize::from(decision == SupervisionDecision::Restart);
        until(|| plans[0].inits.load(Ordering::SeqCst) == generation + 1).await;
        assert_eq!(a.ask(Op::Read).await.unwrap(), generation);
        assert_eq!(b.ask(Op::Read).await.unwrap(), 0);
        cohort.stop().await.unwrap();
    }
    engine.shutdown().await;
});

both!(
    stop_during_preparation_owns_cleanup_after_waiter_cancellation,
    engine,
    {
        let events = Events::default();
        let plans = [Plan::new(0, &events), Plan::new(1, &events)];
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        *plans[0].construct_gate.lock().unwrap() = Some((1, gate.clone()));
        let cohort = engine
            .cohort(policy(true, 3, SupervisionDecision::Restart))
            .await;
        let a = engine.spawn(&plans[0], &cohort, false, true).await.unwrap();
        let b = engine.spawn(&plans[1], &cohort, true, true).await.unwrap();
        ready(&plans, 1).await;
        assert!(a.ask(Op::Fail).await.is_err());
        gate.entered().await;
        let mut first = cohort.stop();
        assert!(futures::poll!(&mut first).is_pending());
        drop(first);
        assert!(
            engine
                .spawn(&Plan::new(2, &events), &cohort, true, true)
                .await
                .is_err()
        );
        gate.release();
        tokio::time::timeout(WAIT, cohort.stop())
            .await
            .unwrap()
            .unwrap();
        assert!(
            plans
                .iter()
                .all(|plan| plan.inits.load(Ordering::SeqCst) == 1)
        );
        assert!(b.ask(Op::Read).await.is_err());
        engine.shutdown().await;
    }
);

both!(foreign_and_closed_admission_are_rejected, engine, {
    let other = Engine::new(matches!(engine, Engine::Actix(_))).await;
    let cohort = engine
        .cohort(policy(true, 1, SupervisionDecision::Restart))
        .await;
    let events = Events::default();
    for (id, typed) in [false, true].into_iter().enumerate() {
        let plan = Plan::new(id, &events);
        assert!(other.spawn(&plan, &cohort, typed, true).await.is_err());
        assert_eq!(plan.builds.load(Ordering::SeqCst), 0);
    }
    cohort.stop().await.unwrap();
    for (id, typed) in [false, true].into_iter().enumerate() {
        let plan = Plan::new(id + 2, &events);
        assert!(engine.spawn(&plan, &cohort, typed, true).await.is_err());
        assert_eq!(plan.builds.load(Ordering::SeqCst), 0);
    }
    other.shutdown().await;
    engine.shutdown().await;
});

both!(system_shutdown_closes_groups_and_policy_disposal, engine, {
    let events = Events::default();
    let plans = [Plan::new(0, &events), Plan::new(1, &events)];
    let cohort = engine
        .cohort(policy(true, 2, SupervisionDecision::Restart))
        .await;
    let _a = engine.spawn(&plans[0], &cohort, false, true).await.unwrap();
    let _b = engine.spawn(&plans[1], &cohort, true, true).await.unwrap();
    ready(&plans, 1).await;
    engine.shutdown().await;
    assert!(
        plans
            .iter()
            .all(|plan| plan.drops.load(Ordering::SeqCst) == 1)
    );
    assert!(
        engine
            .spawn(&Plan::new(2, &events), &cohort, true, true)
            .await
            .is_err()
    );
    cohort.stop().await.unwrap();
});

both!(explicit_member_stop_aborts_prepared_group, engine, {
    let events = Events::default();
    let plans = [Plan::new(0, &events), Plan::new(1, &events)];
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    *plans[0].construct_gate.lock().unwrap() = Some((1, gate.clone()));
    let cohort = engine
        .cohort(policy(true, 2, SupervisionDecision::Restart))
        .await;
    let a = engine.spawn(&plans[0], &cohort, false, true).await.unwrap();
    let b = engine.spawn(&plans[1], &cohort, true, true).await.unwrap();
    ready(&plans, 1).await;
    assert!(a.ask(Op::Fail).await.is_err());
    gate.entered().await;
    let mut stopped = Box::pin(b.stop());
    assert!(futures::poll!(&mut stopped).is_pending());
    gate.release();
    stopped.await;
    until(|| cohort.members().is_empty()).await;
    assert!(
        plans
            .iter()
            .all(|plan| plan.inits.load(Ordering::SeqCst) == 1)
    );
    cohort.stop().await.unwrap();
    engine.shutdown().await;
});

both!(
    shutdown_aborts_preparation_and_joins_actual_outputs,
    engine,
    {
        let events = Events::default();
        let plans = [Plan::new(0, &events), Plan::new(1, &events)];
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        *plans[0].construct_gate.lock().unwrap() = Some((1, gate.clone()));
        let cohort = engine
            .cohort(policy(true, 2, SupervisionDecision::Restart))
            .await;
        let a = engine.spawn(&plans[0], &cohort, true, true).await.unwrap();
        let _b = engine.spawn(&plans[1], &cohort, false, true).await.unwrap();
        ready(&plans, 1).await;
        assert!(a.ask(Op::Fail).await.is_err());
        gate.entered().await;
        let mut shutdown = Box::pin(engine.shutdown_result());
        assert!(futures::poll!(&mut shutdown).is_pending());
        assert!(
            engine
                .spawn(&Plan::new(2, &events), &cohort, true, true)
                .await
                .is_err()
        );
        gate.release();
        // The failing root may retain its original handler error when shutdown
        // interrupts reconstruction. A fabricated timeout must never replace it.
        if let Err(error) = tokio::time::timeout(WAIT, shutdown).await.unwrap() {
            assert!(
                error.contains("deliberate root failure"),
                "unexpected shutdown failure: {error}"
            );
        }
        assert!(
            plans
                .iter()
                .all(|plan| plan.inits.load(Ordering::SeqCst) == 1)
        );
        assert!(plans.iter().all(|plan| plan.drops.load(Ordering::SeqCst) == plan.builds.load(Ordering::SeqCst)));
        assert!(cohort.members().is_empty());
    }
);

both!(
    initial_constructor_is_a_real_member_before_publication,
    engine,
    {
        for (id, typed) in [false, true].into_iter().enumerate() {
            let events = Events::default();
            let plans = [Plan::new(id * 2, &events), Plan::new(id * 2 + 1, &events)];
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            *plans[1].construct_gate.lock().unwrap() = Some((0, gate.clone()));
            let cohort = engine
                .cohort(policy(true, 2, SupervisionDecision::Restart))
                .await;
            let a = engine
                .spawn(&plans[0], &cohort, !typed, true)
                .await
                .unwrap();
            assert_eq!(a.ask(Op::Read).await.unwrap(), 0);
            let system = engine.clone();
            let group = cohort.clone();
            let plan = plans[1].clone();
            let pending =
                tokio::spawn(async move { system.spawn(&plan, &group, typed, true).await });
            gate.entered().await;
            assert_eq!(cohort.members().len(), 2);
            assert!(a.ask(Op::Fail).await.is_err());
            until(|| plans[0].drops.load(Ordering::SeqCst) == 1).await;
            // Disposal can precede policy selection. Keep the pending first
            // constructor blocked until the group has actually begun recovery.
            // Probe the existing path so an early attempt cannot add a member.
            tokio::time::timeout(WAIT, async {
                loop {
                    let error = match engine.spawn(&plans[0], &cohort, !typed, true).await {
                        Err(error) => error.to_string(),
                        Ok(_) => panic!("duplicate root was admitted during recovery probe"),
                    };
                    if error.contains("child group is handling a failure; retry after recovery") {
                        break;
                    }
                    assert!(
                        error.to_ascii_lowercase().contains("actor path already"),
                        "unexpected admission failure before recovery: {error}"
                    );
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("recovery must close admission before releasing initial construction");
            assert_eq!(plans[0].builds.load(Ordering::SeqCst), 1);
            gate.release();
            let b = tokio::time::timeout(WAIT, pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            until(|| {
                plans[0].inits.load(Ordering::SeqCst) == 2
                    && plans[1].inits.load(Ordering::SeqCst) == 1
            })
            .await;
            assert_eq!(a.ask(Op::Read).await.unwrap(), 1);
            assert_eq!(b.ask(Op::Read).await.unwrap(), 1);
            barrier_order(&events, &[plans[0].id, plans[1].id], 1);
            cohort.stop().await.unwrap();
        }
        engine.shutdown().await;
    }
);

both!(dropped_group_handle_does_not_detach_supervision, engine, {
    let events = Events::default();
    let plans = [Plan::new(0, &events), Plan::new(1, &events)];
    let cohort = engine
        .cohort(policy(true, 1, SupervisionDecision::Restart))
        .await;
    let a = engine.spawn(&plans[0], &cohort, false, true).await.unwrap();
    let b = engine.spawn(&plans[1], &cohort, true, true).await.unwrap();
    ready(&plans, 1).await;
    drop(cohort);
    assert!(a.ask(Op::Fail).await.is_err());
    ready(&plans, 2).await;
    assert_eq!(b.ask(Op::Read).await.unwrap(), 1);
    engine.shutdown().await;
});

both!(
    cancellation_preserves_actual_output_destructor_failure,
    engine,
    {
        let events = Events::default();
        let plans = [Plan::new(0, &events), Plan::new(1, &events)];
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        *plans[0].construct_gate.lock().unwrap() = Some((1, gate.clone()));
        plans[0].panic_drop_at.store(1, Ordering::SeqCst);
        let cohort = engine
            .cohort(policy(true, 2, SupervisionDecision::Restart))
            .await;
        let a = engine.spawn(&plans[0], &cohort, true, true).await.unwrap();
        let _b = engine.spawn(&plans[1], &cohort, false, true).await.unwrap();
        ready(&plans, 1).await;
        assert!(a.ask(Op::Fail).await.is_err());
        gate.entered().await;
        let mut stopped = cohort.stop();
        assert!(futures::poll!(&mut stopped).is_pending());
        gate.release();
        let error = tokio::time::timeout(WAIT, stopped)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("deliberate root destructor failure"),
            "{error}"
        );
        assert!(
            plans
                .iter()
                .all(|plan| plan.inits.load(Ordering::SeqCst) == 1)
        );
        assert!(plans.iter().all(|plan| plan.drops.load(Ordering::SeqCst) == plan.builds.load(Ordering::SeqCst)));
        assert!(
            cohort
                .stop()
                .await
                .unwrap_err()
                .to_string()
                .contains("deliberate root destructor failure")
        );
        engine.shutdown().await;
    }
);

both!(
    real_deadline_does_not_publish_actual_retirement_early,
    original,
    {
        let engine = Engine::with_timeout(
            matches!(original, Engine::Actix(_)),
            Duration::from_millis(150),
        )
        .await;
        let events = Events::default();
        let plans = [Plan::new(0, &events), Plan::new(1, &events)];
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        *plans[0].construct_gate.lock().unwrap() = Some((1, gate.clone()));
        let cohort = engine
            .cohort(policy(true, 2, SupervisionDecision::Restart))
            .await;
        let a = engine.spawn(&plans[0], &cohort, true, true).await.unwrap();
        let b = engine.spawn(&plans[1], &cohort, false, true).await.unwrap();
        ready(&plans, 1).await;
        assert!(a.ask(Op::Fail).await.is_err());
        gate.entered().await;
        let error = tokio::time::timeout(WAIT, a.stop_result())
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(error, ActorError::Timeout | ActorError::TimeoutDetail(_)),
            "{error:?}"
        );
        assert!(!cohort.members().is_empty());
        let mut actual = cohort.stop();
        assert!(futures::poll!(&mut actual).is_pending());
        assert_eq!(plans[0].drops.load(Ordering::SeqCst), 1);
        gate.release();
        tokio::time::timeout(WAIT, actual).await.unwrap().unwrap();
        assert!(cohort.members().is_empty());
        assert!(
            plans
                .iter()
                .all(|plan| plan.inits.load(Ordering::SeqCst) == 1)
        );
        assert!(b.ask(Op::Read).await.is_err());
        engine.shutdown().await;
        original.shutdown().await;
    }
);

struct PolicyCapture {
    gate: Arc<Gate>,
    drops: Arc<AtomicUsize>,
}
impl PolicyCapture {
    fn decide(&self) -> SupervisionDecision {
        SupervisionDecision::Restart
    }
}
impl Drop for PolicyCapture {
    fn drop(&mut self) {
        self.gate.block();
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
both!(
    policy_capture_retirement_survives_cancelled_stop_waiter,
    engine,
    {
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = PolicyCapture {
            gate: gate.clone(),
            drops: drops.clone(),
        };
        let cohort = engine
            .cohort(SupervisorStrategyType::OneForAll(OneForAllStrategy {
                max_restarts: 1,
                within: WAIT,
                decider: BasicDecisionFn::new(move |_| capture.decide()),
            }))
            .await;
        let mut first = cohort.stop();
        assert!(futures::poll!(&mut first).is_pending());
        gate.entered().await;
        drop(first);
        let mut second = cohort.stop();
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        gate.release();
        second.await.unwrap();
        cohort.stop().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        engine.shutdown().await;
    }
);

both!(group_capacity_reopens_only_after_retirement, engine, {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = PolicyCapture {
        gate: gate.clone(),
        drops: drops.clone(),
    };
    let first = engine
        .cohort(SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 0,
            within: WAIT,
            decider: BasicDecisionFn::new(move |_| capture.decide()),
        }))
        .await;
    let mut groups = vec![first];
    for _ in 1..1024 {
        groups.push(
            engine
                .cohort(policy(true, 0, SupervisionDecision::Stop))
                .await,
        );
    }
    assert!(
        engine
            .try_cohort(policy(true, 0, SupervisionDecision::Stop))
            .await
            .is_err()
    );
    let mut stopped = groups[0].stop();
    assert!(futures::poll!(&mut stopped).is_pending());
    gate.entered().await;
    assert!(
        engine
            .try_cohort(policy(true, 0, SupervisionDecision::Stop))
            .await
            .is_err()
    );
    gate.release();
    stopped.await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let replacement = engine
        .cohort(policy(true, 0, SupervisionDecision::Stop))
        .await;
    // Retaining the old handle cannot re-admit members or consume capacity.
    assert!(groups[0].members().is_empty());
    replacement.stop().await.unwrap();
    for group in groups {
        group.stop().await.unwrap();
    }
    engine.shutdown().await;
});

#[actix::test]
async fn dynamic_thread_roots_recover_on_every_scheduler() {
    use parrot::thread::SchedulingMode;
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 4,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "root-cohort".into(),
        },
    ] {
        let engine = Engine::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 2,
            shared_burst_workers_max: 0,
            default_scheduling_mode: mode,
            ..Default::default()
        }));
        let events = Events::default();
        let plans = [Plan::new(0, &events), Plan::new(1, &events)];
        let group = engine
            .cohort(policy(true, 1, SupervisionDecision::Restart))
            .await;
        let a = engine.spawn(&plans[0], &group, false, true).await.unwrap();
        let b = engine.spawn(&plans[1], &group, false, true).await.unwrap();
        ready(&plans, 1).await;
        assert!(a.ask(Op::Fail).await.is_err());
        ready(&plans, 2).await;
        assert_eq!(a.ask(Op::Read).await.unwrap(), 1);
        assert_eq!(b.ask(Op::Read).await.unwrap(), 1);
        barrier_order(&events, &[0, 1], 1);
        group.stop().await.unwrap();
        engine.shutdown().await;
    }
}

struct Capture {
    plan: Arc<Plan>,
    dropped: Arc<AtomicUsize>,
}
impl Capture {
    fn make(&self) -> Probe {
        self.plan.make()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
both!(
    unpolled_factory_inputs_retire_without_construction,
    engine,
    {
        let events = Events::default();
        let cohort = engine
            .cohort(policy(true, 1, SupervisionDecision::Restart))
            .await;
        for (id, typed) in [false, true].into_iter().enumerate() {
            let plan = Plan::new(id, &events);
            let dropped = Arc::new(AtomicUsize::new(0));
            let capture = Capture {
                plan: plan.clone(),
                dropped: dropped.clone(),
            };
            match (&engine, typed) {
                (Engine::Thread(system), true) => {
                    drop(system.spawn_typed_factory_in_cohort::<_, Op, _>(
                        move || capture.make(),
                        "/unpolled",
                        &cohort,
                    ))
                }
                (Engine::Actix(system), true) => {
                    drop(system.spawn_typed_factory_in_cohort::<_, Op, _>(
                        move || capture.make(),
                        "/unpolled",
                        &cohort,
                    ))
                }
                (Engine::Thread(system), false) => drop(system.spawn_supervised_in_cohort(
                    move || ThreadProbe(capture.make()),
                    "/unpolled",
                    Default::default(),
                    &cohort,
                )),
                (Engine::Actix(system), false) => drop(system.spawn_root_factory_in_cohort(
                    move || ActixProbe(capture.make()),
                    "/unpolled",
                    &cohort,
                )),
            }
            until(|| dropped.load(Ordering::SeqCst) == 1).await;
            assert_eq!(plan.builds.load(Ordering::SeqCst), 0);
            assert!(cohort.members().is_empty());
        }
        cohort.stop().await.unwrap();
        engine.shutdown().await;
    }
);
