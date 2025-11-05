//! Static factory children own real generations and participate in cohort barriers.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    Message,
    errors::ActorError,
    supervisor::{
        BasicDecisionFn, OneForAllStrategy, OneForOneStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
impl Engine {
    async fn new(actix: bool, workers: usize, timeout: Duration) -> Self {
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_settings(ActixSystemSettings {
                    arbiter_count: workers,
                    shutdown_timeout: timeout,
                    ..Default::default()
                })
                .await
                .unwrap(),
            ))
        } else {
            Self::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
                shared_pool_size: workers,
                shared_burst_workers_max: 0,
                shutdown_timeout: timeout,
                ..Default::default()
            }))
        }
    }
    async fn root(&self) -> (TypedActorRef<Parent, Context>, Arc<AtomicUsize>) {
        let drops = Arc::new(AtomicUsize::new(0));
        let actor = Parent(drops.clone());
        let reference = match self {
            Self::Thread(s) => s.spawn_typed(actor, "/root").await.unwrap(),
            Self::Actix(s) => s.spawn_typed(actor, "/root").await.unwrap(),
        };
        (reference, drops)
    }
    fn count(&self) -> usize {
        match self {
            Self::Thread(s) => s.actor_count(),
            Self::Actix(s) => s.status().active_actors,
        }
    }
    async fn shutdown(&self) {
        match self {
            Self::Thread(s) => s.shutdown_internal().await.unwrap(),
            Self::Actix(s) => s.as_ref().clone().shutdown().await.unwrap(),
        }
    }
}
struct Context;
impl Message for Context {
    type Result = TypedContext;
}
struct Parent(Arc<AtomicUsize>);
impl Drop for Parent {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotMsgVariant<Parent> for Context {
    fn inject(x: Self) -> Self {
        x
    }
    fn extract(x: TypedContext) -> ActorResult<TypedContext> {
        Ok(x)
    }
}
impl ParrotTypedDispatch for Parent {
    type Msg = Context;
    type Reply = TypedContext;
    fn dispatch(&mut self, _: Context) -> BoxedFuture<'_, ActorResult<TypedContext>> {
        Box::pin(async { Err(ActorError::InternalError("managed context required".into())) })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        _: Context,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move { Ok(ctx.clone()) })
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
        let (opened, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(8), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out() || *opened, "test gate not released");
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
type Events = Arc<Mutex<Vec<(usize, usize, &'static str)>>>;
struct Stats {
    id: usize,
    built: AtomicUsize,
    init: AtomicUsize,
    drops: AtomicUsize,
    hooks: AtomicUsize,
    events: Events,
}
impl Stats {
    fn new(id: usize, events: &Events) -> Arc<Self> {
        Arc::new(Self {
            id,
            built: AtomicUsize::new(0),
            init: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            hooks: AtomicUsize::new(0),
            events: events.clone(),
        })
    }
    fn event(&self, generation: usize, event: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((self.id, generation, event));
    }
}
#[derive(Default)]
struct Hold {
    entered: Notify,
    cancelled: AtomicUsize,
}
struct Pending(Arc<Hold>);
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}
struct Child {
    stats: Arc<Stats>,
    generation: usize,
    value: usize,
    drop_gate: Option<Arc<Gate>>,
    bad_drop: bool,
    bad_init: bool,
    init_hold: Option<Arc<Hold>>,
    expected_parent: String,
}
fn instance(stats: &Arc<Stats>) -> Child {
    let generation = stats.built.fetch_add(1, Ordering::SeqCst);
    stats.event(generation, "built");
    Child {
        stats: stats.clone(),
        generation,
        value: 0,
        drop_gate: None,
        bad_drop: false,
        bad_init: false,
        init_hold: None,
        expected_parent: "/root".into(),
    }
}
enum Op {
    Hold(Arc<Hold>),
    Read,
    Fail,
    FailWait(Arc<Notify>, Arc<Notify>),
}
struct Info {
    generation: usize,
    value: usize,
    context: TypedContext,
}
impl Message for Op {
    type Result = Info;
}
impl ParrotMsgVariant<Child> for Op {
    fn inject(x: Self) -> Self {
        x
    }
    fn extract(x: Info) -> ActorResult<Info> {
        Ok(x)
    }
}
impl ParrotTypedDispatch for Child {
    type Msg = Op;
    type Reply = Info;
    fn dispatch(&mut self, _: Op) -> BoxedFuture<'_, ActorResult<Info>> {
        Box::pin(async { Err(ActorError::InternalError("managed context required".into())) })
    }
    fn init_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(ctx.parent().unwrap().path(), self.expected_parent);
            self.stats.init.fetch_add(1, Ordering::SeqCst);
            self.stats.event(self.generation, "init");
            if let Some(hold) = &self.init_hold {
                let _pending = Pending(hold.clone());
                hold.entered.notify_one();
                std::future::pending::<()>().await;
            }
            if self.bad_init {
                return Err(ActorError::InitializationError(
                    "static child init failed".into(),
                ));
            }
            Ok(())
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.stats.hooks.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        op: Op,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<Info>> {
        Box::pin(async move {
            match op {
                Op::Hold(hold) => {
                    let _pending = Pending(hold.clone());
                    hold.entered.notify_one();
                    std::future::pending().await
                }
                Op::Read => Ok(Info {
                    generation: self.generation,
                    value: self.value,
                    context: ctx.clone(),
                }),
                Op::Fail | Op::FailWait(..) => {
                    self.value += 1;
                    if let Op::FailWait(entered, release) = op {
                        entered.notify_one();
                        release.notified().await;
                    }
                    Err(ActorError::MessageHandlingError(
                        "static child failure".into(),
                    ))
                }
            }
        })
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.block();
        }
        self.stats.event(self.generation, "drop");
        self.stats.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.bad_drop, "static child destructor panic");
    }
}
type Ref = TypedActorRef<Child, Op>;
async fn spawn(ctx: &TypedContext, stats: &Arc<Stats>) -> Ref {
    let stats = stats.clone();
    ctx.spawn_typed_child_factory(move || instance(&stats))
        .await
        .unwrap()
}
fn policy(
    all: bool,
    maximum: u32,
    decision: SupervisionDecision,
    calls: &Arc<AtomicUsize>,
) -> SupervisorStrategyType {
    let calls = calls.clone();
    let decider = BasicDecisionFn::new(move |error| {
        assert!(matches!(error, ActorError::MessageHandlingError(_)));
        calls.fetch_add(1, Ordering::SeqCst);
        decision
    });
    if all {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: maximum,
            within: Duration::from_secs(30),
            decider,
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: maximum,
            within: Duration::from_secs(30),
            decider,
        })
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
fn fixtures() -> (Events, Arc<Stats>, Arc<Stats>, Arc<AtomicUsize>) {
    let events = Events::default();
    let a = Stats::new(0, &events);
    let b = Stats::new(1, &events);
    (events, a, b, Arc::new(AtomicUsize::new(0)))
}

#[actix::test]
async fn inherited_restart_preserves_child_identity_queue_and_generation_isolation() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, stats, _, calls) = fixtures();
        ctx.set_child_strategy(policy(false, 1, SupervisionDecision::Restart, &calls));
        let child = spawn(&ctx, &stats).await;
        let old = child.ask(Op::Read).await.unwrap().context;
        let path = child.path().to_owned();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let failed = child.ask(Op::FailWait(entered.clone(), release.clone()));
        tokio::pin!(failed);
        assert!(futures::poll!(&mut failed).is_pending());
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let queued = child.ask(Op::Read);
        tokio::pin!(queued);
        assert!(futures::poll!(&mut queued).is_pending());
        release.notify_one();
        assert!(failed.await.is_err());
        let current = queued.await.unwrap();
        assert_eq!((current.generation, current.value), (1, 0));
        assert_eq!(child.path(), path);
        assert_eq!(ctx.children().len(), 1);
        old.stop();
        assert_eq!(child.ask(Op::Read).await.unwrap().generation, 1);
        let rejected = stats.clone();
        assert!(
            old.spawn_typed_child_factory::<_, Op, _>(move || instance(&rejected))
                .await
                .is_err()
        );
        assert_eq!(stats.built.load(Ordering::SeqCst), 2);
        assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
        assert!(child.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(stats.built.load(Ordering::SeqCst), 2);
        assert_eq!(stats.drops.load(Ordering::SeqCst), 2);
        assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
        root.stop().await.unwrap();
        assert_eq!(e.count(), 0);
        e.shutdown().await;
    }
}

#[actix::test]
async fn explicit_stop_policy_overrides_inherited_restart_without_reconstruction() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, _, parent_calls) = fixtures();
        ctx.set_child_strategy(policy(
            false,
            3,
            SupervisionDecision::Restart,
            &parent_calls,
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let made = a.clone();
        let child = ctx
            .spawn_typed_child_factory_with_strategy::<_, Op, _>(
                move || instance(&made),
                policy(false, 3, SupervisionDecision::Stop, &calls),
            )
            .await
            .unwrap();
        assert!(child.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(a.drops.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(parent_calls.load(Ordering::SeqCst), 0);
        root.ask(Context).await.unwrap();
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}

#[actix::test]
async fn one_for_all_stop_retires_factory_siblings_and_leaves_parent_usable() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 3, SupervisionDecision::Stop, &calls));
        let first = spawn(&ctx, &a).await;
        let second = spawn(&ctx, &b).await;
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(a.drops.load(Ordering::SeqCst), 1);
        assert_eq!(b.drops.load(Ordering::SeqCst), 1);
        root.ask(Context).await.unwrap();
        root.stop().await.unwrap();
        assert_eq!(e.count(), 0);
        e.shutdown().await;
    }
}

#[actix::test]
async fn child_factory_escalation_reaches_real_parent_after_input_retirement() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, parent_drops) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(false, 1, SupervisionDecision::Escalate, &calls));
        let first = spawn(&ctx, &a).await;
        let second = spawn(&ctx, &b).await;
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        until(|| e.count() == 0).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(a.drops.load(Ordering::SeqCst), 1);
        assert_eq!(b.drops.load(Ordering::SeqCst), 1);
        assert_eq!(parent_drops.load(Ordering::SeqCst), 1);
        assert!(root.stop().await.is_err());
        e.shutdown().await;
    }
}

#[actix::test]
async fn one_for_all_reconstructs_whole_static_cohort_with_one_constructor_permit() {
    for actix in [false, true] {
        let e = Engine::new(actix, 1, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (events, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let second = spawn(&ctx, &b).await;
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        until(|| a.init.load(Ordering::SeqCst) == 2 && b.init.load(Ordering::SeqCst) == 2).await;
        assert_eq!(first.ask(Op::Read).await.unwrap().generation, 1);
        assert_eq!(second.ask(Op::Read).await.unwrap().generation, 1);
        assert_eq!(ctx.children().len(), 2);
        {
            let events = events.lock().unwrap();
            let last_retired = events
                .iter()
                .rposition(|(_, g, e)| *g == 0 && *e == "drop")
                .unwrap();
            let first_built = events
                .iter()
                .position(|(_, g, e)| *g == 1 && *e == "built")
                .unwrap();
            let last_built = events
                .iter()
                .rposition(|(_, g, e)| *g == 1 && *e == "built")
                .unwrap();
            let first_init = events
                .iter()
                .position(|(_, g, e)| *g == 1 && *e == "init")
                .unwrap();
            assert!(
                last_retired < first_built && last_built < first_init,
                "{events:?}"
            );
        }
        assert!(second.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        assert_eq!(a.built.load(Ordering::SeqCst), 2);
        assert_eq!(b.built.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        root.stop().await.unwrap();
        assert_eq!(e.count(), 0);
        e.shutdown().await;
    }
}

#[actix::test]
async fn mixed_static_instance_factory_cohort_stops_without_partial_replacement() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 3, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let second = ctx.spawn_typed_child::<_, Op>(instance(&b)).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(b.built.load(Ordering::SeqCst), 1);
        assert_eq!(a.drops.load(Ordering::SeqCst), 1);
        assert_eq!(b.drops.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}

#[actix::test]
async fn cohort_waits_for_actual_old_instance_drop_before_any_reconstruction() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let made = a.clone();
        let first = ctx
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let mut actor = instance(&made);
                if actor.generation == 0 {
                    actor.drop_gate = Some(blocked.clone());
                }
                actor
            })
            .await
            .unwrap();
        let second = spawn(&ctx, &b).await;
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(second.ask(Op::Fail).await.is_err());
        gate.entered().await;
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(b.built.load(Ordering::SeqCst), 1);
        assert_eq!(e.count(), 3);
        assert_eq!(ctx.children().len(), 2);
        until(|| b.drops.load(Ordering::SeqCst) == 1).await;
        let queued = second.ask(Op::Read);
        tokio::pin!(queued);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut queued)
                .await
                .is_err()
        );
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(b.built.load(Ordering::SeqCst), 1);
        gate.release();
        assert_eq!(queued.await.unwrap().generation, 1);
        until(|| a.init.load(Ordering::SeqCst) == 2 && b.init.load(Ordering::SeqCst) == 2).await;
        assert_eq!(first.ask(Op::Read).await.unwrap().generation, 1);
        root.stop().await.unwrap();
        assert_eq!(e.count(), 0);
        e.shutdown().await;
    }
}

#[actix::test]
async fn failed_cohort_preparation_activates_no_partial_replacement() {
    for actix in [false, true] {
        let e = Engine::new(actix, 1, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let made = b.clone();
        let second = ctx
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let actor = instance(&made);
                assert_eq!(actor.generation, 0, "replacement constructor failed");
                actor
            })
            .await
            .unwrap();
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        assert_eq!(a.init.load(Ordering::SeqCst), 1);
        assert_eq!(b.init.load(Ordering::SeqCst), 1);
        assert_eq!(
            a.drops.load(Ordering::SeqCst),
            a.built.load(Ordering::SeqCst)
        );
        assert_eq!(
            b.drops.load(Ordering::SeqCst),
            b.built.load(Ordering::SeqCst)
        );
        let third = spawn(&ctx, &a).await;
        assert_eq!(
            third
                .ask(Op::Read)
                .await
                .unwrap()
                .context
                .parent()
                .unwrap()
                .path(),
            "/root"
        );
        root.stop().await.unwrap();
        assert_eq!(e.count(), 0);
        e.shutdown().await;
    }
}

#[actix::test]
async fn group_recovery_during_initial_construction_waits_for_unpublished_output() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        first.ask(Op::Read).await.unwrap();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let made = b.clone();
        let second = ctx
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let actor = instance(&made);
                if actor.generation == 0 {
                    blocked.block();
                }
                actor
            })
            .await
            .unwrap();
        gate.entered().await;
        assert!(first.ask(Op::Fail).await.is_err());
        until(|| a.drops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(b.drops.load(Ordering::SeqCst), 0);
        assert_eq!(b.init.load(Ordering::SeqCst), 0);
        assert_eq!(e.count(), 3);
        gate.release();
        until(|| a.init.load(Ordering::SeqCst) == 2 && b.init.load(Ordering::SeqCst) == 1).await;
        assert_eq!(second.ask(Op::Read).await.unwrap().generation, 1);
        assert_eq!(b.drops.load(Ordering::SeqCst), 1);
        root.stop().await.unwrap();
        assert_eq!(e.count(), 0);
        e.shutdown().await;
    }
}

#[actix::test]
async fn prepared_sibling_cannot_initialize_before_last_constructor_finishes() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let made = b.clone();
        let second = ctx
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let actor = instance(&made);
                if actor.generation == 1 {
                    blocked.block();
                }
                actor
            })
            .await
            .unwrap();
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        gate.entered().await;
        until(|| a.built.load(Ordering::SeqCst) == 2).await;
        let queued = first.ask(Op::Read);
        tokio::pin!(queued);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut queued)
                .await
                .is_err()
        );
        assert_eq!(a.init.load(Ordering::SeqCst), 1);
        assert_eq!(b.init.load(Ordering::SeqCst), 1);
        gate.release();
        assert_eq!(queued.await.unwrap().generation, 1);
        assert_eq!(second.ask(Op::Read).await.unwrap().generation, 1);
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}

#[actix::test]
async fn cohort_recovery_interrupts_pending_init_and_handler_without_losing_unread_requests() {
    for actix in [false, true] {
        for initialization in [false, true] {
            let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
            let (root, _) = e.root().await;
            let ctx = root.ask(Context).await.unwrap();
            let (_, a, b, calls) = fixtures();
            ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
            let first = spawn(&ctx, &a).await;
            first.ask(Op::Read).await.unwrap();
            let hold = Arc::new(Hold::default());
            let waiting = hold.clone();
            let made = b.clone();
            let second = ctx
                .spawn_typed_child_factory::<_, Op, _>(move || {
                    let mut actor = instance(&made);
                    if initialization && actor.generation == 0 {
                        actor.init_hold = Some(waiting.clone());
                    }
                    actor
                })
                .await
                .unwrap();
            let held = second.ask(Op::Hold(hold.clone()));
            tokio::pin!(held);
            if !initialization {
                second.ask(Op::Read).await.unwrap();
                assert!(futures::poll!(&mut held).is_pending());
            }
            tokio::time::timeout(Duration::from_secs(2), hold.entered.notified())
                .await
                .unwrap();
            let queued = second.ask(Op::Read);
            tokio::pin!(queued);
            assert!(futures::poll!(&mut queued).is_pending());
            assert!(first.ask(Op::Fail).await.is_err());
            assert_eq!(queued.await.unwrap().generation, 1);
            if !initialization {
                assert!(held.await.is_err());
            }
            assert_eq!(hold.cancelled.load(Ordering::SeqCst), 1);
            assert_eq!(a.drops.load(Ordering::SeqCst), 1);
            assert_eq!(b.drops.load(Ordering::SeqCst), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            root.stop().await.unwrap();
            e.shutdown().await;
        }
    }
}

struct Capture {
    drops: Arc<AtomicUsize>,
    panic: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "static factory capture panic");
    }
}
#[actix::test]
async fn stop_during_group_construction_keeps_blocked_output_and_parent_owned_after_timeout() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_millis(120)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let made = b.clone();
        let captures = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            drops: captures.clone(),
            panic: false,
        };
        let second = ctx
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let _ = &capture;
                let actor = instance(&made);
                if actor.generation == 1 {
                    blocked.block();
                }
                actor
            })
            .await
            .unwrap();
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        gate.entered().await;
        assert!(root.stop().await.is_err());
        assert!(e.count() >= 2);
        assert!(
            ctx.children()
                .read_all()
                .iter()
                .any(|child| child.path() == second.path())
        );
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert_eq!(b.drops.load(Ordering::SeqCst), 1);
        assert_eq!(b.init.load(Ordering::SeqCst), 1);
        gate.release();
        until(|| e.count() == 0).await;
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(b.drops.load(Ordering::SeqCst), 2);
        assert_eq!(a.init.load(Ordering::SeqCst), 1);
        assert!(root.stop().await.is_err());
        e.shutdown().await;
    }
}

#[actix::test]
async fn cancelled_parent_stop_waiter_does_not_abandon_aborted_cohort() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let made = b.clone();
        let second = ctx
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let actor = instance(&made);
                if actor.generation == 1 {
                    blocked.block();
                }
                actor
            })
            .await
            .unwrap();
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        gate.entered().await;
        let mut stopping = Box::pin(root.stop());
        assert!(futures::poll!(&mut stopping).is_pending());
        drop(stopping);
        assert!(e.count() >= 2);
        assert!(
            ctx.children()
                .read_all()
                .iter()
                .any(|child| child.path() == second.path())
        );
        gate.release();
        until(|| e.count() == 0).await;
        assert_eq!(a.init.load(Ordering::SeqCst), 1);
        assert_eq!(b.init.load(Ordering::SeqCst), 1);
        assert_eq!(
            a.drops.load(Ordering::SeqCst),
            a.built.load(Ordering::SeqCst)
        );
        assert_eq!(
            b.drops.load(Ordering::SeqCst),
            b.built.load(Ordering::SeqCst)
        );
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}

#[actix::test]
async fn failed_old_generation_destructor_aborts_cohort_and_preserves_its_error() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let made = a.clone();
        let first = ctx
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let mut actor = instance(&made);
                actor.bad_drop = true;
                actor
            })
            .await
            .unwrap();
        let second = spawn(&ctx, &b).await;
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(second.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        let error = first.stop().await.unwrap_err().to_string();
        assert!(error.contains("static child destructor panic"), "{error}");
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(b.built.load(Ordering::SeqCst), 1);
        assert_eq!(a.drops.load(Ordering::SeqCst), 1);
        assert_eq!(b.drops.load(Ordering::SeqCst), 1);
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}

#[actix::test]
async fn child_initialization_failure_uses_real_error_policy_and_never_activates_resume() {
    for actix in [false, true] {
        for resume in [false, true] {
            let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
            let (root, _) = e.root().await;
            let ctx = root.ask(Context).await.unwrap();
            let (_, a, _, calls) = fixtures();
            let decided = calls.clone();
            ctx.set_child_strategy(SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(10),
                decider: BasicDecisionFn::new(move |error| {
                    assert!(matches!(error, ActorError::InitializationError(_)));
                    decided.fetch_add(1, Ordering::SeqCst);
                    if resume {
                        SupervisionDecision::Resume
                    } else {
                        SupervisionDecision::Restart
                    }
                }),
            }));
            let made = a.clone();
            let child = ctx
                .spawn_typed_child_factory::<_, Op, _>(move || {
                    let mut actor = instance(&made);
                    actor.bad_init = actor.generation == 0;
                    actor
                })
                .await
                .unwrap();
            let reply = child.ask(Op::Read).await;
            if resume {
                assert!(reply.is_err());
                until(|| ctx.children().is_empty()).await;
                assert_eq!(a.built.load(Ordering::SeqCst), 1);
            } else {
                assert_eq!(reply.unwrap().generation, 1);
                assert_eq!(a.built.load(Ordering::SeqCst), 2);
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            root.stop().await.unwrap();
            assert_eq!(
                a.drops.load(Ordering::SeqCst),
                a.built.load(Ordering::SeqCst)
            );
            e.shutdown().await;
        }
    }
}

struct EventsRecipient(tokio::sync::mpsc::UnboundedSender<parrot_api::context::ActorTerminated>);
impl parrot_api::death_watch::TerminationRecipient for EventsRecipient {
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
#[actix::test]
async fn child_cohort_replacement_preserves_incoming_logical_death_watch() {
    for actix in [false, true] {
        let e = Engine::new(actix, 1, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let second = spawn(&ctx, &b).await;
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        let target = first.clone().into_dyn().death_watch().unwrap();
        let watcher = parrot_api::death_watch::DeathWatch::new(2);
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        watcher
            .set_recipient(Arc::new(EventsRecipient(send)))
            .unwrap();
        target.watch(&watcher).unwrap();
        let identity = target.id();
        assert!(second.ask(Op::Fail).await.is_err());
        until(|| a.init.load(Ordering::SeqCst) == 2 && b.init.load(Ordering::SeqCst) == 2).await;
        assert_eq!(
            first.clone().into_dyn().death_watch().unwrap().id(),
            identity
        );
        assert!(receive.try_recv().is_err());
        root.stop().await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), receive.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.generation, identity);
        assert_eq!(event.path, first.path());
        assert!(receive.try_recv().is_err());
        e.shutdown().await;
    }
}

#[actix::test]
async fn rejected_child_factory_and_policy_capture_panics_are_independently_retired() {
    const FLAG: &str = "PARROT_STATIC_CHILD_FACTORY_REJECTION_PROCESS";
    const NAME: &str = "rejected_child_factory_and_policy_capture_panics_are_independently_retired";
    if std::env::var_os(FLAG).is_some() {
        for actix in [false, true] {
            let e = Engine::new(actix, 1, Duration::from_secs(3)).await;
            let (root, _) = e.root().await;
            let ctx = root.ask(Context).await.unwrap();
            root.stop().await.unwrap();
            let factory_drops = Arc::new(AtomicUsize::new(0));
            let policy_drops = Arc::new(AtomicUsize::new(0));
            let capture = Capture {
                drops: factory_drops.clone(),
                panic: true,
            };
            let policy_capture = Capture {
                drops: policy_drops.clone(),
                panic: true,
            };
            let result = ctx
                .spawn_typed_child_factory_with_strategy::<Child, Op, _>(
                    move || {
                        let _ = &capture;
                        panic!("closed scope must not invoke factory")
                    },
                    SupervisorStrategyType::OneForOne(OneForOneStrategy {
                        max_restarts: 1,
                        within: Duration::from_secs(1),
                        decider: BasicDecisionFn::new(move |_| {
                            let _ = &policy_capture;
                            SupervisionDecision::Restart
                        }),
                    }),
                )
                .await;
            assert!(result.is_err());
            assert_eq!(factory_drops.load(Ordering::SeqCst), 1);
            assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
            assert_eq!(e.count(), 0);
            e.shutdown().await;
        }
        println!("STATIC_CHILD_REJECTION_RETIRED");
        return;
    }
    use std::process::{Command, Stdio};
    struct Logs(std::path::PathBuf);
    impl Drop for Logs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let logs = Logs(std::env::temp_dir().join(format!(
        "parrot-static-child-reject-{}",
        uuid::Uuid::new_v4()
    )));
    std::fs::create_dir(&logs.0).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture"])
        .env(FLAG, "1")
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let output = std::fs::read_to_string(logs.0.join("stdout")).unwrap();
    let error = std::fs::read_to_string(logs.0.join("stderr")).unwrap();
    assert!(
        !timed_out && status.success(),
        "child failed: {status}: {output}\n{error}"
    );
    assert!(
        output.contains("STATIC_CHILD_REJECTION_RETIRED"),
        "{output}\n{error}"
    );
}

#[actix::test]
async fn grandchild_factory_escalation_rebuilds_actual_factory_parent_and_closes_old_scope() {
    for actix in [false, true] {
        let e = Engine::new(actix, 1, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(false, 1, SupervisionDecision::Restart, &calls));
        let parent = spawn(&ctx, &a).await;
        let old = parent.ask(Op::Read).await.unwrap().context;
        let child_calls = Arc::new(AtomicUsize::new(0));
        old.set_child_strategy(policy(
            false,
            0,
            SupervisionDecision::Escalate,
            &child_calls,
        ));
        let parent_path = parent.path().to_owned();
        let made = b.clone();
        let child = old
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let mut actor = instance(&made);
                actor.expected_parent = parent_path.clone();
                actor
            })
            .await
            .unwrap();
        assert_eq!(
            child
                .ask(Op::Read)
                .await
                .unwrap()
                .context
                .parent()
                .unwrap()
                .path(),
            parent.path()
        );
        assert!(child.ask(Op::Fail).await.is_err());
        until(|| a.init.load(Ordering::SeqCst) == 2).await;
        assert_eq!(parent.ask(Op::Read).await.unwrap().generation, 1);
        assert_eq!(b.drops.load(Ordering::SeqCst), 1);
        assert_eq!(a.drops.load(Ordering::SeqCst), 1);
        assert_eq!(child_calls.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(old.children().is_empty());
        old.stop();
        assert_eq!(parent.ask(Op::Read).await.unwrap().generation, 1);
        assert_eq!(e.count(), 2);
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}

#[actix::test]
async fn abort_keeps_parent_child_admission_closed_until_blocked_member_really_retires() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let made = b.clone();
        let second = ctx
            .spawn_typed_child_factory::<_, Op, _>(move || {
                let actor = instance(&made);
                if actor.generation == 1 {
                    blocked.block();
                }
                actor
            })
            .await
            .unwrap();
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        gate.entered().await;
        let _ = first.stop().await;
        root.ask(Context).await.unwrap();
        assert!(
            ctx.children()
                .read_all()
                .iter()
                .any(|child| child.path() == second.path())
        );
        let invoked = Arc::new(AtomicUsize::new(0));
        let attempted = invoked.clone();
        let extra = a.clone();
        assert!(
            ctx.spawn_typed_child_factory::<_, Op, _>(move || {
                attempted.fetch_add(1, Ordering::SeqCst);
                instance(&extra)
            })
            .await
            .is_err()
        );
        assert_eq!(invoked.load(Ordering::SeqCst), 0);
        gate.release();
        until(|| ctx.children().is_empty()).await;
        let third = spawn(&ctx, &a).await;
        third.ask(Op::Read).await.unwrap();
        root.stop().await.unwrap();
        assert_eq!(e.count(), 0);
        e.shutdown().await;
    }
}

#[actix::test]
async fn cohort_waits_for_interrupted_failure_decider_and_does_not_reuse_its_old_result() {
    for actix in [false, true] {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, _) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let made = b.clone();
        let decisions = Arc::new(AtomicUsize::new(0));
        let decided = decisions.clone();
        let second = ctx
            .spawn_typed_child_factory_with_strategy::<_, Op, _>(
                move || instance(&made),
                SupervisorStrategyType::OneForAll(OneForAllStrategy {
                    max_restarts: 1,
                    within: Duration::from_secs(30),
                    decider: BasicDecisionFn::new(move |error| {
                        assert!(matches!(error, ActorError::MessageHandlingError(_)));
                        if decided.fetch_add(1, Ordering::SeqCst) == 0 {
                            blocked.block();
                        }
                        SupervisionDecision::Restart
                    }),
                }),
            )
            .await
            .unwrap();
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        let pending = second.ask(Op::Fail);
        tokio::pin!(pending);
        assert!(futures::poll!(&mut pending).is_pending());
        gate.entered().await;
        assert!(first.ask(Op::Fail).await.is_err());
        until(|| a.drops.load(Ordering::SeqCst) == 1 && b.drops.load(Ordering::SeqCst) == 1).await;
        let queued = first.ask(Op::Read);
        tokio::pin!(queued);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut queued)
                .await
                .is_err(),
            "old failure decider still owns its generation"
        );
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(b.built.load(Ordering::SeqCst), 1);
        gate.release();
        assert!(pending.await.is_err());
        assert_eq!(queued.await.unwrap().generation, 1);
        assert_eq!(second.ask(Op::Read).await.unwrap().generation, 1);
        assert_eq!(decisions.load(Ordering::SeqCst), 1);
        assert!(second.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        assert_eq!(
            decisions.load(Ordering::SeqCst),
            2,
            "a new failure must run its own decision"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}

mod recovery_sources {
    use super::*;
    use std::error::Error;
    struct Source {
        fail: bool,
        hostile: bool,
        formats: AtomicUsize,
        sources: AtomicUsize,
        drops: AtomicUsize,
        hooks: AtomicUsize,
    }
    struct Business(Arc<Source>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("RecoveryBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(!self.0.hostile, "recovery invoked unknown business Display");
            f.write_str("original recovery descendant error")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("recovery traversed unknown business source");
        }
    }
    struct Leaf(Arc<Source>);
    impl Drop for Leaf {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl ParrotTypedDispatch for Leaf {
        type Msg = ();
        type Reply = ();
        fn dispatch(&mut self, _: ()) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.0.hooks.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self.0.fail {
                    Err(ActorError::Other(anyhow::Error::new(Business(
                        self.0.clone(),
                    ))))
                } else {
                    Ok(())
                }
            })
        }
    }
    struct Ping;
    impl Message for Ping {
        type Result = ();
    }
    impl ParrotMsgVariant<Leaf> for Ping {
        fn inject(_: Self) {}
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    fn original(error: &ActorError, source: &Arc<Source>) -> usize {
        let mut current = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let Some(error) = current else { break };
            if let Some(business) = error.downcast_ref::<Business>() {
                assert!(Arc::ptr_eq(&business.0, source));
                return business as *const Business as usize;
            }
            current = error.source();
        }
        panic!("recovery erased the original business cause");
    }
    async fn subtree(actix: bool, all: bool, fail: bool, hostile: bool) {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, root_drops) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(all, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let second = spawn(&ctx, &b).await;
        let first_context = first.ask(Op::Read).await.unwrap().context;
        second.ask(Op::Read).await.unwrap();
        let source = Arc::new(Source {
            fail,
            hostile,
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            hooks: AtomicUsize::new(0),
        });
        let leaf = first_context
            .spawn_typed_child::<_, Ping>(Leaf(source.clone()))
            .await
            .unwrap();
        leaf.ask(Ping).await.unwrap();
        assert!(first.ask(Op::Fail).await.is_err());
        // Read either observes the new generation or the failed recovery's
        // closed mailbox. Explicit stop must not initiate this transition.
        let recovered = first.ask(Op::Read).await;
        let failures = if fail {
            assert!(recovered.is_err());
            let first = first.stop().await.unwrap_err();
            let leaf = leaf.stop().await.unwrap_err();
            root.ask(Context).await.unwrap();
            root.stop().await.unwrap();
            assert_eq!(
                a.built.load(Ordering::SeqCst),
                1,
                "failed subtree forbids replacement"
            );
            assert_eq!(
                b.built.load(Ordering::SeqCst),
                1,
                "cohort must not construct past failed retirement"
            );
            Some((first, leaf))
        } else {
            assert_eq!(recovered.unwrap().generation, 1);
            assert_eq!(
                second.ask(Op::Read).await.unwrap().generation,
                usize::from(all)
            );
            assert_eq!(a.built.load(Ordering::SeqCst), 2);
            assert_eq!(b.built.load(Ordering::SeqCst), 1 + usize::from(all));
            root.stop().await.unwrap();
            leaf.stop().await.unwrap();
            None
        };
        assert_eq!(e.count(), 0);
        e.shutdown().await;
        assert_eq!(source.hooks.load(Ordering::SeqCst), 1);
        assert_eq!(source.drops.load(Ordering::SeqCst), 1);
        assert_eq!(root_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            a.drops.load(Ordering::SeqCst),
            a.built.load(Ordering::SeqCst)
        );
        assert_eq!(
            b.drops.load(Ordering::SeqCst),
            b.built.load(Ordering::SeqCst)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if let Some((first, leaf)) = failures {
            let pointer = original(&leaf, &source);
            assert_eq!(original(&first, &source), pointer);
        }
        assert_eq!(
            source.formats.load(Ordering::SeqCst),
            0,
            "recovery ran business Display"
        );
        assert_eq!(source.sources.load(Ordering::SeqCst), 0);
    }
    fn decision_classification(error: &ActorError) {
        let mut current = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let Some(error) = current else { break };
            if let Some(error) = error.downcast_ref::<ActorError>() {
                assert!(
                    !matches!(error, ActorError::Panic(_)),
                    "ordinary decision failure was reclassified as actor panic"
                );
                if let ActorError::InternalError(message) = error
                    && message.starts_with("supervision decider failed:")
                    && message.contains("recovering_decider_failed")
                {
                    return;
                }
            }
            if let Some(error) = error.downcast_ref::<parrot::thread::error::SystemError>() {
                assert!(
                    !matches!(error, parrot::thread::error::SystemError::Panic(_)),
                    "decision error became input-disposal panic"
                );
            }
            current = error.source();
        }
        panic!("original decision error variant was lost");
    }
    async fn interrupted_decision(actix: bool, bad: bool, close_parent: bool) {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, root_drops) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, b, calls) = fixtures();
        ctx.set_child_strategy(policy(true, 1, SupervisionDecision::Restart, &calls));
        let first = spawn(&ctx, &a).await;
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let blocked = gate.clone();
        let made = b.clone();
        let decisions = Arc::new(AtomicUsize::new(0));
        let decided = decisions.clone();
        let second = ctx
            .spawn_typed_child_factory_with_strategy::<_, Op, _>(
                move || instance(&made),
                SupervisorStrategyType::OneForAll(OneForAllStrategy {
                    max_restarts: 1,
                    within: Duration::from_secs(30),
                    decider: BasicDecisionFn::new(move |_| {
                        decided.fetch_add(1, Ordering::SeqCst);
                        blocked.block();
                        assert!(!bad, "recovering_decider_failed");
                        SupervisionDecision::Restart
                    }),
                }),
            )
            .await
            .unwrap();
        first.ask(Op::Read).await.unwrap();
        second.ask(Op::Read).await.unwrap();
        let pending = second.ask(Op::Fail);
        tokio::pin!(pending);
        assert!(futures::poll!(&mut pending).is_pending());
        gate.entered().await;
        assert!(first.ask(Op::Fail).await.is_err());
        until(|| a.drops.load(Ordering::SeqCst) == 1 && b.drops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(b.built.load(Ordering::SeqCst), 1);
        let mut stopping = close_parent.then(|| Box::pin(root.stop()));
        if let Some(stopping) = &mut stopping {
            assert!(futures::poll!(stopping.as_mut()).is_pending());
        }
        drop(release);
        assert!(pending.await.is_err());
        let resumed = first.ask(Op::Read).await;
        let failures = if bad {
            assert!(resumed.is_err());
            until(|| ctx.children().is_empty()).await;
            let second = second.stop().await.unwrap_err();
            let parent =
                if let Some(stopping) = stopping {
                    Some(stopping.await.expect_err(
                        "parent already joining must retain failed decision retirement",
                    ))
                } else {
                    root.ask(Context).await.unwrap();
                    root.stop().await.unwrap();
                    None
                };
            assert_eq!(a.built.load(Ordering::SeqCst), 1);
            assert_eq!(b.built.load(Ordering::SeqCst), 1);
            Some((second, parent))
        } else {
            assert_eq!(resumed.unwrap().generation, 1);
            assert_eq!(second.ask(Op::Read).await.unwrap().generation, 1);
            assert_eq!(a.built.load(Ordering::SeqCst), 2);
            assert_eq!(b.built.load(Ordering::SeqCst), 2);
            root.stop().await.unwrap();
            None
        };
        assert_eq!(e.count(), 0);
        e.shutdown().await;
        assert_eq!(root_drops.load(Ordering::SeqCst), 1);
        assert_eq!(decisions.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            a.drops.load(Ordering::SeqCst),
            a.built.load(Ordering::SeqCst)
        );
        assert_eq!(
            b.drops.load(Ordering::SeqCst),
            b.built.load(Ordering::SeqCst)
        );
        if let Some((second, parent)) = failures {
            decision_classification(&second);
            if let Some(parent) = parent {
                decision_classification(&parent);
            }
        }
    }
    async fn direct_decision(actix: bool) {
        let e = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let (root, root_drops) = e.root().await;
        let ctx = root.ask(Context).await.unwrap();
        let (_, a, _, _) = fixtures();
        let made = a.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let child = ctx
            .spawn_typed_child_factory_with_strategy::<_, Op, _>(
                move || instance(&made),
                SupervisorStrategyType::OneForOne(OneForOneStrategy {
                    max_restarts: 1,
                    within: Duration::from_secs(30),
                    decider: BasicDecisionFn::new(move |_| -> SupervisionDecision {
                        called.fetch_add(1, Ordering::SeqCst);
                        panic!("recovering_decider_failed");
                    }),
                }),
            )
            .await
            .unwrap();
        child.ask(Op::Read).await.unwrap();
        assert!(child.ask(Op::Fail).await.is_err());
        until(|| ctx.children().is_empty()).await;
        let error = child.stop().await.unwrap_err();
        root.ask(Context).await.unwrap();
        root.stop().await.unwrap();
        assert_eq!(e.count(), 0);
        e.shutdown().await;
        assert_eq!(a.built.load(Ordering::SeqCst), 1);
        assert_eq!(a.drops.load(Ordering::SeqCst), 1);
        assert_eq!(root_drops.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        decision_classification(&error);
    }
    macro_rules! trees {($($name:ident:$actix:expr,$all:expr,$fail:expr,$hostile:expr;)*)=>{$(
        #[actix::test] async fn $name(){subtree($actix,$all,$fail,$hostile).await;}
    )*};}
    trees! {
        thread_one_healthy:false,false,false,false;
        thread_all_healthy:false,true,false,false;
        actix_one_healthy:true,false,false,false;
        actix_all_healthy:true,true,false,false;
        thread_one_source:false,false,true,false;
        thread_all_source:false,true,true,false;
        actix_one_source:true,false,true,false;
        actix_all_source:true,true,true,false;
        thread_one_hostile:false,false,true,true;
        thread_all_hostile:false,true,true,true;
        actix_one_hostile:true,false,true,true;
        actix_all_hostile:true,true,true,true;
    }
    #[actix::test]
    async fn thread_decision_failure() {
        interrupted_decision(false, true, false).await;
    }
    #[actix::test]
    async fn actix_decision_failure() {
        interrupted_decision(true, true, false).await;
    }
    #[actix::test]
    async fn thread_decision_healthy() {
        interrupted_decision(false, false, false).await;
    }
    #[actix::test]
    async fn actix_decision_healthy() {
        interrupted_decision(true, false, false).await;
    }
    #[actix::test]
    async fn thread_joining_parent_decision() {
        interrupted_decision(false, true, true).await;
    }
    #[actix::test]
    async fn actix_joining_parent_decision() {
        interrupted_decision(true, true, true).await;
    }
    #[actix::test]
    async fn thread_direct_decision() {
        direct_decision(false).await;
    }
    #[actix::test]
    async fn actix_direct_decision() {
        direct_decision(true).await;
    }
}

mod escalation_sources {
    use super::*;
    use std::error::Error;
    struct Stats {
        formats: AtomicUsize,
        sources: AtomicUsize,
        drops: AtomicUsize,
        hostile: bool,
    }
    struct Business(Arc<Stats>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("EscalationBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.0.hostile,
                "escalation invoked unknown business Display"
            );
            f.write_str("original escalated failure")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("escalation invoked unknown business source")
        }
    }
    struct Leaf(Arc<Stats>, bool);
    impl Drop for Leaf {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Fail(bool);
    impl Message for Fail {
        type Result = ();
    }
    impl ParrotMsgVariant<Leaf> for Fail {
        fn inject(x: Self) -> Self {
            x
        }
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    impl ParrotTypedDispatch for Leaf {
        type Msg = Fail;
        type Reply = ();
        fn dispatch(&mut self, fail: Fail) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                if fail.0 && self.1 {
                    Err(ActorError::Panic("known_child_failure".into()))
                } else if fail.0 {
                    Err(ActorError::Other(anyhow::Error::new(Business(
                        self.0.clone(),
                    ))))
                } else {
                    Ok(())
                }
            })
        }
    }
    struct ParentState {
        drops: AtomicUsize,
        retired_stage: AtomicUsize,
        entered: Notify,
    }
    struct StageGuard(Arc<ParentState>);
    impl Drop for StageGuard {
        fn drop(&mut self) {
            self.0.retired_stage.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Root {
        state: Arc<ParentState>,
        stage: u8,
        context: Option<tokio::sync::oneshot::Sender<TypedContext>>,
    }
    impl Drop for Root {
        fn drop(&mut self) {
            self.state.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Hold(bool);
    impl Message for Hold {
        type Result = ();
    }
    impl ParrotMsgVariant<Root> for Hold {
        fn inject(x: Self) -> Self {
            x
        }
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    impl ParrotTypedDispatch for Root {
        type Msg = Hold;
        type Reply = ();
        fn init_with_context<'a>(
            &'a mut self,
            ctx: &'a TypedContext,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                assert!(self.context.take().unwrap().send(ctx.clone()).is_ok());
                if self.stage == 0 {
                    let _guard = StageGuard(self.state.clone());
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
        fn dispatch(&mut self, hold: Hold) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                if hold.0 {
                    let _guard = StageGuard(self.state.clone());
                    self.state.entered.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    }
    fn original(error: &ActorError, state: &Arc<Stats>) -> usize {
        let mut current = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let Some(error) = current else { break };
            if let Some(business) = error.downcast_ref::<Business>() {
                assert!(Arc::ptr_eq(&business.0, state));
                return business as *const Business as usize;
            }
            current = error.source();
        }
        panic!("escalation erased original business source");
    }
    async fn run(actix: bool, stage: u8, fail: bool, hostile: bool, known: bool) {
        let engine = Engine::new(actix, 2, Duration::from_secs(3)).await;
        let state = Arc::new(ParentState {
            drops: AtomicUsize::new(0),
            retired_stage: AtomicUsize::new(0),
            entered: Notify::new(),
        });
        let (tx, rx) = tokio::sync::oneshot::channel();
        let root = Root {
            state: state.clone(),
            stage,
            context: Some(tx),
        };
        let root = match &engine {
            Engine::Thread(s) => s.spawn_typed::<_, Hold>(root, "/escalating").await.unwrap(),
            Engine::Actix(s) => s.spawn_typed::<_, Hold>(root, "/escalating").await.unwrap(),
        };
        let ctx = tokio::time::timeout(Duration::from_secs(3), rx)
            .await
            .unwrap()
            .unwrap();
        if stage != 0 {
            root.ask(Hold(false)).await.unwrap();
        }
        let mut pending = (stage == 2).then(|| Box::pin(root.ask(Hold(true))));
        if let Some(pending) = &mut pending {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
                .await
                .unwrap();
        }
        ctx.set_child_strategy(SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(30),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Escalate),
        }));
        let stats = Arc::new(Stats {
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            hostile,
        });
        let child = ctx
            .spawn_typed_child::<_, Fail>(Leaf(stats.clone(), known))
            .await
            .unwrap();
        child.ask(Fail(false)).await.unwrap();
        let receipts = if fail {
            let reply = child.ask(Fail(true)).await.unwrap_err();
            until(|| engine.count() == 0).await;
            let child = child.stop().await.unwrap_err();
            let parent = root.stop().await.unwrap_err();
            Some((reply, child, parent))
        } else {
            root.stop().await.unwrap();
            child.stop().await.unwrap();
            None
        };
        if let Some(pending) = pending {
            assert!(pending.await.is_err());
        }
        assert_eq!(engine.count(), 0);
        engine.shutdown().await;
        assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
        assert_eq!(state.drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.retired_stage.load(Ordering::SeqCst),
            usize::from(stage != 1)
        );
        if let Some((reply, child, parent)) = receipts {
            if known {
                if actix {
                    assert!(
                        matches!(&parent,ActorError::Panic(message) if message=="known_child_failure")
                    );
                } else {
                    let mut current = Some(&parent as &(dyn Error + 'static));
                    let mut found = false;
                    for _ in 0..128 {
                        let Some(error) = current else { break };
                        if let Some(parrot::thread::error::SystemError::WorkerStateError(message)) =
                            error.downcast_ref::<parrot::thread::error::SystemError>()
                        {
                            assert_eq!(message, "child escalated: Panic: known_child_failure");
                            found = true;
                            break;
                        }
                        current = error.source();
                    }
                    assert!(
                        found,
                        "known child panic must retain the established parent escalation classification"
                    );
                }
            } else {
                let pointer = original(&reply, &stats);
                assert_eq!(original(&child, &stats), pointer);
                assert_eq!(original(&parent, &stats), pointer);
            }
        }
        assert_eq!(stats.formats.load(Ordering::SeqCst), 0);
        assert_eq!(stats.sources.load(Ordering::SeqCst), 0);
    }
    macro_rules! cases {($($name:ident:$engine:expr,$stage:expr,$fail:expr,$hostile:expr;)*)=>{$(
        #[actix::test] async fn $name(){run($engine,$stage,$fail,$hostile,false).await;}
    )*};}
    cases! {
        thread_init_healthy:false,0,false,false;
        thread_idle_healthy:false,1,false,false;
        thread_dispatch_healthy:false,2,false,false;
        actix_init_healthy:true,0,false,false;
        actix_idle_healthy:true,1,false,false;
        actix_dispatch_healthy:true,2,false,false;
        thread_init_source:false,0,true,false;
        thread_idle_source:false,1,true,false;
        thread_dispatch_source:false,2,true,false;
        actix_init_source:true,0,true,false;
        actix_idle_source:true,1,true,false;
        actix_dispatch_source:true,2,true,false;
        thread_init_hostile:false,0,true,true;
        thread_idle_hostile:false,1,true,true;
        thread_dispatch_hostile:false,2,true,true;
        actix_init_hostile:true,0,true,true;
        actix_idle_hostile:true,1,true,true;
        actix_dispatch_hostile:true,2,true,true;
    }
    #[actix::test]
    async fn thread_init_known() {
        run(false, 0, true, false, true).await;
    }
    #[actix::test]
    async fn thread_idle_known() {
        run(false, 1, true, false, true).await;
    }
    #[actix::test]
    async fn thread_dispatch_known() {
        run(false, 2, true, false, true).await;
    }
    #[actix::test]
    async fn actix_init_known() {
        run(true, 0, true, false, true).await;
    }
    #[actix::test]
    async fn actix_idle_known() {
        run(true, 1, true, false, true).await;
    }
    #[actix::test]
    async fn actix_dispatch_known() {
        run(true, 2, true, false, true).await;
    }
}
