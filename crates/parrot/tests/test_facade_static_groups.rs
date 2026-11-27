//! Facade groups must share actual native recovery across static and erased members.
#[path = "support/isolated.rs"]
mod isolated;

use parrot::{
    actix::ActixActorSystem,
    system::{FacadeRootCohort, ParrotActorSystem},
    thread::{ThreadActorSystem, typed::SingleVariant},
};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, AsyncRootCohort, LocalActor, SpawnOptions},
    errors::ActorError,
    local_typed::LocalBoxedFuture,
    message::Message,
    supervisor::{
        BasicDecisionFn, DefaultStrategy, OneForAllStrategy, OneForOneStrategy,
        SupervisionDecision, SupervisorStrategyType,
    },
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct BlockGate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl BlockGate {
    fn wait(&self) {
        self.entered.notify_one();
        let (guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(
            *guard && !timeout.timed_out(),
            "test disposal gate was not released"
        );
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<BlockGate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Default)]
struct Plan {
    built: AtomicUsize,
    dropped: AtomicUsize,
    initialized: AtomicUsize,
    hooks: AtomicUsize,
    block_drop: Option<Arc<BlockGate>>,
    bad_drop: bool,
    observed_worker: Mutex<Option<std::thread::Thread>>,
}
struct Member {
    plan: Arc<Plan>,
    generation: usize,
    failures: usize,
}
impl Member {
    fn new(plan: &Arc<Plan>) -> Self {
        Self {
            generation: plan.built.fetch_add(1, Ordering::SeqCst),
            plan: plan.clone(),
            failures: 0,
        }
    }
    fn handle(&mut self, op: Op, path: String, root: bool) -> ActorResult<Report> {
        assert!(root, "cohort introduced a synthetic parent");
        if matches!(op, Op::ObserveWorker) {
            *self.plan.observed_worker.lock().unwrap() = Some(std::thread::current());
        }
        if matches!(op, Op::Fail) {
            self.failures += 1;
            return Err(ActorError::MessageHandlingError(
                "cohort requested failure".into(),
            ));
        }
        Ok(Report(self.generation, self.failures, path))
    }
}
impl Drop for Member {
    fn drop(&mut self) {
        if self.generation == 0
            && let Some(gate) = &self.plan.block_drop
        {
            gate.wait();
        }
        self.plan.dropped.fetch_add(1, Ordering::SeqCst);
        assert!(!self.plan.bad_drop, "cohort member disposal witness");
    }
}
#[derive(Clone, Copy)]
enum Op {
    Read,
    Fail,
    ObserveWorker,
}
#[derive(Debug, PartialEq, Eq)]
struct Report(usize, usize, String);
impl Message for Op {
    type Result = Report;
}
impl ParrotMsgVariant<Member> for Op {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(value: Report) -> ActorResult<Report> {
        Ok(value)
    }
}
impl ParrotTypedDispatch for Member {
    type Msg = Op;
    type Reply = Report;
    fn dispatch(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<Report>> {
        Box::pin(async move { self.handle(op, String::new(), true) })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        op: Op,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<Report>> {
        Box::pin(async move { self.handle(op, context.path().into(), context.parent().is_none()) })
    }
    fn init_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(context.parent().is_none());
        self.plan.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.plan.hooks.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
impl TypedReceive<Op> for Member {
    fn receive_typed(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<Report>> {
        self.dispatch(op)
    }
    fn receive_typed_with_context<'a>(
        &'a mut self,
        op: Op,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<Report>> {
        self.dispatch_with_context(op, context)
    }
}
impl Actor for Member {
    type Config = Box<dyn std::any::Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, context: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(context.parent().is_none());
        self.plan.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        op: BoxedMessage,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.handle(
                *op.downcast::<Op>().unwrap(),
                context.path().path.clone(),
                context.parent().is_none(),
            )
            .map(|reply| Box::new(reply) as BoxedMessage)
        })
    }
}
struct LocalMember {
    member: Member,
    owner: std::thread::ThreadId,
    local: Rc<()>,
}
impl LocalMember {
    fn check(&self) {
        let _ = Rc::strong_count(&self.local);
        assert_eq!(self.owner, std::thread::current().id());
    }
}
impl Drop for LocalMember {
    fn drop(&mut self) {
        self.check();
    }
}
impl LocalActor for LocalMember {
    fn init<'a>(&'a mut self, context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.check();
            assert!(context.parent().is_none());
            self.member.plan.initialized.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive<'a>(
        &'a mut self,
        op: BoxedMessage,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.check();
            tokio::task::yield_now().await;
            self.check();
            self.member
                .handle(
                    *op.downcast::<Op>().unwrap(),
                    context.path().into(),
                    context.parent().is_none(),
                )
                .map(|reply| Box::new(reply) as BoxedMessage)
        })
    }
}
async fn facade(actix: bool) -> ParrotActorSystem {
    let system = ParrotActorSystem::new(Default::default()).await.unwrap();
    register(&system, "native", actix, true).await;
    system
}
async fn register(system: &ParrotActorSystem, name: &str, actix: bool, default: bool) {
    if actix {
        system
            .register_actix_system(
                name.into(),
                ActixActorSystem::with_arbiter_count(2).await.unwrap(),
                default,
            )
            .await
            .unwrap();
    } else {
        system
            .register_thread_system(
                name.into(),
                ThreadActorSystem::shared(Default::default()),
                default,
            )
            .await
            .unwrap();
    }
}
fn all() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForAll(OneForAllStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    })
}
fn restart() -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    }
}
fn resume() -> SupervisorStrategyType {
    SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure)
}
async fn bounded(future: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(WAIT * 2, future).await.unwrap();
}
async fn shutdown(system: &ParrotActorSystem) {
    if let Err(error) = AsyncEngineRuntime::shutdown(system).await {
        assert!(
            error.to_string().contains("cohort requested failure"),
            "{error}"
        );
    }
}
async fn dynamic(reference: &BoxedActorRef, op: Op) -> ActorResult<Report> {
    reference
        .send(Box::new(op))
        .await
        .map(|value| *value.downcast::<Report>().unwrap())
}
async fn recovered(reference: &BoxedActorRef, path: &str) {
    loop {
        match dynamic(reference, Op::Read).await {
            Ok(reply) => {
                assert_eq!(reply, Report(1, 0, path.into()));
                return;
            }
            Err(ActorError::Stopped) => tokio::task::yield_now().await,
            Err(error) => panic!("unexpected replacement error: {error}"),
        }
    }
}

#[actix::test]
async fn eight_single_protocol_routes_keep_concrete_replies_and_native_paths() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let plan = Arc::new(Plan::default());
            for route in 0..8 {
                let path = format!("/single-{route}");
                let made = plan.clone();
                let factory = move || Member::new(&made);
                let actor = match route {
                    0 => {
                        system
                            .spawn_typed_single::<_, Op>(Member::new(&plan), &path)
                            .await
                    }
                    1 => {
                        system
                            .spawn_typed_single_on::<_, Op>("native", Member::new(&plan), &path)
                            .await
                    }
                    2 => {
                        system
                            .spawn_typed_single_with_strategy::<_, Op>(
                                Member::new(&plan),
                                &path,
                                resume(),
                            )
                            .await
                    }
                    3 => {
                        system
                            .spawn_typed_single_with_strategy_on::<_, Op>(
                                "native",
                                Member::new(&plan),
                                &path,
                                resume(),
                            )
                            .await
                    }
                    4 => {
                        system
                            .spawn_typed_single_factory::<_, Op, _>(factory, &path)
                            .await
                    }
                    5 => {
                        system
                            .spawn_typed_single_factory_on::<_, Op, _>("native", factory, &path)
                            .await
                    }
                    6 => {
                        system
                            .spawn_typed_single_factory_with_strategy::<_, Op, _>(
                                factory,
                                &path,
                                restart(),
                            )
                            .await
                    }
                    _ => {
                        system
                            .spawn_typed_single_factory_with_strategy_on::<_, Op, _>(
                                "native",
                                factory,
                                &path,
                                restart(),
                            )
                            .await
                    }
                }
                .unwrap();
                assert_eq!(
                    actor.ask(SingleVariant::wrap(Op::Read)).await.unwrap(),
                    Report(route, 0, path.clone())
                );
                assert!(
                    AsyncEngineRuntime::get_actor(&system, &path)
                        .await
                        .is_some()
                );
                actor.stop().await.unwrap();
            }
            shutdown(&system).await;
            assert_eq!(plan.built.load(Ordering::SeqCst), 8);
            assert_eq!(plan.dropped.load(Ordering::SeqCst), 8);
        }
    })
    .await;
}

#[actix::test]
async fn single_protocol_explicit_policies_preserve_resume_and_factory_restart_budget() {
    bounded(async {
        for actix in [false, true] {
            for named in [false, true] {
                let system = facade(actix).await;
                let plan = Arc::new(Plan::default());
                let actor = if named {
                    system
                        .spawn_typed_single_with_strategy_on::<_, Op>(
                            "native",
                            Member::new(&plan),
                            "/resume",
                            resume(),
                        )
                        .await
                } else {
                    system
                        .spawn_typed_single_with_strategy::<_, Op>(
                            Member::new(&plan),
                            "/resume",
                            resume(),
                        )
                        .await
                }
                .unwrap();
                assert!(actor.ask(SingleVariant::wrap(Op::Fail)).await.is_err());
                assert_eq!(
                    actor.ask(SingleVariant::wrap(Op::Read)).await.unwrap(),
                    Report(0, 1, "/resume".into())
                );
                actor.stop().await.unwrap();
                let made = plan.clone();
                let factory = move || Member::new(&made);
                let actor = if named {
                    system
                        .spawn_typed_single_factory_with_strategy_on::<_, Op, _>(
                            "native",
                            factory,
                            "/restart",
                            restart(),
                        )
                        .await
                } else {
                    system
                        .spawn_typed_single_factory_with_strategy::<_, Op, _>(
                            factory,
                            "/restart",
                            restart(),
                        )
                        .await
                }
                .unwrap();
                assert_eq!(actor.ask(SingleVariant::wrap(Op::Read)).await.unwrap().0, 1);
                assert!(actor.ask(SingleVariant::wrap(Op::Fail)).await.is_err());
                assert_eq!(actor.ask(SingleVariant::wrap(Op::Read)).await.unwrap().0, 2);
                assert!(actor.ask(SingleVariant::wrap(Op::Fail)).await.is_err());
                assert!(actor.ask(SingleVariant::wrap(Op::Read)).await.is_err());
                shutdown(&system).await;
                assert_eq!(plan.built.load(Ordering::SeqCst), 3);
                assert_eq!(plan.dropped.load(Ordering::SeqCst), 3);
            }
        }
    })
    .await;
}

#[actix::test]
async fn static_dynamic_and_local_members_share_real_one_for_all_barriers_and_isolation() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let group = system.native_root_cohort(all()).await.unwrap();
            let separate = system.native_root_cohort(all()).await.unwrap();
            let a = Arc::new(Plan::default());
            let gate = Arc::new(BlockGate::default());
            let _release = Release(gate.clone());
            let b = Arc::new(Plan {
                block_drop: Some(gate.clone()),
                ..Default::default()
            });
            let outsider = Arc::new(Plan::default());
            let made = a.clone();
            let typed = group
                .spawn_typed_factory::<_, Op, _>(move || Member::new(&made), "/typed")
                .await
                .unwrap();
            let made = b.clone();
            let erased = group
                .spawn_factory(
                    Box::new(move || Box::new(Member::new(&made))),
                    SpawnOptions::at("/dynamic"),
                )
                .await
                .unwrap();
            let made = outsider.clone();
            let other = separate
                .spawn_typed_factory::<_, Op, _>(move || Member::new(&made), "/outside")
                .await
                .unwrap();
            let local_plan = Arc::new(Plan::default());
            let local = if let Some(local) = group.local_runtime() {
                let made = local_plan.clone();
                Some(
                    local
                        .spawn_local_factory(
                            Box::new(move || {
                                Box::new(LocalMember {
                                    member: Member::new(&made),
                                    owner: std::thread::current().id(),
                                    local: Rc::new(()),
                                })
                            }),
                            SpawnOptions::at("/local"),
                        )
                        .await
                        .unwrap(),
                )
            } else {
                assert!(!actix);
                None
            };
            typed.ask(Op::Read).await.unwrap();
            dynamic(&erased, Op::Read).await.unwrap();
            other.ask(Op::Read).await.unwrap();
            if let Some(local) = &local {
                dynamic(local, Op::Read).await.unwrap();
            }
            assert_eq!(group.members().len(), if actix { 3 } else { 2 });
            assert!(typed.ask(Op::Fail).await.is_err());
            gate.entered.notified().await;
            assert_eq!(a.built.load(Ordering::SeqCst), 1);
            assert_eq!(b.built.load(Ordering::SeqCst), 1);
            assert_eq!(outsider.built.load(Ordering::SeqCst), 1);
            if actix {
                assert_eq!(local_plan.built.load(Ordering::SeqCst), 1);
            }
            gate.release();
            loop {
                match typed.ask(Op::Read).await {
                    Ok(reply) => {
                        assert_eq!(reply, Report(1, 0, "/typed".into()));
                        break;
                    }
                    Err(ActorError::Stopped) => tokio::task::yield_now().await,
                    Err(error) => panic!("typed replacement failed: {error}"),
                }
            }
            recovered(&erased, "/dynamic").await;
            if let Some(local) = &local {
                recovered(local, "/local").await;
            }
            assert_eq!(
                other.ask(Op::Read).await.unwrap(),
                Report(0, 0, "/outside".into())
            );
            group.stop().await.unwrap();
            separate.stop().await.unwrap();
            shutdown(&system).await;
            assert_eq!(a.dropped.load(Ordering::SeqCst), 2);
            assert_eq!(b.dropped.load(Ordering::SeqCst), 2);
            if actix {
                assert_eq!(local_plan.dropped.load(Ordering::SeqCst), 2);
            }
            assert_eq!(outsider.dropped.load(Ordering::SeqCst), 1);
        }
    })
    .await;
}

#[actix::test]
async fn instance_member_prevents_partial_group_reconstruction() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let group = system.native_root_cohort_on("native", all()).await.unwrap();
            let instance_plan = Arc::new(Plan::default());
            let factory_plan = Arc::new(Plan::default());
            let instance = group
                .spawn_typed::<_, Op>(Member::new(&instance_plan), "/instance")
                .await
                .unwrap();
            let made = factory_plan.clone();
            let factory = group
                .spawn_typed_factory::<_, Op, _>(move || Member::new(&made), "/factory")
                .await
                .unwrap();
            instance.ask(Op::Read).await.unwrap();
            factory.ask(Op::Read).await.unwrap();
            assert!(factory.ask(Op::Fail).await.is_err());
            while instance_plan.dropped.load(Ordering::SeqCst) != 1
                || factory_plan.dropped.load(Ordering::SeqCst) != 1
            {
                tokio::task::yield_now().await;
            }
            group.stop().await.unwrap();
            assert_eq!(instance_plan.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(factory_plan.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(factory_plan.built.load(Ordering::SeqCst), 1);
            assert!(instance.ask(Op::Read).await.is_err());
            shutdown(&system).await;
        }
    })
    .await;
}

#[actix::test]
async fn group_single_protocol_instances_and_factories_share_resume_policy() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let group = system.native_root_cohort(resume()).await.unwrap();
            let plan = Arc::new(Plan::default());
            let a = group
                .spawn_typed_single::<_, Op>(Member::new(&plan), "/single-instance")
                .await
                .unwrap();
            let made = plan.clone();
            let b = group
                .spawn_typed_single_factory::<_, Op, _>(
                    move || Member::new(&made),
                    "/single-factory",
                )
                .await
                .unwrap();
            assert!(a.ask(SingleVariant::wrap(Op::Fail)).await.is_err());
            assert!(b.ask(SingleVariant::wrap(Op::Fail)).await.is_err());
            assert_eq!(
                a.ask(SingleVariant::wrap(Op::Read)).await.unwrap(),
                Report(0, 1, "/single-instance".into())
            );
            assert_eq!(
                b.ask(SingleVariant::wrap(Op::Read)).await.unwrap(),
                Report(1, 1, "/single-factory".into())
            );
            assert_eq!(group.members().len(), 2);
            group.stop().await.unwrap();
            assert!(group.members().is_empty());
            assert_eq!(plan.dropped.load(Ordering::SeqCst), 2);
            shutdown(&system).await;
        }
    })
    .await;
}

#[actix::test]
async fn group_creation_captures_nested_leaf_and_survives_catalog_removal() {
    bounded(async {
        for actix in [false, true] {
            for named in [false, true] {
                let inner = Arc::new(facade(actix).await);
                let original = inner.runtime("native").unwrap();
                let system = ParrotActorSystem::new(Default::default()).await.unwrap();
                system
                    .register_runtime("nested".into(), inner.clone(), true)
                    .unwrap();
                let pending = if named {
                    system.native_root_cohort_on("nested", all())
                } else {
                    system.native_root_cohort(all())
                };
                register(&inner, "replacement", !actix, true).await;
                inner.unregister_system("native").unwrap();
                let group = pending.await.unwrap();
                let plan = Arc::new(Plan::default());
                let actor = group
                    .spawn_typed::<_, Op>(Member::new(&plan), "/captured")
                    .await
                    .unwrap();
                actor.ask(Op::Read).await.unwrap();
                assert!(original.get_actor("/captured").await.is_some());
                assert!(
                    inner
                        .runtime("replacement")
                        .unwrap()
                        .get_actor("/captured")
                        .await
                        .is_none()
                );
                group.stop().await.unwrap();
                original.shutdown().await.unwrap();
                shutdown(&system).await;
                assert_eq!(plan.dropped.load(Ordering::SeqCst), 1);
            }
        }
    })
    .await;
}

struct Capture(Arc<AtomicUsize>, bool);
impl Capture {
    fn check(&self) {
        let _ = self.0.load(Ordering::SeqCst);
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "cohort policy/capture disposal witness");
    }
}
fn captured_policy(capture: Capture) -> SupervisorStrategyType {
    SupervisorStrategyType::OneForAll(OneForAllStrategy {
        max_restarts: 1,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            capture.check();
            SupervisionDecision::Restart
        }),
    })
}

#[actix::test]
async fn unpolled_and_closed_group_inputs_retire_without_invoking_factories() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let group = system.native_root_cohort(all()).await.unwrap();
            let plan = Arc::new(Plan::default());
            drop(group.spawn_typed::<_, Op>(Member::new(&plan), "/unpolled"));
            let captures = Arc::new(AtomicUsize::new(0));
            let capture = Capture(captures.clone(), true);
            let made = plan.clone();
            drop(group.spawn_typed_factory::<_, Op, _>(
                move || {
                    capture.check();
                    Member::new(&made)
                },
                "/unpolled-factory",
            ));
            while captures.load(Ordering::SeqCst) != 1 || plan.dropped.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
            assert_eq!(plan.built.load(Ordering::SeqCst), 1);
            assert_eq!(plan.initialized.load(Ordering::SeqCst), 0);
            group.stop().await.unwrap();
            let bad = Arc::new(Plan {
                bad_drop: true,
                ..Default::default()
            });
            let error = group
                .spawn_typed::<_, Op>(Member::new(&bad), "/closed")
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("disposal"), "{error}");
            assert_eq!(bad.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(bad.initialized.load(Ordering::SeqCst), 0);
            let rejected_captures = Arc::new(AtomicUsize::new(0));
            let capture = Capture(rejected_captures.clone(), true);
            let made = plan.clone();
            let error = group
                .spawn_typed_factory::<_, Op, _>(
                    move || {
                        capture.check();
                        Member::new(&made)
                    },
                    "/closed-factory",
                )
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("disposal"), "{error}");
            assert_eq!(rejected_captures.load(Ordering::SeqCst), 1);
            assert_eq!(plan.built.load(Ordering::SeqCst), 1);
            shutdown(&system).await;
        }
    })
    .await;
}

#[actix::test]
async fn group_policy_unpolled_and_missing_engine_rejection_isolate_destructors() {
    bounded(async {
        let system = facade(false).await;
        let drops = Arc::new(AtomicUsize::new(0));
        drop(system.native_root_cohort(captured_policy(Capture(drops.clone(), true))));
        let error = system
            .native_root_cohort_on("missing", captured_policy(Capture(drops.clone(), true)))
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("disposal"), "{error}");
        while drops.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
        shutdown(&system).await;
    })
    .await;
}

#[actix::test]
async fn cancelled_group_stop_waiter_keeps_blocked_member_retirement_owned() {
    bounded(async {
        for actix in [false,true] {
            let system=facade(actix).await;
            let group=system.native_root_cohort(resume()).await.unwrap();
            let gate=Arc::new(BlockGate::default());
            let _release=Release(gate.clone());
            let plan=Arc::new(Plan {block_drop:Some(gate.clone()),..Default::default()});
            let actor=group.spawn_typed::<_,Op>(Member::new(&plan),"/blocked").await.unwrap();
            actor.ask(Op::Read).await.unwrap();
            let mut stop=group.stop();
            tokio::select! { result=&mut stop=>panic!("premature group retirement: {result:?}"), _=gate.entered.notified()=>{} }
            drop(stop);
            assert_eq!(group.members().len(),1);
            assert_eq!(plan.dropped.load(Ordering::SeqCst),0);
            gate.release();
            group.stop().await.unwrap();
            assert_eq!(plan.dropped.load(Ordering::SeqCst),1);
            assert!(group.members().is_empty());
            shutdown(&system).await;
        }
    }).await;
}

#[actix::test]
async fn facade_closure_rejects_pending_creation_even_when_captured_engine_remains_live() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let original = system.runtime("native").unwrap();
            let plan = Arc::new(Plan::default());
            let actor = system.spawn_typed::<_, Op>(Member::new(&plan), "/late-instance");
            let pending = system.native_root_cohort(all());
            system.unregister_system("native").unwrap();
            shutdown(&system).await;
            assert!(matches!(actor.await, Err(ActorError::Stopped)));
            assert!(matches!(
                pending.await,
                Err(parrot_api::system::SystemError::ShuttingDown)
            ));
            assert_eq!(plan.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(plan.initialized.load(Ordering::SeqCst), 0);
            assert!(original.get_actor("/late-instance").await.is_none());
            original.shutdown().await.unwrap();
        }
    })
    .await;
}

#[test]
fn native_facade_groups_join_static_members_after_original_host_exit() {
    const CASE: &str = "native_facade_groups_join_static_members_after_original_host_exit";
    if std::env::var("PARROT_FACADE_GROUP_HOST_CASE").as_deref() != Ok(CASE) {
        let receipt =
            std::env::temp_dir().join(format!("parrot-facade-group-host-{}", uuid::Uuid::new_v4()));
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CASE, "--nocapture"])
            .env("PARROT_FACADE_GROUP_HOST_CASE", CASE)
            .env("PARROT_FACADE_GROUP_HOST_RECEIPT", &receipt)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + WAIT * 4;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated host-exit failed: {status}");
                assert_eq!(std::fs::read_to_string(&receipt).unwrap(), CASE);
                std::fs::remove_file(receipt).unwrap();
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("isolated host-exit watchdog expired");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    for actix in [false, true] {
        for shape in 0..4 {
            let host = actix::System::new();
            let plan = Arc::new(Plan::default());
            let policies = Arc::new(AtomicUsize::new(0));
            let (system, group, reference): (_, Arc<FacadeRootCohort>, BoxedActorRef) = host
                .block_on(async {
                    let system = ParrotActorSystem::new(Default::default()).await.unwrap();
                    if actix {
                        register(&system, "native", true, true).await;
                    } else {
                        system
                            .register_thread_system(
                                "native".into(),
                                ThreadActorSystem::shared(
                                    parrot::thread::ThreadActorSystemConfig {
                                        shared_pool_size: 1,
                                        shared_burst_workers_max: 0,
                                        ..Default::default()
                                    },
                                ),
                                true,
                            )
                            .await
                            .unwrap();
                    }
                    let group = system
                        .native_root_cohort(captured_policy(Capture(policies.clone(), false)))
                        .await
                        .unwrap();
                    let made = plan.clone();
                    let reference = match shape {
                        0 => {
                            let actor = group
                                .spawn_typed::<_, Op>(Member::new(&plan), "/host-exit")
                                .await
                                .unwrap();
                            actor.ask(Op::Read).await.unwrap();
                            actor.into_dyn()
                        }
                        1 => {
                            let actor = group
                                .spawn_typed_factory::<_, Op, _>(
                                    move || Member::new(&made),
                                    "/host-exit",
                                )
                                .await
                                .unwrap();
                            actor.ask(Op::Read).await.unwrap();
                            actor.into_dyn()
                        }
                        2 => {
                            let actor = group
                                .spawn_typed_single::<_, Op>(Member::new(&plan), "/host-exit")
                                .await
                                .unwrap();
                            actor.ask(SingleVariant::wrap(Op::Read)).await.unwrap();
                            actor.into_dyn()
                        }
                        _ => {
                            let actor = group
                                .spawn_typed_single_factory::<_, Op, _>(
                                    move || Member::new(&made),
                                    "/host-exit",
                                )
                                .await
                                .unwrap();
                            actor.ask(SingleVariant::wrap(Op::Read)).await.unwrap();
                            actor.into_dyn()
                        }
                    };
                    assert_eq!(group.members().len(), 1);
                    (system, group, reference)
                });
            assert_eq!(plan.dropped.load(Ordering::SeqCst), 0);
            drop(host);
            let observer = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            observer.block_on(async {
                tokio::time::timeout(WAIT, group.stop())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(plan.dropped.load(Ordering::SeqCst), 1);
                assert_eq!(policies.load(Ordering::SeqCst), 1);
                assert!(group.members().is_empty());
                group.stop().await.unwrap();
                let stopped = tokio::time::timeout(WAIT, system.shutdown_runtime()).await.unwrap();
                if actix {
                    stopped.unwrap();
                } else {
                    let error = stopped.unwrap_err().to_string();
                    assert!(error.contains("engine \"native\": Failed during shutdown: 1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{error}");
                }
                assert!(!reference.is_alive().await);
            });
        }
    }
    let receipt = std::env::var_os("PARROT_FACADE_GROUP_HOST_RECEIPT").unwrap();
    let mut stream = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(receipt)
        .unwrap();
    std::io::Write::write_all(&mut stream, CASE.as_bytes()).unwrap();
}

fn mixed_group_after_host_exit(
    name: &str,
    mode: parrot::thread::config::SchedulingMode,
    prefix: &str,
) {
    isolated::run(name, "PARROT_MIXED_COHORT_HOST_EXIT", WAIT * 4, || {
        let host = actix::System::new();
        let plans = [Arc::new(Plan::default()), Arc::new(Plan::default())];
        let policy_drops = Arc::new(AtomicUsize::new(0));
        let (system, group, typed, erased) = host.block_on(async {
            let system = ParrotActorSystem::new(Default::default()).await.unwrap();
            let engine = ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
                default_scheduling_mode: mode,
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                ..Default::default()
            });
            system
                .register_thread_system("native".into(), engine, true)
                .await
                .unwrap();
            let group = system
                .native_root_cohort(captured_policy(Capture(policy_drops.clone(), false)))
                .await
                .unwrap();
            let plan = plans[0].clone();
            let typed = group
                .spawn_typed_factory::<_, Op, _>(move || Member::new(&plan), "/typed")
                .await
                .unwrap();
            let plan = plans[1].clone();
            let erased = group
                .spawn_factory(
                    Box::new(move || Box::new(Member::new(&plan))),
                    SpawnOptions::at("/erased"),
                )
                .await
                .unwrap();
            for generation in 0..=1 {
                assert_eq!(
                    typed.ask(Op::ObserveWorker).await.unwrap(),
                    Report(generation, 0, "/typed".into())
                );
                assert_eq!(
                    dynamic(&erased, Op::ObserveWorker).await.unwrap(),
                    Report(generation, 0, "/erased".into())
                );
                let [first, second] = plans
                    .each_ref()
                    .map(|plan| plan.observed_worker.lock().unwrap().take().unwrap());
                for thread in [&first, &second] {
                    assert!(
                        thread.name().unwrap_or_default().starts_with(prefix),
                        "expected {prefix}, got {thread:?}"
                    );
                }
                assert_eq!(first.id() == second.id(), prefix == "parrot-shard-");
                if generation == 0 {
                    assert!(typed.ask(Op::Fail).await.is_err());
                    while plans
                        .iter()
                        .any(|p| p.initialized.load(Ordering::SeqCst) < 2)
                    {
                        tokio::task::yield_now().await;
                    }
                    // Initialization precedes publication of the new generations.
                    recovered(&typed.clone().into_dyn(), "/typed").await;
                    recovered(&erased, "/erased").await;
                }
            }
            (system, group, typed, erased)
        });
        drop(host);
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        observer.block_on(bounded(async {
            use parrot_api::address::RetirementStatus;
            loop {
                match typed.retirement_status() {
                    RetirementStatus::Retired(result) => {
                        result.unwrap();
                        break;
                    }
                    RetirementStatus::Pending => tokio::task::yield_now().await,
                    RetirementStatus::Unsupported => panic!("missing typed retirement receipt"),
                }
            }
            group.stop().await.unwrap();
            group.stop().await.unwrap();
            assert!(group.members().is_empty());
            assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
            for plan in &plans {
                assert_eq!(plan.built.load(Ordering::SeqCst), 2);
                assert_eq!(plan.dropped.load(Ordering::SeqCst), 2);
            }
            for path in ["/typed", "/erased"] {
                assert!(AsyncEngineRuntime::get_actor(&system, path).await.is_none());
            }
            assert!(typed.ask(Op::Read).await.is_err());
            assert!(dynamic(&erased, Op::Read).await.is_err());
            let first = system.shutdown_runtime().await.unwrap_err().to_string();
            assert!(
                first.contains("shared task unexpectedly cancelled"),
                "{first}"
            );
            assert_eq!(
                system.shutdown_runtime().await.unwrap_err().to_string(),
                first
            );
        }));
    });
}

#[test]
fn dedicated_mixed_cohort_recovers_and_retires_after_host_exit() {
    mixed_group_after_host_exit(
        "dedicated_mixed_cohort_recovers_and_retires_after_host_exit",
        parrot::thread::config::SchedulingMode::DedicatedThread,
        "parrot-dedicated-",
    );
}

#[test]
fn sharded_mixed_cohort_recovers_and_retires_after_host_exit() {
    mixed_group_after_host_exit(
        "sharded_mixed_cohort_recovers_and_retires_after_host_exit",
        parrot::thread::config::SchedulingMode::Sharded {
            affinity_key: "cohort".into(),
        },
        "parrot-shard-",
    );
}

struct DynamicOnly(Arc<ThreadActorSystem>);
impl AsyncEngineRuntime for DynamicOnly {
    fn cohort_runtime(&self) -> Option<&dyn parrot_api::engine::AsyncCohortRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        actor: parrot_api::engine::RuntimeActor,
        config: parrot_api::engine::InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, parrot_api::system::SystemError>> {
        AsyncEngineRuntime::spawn(self.0.as_ref(), actor, config, options)
    }
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        AsyncEngineRuntime::get_actor(self.0.as_ref(), path)
    }
    fn status(&self) -> parrot_api::system::SystemStatus {
        AsyncEngineRuntime::status(self.0.as_ref())
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), parrot_api::system::SystemError>> {
        AsyncEngineRuntime::shutdown(self.0.as_ref())
    }
}
impl parrot_api::engine::AsyncCohortRuntime for DynamicOnly {
    fn root_cohort(
        &self,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, parrot_api::system::SystemError>> {
        parrot_api::engine::AsyncCohortRuntime::root_cohort(self.0.as_ref(), strategy)
    }
}

#[actix::test]
async fn external_dynamic_cohorts_remain_usable_and_native_rejection_retires_policy() {
    bounded(async {
        let system = ParrotActorSystem::new(Default::default()).await.unwrap();
        system
            .register_runtime(
                "extension".into(),
                Arc::new(DynamicOnly(ThreadActorSystem::shared(Default::default()))),
                true,
            )
            .unwrap();
        let policies = Arc::new(AtomicUsize::new(0));
        let error = system
            .native_root_cohort(captured_policy(Capture(policies.clone(), false)))
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("no native static root cohort bridge"),
            "{error}"
        );
        assert_eq!(policies.load(Ordering::SeqCst), 1);
        let error = system
            .native_root_cohort_on(
                "extension",
                captured_policy(Capture(policies.clone(), true)),
            )
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("disposal"), "{error}");
        assert_eq!(policies.load(Ordering::SeqCst), 2);
        let group = system
            .cohort_runtime()
            .unwrap()
            .root_cohort(resume())
            .await
            .unwrap();
        let plan = Arc::new(Plan::default());
        let actor = group
            .spawn(
                Box::new(Member::new(&plan)),
                Box::new(()),
                SpawnOptions::at("/external"),
            )
            .await
            .unwrap();
        assert_eq!(
            dynamic(&actor, Op::Read).await.unwrap(),
            Report(0, 0, "/external".into())
        );
        group.stop().await.unwrap();
        assert_eq!(plan.dropped.load(Ordering::SeqCst), 1);
        shutdown(&system).await;
    })
    .await;
}
