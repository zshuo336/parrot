//! Guardian settings must control actual root recovery on every managed entry.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
    typed_context::TypedContextExt,
};
use parrot_api::{
    Message,
    actor::{Actor, ActorState, EmptyConfig},
    context::ActorContext,
    engine::{AsyncEngineRuntime, FactoryOptions, SpawnOptions},
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalMsgVariant, LocalTypedActorRef, LocalTypedDispatch},
    supervisor::{
        BasicDecisionFn, DefaultStrategy, OneForAllStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    system::{ActorSystem, ActorSystemConfig, GuardianConfig},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Gate {
    entered: AtomicUsize,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
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

#[derive(Default)]
struct Plan {
    builds: AtomicUsize,
    inits: AtomicUsize,
    drops: AtomicUsize,
    paths: Mutex<Vec<String>>,
    context: Mutex<Option<TypedContext>>,
    child: bool,
    gate: Option<(usize, Arc<Gate>)>,
}
impl Plan {
    fn make(self: &Arc<Self>) -> Probe {
        let actor = Probe {
            plan: self.clone(),
            generation: self.builds.fetch_add(1, Ordering::SeqCst),
            failures: 0,
        };
        if let Some((generation, gate)) = &self.gate
            && actor.generation == *generation
        {
            gate.entered.store(1, Ordering::SeqCst);
            let mut open = gate.open.lock().unwrap();
            while !*open {
                open = gate.changed.wait(open).unwrap();
            }
        }
        actor
    }
}
struct Probe {
    plan: Arc<Plan>,
    generation: usize,
    failures: usize,
}
impl Probe {
    fn initialized(&self, path: String, parent: Option<BoxedActorRef>) {
        assert!(
            parent.is_some() == self.plan.child,
            "guardian must preserve the original parent identity"
        );
        self.plan.paths.lock().unwrap().push(path);
        self.plan.inits.fetch_add(1, Ordering::SeqCst);
    }
    fn receive(&mut self, op: Op) -> ActorResult<usize> {
        match op {
            Op::Read => Ok(self.generation),
            Op::Value => Ok(self.failures),
            Op::Fail => {
                self.failures += 1;
                Err(ActorError::MessageHandlingError("guardian witness".into()))
            }
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.plan.drops.fetch_add(1, Ordering::SeqCst);
    }
}
enum Op {
    Read,
    Value,
    Fail,
}
impl Message for Op {
    type Result = usize;
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(op: Self) -> Op {
        op
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = usize;
    fn init_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.initialized(context.path().to_owned(), context.parent());
        *self.plan.context.lock().unwrap() = Some(context.clone());
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move { self.receive(op) })
    }
}
macro_rules! dynamic {
    ($name:ident, $config:ty, $context:ty) => {
        struct $name(Probe);
        impl Actor for $name {
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
                context: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                self.0
                    .initialized(context.get_self_ref().path(), context.parent());
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
                        .map(|value| Box::new(value) as BoxedMessage)
                })
            }
        }
    };
}
dynamic!(
    ThreadProbe,
    EmptyConfig,
    parrot::thread::ThreadContext<Self>
);
dynamic!(
    ActixProbe,
    EmptyConfig,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
impl parrot_api::actor::ActixEngineExt for ActixProbe {}
dynamic!(NeutralProbe, Box<dyn Any + Send>, dyn ActorContext);

struct LocalProbe {
    inner: Probe,
    owner: std::thread::ThreadId,
    _local: Rc<()>,
}
impl LocalProbe {
    fn new(plan: &Arc<Plan>) -> Self {
        Self {
            inner: plan.make(),
            owner: std::thread::current().id(),
            _local: Rc::new(()),
        }
    }
}
impl Drop for LocalProbe {
    fn drop(&mut self) {
        assert_eq!(std::thread::current().id(), self.owner);
    }
}
impl LocalMsgVariant<Op> for LocalProbe {
    fn inject(op: Op) -> Op {
        op
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl LocalTypedDispatch for LocalProbe {
    type Msg = Op;
    type Reply = usize;
    fn init<'a>(&'a mut self, context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        assert_eq!(std::thread::current().id(), self.owner);
        self.inner
            .initialized(context.path().to_owned(), context.parent());
        Box::pin(async { Ok(()) })
    }
    fn dispatch<'a>(
        &'a mut self,
        op: Op,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<usize>> {
        assert_eq!(std::thread::current().id(), self.owner);
        Box::pin(async move { self.inner.receive(op) })
    }
}
enum Reference {
    Typed(TypedActorRef<Probe, Op>),
    Dynamic(BoxedActorRef),
    Local(LocalTypedActorRef<LocalProbe, Op>),
}
impl Reference {
    async fn ask(&self, op: Op) -> ActorResult<usize> {
        match self {
            Self::Typed(reference) => reference.ask(op).await,
            Self::Dynamic(reference) => reference
                .send(Box::new(op))
                .await
                .map(|reply| *reply.downcast::<usize>().unwrap()),
            Self::Local(reference) => reference.ask(op).await,
        }
    }
}
enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
impl Engine {
    async fn new(actix: bool, guardian: GuardianConfig) -> Self {
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_guardian(
                    ActixSystemSettings {
                        arbiter_count: 4,
                        shutdown_timeout: WAIT,
                        ..Default::default()
                    },
                    guardian,
                )
                .await
                .unwrap(),
            ))
        } else {
            Self::Thread(
                ThreadActorSystem::with_guardian(
                    ThreadActorSystemConfig {
                        shared_pool_size: 4,
                        shared_burst_workers_max: 0,
                        shutdown_timeout: WAIT,
                        ..Default::default()
                    },
                    guardian,
                )
                .await
                .unwrap(),
            )
        }
    }
    fn runtime(&self) -> &dyn AsyncEngineRuntime {
        match self {
            Self::Thread(system) => system.as_ref(),
            Self::Actix(system) => system.as_ref(),
        }
    }
    fn kinds(&self) -> usize {
        if matches!(self, Self::Actix(_)) { 4 } else { 3 }
    }
    async fn spawn(&self, plan: &Arc<Plan>, kind: usize, factory: bool, path: &str) -> Reference {
        let plan = plan.clone();
        match (self, kind, factory) {
            (Self::Thread(system), 0, true) => Reference::Typed(
                system
                    .spawn_typed_factory(move || plan.make(), path)
                    .await
                    .unwrap(),
            ),
            (Self::Actix(system), 0, true) => Reference::Typed(
                system
                    .spawn_typed_factory(move || plan.make(), path)
                    .await
                    .unwrap(),
            ),
            (Self::Thread(system), 0, false) => {
                Reference::Typed(system.spawn_typed(plan.make(), path).await.unwrap())
            }
            (Self::Actix(system), 0, false) => {
                Reference::Typed(system.spawn_typed(plan.make(), path).await.unwrap())
            }
            (Self::Thread(system), 1, true) => Reference::Dynamic(Box::new(
                system
                    .spawn_supervised(
                        move || ThreadProbe(plan.make()),
                        path,
                        parrot::thread::SupervisorStrategy::Stop,
                    )
                    .await
                    .unwrap(),
            )),
            (Self::Actix(system), 1, true) => Reference::Dynamic(
                system
                    .spawn_root_factory(move || ActixProbe(plan.make()))
                    .await
                    .unwrap(),
            ),
            (Self::Thread(system), 1, false) => Reference::Dynamic(Box::new(
                system
                    .spawn_at(ThreadProbe(plan.make()), path, None, Default::default())
                    .await
                    .unwrap(),
            )),
            (Self::Actix(system), 1, false) => Reference::Dynamic(
                system
                    .spawn_root_typed(ActixProbe(plan.make()), EmptyConfig)
                    .await
                    .unwrap(),
            ),
            (_, 2, true) => Reference::Dynamic(
                self.runtime()
                    .factory_runtime()
                    .unwrap()
                    .spawn_factory(
                        Box::new(move || Box::new(NeutralProbe(plan.make()))),
                        FactoryOptions {
                            spawn: SpawnOptions::at(path),
                            max_restarts: 99,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap(),
            ),
            (_, 2, false) => Reference::Dynamic(
                self.runtime()
                    .spawn(
                        Box::new(NeutralProbe(plan.make())),
                        Box::new(()),
                        SpawnOptions::at(path),
                    )
                    .await
                    .unwrap(),
            ),
            (Self::Actix(system), 3, true) => Reference::Local(
                system
                    .spawn_local_typed_factory(move || LocalProbe::new(&plan), path)
                    .await
                    .unwrap(),
            ),
            (Self::Actix(system), 3, false) => Reference::Local(
                system
                    .spawn_local_typed(move || LocalProbe::new(&plan), path)
                    .await
                    .unwrap(),
            ),
            _ => unreachable!(),
        }
    }
    async fn shutdown(&self) {
        // A failed root can retire before shutdown snapshots it, or still own
        // its original failure receipt. Neither case permits a new timeout.
        if let Err(error) = self.runtime().shutdown().await {
            assert!(error.to_string().contains("guardian witness"), "{error}");
        }
    }
}
fn all(maximum: u32) -> GuardianConfig {
    GuardianConfig {
        max_restarts: maximum,
        restart_window: Duration::from_secs(60),
        supervision_strategy: SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 99,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        }),
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("guardian contract did not reach its real terminal state");
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
async fn shared_budget(actix: bool) {
    let engine = Engine::new(actix, all(2)).await;
    let plans: Vec<_> = (0..engine.kinds())
        .map(|_| Arc::new(Plan::default()))
        .collect();
    let mut roots = Vec::new();
    for (kind, plan) in plans.iter().enumerate() {
        roots.push(
            engine
                .spawn(plan, kind, true, &format!("/guardian-{kind}"))
                .await,
        );
    }
    for root in &roots {
        assert_eq!(root.ask(Op::Read).await.unwrap(), 0);
    }
    for epoch in 1..=2 {
        assert!(roots[epoch % roots.len()].ask(Op::Fail).await.is_err());
        until(|| {
            plans
                .iter()
                .all(|plan| plan.inits.load(Ordering::SeqCst) == epoch + 1)
        })
        .await;
        for root in &roots {
            assert_eq!(root.ask(Op::Read).await.unwrap(), epoch);
        }
    }
    assert!(roots[0].ask(Op::Fail).await.is_err());
    until(|| {
        plans
            .iter()
            .all(|plan| plan.drops.load(Ordering::SeqCst) == 3)
    })
    .await;
    for (root, plan) in roots.iter().zip(&plans) {
        assert!(root.ask(Op::Read).await.is_err());
        assert_eq!(
            plan.builds.load(Ordering::SeqCst),
            3,
            "top-level guardian budget was ignored"
        );
        let paths = plan.paths.lock().unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths.iter().all(|path| path == &paths[0]));
    }
    engine.shutdown().await;
}
both!(
    one_for_all_uses_one_budget_across_all_root_entries,
    shared_budget
);

async fn mixed_instance(actix: bool) {
    for kind in 0..if actix { 4 } else { 3 } {
        let engine = Engine::new(actix, all(2)).await;
        let factory = Arc::new(Plan::default());
        let instance = Arc::new(Plan::default());
        let a = engine.spawn(&factory, 0, true, "/repeatable").await;
        let b = engine.spawn(&instance, kind, false, "/instance").await;
        assert_eq!(a.ask(Op::Read).await.unwrap(), 0);
        assert_eq!(b.ask(Op::Read).await.unwrap(), 0);
        assert!(a.ask(Op::Fail).await.is_err());
        until(|| {
            factory.drops.load(Ordering::SeqCst) == 1 && instance.drops.load(Ordering::SeqCst) == 1
        })
        .await;
        assert!(b.ask(Op::Read).await.is_err());
        assert_eq!(factory.builds.load(Ordering::SeqCst), 1);
        assert_eq!(instance.builds.load(Ordering::SeqCst), 1);
        engine.shutdown().await;
    }
}
both!(
    nonrepeatable_root_stops_the_whole_selected_group,
    mixed_instance
);

async fn default_budget(actix: bool) {
    let engine = Engine::new(
        actix,
        GuardianConfig {
            max_restarts: 1,
            restart_window: Duration::from_secs(60),
            supervision_strategy: SupervisorStrategyType::Default(
                DefaultStrategy::RestartOnFailure,
            ),
        },
    )
    .await;
    for kind in 0..engine.kinds() {
        let plan = Arc::new(Plan::default());
        let root = engine
            .spawn(&plan, kind, true, &format!("/bounded-{kind}"))
            .await;
        assert_eq!(root.ask(Op::Read).await.unwrap(), 0);
        assert!(root.ask(Op::Fail).await.is_err());
        until(|| plan.inits.load(Ordering::SeqCst) == 2).await;
        assert_eq!(root.ask(Op::Read).await.unwrap(), 1);
        assert!(root.ask(Op::Fail).await.is_err());
        until(|| plan.drops.load(Ordering::SeqCst) == 2).await;
        assert!(root.ask(Op::Read).await.is_err());
        assert_eq!(plan.builds.load(Ordering::SeqCst), 2);
    }
    engine.shutdown().await;
}
both!(default_restart_obeys_the_configured_limit, default_budget);

async fn resume(actix: bool) {
    let engine = Engine::new(
        actix,
        GuardianConfig {
            supervision_strategy: SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure),
            ..Default::default()
        },
    )
    .await;
    for kind in 0..engine.kinds() {
        let plan = Arc::new(Plan::default());
        let root = engine
            .spawn(&plan, kind, false, &format!("/resume-{kind}"))
            .await;
        for expected in 1..=2 {
            assert!(root.ask(Op::Fail).await.is_err());
            assert_eq!(root.ask(Op::Value).await.unwrap(), expected);
        }
        assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
        assert_eq!(plan.inits.load(Ordering::SeqCst), 1);
        assert_eq!(plan.drops.load(Ordering::SeqCst), 0);
    }
    engine.shutdown().await;
}
both!(instance_resume_keeps_state_without_reconstruction, resume);

#[tokio::test]
async fn trait_start_uses_guardian_config_for_actual_root_failure() {
    let system = Arc::new(
        ThreadActorSystem::start(ActorSystemConfig {
            guardian_config: all(1),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    system.set_self_weak(Arc::downgrade(&system));
    let first = Arc::new(Plan::default());
    let second = Arc::new(Plan::default());
    let a_plan = first.clone();
    let b_plan = second.clone();
    let a = system
        .spawn_typed_factory(move || a_plan.make(), "/trait-a")
        .await
        .unwrap();
    let b = system
        .spawn_typed_factory(move || b_plan.make(), "/trait-b")
        .await
        .unwrap();
    assert_eq!(a.ask(Op::Read).await.unwrap(), 0);
    assert_eq!(b.ask(Op::Read).await.unwrap(), 0);
    assert!(a.ask(Op::Fail).await.is_err());
    until(|| second.inits.load(Ordering::SeqCst) == 2).await;
    assert_eq!(b.ask(Op::Read).await.unwrap(), 1);
    system.shutdown_internal().await.unwrap();
}

async fn zero_budget(actix: bool) {
    let engine = Engine::new(
        actix,
        GuardianConfig {
            restart_window: Duration::ZERO,
            ..all(0)
        },
    )
    .await;
    let mut plans = Vec::new();
    let mut roots = Vec::new();
    for kind in 0..engine.kinds() {
        let plan = Arc::new(Plan::default());
        roots.push(
            engine
                .spawn(&plan, kind, true, &format!("/zero-{kind}"))
                .await,
        );
        plans.push(plan);
    }
    for root in &roots {
        assert_eq!(root.ask(Op::Read).await.unwrap(), 0);
    }
    assert!(roots[0].ask(Op::Fail).await.is_err());
    until(|| {
        plans
            .iter()
            .all(|plan| plan.drops.load(Ordering::SeqCst) == 1)
    })
    .await;
    for (root, plan) in roots.iter().zip(&plans) {
        assert!(root.ask(Op::Read).await.is_err());
        assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    }
    engine.shutdown().await;
}
both!(zero_budget_stops_without_any_reconstruction, zero_budget);

async fn isolation(actix: bool) {
    let engine = Engine::new(actix, all(1)).await;
    let ordinary = Arc::new(Plan::default());
    let isolated = Arc::new(Plan::default());
    let isolated_source = isolated.clone();
    let (group, isolated_ref) = match &engine {
        Engine::Thread(system) => {
            let group = system
                .root_cohort(all(1).supervision_strategy)
                .await
                .unwrap();
            let reference = system
                .spawn_typed_factory_in_cohort(move || isolated_source.make(), "/isolated", &group)
                .await
                .unwrap();
            (group, reference)
        }
        Engine::Actix(system) => {
            let group = system
                .root_cohort(all(1).supervision_strategy)
                .await
                .unwrap();
            let reference = system
                .spawn_typed_factory_in_cohort(move || isolated_source.make(), "/isolated", &group)
                .await
                .unwrap();
            (group, reference)
        }
    };
    let root = engine.spawn(&ordinary, 0, true, "/ordinary").await;
    assert_eq!(root.ask(Op::Read).await.unwrap(), 0);
    assert_eq!(isolated_ref.ask(Op::Read).await.unwrap(), 0);
    assert!(root.ask(Op::Fail).await.is_err());
    until(|| ordinary.inits.load(Ordering::SeqCst) == 2).await;
    assert_eq!(root.ask(Op::Read).await.unwrap(), 1);
    assert_eq!(isolated_ref.ask(Op::Read).await.unwrap(), 0);
    assert!(isolated_ref.ask(Op::Fail).await.is_err());
    until(|| isolated.inits.load(Ordering::SeqCst) == 2).await;
    assert_eq!(isolated_ref.ask(Op::Read).await.unwrap(), 1);
    assert_eq!(root.ask(Op::Read).await.unwrap(), 1);
    group.stop().await.unwrap();
    engine.shutdown().await;
}
both!(
    explicit_cohort_is_isolated_from_automatic_guardian,
    isolation
);

async fn child_scope(actix: bool) {
    let engine = Engine::new(actix, all(1)).await;
    let parent = Arc::new(Plan::default());
    let root = engine.spawn(&parent, 0, true, "/parent").await;
    assert_eq!(root.ask(Op::Read).await.unwrap(), 0);
    let context = parent.context.lock().unwrap().clone().unwrap();
    context.set_child_strategy(all(2).supervision_strategy);
    let child = Arc::new(Plan {
        child: true,
        ..Default::default()
    });
    let source = child.clone();
    let reference = context
        .spawn_typed_child_factory(move || source.make())
        .await
        .unwrap();
    for generation in 0..2 {
        assert_eq!(reference.ask(Op::Read).await.unwrap(), generation);
        assert!(reference.ask(Op::Fail).await.is_err());
        until(|| child.inits.load(Ordering::SeqCst) == generation + 2).await;
        assert_eq!(root.ask(Op::Read).await.unwrap(), 0);
    }
    assert!(root.ask(Op::Fail).await.is_err());
    until(|| parent.inits.load(Ordering::SeqCst) == 2).await;
    assert_eq!(root.ask(Op::Read).await.unwrap(), 1);
    assert_eq!(child.drops.load(Ordering::SeqCst), 3);
    engine.shutdown().await;
}
both!(
    children_keep_their_parent_scope_and_do_not_charge_guardian,
    child_scope
);

#[actix::test]
async fn shutdown_cancels_local_preparation_and_joins_real_outputs() {
    let engine = Engine::new(true, all(2)).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let local = Arc::new(Plan {
        gate: Some((1, gate.clone())),
        ..Default::default()
    });
    let failing = Arc::new(Plan::default());
    let a = engine.spawn(&failing, 0, true, "/failure").await;
    let b = engine.spawn(&local, 3, true, "/blocked-local").await;
    assert_eq!(a.ask(Op::Read).await.unwrap(), 0);
    assert_eq!(b.ask(Op::Read).await.unwrap(), 0);
    assert!(a.ask(Op::Fail).await.is_err());
    until(|| gate.entered.load(Ordering::SeqCst) == 1).await;
    let mut shutdown = Box::pin(engine.runtime().shutdown());
    assert!(futures::poll!(&mut shutdown).is_pending());
    gate.release();
    if let Err(error) = tokio::time::timeout(WAIT, shutdown).await.unwrap() {
        assert!(error.to_string().contains("guardian witness"), "{error}");
    }
    for plan in [&local, &failing] {
        assert_eq!(plan.inits.load(Ordering::SeqCst), 1);
        assert_eq!(
            plan.builds.load(Ordering::SeqCst),
            plan.drops.load(Ordering::SeqCst)
        );
    }
}

async fn invalid_window(actix: bool) {
    let config = GuardianConfig {
        restart_window: Duration::ZERO,
        ..all(1)
    };
    let error = if actix {
        ActixActorSystem::with_guardian(Default::default(), config)
            .await
            .err()
            .unwrap()
    } else {
        ThreadActorSystem::with_guardian(Default::default(), config)
            .await
            .err()
            .unwrap()
    };
    assert!(
        error.to_string().contains("guardian restart_window"),
        "{error}"
    );
}
both!(nonzero_budget_requires_positive_window, invalid_window);

struct Capture(Arc<AtomicUsize>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn captured_policy(drops: &Arc<AtomicUsize>) -> GuardianConfig {
    let capture = Capture(drops.clone());
    let mut config = all(1);
    if let SupervisorStrategyType::OneForAll(strategy) = &mut config.supervision_strategy {
        strategy.decider = BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Restart
        });
    }
    config
}
async fn policy_disposal(actix: bool) {
    let drops = Arc::new(AtomicUsize::new(0));
    if actix {
        drop(ActixActorSystem::with_guardian(
            Default::default(),
            captured_policy(&drops),
        ));
    } else {
        drop(ThreadActorSystem::with_guardian(
            Default::default(),
            captured_policy(&drops),
        ));
    }
    until(|| drops.load(Ordering::SeqCst) == 1).await;
    let engine = Engine::new(actix, captured_policy(&drops)).await;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    engine.runtime().shutdown().await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
both!(
    unpolled_constructor_and_shutdown_retire_policy_captures,
    policy_disposal
);
