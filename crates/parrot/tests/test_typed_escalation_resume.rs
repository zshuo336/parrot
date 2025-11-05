//! Escalation decisions preserve the parent's in-flight stage when they Resume.
use futures::FutureExt;
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    Message, ParrotTypedActor,
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    typed::{TypedLifecycle, TypedReceive},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Stats {
    built: AtomicUsize,
    init: AtomicUsize,
    hooks: AtomicUsize,
    drops: AtomicUsize,
    child_drops: AtomicUsize,
    decisions: AtomicUsize,
    init_hold: Mutex<Option<Arc<Hold>>>,
    init_context: Mutex<Option<TypedContext>>,
}
pub struct Query;
impl Message for Query {
    type Result = (usize, TypedContext);
}
pub struct Spawn;
impl Message for Spawn {
    type Result = TypedActorRef<Child, FailChild>;
}
pub struct FailChild;
impl Message for FailChild {
    type Result = ();
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(FailChild))]
pub struct Child(Arc<Stats>);
impl TypedReceive<FailChild> for Child {
    fn receive_typed(&mut self, _: FailChild) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Err(ActorError::MessageHandlingError("child-escalation".into())) })
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.0.child_drops.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Query, Spawn, HoldMessage, OwnFail), lifecycle)]
struct Parent {
    stats: Arc<Stats>,
    value: usize,
}
impl Parent {
    fn new(stats: &Arc<Stats>) -> Self {
        stats.built.fetch_add(1, Ordering::SeqCst);
        Self {
            stats: stats.clone(),
            value: 0,
        }
    }
}
impl TypedLifecycle for Parent {
    fn init_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            <Self as TypedLifecycle>::init(self).await?;
            *self.stats.init_context.lock().unwrap() = Some(ctx.clone());
            let hold = self.stats.init_hold.lock().unwrap().clone();
            if let Some(hold) = hold {
                hold.wait().await;
                self.value += 10;
            }
            Ok(())
        })
    }
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.stats.init.fetch_add(1, Ordering::SeqCst);
            self.value = 10;
            Ok(())
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.stats.hooks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
impl TypedReceive<Query> for Parent {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Query,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<(usize, TypedContext)>> {
        Box::pin(async move { Ok((self.value, ctx.clone())) })
    }
}
impl TypedReceive<Spawn> for Parent {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Spawn,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<Child, FailChild>>> {
        Box::pin(async move {
            self.value += 1;
            ctx.spawn_typed_child_with_strategy(
                Child(self.stats.clone()),
                SupervisorStrategyType::OneForOne(OneForOneStrategy {
                    max_restarts: 0,
                    within: Duration::from_secs(30),
                    decider: BasicDecisionFn::new(|_| SupervisionDecision::Escalate),
                }),
            )
            .await
        })
    }
}
impl Drop for Parent {
    fn drop(&mut self) {
        self.stats.drops.fetch_add(1, Ordering::SeqCst);
    }
}
enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
impl Engine {
    async fn new(actix: bool, timeout: Duration) -> Self {
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_settings(ActixSystemSettings {
                    arbiter_count: 2,
                    shutdown_timeout: timeout,
                    ..Default::default()
                })
                .await
                .unwrap(),
            ))
        } else {
            Self::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
                shared_pool_size: 2,
                shared_burst_workers_max: 0,
                shutdown_timeout: timeout,
                ..Default::default()
            }))
        }
    }
    async fn parent(
        &self,
        stats: &Arc<Stats>,
        factory: bool,
        policy: OneForOneStrategy,
    ) -> TypedActorRef<Parent, Query> {
        let made = stats.clone();
        match (self, factory) {
            (Self::Thread(s), false) => s
                .spawn_typed_with_strategy(
                    Parent::new(stats),
                    "/parent",
                    SupervisorStrategyType::OneForOne(policy),
                )
                .await
                .unwrap(),
            (Self::Actix(s), false) => s
                .spawn_typed_with_strategy(
                    Parent::new(stats),
                    "/parent",
                    SupervisorStrategyType::OneForOne(policy),
                )
                .await
                .unwrap(),
            (Self::Thread(s), true) => s
                .spawn_typed_factory_with_strategy(move || Parent::new(&made), "/parent", policy)
                .await
                .unwrap(),
            (Self::Actix(s), true) => s
                .spawn_typed_factory_with_strategy(move || Parent::new(&made), "/parent", policy)
                .await
                .unwrap(),
        }
    }
    fn count(&self) -> usize {
        match self {
            Self::Thread(s) => s.actor_count(),
            Self::Actix(s) => s.status().active_actors,
        }
    }
    async fn shutdown(&self) {
        assert_eq!(self.count(), 0);
        match self {
            Self::Thread(s) => s.shutdown_internal().await.unwrap(),
            Self::Actix(s) => s.as_ref().clone().shutdown().await.unwrap(),
        }
    }
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn resume_policy(stats: &Arc<Stats>) -> OneForOneStrategy {
    let calls = stats.clone();
    OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(30),
        decider: BasicDecisionFn::new(move |error| {
            assert!(matches!(error,ActorError::MessageHandlingError(s) if s=="child-escalation"));
            assert_eq!(
                calls.child_drops.load(Ordering::SeqCst),
                calls.decisions.load(Ordering::SeqCst) + 1
            );
            calls.decisions.fetch_add(1, Ordering::SeqCst);
            SupervisionDecision::Resume
        }),
    }
}
async fn idle_resume(actix: bool, factory: bool) {
    let timeout = Duration::from_millis(150);
    let e = Engine::new(actix, timeout).await;
    let stats = Arc::new(Stats::default());
    let parent = e.parent(&stats, factory, resume_policy(&stats)).await;
    let target = parent.clone().into_dyn().death_watch().unwrap();
    let identity = target.id();
    let watcher = parrot_api::death_watch::DeathWatch::new(2);
    let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
    watcher.set_recipient(Arc::new(Events(send))).unwrap();
    target.watch(&watcher).unwrap();

    for expected in 1..=2 {
        let child = parent.ref_for::<Spawn>().ask(Spawn).await.unwrap();
        assert!(
            matches!(child.ask(FailChild).await,Err(ActorError::MessageHandlingError(ref s)) if s=="child-escalation")
        );
        until(|| stats.decisions.load(Ordering::SeqCst) == expected).await;
        let (value, ctx) = parent
            .ask(Query)
            .await
            .expect("Resume must retain the original usable parent");
        assert_eq!(value, 10 + expected);
        until(|| ctx.children().is_empty()).await;
        assert!(ctx.children().is_empty());
    }
    tokio::time::sleep(timeout * 2).await;
    assert_eq!(
        parent.ask(Query).await.unwrap().0,
        12,
        "completed escalation must clear its temporary deadline"
    );
    assert_eq!(
        parent.clone().into_dyn().death_watch().unwrap().id(),
        identity
    );
    assert!(
        events.try_recv().is_err(),
        "Resume must not emit logical termination"
    );
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.init.load(Ordering::SeqCst), 1);
    parent.stop().await.unwrap();
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.generation, identity);
    assert_eq!(event.path, parent.path());
    assert!(matches!(
        event.reason,
        parrot_api::supervisor::DeathReason::Normal
    ));
    assert!(events.try_recv().is_err());
    e.shutdown().await;
}
#[actix::test]
async fn thread_instance_resumes_its_child_escalation_without_reinitialization() {
    idle_resume(false, false).await;
}
#[actix::test]
async fn actix_instance_resumes_its_child_escalation_without_reinitialization() {
    idle_resume(true, false).await;
}
#[actix::test]
async fn thread_factory_resumes_its_child_escalation_without_reconstruction() {
    idle_resume(false, true).await;
}
#[actix::test]
async fn actix_factory_resumes_its_child_escalation_without_reconstruction() {
    idle_resume(true, true).await;
}

#[derive(Default)]
pub struct Hold {
    entered: tokio::sync::Notify,
    changed: tokio::sync::Notify,
    open: AtomicBool,
    started: AtomicUsize,
    completed: AtomicUsize,
    cancelled: AtomicUsize,
}
impl Hold {
    async fn wait(self: &Arc<Self>) {
        struct Guard {
            hold: Arc<Hold>,
            done: bool,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                if !self.done {
                    self.hold.cancelled.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        let mut guard = Guard {
            hold: self.clone(),
            done: false,
        };
        self.started.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                break;
            }
            changed.await;
        }
        guard.done = true;
        self.completed.fetch_add(1, Ordering::SeqCst);
    }
    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.entered.notified())
            .await
            .unwrap();
    }
}
pub struct HoldMessage(Arc<Hold>);
impl Message for HoldMessage {
    type Result = usize;
}
pub struct OwnFail;
impl Message for OwnFail {
    type Result = ();
}
impl TypedReceive<HoldMessage> for Parent {
    fn receive_typed(&mut self, msg: HoldMessage) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move {
            self.value += 100;
            msg.0.wait().await;
            self.value += 1000;
            Ok(self.value)
        })
    }
}
impl TypedReceive<OwnFail> for Parent {
    fn receive_typed(&mut self, _: OwnFail) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.value += 500;
            Err(ActorError::MessageHandlingError("parent-message".into()))
        })
    }
}
async fn child_at(ctx: &TypedContext, stats: &Arc<Stats>) -> TypedActorRef<Child, FailChild> {
    ctx.spawn_typed_child_with_strategy(
        Child(stats.clone()),
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(30),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Escalate),
        }),
    )
    .await
    .unwrap()
}
#[actix::test]
async fn escalation_resume_keeps_pending_initializer_and_really_queued_request() {
    for actix in [false, true] {
        for factory in [false, true] {
            let e = Engine::new(actix, Duration::from_secs(3)).await;
            let stats = Arc::new(Stats::default());
            let hold = Arc::new(Hold::default());
            *stats.init_hold.lock().unwrap() = Some(hold.clone());
            let parent = e.parent(&stats, factory, resume_policy(&stats)).await;
            hold.entered().await;
            let ctx = stats.init_context.lock().unwrap().clone().unwrap();
            let child = child_at(&ctx, &stats).await;
            let mut queued = Box::pin(parent.ask(Query));
            assert!(queued.as_mut().now_or_never().is_none());
            assert!(
                matches!(child.ask(FailChild).await,Err(ActorError::MessageHandlingError(ref s)) if s=="child-escalation")
            );
            until(|| stats.decisions.load(Ordering::SeqCst) == 1).await;
            assert!(
                queued.as_mut().now_or_never().is_none(),
                "Resume must not activate an incomplete init"
            );
            assert_eq!(hold.started.load(Ordering::SeqCst), 1);
            assert_eq!(hold.cancelled.load(Ordering::SeqCst), 0);
            hold.release();
            assert_eq!(queued.await.unwrap().0, 20);
            assert_eq!(hold.completed.load(Ordering::SeqCst), 1);
            assert_eq!(stats.init.load(Ordering::SeqCst), 1);
            assert_eq!(stats.built.load(Ordering::SeqCst), 1);
            parent.stop().await.unwrap();
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
            e.shutdown().await;
        }
    }
}
#[actix::test]
async fn escalation_resume_keeps_pending_handler_state_future_and_reply() {
    for actix in [false, true] {
        for factory in [false, true] {
            let e = Engine::new(actix, Duration::from_secs(3)).await;
            let stats = Arc::new(Stats::default());
            let parent = e.parent(&stats, factory, resume_policy(&stats)).await;
            let child = parent.ref_for::<Spawn>().ask(Spawn).await.unwrap();
            let hold = Arc::new(Hold::default());
            let view = parent.ref_for::<HoldMessage>();
            let mut reply = Box::pin(view.ask(HoldMessage(hold.clone())));
            assert!(reply.as_mut().now_or_never().is_none());
            hold.entered().await;
            assert!(
                matches!(child.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
            );
            until(|| stats.decisions.load(Ordering::SeqCst) == 1).await;
            assert!(reply.as_mut().now_or_never().is_none());
            assert_eq!(hold.cancelled.load(Ordering::SeqCst), 0);
            hold.release();
            assert_eq!(reply.await.unwrap(), 1111);
            assert_eq!(parent.ask(Query).await.unwrap().0, 1111);
            assert_eq!(hold.started.load(Ordering::SeqCst), 1);
            assert_eq!(hold.completed.load(Ordering::SeqCst), 1);
            parent.stop().await.unwrap();
            e.shutdown().await;
        }
    }
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
        let (open, timed) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(8), |open| {
                !*open
            })
            .unwrap();
        assert!(!timed.timed_out() || *open, "test gate not released");
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.entered.notified())
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
async fn overlapping_parent_and_child_failures_keep_independent_decision_receipts() {
    for actix in [false, true] {
        for factory in [false, true] {
            for child_stops in [false, true] {
                let e = Engine::new(actix, Duration::from_secs(5)).await;
                let stats = Arc::new(Stats::default());
                let message = Arc::new(Gate::default());
                let escalation = Arc::new(Gate::default());
                let _r1 = Release(message.clone());
                let _r2 = Release(escalation.clone());
                let parent_calls = Arc::new(AtomicUsize::new(0));
                let child_calls = Arc::new(AtomicUsize::new(0));
                let mg = message.clone();
                let eg = escalation.clone();
                let pc = parent_calls.clone();
                let cc = child_calls.clone();
                let policy = OneForOneStrategy {
                    max_restarts: 0,
                    within: Duration::from_secs(30),
                    decider: BasicDecisionFn::new(move |error| match error {
                        ActorError::MessageHandlingError(s) if s == "parent-message" => {
                            pc.fetch_add(1, Ordering::SeqCst);
                            mg.block();
                            SupervisionDecision::Resume
                        }
                        ActorError::MessageHandlingError(s) if s == "child-escalation" => {
                            cc.fetch_add(1, Ordering::SeqCst);
                            eg.block();
                            if child_stops {
                                SupervisionDecision::Stop
                            } else {
                                SupervisionDecision::Resume
                            }
                        }
                        _ => panic!("unexpected failure {error}"),
                    }),
                };
                let parent = e.parent(&stats, factory, policy).await;
                let child = parent.ref_for::<Spawn>().ask(Spawn).await.unwrap();
                let view = parent.ref_for::<OwnFail>();
                let mut reply = Box::pin(view.ask(OwnFail));
                assert!(reply.as_mut().now_or_never().is_none());
                message.entered().await;
                assert!(
                    matches!(child.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
                );
                escalation.entered().await;
                assert_eq!(parent_calls.load(Ordering::SeqCst), 1);
                assert_eq!(child_calls.load(Ordering::SeqCst), 1);
                escalation.release();
                if child_stops {
                    until(|| stats.drops.load(Ordering::SeqCst) == 1).await;
                    until(|| e.count() == 1).await;
                    assert_eq!(
                        e.count(),
                        1,
                        "pending message decider still owns retirement"
                    );
                    assert!(matches!(parent.ask(Query).await, Err(ActorError::Stopped)));
                    message.release();
                    until(|| e.count() == 0).await;
                    assert!(reply.await.is_err());
                    assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
                } else {
                    assert!(reply.as_mut().now_or_never().is_none());
                    message.release();
                    assert!(
                        matches!(reply.await,Err(ActorError::MessageHandlingError(ref s)) if s=="parent-message")
                    );
                    assert_eq!(parent.ask(Query).await.unwrap().0, 511);
                    parent.stop().await.unwrap();
                }
                assert_eq!(stats.built.load(Ordering::SeqCst), 1);
                e.shutdown().await;
            }
        }
    }
}
#[actix::test]
async fn stopping_during_escalation_decision_cancels_parent_stage_but_keeps_real_receipt() {
    for actix in [false, true] {
        for factory in [false, true] {
            for cancel_waiter in [false, true] {
                let e = Engine::new(actix, Duration::from_millis(100)).await;
                let stats = Arc::new(Stats::default());
                let gate = Arc::new(Gate::default());
                let _release = Release(gate.clone());
                let blocked = gate.clone();
                let policy = OneForOneStrategy {
                    max_restarts: 0,
                    within: Duration::from_secs(30),
                    decider: BasicDecisionFn::new(move |_| {
                        blocked.block();
                        SupervisionDecision::Resume
                    }),
                };
                let parent = e.parent(&stats, factory, policy).await;
                let child = parent.ref_for::<Spawn>().ask(Spawn).await.unwrap();
                let hold = Arc::new(Hold::default());
                let view = parent.ref_for::<HoldMessage>();
                let mut reply = Box::pin(view.ask(HoldMessage(hold.clone())));
                assert!(reply.as_mut().now_or_never().is_none());
                hold.entered().await;
                assert!(
                    matches!(child.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
                );
                gate.entered().await;
                if cancel_waiter {
                    let stopped = parent.clone();
                    let waiter = tokio::spawn(async move { stopped.stop().await });
                    until(|| hold.cancelled.load(Ordering::SeqCst) == 1).await;
                    waiter.abort();
                    assert!(waiter.await.unwrap_err().is_cancelled());
                }
                let error = parent.stop().await.unwrap_err();
                assert!(
                    error.to_string().to_lowercase().contains("timeout")
                        || error.to_string().contains("exceeded"),
                    "{error}"
                );
                assert_eq!(hold.cancelled.load(Ordering::SeqCst), 1);
                assert_eq!(e.count(), 1);
                assert!(matches!(parent.ask(Query).await, Err(ActorError::Stopped)));
                gate.release();
                until(|| e.count() == 0).await;
                assert!(reply.await.is_err());
                assert_eq!(stats.built.load(Ordering::SeqCst), 1);
                assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
                e.shutdown().await;
            }
        }
    }
}
#[actix::test]
async fn resumed_escalations_do_not_spend_later_factory_restart_budget() {
    for actix in [false, true] {
        let e = Engine::new(actix, Duration::from_secs(3)).await;
        let stats = Arc::new(Stats::default());
        let calls = stats.clone();
        let policy = OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(30),
            decider: BasicDecisionFn::new(move |error| match error {
                ActorError::MessageHandlingError(s) if s == "child-escalation" => {
                    calls.decisions.fetch_add(1, Ordering::SeqCst);
                    SupervisionDecision::Resume
                }
                ActorError::MessageHandlingError(s) if s == "parent-message" => {
                    SupervisionDecision::Restart
                }
                _ => panic!("unexpected failure {error}"),
            }),
        };
        let parent = e.parent(&stats, true, policy).await;
        for expected in 11..=12 {
            let child = parent.ref_for::<Spawn>().ask(Spawn).await.unwrap();
            assert!(
                matches!(child.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
            );
            until(|| stats.decisions.load(Ordering::SeqCst) == expected - 10).await;
            assert_eq!(parent.ask(Query).await.unwrap().0, expected);
        }
        assert!(
            matches!(parent.ref_for::<OwnFail>().ask(OwnFail).await,Err(ActorError::MessageHandlingError(ref s)) if s=="parent-message")
        );
        assert_eq!(parent.ask(Query).await.unwrap().0, 10);
        assert_eq!(stats.built.load(Ordering::SeqCst), 2);
        assert_eq!(stats.init.load(Ordering::SeqCst), 2);
        parent.stop().await.unwrap();
        assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
        e.shutdown().await;
    }
}
#[actix::test]
async fn one_for_all_resume_of_a_parent_preserves_its_actual_siblings() {
    for actix in [false, true] {
        for factory in [false, true] {
            let e = Engine::new(actix, Duration::from_secs(3)).await;
            let grand = Arc::new(Stats::default());
            let root = e
                .parent(
                    &grand,
                    false,
                    OneForOneStrategy {
                        max_restarts: 0,
                        within: Duration::from_secs(30),
                        decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
                    },
                )
                .await;
            let ctx = root.ask(Query).await.unwrap().1;
            let left = Arc::new(Stats::default());
            let right = Arc::new(Stats::default());
            let one = resume_policy(&left);
            let policy =
                SupervisorStrategyType::OneForAll(parrot_api::supervisor::OneForAllStrategy {
                    max_restarts: one.max_restarts,
                    within: one.within,
                    decider: one.decider,
                });
            let parent: TypedActorRef<Parent, Query> = if factory {
                let made = left.clone();
                ctx.spawn_typed_child_factory_with_strategy(move || Parent::new(&made), policy)
                    .await
                    .unwrap()
            } else {
                ctx.spawn_typed_child_with_strategy(Parent::new(&left), policy)
                    .await
                    .unwrap()
            };
            let sibling = ctx
                .spawn_typed_child::<_, Query>(Parent::new(&right))
                .await
                .unwrap();
            assert_eq!(sibling.ask(Query).await.unwrap().0, 10);
            let child = parent.ref_for::<Spawn>().ask(Spawn).await.unwrap();
            assert!(
                matches!(child.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
            );
            until(|| left.decisions.load(Ordering::SeqCst) == 1).await;
            assert_eq!(parent.ask(Query).await.unwrap().0, 11);
            assert_eq!(sibling.ask(Query).await.unwrap().0, 10);
            assert_eq!(left.built.load(Ordering::SeqCst), 1);
            assert_eq!(right.drops.load(Ordering::SeqCst), 0);
            root.stop().await.unwrap();
            assert_eq!(
                left.hooks.load(Ordering::SeqCst),
                1,
                "actix={actix} factory={factory} decisions={} drops={}",
                left.decisions.load(Ordering::SeqCst),
                left.drops.load(Ordering::SeqCst)
            );
            assert_eq!(right.hooks.load(Ordering::SeqCst), 1);
            e.shutdown().await;
        }
    }
}

#[actix::test]
async fn concurrent_child_escalations_coalesce_without_a_stale_deadline() {
    for actix in [false, true] {
        for factory in [false, true] {
            let timeout = Duration::from_millis(500);
            let e = Engine::new(actix, timeout).await;
            let stats = Arc::new(Stats::default());
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            let blocked = gate.clone();
            let calls = stats.clone();
            let policy = OneForOneStrategy {
                max_restarts: 0,
                within: Duration::from_secs(30),
                decider: BasicDecisionFn::new(move |_| {
                    calls.decisions.fetch_add(1, Ordering::SeqCst);
                    blocked.block();
                    SupervisionDecision::Resume
                }),
            };
            let parent = e.parent(&stats, factory, policy).await;
            let ctx = parent.ask(Query).await.unwrap().1;
            let first = child_at(&ctx, &stats).await;
            let second = child_at(&ctx, &stats).await;
            assert!(
                matches!(first.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
            );
            gate.entered().await;
            assert!(
                matches!(second.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
            );
            // Actual child retirement occurs after its escalation is published.
            // A failed ASK alone is not that barrier.
            until(|| ctx.children().is_empty()).await;
            assert_eq!(stats.child_drops.load(Ordering::SeqCst), 2);
            assert_eq!(stats.decisions.load(Ordering::SeqCst), 1);
            let mut queued = Box::pin(parent.ask(Query));
            assert!(queued.as_mut().now_or_never().is_none());
            gate.release();
            assert_eq!(queued.await.unwrap().0, 10);
            tokio::time::sleep(timeout * 2).await;
            assert_eq!(parent.ask(Query).await.unwrap().0, 10);
            assert_eq!(stats.decisions.load(Ordering::SeqCst), 1);
            // A subsequent, non-overlapping failure requires a fresh decision.
            let third = child_at(&ctx, &stats).await;
            assert!(
                matches!(third.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
            );
            until(|| stats.decisions.load(Ordering::SeqCst) == 2).await;
            assert_eq!(parent.ask(Query).await.unwrap().0, 10);
            assert_eq!(stats.built.load(Ordering::SeqCst), 1);
            parent.stop().await.unwrap();
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
            e.shutdown().await;
        }
    }
}

#[actix::test]
async fn expired_escalation_cancels_stage_and_late_resume_cannot_revive_it() {
    for actix in [false, true] {
        for factory in [false, true] {
            let e = Engine::new(actix, Duration::from_millis(100)).await;
            let stats = Arc::new(Stats::default());
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            let blocked = gate.clone();
            let policy = OneForOneStrategy {
                max_restarts: 0,
                within: Duration::from_secs(30),
                decider: BasicDecisionFn::new(move |_| {
                    blocked.block();
                    SupervisionDecision::Resume
                }),
            };
            let parent = e.parent(&stats, factory, policy).await;
            let child = parent.ref_for::<Spawn>().ask(Spawn).await.unwrap();
            let hold = Arc::new(Hold::default());
            let view = parent.ref_for::<HoldMessage>();
            let mut reply = Box::pin(view.ask(HoldMessage(hold.clone())));
            assert!(reply.as_mut().now_or_never().is_none());
            hold.entered().await;
            assert!(
                matches!(child.ask(FailChild).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child-escalation")
            );
            gate.entered().await;
            // No stop request: the decision deadline itself must cancel the stage.
            until(|| hold.cancelled.load(Ordering::SeqCst) == 1).await;
            until(|| e.count() == 1).await;
            assert!(matches!(parent.ask(Query).await, Err(ActorError::Stopped)));
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
            gate.release();
            until(|| e.count() == 0).await;
            assert!(reply.await.is_err());
            assert_eq!(stats.built.load(Ordering::SeqCst), 1);
            assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
            assert!(matches!(parent.ask(Query).await, Err(ActorError::Stopped)));
            e.shutdown().await;
        }
    }
}

struct Events(tokio::sync::mpsc::UnboundedSender<parrot_api::context::ActorTerminated>);
impl parrot_api::death_watch::TerminationRecipient for Events {
    fn deliver(
        &self,
        event: parrot_api::context::ActorTerminated,
    ) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0
                .send(event)
                .map_err(|e| ActorError::InternalError(e.to_string()))
        })
    }
}
