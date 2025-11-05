//! Real Actix static topology contracts; each fixture runs on owned arbiters.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
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
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

type Ref = TypedActorRef<Family, Op>;
enum Op {
    Hold(Arc<tokio::sync::Notify>, Arc<AtomicUsize>),
    Info,
    Spawn(usize),
    Context,
    Fail,
    Policy(SupervisorStrategyType),
}
enum Reply {
    Info(String, Option<String>, usize),
    Child(Ref),
    Context(TypedContext),
    Done,
}
impl Message for Op {
    type Result = Reply;
}
impl ParrotMsgVariant<Family> for Op {
    fn inject(value: Self) -> Op {
        value
    }
    fn extract(value: Reply) -> ActorResult<Reply> {
        Ok(value)
    }
}
#[derive(Default)]
struct Stats {
    events: Mutex<Vec<(String, &'static str)>>,
    contexts: Mutex<Vec<TypedContext>>,
    drops: AtomicUsize,
    affinity: Mutex<Vec<(String, std::thread::ThreadId)>>,
    arbiters: Mutex<Vec<(String, actix::ArbiterHandle)>>,
}
#[derive(Default)]
struct InitHold {
    entered: tokio::sync::Notify,
    cancelled: AtomicUsize,
}
struct PendingInit(Arc<InitHold>);
impl Drop for PendingInit {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}
struct Family {
    stats: Arc<Stats>,
    path: String,
    depth: usize,
    fail_init: bool,
    init_hold: Option<Arc<InitHold>>,
}
impl Family {
    fn new(stats: Arc<Stats>, depth: usize) -> Self {
        Self {
            stats,
            path: String::new(),
            depth,
            fail_init: false,
            init_hold: None,
        }
    }
}
impl Drop for Family {
    fn drop(&mut self) {
        self.stats
            .affinity
            .lock()
            .unwrap()
            .push((self.path.clone(), std::thread::current().id()));
        self.stats
            .events
            .lock()
            .unwrap()
            .push((self.path.clone(), "drop"));
        self.stats.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotTypedDispatch for Family {
    type Msg = Op;
    type Reply = Reply;
    fn dispatch(&mut self, _: Op) -> BoxedFuture<'_, ActorResult<Reply>> {
        Box::pin(async { Err(ActorError::InternalError("managed context required".into())) })
    }
    fn init_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.path = ctx.path().to_owned();
            self.stats
                .affinity
                .lock()
                .unwrap()
                .push((self.path.clone(), std::thread::current().id()));
            self.stats
                .arbiters
                .lock()
                .unwrap()
                .push((self.path.clone(), actix::Arbiter::current()));
            self.stats
                .events
                .lock()
                .unwrap()
                .push((self.path.clone(), "init"));
            self.stats.contexts.lock().unwrap().push(ctx.clone());
            if self.depth > 0 {
                let child = ctx
                    .spawn_typed_child::<_, Op>(Family::new(self.stats.clone(), self.depth - 1))
                    .await?;
                child.ask(Op::Info).await?;
            }
            if let Some(hold) = &self.init_hold {
                let _pending = PendingInit(hold.clone());
                hold.entered.notify_one();
                std::future::pending::<()>().await;
            }
            if self.fail_init {
                return Err(ActorError::InitializationError("parent init failed".into()));
            }
            Ok(())
        })
    }
    fn before_stop_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.stats
                .affinity
                .lock()
                .unwrap()
                .push((self.path.clone(), std::thread::current().id()));
            assert!(
                ctx.children().is_empty(),
                "hook must follow descendant retirement"
            );
            self.stats
                .events
                .lock()
                .unwrap()
                .push((self.path.clone(), "hook"));
            Ok(())
        })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        msg: Op,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<Reply>> {
        Box::pin(async move {
            self.stats
                .affinity
                .lock()
                .unwrap()
                .push((self.path.clone(), std::thread::current().id()));
            match msg {
                Op::Hold(entered, dropped) => {
                    struct Pending(Arc<AtomicUsize>);
                    impl Drop for Pending {
                        fn drop(&mut self) {
                            self.0.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    let _pending = Pending(dropped);
                    entered.notify_one();
                    std::future::pending().await
                }
                Op::Info => Ok(Reply::Info(
                    ctx.path().to_owned(),
                    ctx.parent().map(|p| p.path().to_string()),
                    ctx.children().len(),
                )),
                Op::Context => Ok(Reply::Context(ctx.clone())),
                Op::Spawn(depth) => Ok(Reply::Child(
                    ctx.spawn_typed_child::<_, Op>(Family::new(self.stats.clone(), depth))
                        .await?,
                )),
                Op::Fail => Err(ActorError::MessageHandlingError(
                    "family handler failed".into(),
                )),
                Op::Policy(policy) => {
                    ctx.set_child_strategy(policy);
                    Ok(Reply::Done)
                }
            }
        })
    }
}
async fn system() -> Arc<ActixActorSystem> {
    Arc::new(ActixActorSystem::with_arbiter_count(2).await.unwrap())
}
async fn info(actor: &Ref) -> (String, Option<String>, usize) {
    match actor.ask(Op::Info).await.unwrap() {
        Reply::Info(a, b, c) => (a, b, c),
        _ => panic!("wrong reply"),
    }
}
async fn context(actor: &Ref) -> TypedContext {
    match actor.ask(Op::Context).await.unwrap() {
        Reply::Context(c) => c,
        _ => panic!("wrong reply"),
    }
}
async fn child(actor: &Ref, depth: usize) -> Ref {
    match actor.ask(Op::Spawn(depth)).await.unwrap() {
        Reply::Child(c) => c,
        _ => panic!("wrong reply"),
    }
}
async fn absent(system: &ActixActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.get_actor(&path.to_owned()).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn one(decision: SupervisionDecision) -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(10),
        decider: BasicDecisionFn::new(move |_| decision),
    })
}

#[actix::test]
async fn typed_context_reports_real_parent_and_recursively_retires_descendants_before_hooks() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 2), "/tree")
        .await
        .unwrap();
    assert_eq!(info(&root).await, ("/tree".into(), None, 1));
    assert_eq!(s.status().active_actors, 3);
    let contexts = stats.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    for pair in contexts.windows(2) {
        assert_eq!(pair[1].parent().unwrap().path(), pair[0].path());
        assert!(pair[1].path().starts_with(&format!("{}/", pair[0].path())));
    }
    root.stop().await.unwrap();
    assert_eq!(s.status().active_actors, 0);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
    let events = stats.events.lock().unwrap().clone();
    let child_drop = events
        .iter()
        .position(|(p, e)| p == contexts[2].path() && *e == "drop")
        .unwrap();
    let parent_hook = events
        .iter()
        .position(|(p, e)| p == contexts[1].path() && *e == "hook")
        .unwrap();
    assert!(child_drop < parent_hook);
    for ctx in contexts {
        assert!(
            ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
                .await
                .is_err()
        );
    }
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_child_stop_removes_ownership_without_stopping_parent_or_sibling() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats, 0), "/tree")
        .await
        .unwrap();
    let a = child(&root, 0).await;
    let b = child(&root, 0).await;
    let path = info(&a).await.0;
    a.stop().await.unwrap();
    absent(&s, &path).await;
    assert_eq!(info(&root).await.2, 1);
    assert_eq!(info(&b).await.1, Some("/tree".into()));
    root.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_parent_initialization_failure_retires_already_created_descendants() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let mut parent = Family::new(stats.clone(), 2);
    parent.fail_init = true;
    let root = s.spawn_typed::<_, Op>(parent, "/init").await.unwrap();
    assert!(root.ask(Op::Info).await.is_err());
    absent(&s, "/init").await;
    assert_eq!(s.status().active_actors, 0);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
    assert!(
        !stats
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(p, e)| p == "/init" && *e == "hook")
    );
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_children_inherit_live_resume_policy_without_replaying_failure() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats, 0), "/resume")
        .await
        .unwrap();
    let c = child(&root, 0).await;
    root.ask(Op::Policy(one(SupervisionDecision::Resume)))
        .await
        .unwrap();
    assert!(c.ask(Op::Fail).await.is_err());
    assert_eq!(info(&c).await.1, Some("/resume".into()));
    assert_eq!(info(&root).await.2, 1);
    root.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_one_for_all_stop_retires_siblings_and_leaves_parent_usable() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats, 0), "/all")
        .await
        .unwrap();
    let a = child(&root, 0).await;
    let b = child(&root, 0).await;
    root.ask(Op::Policy(SupervisorStrategyType::OneForAll(
        OneForAllStrategy {
            max_restarts: 0,
            within: Duration::from_secs(10),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
        },
    )))
    .await
    .unwrap();
    let ap = info(&a).await.0;
    let bp = info(&b).await.0;
    assert!(a.ask(Op::Fail).await.is_err());
    absent(&s, &ap).await;
    absent(&s, &bp).await;
    assert_eq!(info(&root).await.2, 0);
    root.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_explicit_child_policy_overrides_parent_and_escalation_reaches_actual_parent() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/escalate")
        .await
        .unwrap();
    let ctx = context(&root).await;
    root.ask(Op::Policy(one(SupervisionDecision::Resume)))
        .await
        .unwrap();
    let c = ctx
        .spawn_typed_child_with_strategy::<_, Op>(
            Family::new(stats.clone(), 0),
            one(SupervisionDecision::Escalate),
        )
        .await
        .unwrap();
    assert!(c.ask(Op::Fail).await.is_err());
    absent(&s, "/escalate").await;
    assert_eq!(s.status().active_actors, 0);
    assert!(
        !stats
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(p, e)| p == "/escalate" && *e == "hook")
    );
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_root_restart_retires_old_children_and_old_context_cannot_stop_replacement() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let root = s
        .spawn_typed_factory::<_, Op, _>(move || Family::new(made.clone(), 1), "/replace")
        .await
        .unwrap();
    assert_eq!(info(&root).await.2, 1);
    let old = context(&root).await;
    let old_child = old.children().get(0).unwrap().path().to_string();
    assert!(root.ask(Op::Fail).await.is_err());
    assert_eq!(info(&root).await.2, 1);
    absent(&s, &old_child).await;
    assert!(
        old.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
            .await
            .is_err()
    );
    old.stop();
    assert_eq!(info(&root).await.0, "/replace");
    root.stop().await.unwrap();
    assert_eq!(s.status().active_actors, 0);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn derived_multi_protocol_and_single_adapter_dispatch_receive_managed_context() {
    use parrot::thread::typed::SingleVariant;
    use parrot_api::typed::TypedReceive;
    pub struct Path;
    impl Message for Path {
        type Result = String;
    }
    pub struct Count;
    impl Message for Count {
        type Result = usize;
    }
    #[derive(parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Path, Count))]
    struct Derived;
    impl TypedReceive<Path> for Derived {
        fn receive_typed_with_context<'a>(
            &'a mut self,
            _: Path,
            ctx: &'a TypedContext,
        ) -> BoxedFuture<'a, ActorResult<String>> {
            Box::pin(async move { Ok(ctx.path().to_owned()) })
        }
    }
    impl TypedReceive<Count> for Derived {
        fn receive_typed_with_context<'a>(
            &'a mut self,
            _: Count,
            ctx: &'a TypedContext,
        ) -> BoxedFuture<'a, ActorResult<usize>> {
            Box::pin(async move { Ok(ctx.children().len()) })
        }
    }
    let s = system().await;
    let root = s.spawn_typed::<_, Path>(Derived, "/derived").await.unwrap();
    assert_eq!(root.ask(Path).await.unwrap(), "/derived");
    assert_eq!(root.ref_for::<Count>().ask(Count).await.unwrap(), 0);
    let single = s
        .spawn_typed_single::<_, Path>(Derived, "/single")
        .await
        .unwrap();
    assert_eq!(
        single.ask(SingleVariant::wrap(Path)).await.unwrap(),
        "/single"
    );
    root.stop().await.unwrap();
    single.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn nested_typed_initialization_and_stop_make_progress_with_one_arbiter() {
    let s = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            ask_timeout: Duration::from_secs(2),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 5), "/one-permit")
        .await
        .unwrap();
    assert_eq!(info(&root).await.2, 1);
    assert_eq!(s.status().active_actors, 6);
    tokio::time::timeout(Duration::from_secs(3), root.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.drops.load(Ordering::SeqCst), 6);
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_child_failure_is_not_a_parent_cleanup_failure() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/failure")
        .await
        .unwrap();
    let c = child(&root, 0).await;
    let path = info(&c).await.0;
    assert!(c.ask(Op::Fail).await.is_err());
    absent(&s, &path).await;
    assert_eq!(info(&root).await.2, 0);
    root.stop().await.unwrap();
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn typed_context_retention_does_not_keep_system_alive_and_rejection_disposes_input() {
    let s = system().await;
    let weak = Arc::downgrade(&s);
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 1), "/drop")
        .await
        .unwrap();
    let ctx = context(&root).await;
    drop(s);
    assert!(weak.upgrade().is_none());
    tokio::time::timeout(Duration::from_secs(3), async {
        while stats.drops.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
            .await
            .is_err()
    );
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
    assert!(!root.is_alive());
}
#[actix::test]
async fn typed_child_capacity_is_released_only_by_actual_retirement() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/capacity")
        .await
        .unwrap();
    let ctx = context(&root).await;
    let mut children = Vec::new();
    for _ in 0..1024 {
        children.push(
            ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
                .await
                .unwrap(),
        );
    }
    assert_eq!(ctx.children().len(), 1024);
    assert!(
        ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
            .await
            .is_err()
    );
    assert_eq!(s.status().active_actors, 1025);
    children.pop().unwrap().stop().await.unwrap();
    let replacement = ctx
        .spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
        .await
        .unwrap();
    assert_eq!(ctx.children().len(), 1024);
    assert!(replacement.is_alive());
    root.stop().await.unwrap();
    assert_eq!(s.status().active_actors, 0);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[derive(Default)]
struct DropGate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: std::sync::Condvar,
}
impl DropGate {
    fn wait(&self) {
        self.entered.notify_one();
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(5), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out(), "owned drop gate not released");
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct ReleaseGate(Arc<DropGate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct BlockedChild {
    gate: Arc<DropGate>,
    drops: Arc<AtomicUsize>,
}
impl Drop for BlockedChild {
    fn drop(&mut self) {
        self.gate.wait();
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotTypedDispatch for BlockedChild {
    type Msg = ();
    type Reply = ();
    fn dispatch(&mut self, _: ()) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl ParrotMsgVariant<BlockedChild> for Ping {
    fn inject(_: Self) {}
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
#[actix::test]
async fn typed_parent_timeout_keeps_both_paths_until_blocked_child_really_retires() {
    let s = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 2,
            shutdown_timeout: Duration::from_millis(80),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/blocked")
        .await
        .unwrap();
    let ctx = context(&root).await;
    let gate = Arc::new(DropGate::default());
    let _release = ReleaseGate(gate.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let c = ctx
        .spawn_typed_child::<_, Ping>(BlockedChild {
            gate: gate.clone(),
            drops: drops.clone(),
        })
        .await
        .unwrap();
    c.ask(Ping).await.unwrap();
    let stopper = root.clone();
    let stop = tokio::spawn(async move { stopper.stop().await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), stop)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(s.status().active_actors, 2);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 0);
    assert!(
        ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
            .await
            .is_err()
    );
    gate.release();
    absent(&s, "/blocked").await;
    assert_eq!(s.status().active_actors, 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    s.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn racing_typed_child_admission_and_parent_stop_leaves_no_orphans() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/race")
        .await
        .unwrap();
    let ctx = context(&root).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let ctx = ctx.clone();
        let stats = stats.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            ctx.spawn_typed_child::<_, Op>(Family::new(stats, 0)).await
        }));
    }
    barrier.wait().await;
    root.stop().await.unwrap();
    for task in tasks {
        if let Ok(reference) = task.await.unwrap() {
            assert!(!reference.is_alive());
        }
    }
    assert_eq!(s.status().active_actors, 0);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 17);
    s.as_ref().clone().shutdown().await.unwrap();
}

struct BadCleanup {
    hook_panic: bool,
    drop_panic: bool,
    drops: Arc<AtomicUsize>,
}
impl Drop for BadCleanup {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.drop_panic, "child destructor failed");
    }
}
impl ParrotTypedDispatch for BadCleanup {
    type Msg = ();
    type Reply = ();
    fn dispatch(&mut self, _: ()) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            assert!(!self.hook_panic, "child hook panicked");
            if self.drop_panic {
                Ok(())
            } else {
                Err(ActorError::InternalError("child hook failed".into()))
            }
        })
    }
}
impl ParrotMsgVariant<BadCleanup> for Ping {
    fn inject(_: Self) {}
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
#[actix::test]
async fn typed_parent_retirement_preserves_child_stop_hook_error_and_panic() {
    for hook_panic in [false, true] {
        let s = system().await;
        let stats = Arc::new(Stats::default());
        let root = s
            .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/hook")
            .await
            .unwrap();
        let ctx = context(&root).await;
        let drops = Arc::new(AtomicUsize::new(0));
        let child = ctx
            .spawn_typed_child::<_, Ping>(BadCleanup {
                hook_panic,
                drop_panic: false,
                drops: drops.clone(),
            })
            .await
            .unwrap();
        child.ask(Ping).await.unwrap();
        let result = root.stop().await;
        assert!(
            result.is_err(),
            "a child's failed stop hook is a real retirement failure"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(s.status().active_actors, 0);
        assert!(
            !stats
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|(p, e)| p == "/hook" && *e == "hook")
        );
        s.as_ref().clone().shutdown().await.unwrap();
    }
}
#[actix::test]
async fn typed_parent_retirement_preserves_child_destructor_failure() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/bad-drop")
        .await
        .unwrap();
    let ctx = context(&root).await;
    let drops = Arc::new(AtomicUsize::new(0));
    let child = ctx
        .spawn_typed_child::<_, Ping>(BadCleanup {
            hook_panic: false,
            drop_panic: true,
            drops: drops.clone(),
        })
        .await
        .unwrap();
    child.ask(Ping).await.unwrap();
    assert!(root.stop().await.is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(s.status().active_actors, 0);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_grandparent_retirement_preserves_descendant_cleanup_failure() {
    for (hook_panic, drop_panic, depth) in [
        (false, false, 1),
        (true, false, 1),
        (false, true, 1),
        (false, false, 3),
        (true, false, 3),
        (false, true, 3),
    ] {
        let s = system().await;
        let stats = Arc::new(Stats::default());
        let root = s
            .spawn_typed::<_, Op>(Family::new(stats.clone(), depth), "/grandparent")
            .await
            .unwrap();
        assert_eq!(info(&root).await.2, 1);
        let parent = stats.contexts.lock().unwrap().last().unwrap().clone();
        let drops = Arc::new(AtomicUsize::new(0));
        let child = parent
            .spawn_typed_child::<_, Ping>(BadCleanup {
                hook_panic,
                drop_panic,
                drops: drops.clone(),
            })
            .await
            .unwrap();
        child.ask(Ping).await.unwrap();
        let error = root
            .stop()
            .await
            .expect_err("descendant cleanup failure must reach the grandparent")
            .to_string();
        let expected = if drop_panic {
            "child destructor failed"
        } else if hook_panic {
            "child hook panicked"
        } else {
            "child hook failed"
        };
        assert!(
            error.contains(expected),
            "wrong descendant diagnostic: {error}"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(s.status().active_actors, 0);
        assert!(
            !stats
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|(_, event)| *event == "hook")
        );
        s.as_ref().clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn typed_factory_cannot_replace_a_generation_with_failed_descendant_retirement() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let builds = Arc::new(AtomicUsize::new(0));
    let made = stats.clone();
    let built = builds.clone();
    let root = s
        .spawn_typed_factory::<_, Op, _>(
            move || {
                built.fetch_add(1, Ordering::SeqCst);
                Family::new(made.clone(), 1)
            },
            "/failed-tree-factory",
        )
        .await
        .unwrap();
    assert_eq!(info(&root).await.2, 1);
    let parent = stats.contexts.lock().unwrap()[1].clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let leaf = parent
        .spawn_typed_child::<_, Ping>(BadCleanup {
            hook_panic: false,
            drop_panic: true,
            drops: drops.clone(),
        })
        .await
        .unwrap();
    leaf.ask(Ping).await.unwrap();
    assert!(root.ask(Op::Fail).await.is_err());
    absent(&s, "/failed-tree-factory").await;
    let error = root.stop().await.unwrap_err().to_string();
    assert!(error.contains("child destructor failed"), "{error}");
    assert_eq!(
        builds.load(Ordering::SeqCst),
        1,
        "failed subtree retirement forbids replacement"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 2);
    assert_eq!(s.status().active_actors, 0);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_child_escalation_reconstructs_repeatable_parent_through_its_own_policy() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let made = stats.clone();
    let decisions = Arc::new(AtomicUsize::new(0));
    let decided = decisions.clone();
    let root = s
        .spawn_typed_factory_with_strategy::<_, Op, _>(
            move || Family::new(made.clone(), 1),
            "/escalated-factory",
            OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(10),
                decider: BasicDecisionFn::new(move |_| {
                    decided.fetch_add(1, Ordering::SeqCst);
                    SupervisionDecision::Restart
                }),
            },
        )
        .await
        .unwrap();
    for generation in 0..2 {
        assert_eq!(info(&root).await.2, 1);
        let ctx = context(&root).await;
        let c = ctx
            .spawn_typed_child_with_strategy::<_, Op>(
                Family::new(stats.clone(), 0),
                one(SupervisionDecision::Escalate),
            )
            .await
            .unwrap();
        assert!(c.ask(Op::Fail).await.is_err());
        if generation == 0 {
            tokio::time::timeout(Duration::from_secs(2), async {
                while stats
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(p, e)| p == "/escalated-factory" && *e == "init")
                    .count()
                    < 2
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(info(&root).await.2, 1);
            assert!(
                ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
                    .await
                    .is_err()
            );
        } else {
            absent(&s, "/escalated-factory").await;
        }
    }
    assert_eq!(decisions.load(Ordering::SeqCst), 2);
    assert_eq!(s.status().active_actors, 0);
    assert!(
        !stats
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(p, e)| p == "/escalated-factory" && *e == "hook")
    );
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_tree_lifecycle_keeps_each_instances_arbiter_through_parent_replacement() {
    for repeatable in [false, true] {
        let s = system().await;
        let stats = Arc::new(Stats::default());
        let root = if repeatable {
            let made = stats.clone();
            s.spawn_typed_factory::<_, Op, _>(move || Family::new(made.clone(), 1), "/affinity")
                .await
                .unwrap()
        } else {
            s.spawn_typed::<_, Op>(Family::new(stats.clone(), 1), "/affinity")
                .await
                .unwrap()
        };
        assert_eq!(info(&root).await.2, 1);
        let first_child = stats.contexts.lock().unwrap()[1].path().to_owned();
        let first_threads = stats.affinity.lock().unwrap().clone();
        let root_thread = first_threads
            .iter()
            .find(|(path, _)| path == "/affinity")
            .unwrap()
            .1;
        let child_thread = first_threads
            .iter()
            .find(|(path, _)| path == &first_child)
            .unwrap()
            .1;
        assert_ne!(root_thread, std::thread::current().id());
        assert_ne!(
            root_thread, child_thread,
            "two-arbiter fixture must exercise a real cross-arbiter child"
        );
        if repeatable {
            assert!(root.ask(Op::Fail).await.is_err());
            assert_eq!(info(&root).await.2, 1);
        }
        root.stop().await.unwrap();
        let observations = stats.affinity.lock().unwrap().clone();
        for (path, thread) in &observations {
            assert_eq!(
                Some(thread),
                observations
                    .iter()
                    .find(|(other, _)| other == path)
                    .map(|(_, id)| id)
            );
        }
        assert!(
            observations
                .iter()
                .filter(|(path, _)| path == "/affinity")
                .all(|(_, thread)| *thread == root_thread)
        );
        assert_eq!(
            stats.drops.load(Ordering::SeqCst),
            if repeatable { 4 } else { 2 }
        );
        s.as_ref().clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn parent_arbiter_cancellation_still_joins_children_on_their_live_arbiter() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 1), "/cancel-tree")
        .await
        .unwrap();
    assert_eq!(info(&root).await.2, 1);
    let ctx = context(&root).await;
    let child_path = stats.contexts.lock().unwrap()[1].path().to_owned();
    let arbiter = stats
        .arbiters
        .lock()
        .unwrap()
        .iter()
        .find(|(path, _)| path == "/cancel-tree")
        .unwrap()
        .1
        .clone();
    arbiter.stop();
    absent(&s, "/cancel-tree").await;
    absent(&s, &child_path).await;
    assert_eq!(stats.drops.load(Ordering::SeqCst), 2);
    assert_eq!(s.status().active_actors, 0);
    assert!(!root.is_alive());
    assert!(root.stop().await.is_err());
    assert!(
        ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
            .await
            .is_err()
    );
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn retained_context_uses_surviving_system_clone_without_owning_its_registry() {
    let s = system().await;
    let surviving = s.as_ref().clone();
    let weak = Arc::downgrade(&s);
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/system-clone")
        .await
        .unwrap();
    let ctx = context(&root).await;
    drop(s);
    assert!(weak.upgrade().is_none());
    let child = ctx
        .spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
        .await
        .unwrap();
    assert_eq!(info(&child).await.1, Some("/system-clone".into()));
    assert_eq!(surviving.status().active_actors, 2);
    surviving.shutdown().await.unwrap();
    assert!(!root.is_alive());
    assert!(!child.is_alive());
    assert_eq!(stats.drops.load(Ordering::SeqCst), 2);
    assert!(
        ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
            .await
            .is_err()
    );
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
}

#[actix::test]
async fn cancelled_parent_keeps_its_path_until_blocked_descendant_retires() {
    let s = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 2,
            shutdown_timeout: Duration::from_millis(80),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/cancel-blocked")
        .await
        .unwrap();
    let ctx = context(&root).await;
    let gate = Arc::new(DropGate::default());
    let _release = ReleaseGate(gate.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let child = ctx
        .spawn_typed_child::<_, Ping>(BlockedChild {
            gate: gate.clone(),
            drops: drops.clone(),
        })
        .await
        .unwrap();
    child.ask(Ping).await.unwrap();
    let arbiter = stats.arbiters.lock().unwrap()[0].1.clone();
    arbiter.stop();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), root.stop())
            .await
            .unwrap()
            .is_err()
    );
    assert!(s.get_actor(&"/cancel-blocked".to_owned()).await.is_some());
    assert!(s.get_actor(&child.path().to_string()).await.is_some());
    assert_eq!(s.status().active_actors, 2);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    // Arbiter cancellation drops the parent's own inputs synchronously; logical
    // retirement must still wait for descendants on the system runtime.
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    gate.release();
    absent(&s, "/cancel-blocked").await;
    assert_eq!(s.status().active_actors, 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(root.stop().await.is_err());
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn failed_child_initialization_executes_inherited_one_for_all_policy() {
    let s = system().await;
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/init-policy")
        .await
        .unwrap();
    let ctx = context(&root).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let decided = calls.clone();
    ctx.set_child_strategy(SupervisorStrategyType::OneForAll(OneForAllStrategy {
        max_restarts: 0,
        within: Duration::from_secs(1),
        decider: BasicDecisionFn::new(move |error| {
            assert!(matches!(error, ActorError::InitializationError(_)));
            decided.fetch_add(1, Ordering::SeqCst);
            SupervisionDecision::Stop
        }),
    }));
    let good = ctx
        .spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
        .await
        .unwrap();
    info(&good).await;
    let mut failing = Family::new(stats.clone(), 0);
    failing.fail_init = true;
    let bad = ctx.spawn_typed_child::<_, Op>(failing).await.unwrap();
    assert!(bad.ask(Op::Info).await.is_err());
    absent(&s, bad.path()).await;
    absent(&s, good.path()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(info(&root).await.2, 0);
    root.stop().await.unwrap();
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
    s.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn child_escalation_interrupts_parent_handler_and_can_reconstruct_its_factory() {
    for repeatable in [false, true] {
        let s = system().await;
        let stats = Arc::new(Stats::default());
        let root = if repeatable {
            let made = stats.clone();
            s.spawn_typed_factory::<_, Op, _>(move || Family::new(made.clone(), 0), "/busy-parent")
                .await
                .unwrap()
        } else {
            s.spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/busy-parent")
                .await
                .unwrap()
        };
        let ctx = context(&root).await;
        let child = ctx
            .spawn_typed_child_with_strategy::<_, Op>(
                Family::new(stats.clone(), 0),
                one(SupervisionDecision::Escalate),
            )
            .await
            .unwrap();
        info(&child).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let cancelled = Arc::new(AtomicUsize::new(0));
        let held = root.clone();
        let signal = entered.clone();
        let dropped = cancelled.clone();
        let request = tokio::spawn(async move { held.ask(Op::Hold(signal, dropped)).await });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert!(child.ask(Op::Fail).await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_secs(2), request)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        if repeatable {
            assert_eq!(info(&root).await.2, 0);
            assert_eq!(
                stats
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(path, event)| path == "/busy-parent" && *event == "init")
                    .count(),
                2
            );
            ctx.stop();
            assert_eq!(info(&root).await.2, 0);
            root.stop().await.unwrap();
        } else {
            absent(&s, "/busy-parent").await;
            assert!(root.stop().await.is_err());
        }
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(s.status().active_actors, 0);
        s.as_ref().clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn child_escalation_interrupts_pending_parent_init_and_preserves_factory_queue() {
    for repeatable in [false, true] {
        let s = system().await;
        let stats = Arc::new(Stats::default());
        let hold = Arc::new(InitHold::default());
        let made = Arc::new(AtomicUsize::new(0));
        let instances = made.clone();
        let state = stats.clone();
        let gate = hold.clone();
        let factory = move || {
            let generation = instances.fetch_add(1, Ordering::SeqCst);
            let mut actor = Family::new(state.clone(), 1);
            if generation == 0 {
                actor.init_hold = Some(gate.clone());
            }
            actor
        };
        let root = if repeatable {
            s.spawn_typed_factory::<_, Op, _>(factory, "/pending-init")
                .await
                .unwrap()
        } else {
            s.spawn_typed::<_, Op>(factory(), "/pending-init")
                .await
                .unwrap()
        };
        tokio::time::timeout(Duration::from_secs(2), hold.entered.notified())
            .await
            .unwrap();
        let ctx = stats
            .contexts
            .lock()
            .unwrap()
            .iter()
            .find(|ctx| ctx.path() == "/pending-init")
            .unwrap()
            .clone();
        let failing = ctx
            .spawn_typed_child_with_strategy::<_, Op>(
                Family::new(stats.clone(), 0),
                one(SupervisionDecision::Escalate),
            )
            .await
            .unwrap();
        info(&failing).await;
        // Admit the request while initialization is pending; it must remain
        // queued across a successful factory replacement.
        let request = root.ask(Op::Info);
        tokio::pin!(request);
        assert!(futures::poll!(&mut request).is_pending());
        assert!(failing.ask(Op::Fail).await.is_err());
        let reply = tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .unwrap();
        if repeatable {
            assert!(
                matches!(reply.unwrap(), Reply::Info(path, None, 1) if path == "/pending-init")
            );
            assert_eq!(made.load(Ordering::SeqCst), 2);
            assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
            ctx.stop();
            assert_eq!(info(&root).await.2, 1);
            root.stop().await.unwrap();
            assert_eq!(stats.drops.load(Ordering::SeqCst), 5);
        } else {
            assert!(reply.is_err());
            absent(&s, "/pending-init").await;
            assert_eq!(made.load(Ordering::SeqCst), 1);
            assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
            assert!(root.stop().await.is_err());
        }
        assert_eq!(hold.cancelled.load(Ordering::SeqCst), 1);
        {
            let events = stats.events.lock().unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|(path, event)| path == "/pending-init" && *event == "hook")
                    .count(),
                usize::from(repeatable)
            );
        }
        assert_eq!(s.status().active_actors, 0);
        s.as_ref().clone().shutdown().await.unwrap();
    }
}
