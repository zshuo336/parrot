//! Local factory generations obey real parent and cohort ownership boundaries.
use parrot::{actix::ActixActorSystem, typed_context::TypedContextExt};
use parrot_api::{
    Message, ParrotTypedActor,
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalTypedActorRef, LocalTypedLifecycle, LocalTypedReceive},
    supervisor::{
        BasicDecisionFn, DefaultStrategy, OneForAllStrategy, OneForOneStrategy,
        SupervisionDecision, SupervisorStrategyType,
    },
    typed_context::TypedContext,
    types::ActorResult,
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};

#[derive(Default)]
struct Stats {
    next: AtomicUsize,
    events: Mutex<Vec<(usize, &'static str, ThreadId)>>,
    failures: AtomicUsize,
}
impl Stats {
    fn record(&self, generation: usize, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((generation, phase, std::thread::current().id()));
    }
    fn count(&self, generation: usize, phase: &str) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.0 == generation && e.1 == phase)
            .count()
    }
    fn phases(&self, phase: &str) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.1 == phase)
            .count()
    }
    fn assert_affinity(&self) {
        let e = self.events.lock().unwrap();
        let owner = e[0].2;
        assert!(e.iter().all(|e| e.2 == owner));
    }
}
pub struct Info;
impl Message for Info {
    type Result = (usize, TypedContext);
}
pub struct Hold(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
impl Message for Hold {
    type Result = usize;
}
pub struct Fail;
impl Message for Fail {
    type Result = ();
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Info, Fail, Hold), local, lifecycle)]
struct Node {
    generation: usize,
    local: Rc<Cell<usize>>,
    stats: Arc<Stats>,
    owner: ThreadId,
    nested: Option<Arc<Stats>>,
    bad_drop: bool,
}
impl Node {
    fn new(stats: Arc<Stats>) -> Self {
        let generation = stats.next.fetch_add(1, Ordering::SeqCst);
        stats.record(generation, "construct");
        Self {
            generation,
            local: Rc::new(Cell::new(0)),
            stats,
            owner: std::thread::current().id(),
            nested: None,
            bad_drop: false,
        }
    }
}
impl LocalTypedLifecycle for Node {
    fn init_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(self.owner, std::thread::current().id());
            self.stats.record(self.generation, "init");
            if let Some(stats) = self.nested.clone() {
                let child: Ref = ctx
                    .spawn_local_typed_child_factory(move || Node::new(stats.clone()))
                    .await?;
                child.ask(Info).await?;
            }
            Ok(())
        })
    }
    fn before_stop_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert!(ctx.children().is_empty());
            self.stats.record(self.generation, "hook");
            Ok(())
        })
    }
}
impl LocalTypedReceive<Info> for Node {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Info,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<(usize, TypedContext)>> {
        let local = self.local.clone();
        Box::pin(async move {
            tokio::task::yield_now().await;
            local.set(local.get() + 1);
            assert_eq!(self.owner, std::thread::current().id());
            self.stats.record(self.generation, "message");
            Ok((self.generation, ctx.clone()))
        })
    }
}
impl LocalTypedReceive<Fail> for Node {
    fn receive_typed(&mut self, _: Fail) -> LocalBoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.stats.failures.fetch_add(1, Ordering::SeqCst);
            Err(ActorError::MessageHandlingError(
                "local-factory-child-failure".into(),
            ))
        })
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        assert_eq!(Rc::strong_count(&self.local), 1);
        self.stats.record(self.generation, "drop");
        assert!(!self.bad_drop, "local-factory-child-drop-failure");
    }
}
type Ref = LocalTypedActorRef<Node, Info>;
fn one() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 2,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    })
}
fn all() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForAll(OneForAllStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    })
}
async fn setup(arbiters: usize) -> (Arc<ActixActorSystem>, Ref, TypedContext) {
    let s = Arc::new(
        ActixActorSystem::with_arbiter_count(arbiters)
            .await
            .unwrap(),
    );
    let parent = s
        .spawn_local_typed_with_strategy(
            || Node::new(Arc::default()),
            "/parent",
            SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure),
        )
        .await
        .unwrap();
    let (_, ctx) = parent.ask(Info).await.unwrap();
    (s, parent, ctx)
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
#[actix::test]
async fn local_parent_factory_retires_old_subtree_before_reconstruction_on_one_arbiter() {
    let s = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let parents = Arc::new(Stats::default());
    let children = Arc::new(Stats::default());
    let made = parents.clone();
    let nested = children.clone();
    let parent: Ref = s
        .spawn_local_typed_factory(
            move || {
                let generation = made.next.load(Ordering::SeqCst);
                if generation > 0 {
                    assert_eq!(nested.phases("drop"), generation);
                    assert_eq!(made.phases("drop"), generation);
                }
                let mut actor = Node::new(made.clone());
                actor.nested = Some(nested.clone());
                actor
            },
            "/factory-parent",
        )
        .await
        .unwrap();
    let (_, old) = parent.ask(Info).await.unwrap();
    assert_eq!(old.children().len(), 1);
    let old_child = old.children().get(0).unwrap();
    assert!(parent.ref_for::<Fail>().ask(Fail).await.is_err());
    let (generation, new) = parent.ask(Info).await.unwrap();
    assert_eq!(generation, 1);
    assert!(!old_child.is_alive().await);
    assert!(old.children().is_empty());
    assert_eq!(new.children().len(), 1);
    assert!(
        old.spawn_local_typed_child_factory::<Node, Info, _>(|| panic!(
            "stale generation constructed child"
        ))
        .await
        .is_err()
    );
    parent.stop().await.unwrap();
    assert_eq!(children.phases("drop"), 2);
    assert_eq!(parents.phases("drop"), 2);
    parents.assert_affinity();
    children.assert_affinity();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn local_factory_child_inherits_live_policy_and_override_without_losing_identity() {
    let (s, parent, ctx) = setup(2).await;
    ctx.set_child_strategy(one());
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let child: Ref = ctx
        .spawn_local_typed_child_factory(move || Node::new(made.clone()))
        .await
        .unwrap();
    assert_eq!(
        child.ask(Info).await.unwrap().1.parent().unwrap().path(),
        parent.path()
    );
    let identity = child.clone().into_dyn().death_watch().unwrap().id();
    assert!(child.ref_for::<Fail>().ask(Fail).await.is_err());
    assert_eq!(child.ask(Info).await.unwrap().0, 1);
    assert_eq!(
        child.clone().into_dyn().death_watch().unwrap().id(),
        identity
    );
    ctx.set_child_strategy(SupervisorStrategyType::Default(
        DefaultStrategy::ResumeOnFailure,
    ));
    assert!(child.ref_for::<Fail>().ask(Fail).await.is_err());
    assert_eq!(child.ask(Info).await.unwrap().0, 1);
    let explicit = Arc::new(Stats::default());
    let made = explicit.clone();
    let stopped: Ref = ctx
        .spawn_local_typed_child_factory_with_strategy(
            move || Node::new(made.clone()),
            SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure),
        )
        .await
        .unwrap();
    stopped.ask(Info).await.unwrap();
    assert!(stopped.ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| ctx.children().len() == 1).await;
    assert_eq!(explicit.next.load(Ordering::SeqCst), 1);
    assert!(!stopped.is_alive());
    parent.stop().await.unwrap();
    stats.assert_affinity();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn local_cohort_larger_than_constructor_pool_restarts_all_before_activation_and_shares_budget()
 {
    let (s, parent, ctx) = setup(2).await;
    ctx.set_child_strategy(all());
    let stats: Vec<_> = (0..5).map(|_| Arc::new(Stats::default())).collect();
    let mut children: Vec<Ref> = vec![];
    for stat in &stats {
        let made = stat.clone();
        let cohort = stats.clone();
        children.push(
            ctx.spawn_local_typed_child_factory(move || {
                if made.next.load(Ordering::SeqCst) > 0 {
                    assert!(cohort.iter().all(|s| s.count(0, "drop") == 1));
                }
                Node::new(made.clone())
            })
            .await
            .unwrap(),
        );
    }
    for child in &children {
        assert_eq!(child.ask(Info).await.unwrap().0, 0);
    }
    assert!(children[0].ref_for::<Fail>().ask(Fail).await.is_err());
    for child in &children {
        assert_eq!(child.ask(Info).await.unwrap().0, 1);
    }
    assert_eq!(ctx.children().len(), 5);
    assert_eq!(stats[0].failures.load(Ordering::SeqCst), 1);
    assert!(children[1].ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| ctx.children().is_empty()).await;
    for stat in &stats {
        assert_eq!(stat.next.load(Ordering::SeqCst), 2);
        assert_eq!(stat.phases("drop"), 2);
        stat.assert_affinity();
    }
    parent.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn local_cohort_constructor_failure_never_initializes_partial_replacements() {
    let (s, parent, ctx) = setup(2).await;
    ctx.set_child_strategy(all());
    let stats: Vec<_> = (0..4).map(|_| Arc::new(Stats::default())).collect();
    let mut children: Vec<Ref> = vec![];
    for (index, stat) in stats.iter().enumerate() {
        let made = stat.clone();
        children.push(
            ctx.spawn_local_typed_child_factory(move || {
                assert!(
                    !(index == 2 && made.next.load(Ordering::SeqCst) > 0),
                    "cohort constructor failure"
                );
                Node::new(made.clone())
            })
            .await
            .unwrap(),
        );
    }
    for child in &children {
        child.ask(Info).await.unwrap();
    }
    assert!(children[0].ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| ctx.children().is_empty()).await;
    for stat in &stats {
        assert_eq!(stat.count(1, "init"), 0);
        assert_eq!(stat.next.load(Ordering::SeqCst), stat.phases("drop"));
    }
    parent.ask(Info).await.unwrap();
    parent.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
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
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(5), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out(), "test gate not released");
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
#[actix::test]
async fn parent_stop_aborts_local_cohort_with_blocked_prepared_output_and_joins_disposal() {
    let (s, parent, ctx) = setup(3).await;
    ctx.set_child_strategy(all());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let stats: Vec<_> = (0..2).map(|_| Arc::new(Stats::default())).collect();
    let mut children: Vec<Ref> = vec![];
    for (index, stat) in stats.iter().enumerate() {
        let made = stat.clone();
        let blocked = gate.clone();
        children.push(
            ctx.spawn_local_typed_child_factory(move || {
                let actor = Node::new(made.clone());
                if index == 0 && actor.generation == 1 {
                    blocked.block();
                }
                actor
            })
            .await
            .unwrap(),
        );
    }
    for child in &children {
        child.ask(Info).await.unwrap();
    }
    assert!(children[1].ref_for::<Fail>().ask(Fail).await.is_err());
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    ctx.stop();
    gate.release();
    parent.stop().await.unwrap();
    assert!(ctx.children().is_empty());
    for stat in &stats {
        assert_eq!(stat.count(1, "init"), 0);
        assert_eq!(stat.next.load(Ordering::SeqCst), stat.phases("drop"));
    }
    assert_eq!(s.status().active_actors, 0);
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn local_factory_child_cleanup_failure_prevents_rebuilding_over_failed_generation() {
    let (s, parent, ctx) = setup(2).await;
    ctx.set_child_strategy(one());
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let child: Ref = ctx
        .spawn_local_typed_child_factory(move || {
            let mut node = Node::new(made.clone());
            node.bad_drop = true;
            node
        })
        .await
        .unwrap();
    child.ask(Info).await.unwrap();
    assert!(child.ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| ctx.children().is_empty()).await;
    assert!(
        child
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("local-factory-child-drop-failure")
    );
    assert_eq!(stats.next.load(Ordering::SeqCst), 1);
    parent.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn aborted_local_cohort_cancels_capacity_waiters_without_starting_late_constructors() {
    #[derive(ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Info))]
    struct Busy;
    impl parrot_api::typed::TypedReceive<Info> for Busy {
        fn receive_typed_with_context<'a>(
            &'a mut self,
            _: Info,
            ctx: &'a TypedContext,
        ) -> parrot_api::types::BoxedFuture<'a, ActorResult<(usize, TypedContext)>> {
            Box::pin(async move { Ok((0, ctx.clone())) })
        }
    }
    let (s, parent, ctx) = setup(2).await;
    ctx.set_child_strategy(all());
    let stats: Vec<_> = (0..2).map(|_| Arc::new(Stats::default())).collect();
    let mut children: Vec<Ref> = vec![];
    for stat in &stats {
        let made = stat.clone();
        children.push(
            ctx.spawn_local_typed_child_factory(move || Node::new(made.clone()))
                .await
                .unwrap(),
        );
    }
    for child in &children {
        child.ask(Info).await.unwrap();
    }
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let entered = Arc::new(AtomicUsize::new(0));
    let mut workers = vec![];
    for index in 0..2 {
        let gate = gate.clone();
        let count = entered.clone();
        workers.push(
            s.spawn_typed_factory::<Busy, Info, _>(
                move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    gate.block();
                    Busy
                },
                &format!("/busy-{index}"),
            )
            .await
            .unwrap(),
        );
    }
    until(|| entered.load(Ordering::SeqCst) == 2).await;
    assert!(children[0].ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| stats.iter().all(|s| s.count(0, "drop") == 1)).await;
    // Both arbiter owners are available, but unrelated Send constructors own
    // all permits. Stopping one cohort member must abort the other waiter now,
    // without first releasing those constructors or waiting for the deadline.
    // The interrupted recovery reports Stopped/cohort cancellation on this
    // member; successful disposal is observed through scope retirement below.
    assert!(children[0].stop().await.is_err());
    tokio::time::timeout(
        Duration::from_millis(500),
        until(|| ctx.children().is_empty()),
    )
    .await
    .expect("aborted Local cohort retained a constructor-capacity waiter");
    for stat in &stats {
        assert_eq!(stat.next.load(Ordering::SeqCst), 1);
    }
    gate.release();
    for worker in workers {
        worker.ask(Info).await.unwrap();
        worker.stop().await.unwrap();
    }
    for stat in &stats {
        assert_eq!(stat.next.load(Ordering::SeqCst), 1);
    }
    parent.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}

struct Capture(Arc<AtomicUsize>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn capture_policy(capture: Capture) -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            std::hint::black_box(&capture);
            SupervisionDecision::Restart
        }),
    }
}
#[actix::test]
async fn unpolled_local_factories_own_both_inputs_without_constructing_root_or_child() {
    let (s, parent, ctx) = setup(1).await;
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(drops.clone());
    let policy = capture_policy(Capture(drops.clone()));
    drop(s.spawn_local_typed_factory_with_strategy::<Node, Info, _>(
        move || {
            std::hint::black_box(&capture);
            panic!("unpolled root constructor ran")
        },
        "/unpolled-root",
        policy,
    ));
    let capture = Capture(drops.clone());
    let policy = capture_policy(Capture(drops.clone()));
    drop(
        ctx.spawn_local_typed_child_factory_with_strategy::<Node, Info, _>(
            move || {
                std::hint::black_box(&capture);
                panic!("unpolled child constructor ran")
            },
            SupervisorStrategyType::OneForOne(policy),
        ),
    );
    until(|| drops.load(Ordering::SeqCst) == 4).await;
    assert!(ctx.children().is_empty());
    assert_eq!(s.status().active_actors, 1);
    parent.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn closed_local_parent_rejects_factory_and_joins_constructor_and_policy_captures() {
    let (s, parent, ctx) = setup(1).await;
    parent.stop().await.unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(drops.clone());
    let policy = capture_policy(Capture(drops.clone()));
    let result = ctx
        .spawn_local_typed_child_factory_with_strategy::<Node, Info, _>(
            move || {
                std::hint::black_box(&capture);
                panic!("closed scope constructed child")
            },
            SupervisorStrategyType::OneForOne(policy),
        )
        .await;
    assert!(matches!(result, Err(ActorError::Stopped)));
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(s.status().active_actors, 0);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn local_cohort_includes_member_still_in_its_first_synchronous_constructor() {
    let (s, parent, ctx) = setup(3).await;
    ctx.set_child_strategy(all());
    let first_stats = Arc::new(Stats::default());
    let made = first_stats.clone();
    let first: Ref = ctx
        .spawn_local_typed_child_factory(move || Node::new(made.clone()))
        .await
        .unwrap();
    first.ask(Info).await.unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let second_stats = Arc::new(Stats::default());
    let made = second_stats.clone();
    let second: Ref = ctx
        .spawn_local_typed_child_factory(move || {
            let actor = Node::new(made.clone());
            if actor.generation == 0 {
                blocked.block();
            }
            actor
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    assert!(first.ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| first_stats.count(0, "drop") == 1).await;
    assert_eq!(second_stats.count(0, "init"), 0);
    assert_eq!(first_stats.next.load(Ordering::SeqCst), 1);
    gate.release();
    assert_eq!(first.ask(Info).await.unwrap().0, 1);
    assert_eq!(second.ask(Info).await.unwrap().0, 1);
    assert_eq!(second_stats.count(0, "init"), 0);
    assert_eq!(second_stats.count(0, "drop"), 1);
    parent.stop().await.unwrap();
    first_stats.assert_affinity();
    second_stats.assert_affinity();
    s.as_ref().clone().shutdown().await.unwrap();
}

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Info, Fail))]
struct SendNode {
    generation: usize,
    stats: Arc<Stats>,
}
impl parrot_api::typed::TypedReceive<Info> for SendNode {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Info,
        ctx: &'a TypedContext,
    ) -> parrot_api::types::BoxedFuture<'a, ActorResult<(usize, TypedContext)>> {
        Box::pin(async move { Ok((self.generation, ctx.clone())) })
    }
}
impl parrot_api::typed::TypedReceive<Fail> for SendNode {
    fn receive_typed(&mut self, _: Fail) -> parrot_api::types::BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async {
            Err(ActorError::MessageHandlingError(
                "mixed cohort failure".into(),
            ))
        })
    }
}
impl Drop for SendNode {
    fn drop(&mut self) {
        self.stats.record(self.generation, "drop");
    }
}
#[actix::test]
async fn local_and_send_factory_siblings_share_one_real_recovery_cohort() {
    let (s, parent, ctx) = setup(2).await;
    ctx.set_child_strategy(all());
    let local_stats = Arc::new(Stats::default());
    let made = local_stats.clone();
    let other_stats = Arc::new(Stats::default());
    let other = other_stats.clone();
    let local: Ref = ctx
        .spawn_local_typed_child_factory(move || {
            if made.next.load(Ordering::SeqCst) > 0 {
                assert_eq!(other.count(0, "drop"), 1);
            }
            Node::new(made.clone())
        })
        .await
        .unwrap();
    let made = other_stats.clone();
    let other = local_stats.clone();
    let send = ctx
        .spawn_typed_child_factory::<SendNode, Info, _>(move || {
            let generation = made.next.fetch_add(1, Ordering::SeqCst);
            if generation > 0 {
                assert_eq!(other.count(0, "drop"), 1);
            }
            SendNode {
                generation,
                stats: made.clone(),
            }
        })
        .await
        .unwrap();
    local.ask(Info).await.unwrap();
    send.ask(Info).await.unwrap();
    assert!(send.ref_for::<Fail>().ask(Fail).await.is_err());
    // A sibling's failure reply precedes the cohort decision. A request sent
    // here could still become an in-flight call on the old Local generation.
    // Observe real replacement initialization, not a sleep or a retry loop.
    until(|| local_stats.count(1, "init") == 1).await;
    assert_eq!(local.ask(Info).await.unwrap().0, 1);
    assert_eq!(send.ask(Info).await.unwrap().0, 1);
    assert_eq!(ctx.children().len(), 2);
    parent.stop().await.unwrap();
    assert_eq!(local_stats.phases("drop"), 2);
    assert_eq!(other_stats.phases("drop"), 2);
    local_stats.assert_affinity();
    s.as_ref().clone().shutdown().await.unwrap();
}

impl LocalTypedReceive<Hold> for Node {
    fn receive_typed(
        &mut self,
        Hold(entered, release): Hold,
    ) -> LocalBoxedFuture<'_, ActorResult<usize>> {
        let local = self.local.clone();
        Box::pin(async move {
            self.stats.record(self.generation, "hold");
            entered.notify_one();
            release.notified().await;
            Ok(local.get())
        })
    }
}
#[actix::test]
async fn local_cohort_cancels_in_flight_sibling_request_but_preserves_unread_message() {
    let (s, parent, ctx) = setup(2).await;
    ctx.set_child_strategy(all());
    let local_stats = Arc::new(Stats::default());
    let made = local_stats.clone();
    let local: Ref = ctx
        .spawn_local_typed_child_factory(move || Node::new(made.clone()))
        .await
        .unwrap();
    let other_stats = Arc::new(Stats::default());
    let made = other_stats.clone();
    let send = ctx
        .spawn_typed_child_factory::<SendNode, Info, _>(move || SendNode {
            generation: made.next.fetch_add(1, Ordering::SeqCst),
            stats: made.clone(),
        })
        .await
        .unwrap();
    local.ask(Info).await.unwrap();
    send.ask(Info).await.unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let held = local.ref_for::<Hold>();
    let pending = held.ask(Hold(entered.clone(), release));
    tokio::pin!(pending);
    tokio::select! { _ = entered.notified() => {}, result = &mut pending => panic!("held request ended early: {result:?}") }
    local.tell(Info).await.unwrap();
    assert!(send.ref_for::<Fail>().ask(Fail).await.is_err());
    assert!(matches!(
        pending.await,
        Err(ActorError::ReplyChannelError(_))
    ));
    until(|| local_stats.count(1, "init") == 1).await;
    assert_eq!(local.ask(Info).await.unwrap().0, 1);
    assert_eq!(local_stats.count(0, "hold"), 1);
    assert_eq!(local_stats.count(1, "hold"), 0);
    assert_eq!(
        local_stats.count(1, "message"),
        2,
        "unread TELL must execute before the new ASK"
    );
    parent.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}
