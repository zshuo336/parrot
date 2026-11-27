use parrot::{
    thread::ThreadActorSystem,
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

#[path = "support/isolated.rs"]
mod isolated;

type Ref = TypedActorRef<Family, Op>;
enum Op {
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
    init_threads: Mutex<Vec<(std::thread::ThreadId, String)>>,
    drops: AtomicUsize,
}
struct Family {
    stats: Arc<Stats>,
    path: String,
    depth: usize,
    fail_init: bool,
}
impl Family {
    fn new(stats: Arc<Stats>, depth: usize) -> Self {
        Self {
            stats,
            path: String::new(),
            depth,
            fail_init: false,
        }
    }
}
impl Drop for Family {
    fn drop(&mut self) {
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
                .events
                .lock()
                .unwrap()
                .push((self.path.clone(), "init"));
            self.stats.contexts.lock().unwrap().push(ctx.clone());
            let thread = std::thread::current();
            self.stats
                .init_threads
                .lock()
                .unwrap()
                .push((thread.id(), thread.name().unwrap_or_default().to_owned()));
            if self.depth > 0 {
                let child = ctx
                    .spawn_typed_child::<_, Op>(Family::new(self.stats.clone(), self.depth - 1))
                    .await?;
                child.ask(Op::Info).await?;
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
            match msg {
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
fn system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(Default::default())
}

fn dedicated_system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
        default_scheduling_mode: parrot::thread::config::SchedulingMode::DedicatedThread,
        ..Default::default()
    })
}

fn sharded_system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
        default_scheduling_mode: parrot::thread::config::SchedulingMode::Sharded {
            affinity_key: "typed-subtree".into(),
        },
        ..Default::default()
    })
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
async fn absent(system: &ThreadActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.actor_paths().iter().any(|p| p == path) {
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

#[tokio::test]
async fn typed_context_reports_real_parent_and_recursively_retires_descendants_before_hooks() {
    recursive_retirement(system(), None).await;
}

#[tokio::test]
async fn dedicated_typed_root_recursively_retires_its_live_subtree() {
    recursive_retirement(dedicated_system(), Some(("parrot-dedicated-", 3))).await;
}

#[tokio::test]
async fn sharded_typed_root_recursively_retires_its_live_subtree() {
    recursive_retirement(sharded_system(), Some(("parrot-shard-", 1))).await;
}

fn native_tree_host_exit(
    name: &str,
    mode: parrot::thread::config::SchedulingMode,
    prefix: &str,
    factory_tree: bool,
) {
    isolated::run(
        name,
        "PARROT_NATIVE_TREE_HOST_EXIT",
        Duration::from_secs(15),
        || {
            let runtime = || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
            };
            let host = runtime();
            let stats = Arc::new(Stats::default());
            let factory_drops = Arc::new(AtomicUsize::new(0));
            let (system, actors, paths) = host.block_on(async {
                let system = ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
                    default_scheduling_mode: mode,
                    shared_pool_size: 1,
                    shared_burst_workers_max: 0,
                    ..Default::default()
                });
                let (root, branch, leaf) = if factory_tree {
                    struct Capture(Arc<AtomicUsize>);
                    impl Drop for Capture {
                        fn drop(&mut self) {
                            self.0.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    let factory = || {
                        let capture = Capture(factory_drops.clone());
                        let stats = stats.clone();
                        move || {
                            let _ = &capture;
                            Family::new(stats.clone(), 0)
                        }
                    };
                    let root = system
                        .spawn_typed_factory::<_, Op, _>(factory(), "/host-tree")
                        .await
                        .unwrap();
                    let branch = context(&root)
                        .await
                        .spawn_typed_child_factory::<_, Op, _>(factory())
                        .await
                        .unwrap();
                    let leaf = context(&branch)
                        .await
                        .spawn_typed_child_factory::<_, Op, _>(factory())
                        .await
                        .unwrap();
                    (root, branch, leaf)
                } else {
                    let root = system
                        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/host-tree")
                        .await
                        .unwrap();
                    let branch = child(&root, 0).await;
                    let leaf = child(&branch, 0).await;
                    (root, branch, leaf)
                };
                let (root_path, parent, children) = info(&root).await;
                assert!(parent.is_none());
                assert_eq!(children, 1);
                let (branch_path, parent, children) = info(&branch).await;
                assert_eq!(parent.as_deref(), Some(root_path.as_str()));
                assert_eq!(children, 1);
                let (leaf_path, parent, children) = info(&leaf).await;
                assert_eq!(parent.as_deref(), Some(branch_path.as_str()));
                assert_eq!(children, 0);
                let owners = stats.init_threads.lock().unwrap();
                assert_eq!(owners.len(), 3);
                assert!(
                    owners.iter().all(|(_, thread)| thread.starts_with(prefix)),
                    "{owners:?}"
                );
                (
                    system,
                    [root, branch, leaf],
                    [root_path, branch_path, leaf_path],
                )
            });
            assert_eq!(factory_drops.load(Ordering::SeqCst), 0);
            drop(host);
            runtime().block_on(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    for actor in &actors {
                        loop {
                            use parrot_api::address::RetirementStatus;
                            match actor.retirement_status() {
                                RetirementStatus::Retired(outcome) => {
                                    outcome.unwrap();
                                    break;
                                }
                                RetirementStatus::Pending => tokio::task::yield_now().await,
                                RetirementStatus::Unsupported => {
                                    panic!("managed actor must expose retirement")
                                }
                            }
                        }
                        assert!(!actor.is_alive());
                        assert!(actor.ask(Op::Info).await.is_err());
                    }
                })
                .await
                .expect("original host exit must retire the live native subtree");
                assert_eq!(system.actor_count(), 0);
                assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
                assert_eq!(factory_drops.load(Ordering::SeqCst), if factory_tree { 3 } else { 0 });
                for path in paths {
                    assert_eq!(
                        stats
                            .events
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|(actor, event)| actor == &path && *event == "drop")
                            .count(),
                        1
                    );
                }
                for _ in 0..2 {
                    let result = system.shutdown_internal().await;
                    assert!(matches!(result, Err(parrot::thread::error::SystemError::ShutdownError(ref text))
                        if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{result:?}");
                }
            });
        },
    );
}

#[test]
fn dedicated_typed_subtree_retires_after_host_exit_with_live_references() {
    native_tree_host_exit(
        "dedicated_typed_subtree_retires_after_host_exit_with_live_references",
        parrot::thread::config::SchedulingMode::DedicatedThread,
        "parrot-dedicated-",
        false,
    );
}

#[test]
fn sharded_typed_subtree_retires_after_host_exit_with_live_references() {
    native_tree_host_exit(
        "sharded_typed_subtree_retires_after_host_exit_with_live_references",
        parrot::thread::config::SchedulingMode::Sharded {
            affinity_key: "host-tree".into(),
        },
        "parrot-shard-",
        false,
    );
}

#[test]
fn dedicated_factory_subtree_retires_after_host_exit_with_live_references() {
    native_tree_host_exit(
        "dedicated_factory_subtree_retires_after_host_exit_with_live_references",
        parrot::thread::config::SchedulingMode::DedicatedThread,
        "parrot-dedicated-",
        true,
    );
}

#[test]
fn sharded_factory_subtree_retires_after_host_exit_with_live_references() {
    native_tree_host_exit(
        "sharded_factory_subtree_retires_after_host_exit_with_live_references",
        parrot::thread::config::SchedulingMode::Sharded {
            affinity_key: "host-tree".into(),
        },
        "parrot-shard-",
        true,
    );
}

async fn recursive_retirement(s: Arc<ThreadActorSystem>, placement: Option<(&str, usize)>) {
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 2), "/tree")
        .await
        .unwrap();
    assert_eq!(info(&root).await, ("/tree".into(), None, 1));
    assert_eq!(s.actor_count(), 3);
    let contexts = stats.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    if let Some((prefix, count)) = placement {
        let owners = stats.init_threads.lock().unwrap();
        assert_eq!(owners.len(), 3);
        assert!(
            owners.iter().all(|(_, name)| name.starts_with(prefix)),
            "{owners:?}"
        );
        let threads: std::collections::HashSet<_> = owners.iter().map(|(id, _)| *id).collect();
        assert_eq!(threads.len(), count, "{owners:?}");
    }
    for pair in contexts.windows(2) {
        assert_eq!(pair[1].parent().unwrap().path(), pair[0].path());
        assert!(pair[1].path().starts_with(&format!("{}/", pair[0].path())));
    }
    root.stop().await.unwrap();
    assert_eq!(s.actor_count(), 0);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
    let events = stats.events.lock().unwrap().clone();
    for pair in contexts.windows(2) {
        let child_drop = events
            .iter()
            .position(|(path, event)| path == pair[1].path() && *event == "drop")
            .unwrap();
        let parent_hook = events
            .iter()
            .position(|(path, event)| path == pair[0].path() && *event == "hook")
            .unwrap();
        assert!(child_drop < parent_hook, "{events:?}");
    }
    for ctx in contexts {
        assert!(
            ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
                .await
                .is_err()
        );
    }
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_child_stop_removes_ownership_without_stopping_parent_or_sibling() {
    let s = system();
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
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_parent_initialization_failure_retires_already_created_descendants() {
    let s = system();
    let stats = Arc::new(Stats::default());
    let mut parent = Family::new(stats.clone(), 2);
    parent.fail_init = true;
    let root = s.spawn_typed::<_, Op>(parent, "/init").await.unwrap();
    assert!(root.ask(Op::Info).await.is_err());
    absent(&s, "/init").await;
    assert_eq!(s.actor_count(), 0);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
    assert!(
        !stats
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(p, e)| p == "/init" && *e == "hook")
    );
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_children_inherit_live_resume_policy_without_replaying_failure() {
    let s = system();
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
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_one_for_all_stop_retires_siblings_and_leaves_parent_usable() {
    let s = system();
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
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_explicit_child_policy_overrides_parent_and_escalation_reaches_actual_parent() {
    let s = system();
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
    assert_eq!(s.actor_count(), 0);
    assert!(
        !stats
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(p, e)| p == "/escalate" && *e == "hook")
    );
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_root_restart_retires_old_children_and_old_context_cannot_stop_replacement() {
    restart_with_live_child(system(), None).await;
}

#[tokio::test]
async fn dedicated_factory_restart_replaces_child_and_rejects_stale_context() {
    restart_with_live_child(dedicated_system(), Some("parrot-dedicated-")).await;
}

#[tokio::test]
async fn sharded_factory_restart_replaces_child_and_rejects_stale_context() {
    restart_with_live_child(sharded_system(), Some("parrot-shard-")).await;
}

async fn restart_with_live_child(s: Arc<ThreadActorSystem>, placement: Option<&str>) {
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
    let next = context(&root).await;
    let next_child = next.children().get(0).unwrap().path().to_string();
    assert_ne!(old_child, next_child);
    assert_eq!(s.actor_count(), 2);
    if let Some(prefix) = placement {
        let owners = stats.init_threads.lock().unwrap();
        assert_eq!(owners.len(), 4);
        assert!(
            owners.iter().all(|(_, name)| name.starts_with(prefix)),
            "{owners:?}"
        );
        assert_eq!(owners[0].0, owners[2].0, "factory parent changed worker");
        if prefix == "parrot-shard-" {
            assert!(owners.iter().all(|(id, _)| *id == owners[0].0));
        } else {
            assert_ne!(owners[1].0, owners[0].0);
            assert_ne!(owners[3].0, owners[2].0);
            assert_ne!(
                owners[1].0, owners[3].0,
                "replacement reused the retired child worker"
            );
        }
    }
    assert!(
        old.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
            .await
            .is_err()
    );
    old.stop();
    assert_eq!(info(&root).await.0, "/replace");
    assert_eq!(next.children().get(0).unwrap().path(), next_child);
    root.stop().await.unwrap();
    assert_eq!(s.actor_count(), 0);
    s.shutdown_internal().await.unwrap();
}

#[tokio::test]
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
    let s = system();
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
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn nested_typed_initialization_and_stop_make_progress_with_one_execution_permit() {
    let s = ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
        shared_pool_size: 1,
        default_ask_timeout: Duration::from_secs(2),
        ..Default::default()
    });
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 5), "/one-permit")
        .await
        .unwrap();
    assert_eq!(info(&root).await.2, 1);
    assert_eq!(s.actor_count(), 6);
    tokio::time::timeout(Duration::from_secs(3), root.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.drops.load(Ordering::SeqCst), 6);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_child_failure_is_not_a_parent_cleanup_failure() {
    let s = system();
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
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn typed_context_retention_does_not_keep_system_alive_and_rejection_disposes_input() {
    let s = system();
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
#[tokio::test]
async fn typed_child_capacity_is_released_only_by_actual_retirement() {
    let s = system();
    let stats = Arc::new(Stats::default());
    let root = s
        .spawn_typed::<_, Op>(Family::new(stats.clone(), 0), "/capacity")
        .await
        .unwrap();
    let ctx = context(&root).await;
    let gate = Arc::new(DropGate::default());
    let _release = ReleaseGate(gate.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let blocked = ctx
        .spawn_typed_child::<_, Ping>(BlockedChild {
            gate: gate.clone(),
            drops: drops.clone(),
        })
        .await
        .unwrap();
    blocked.ask(Ping).await.unwrap();
    let mut children = Vec::new();
    for _ in 0..1023 {
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
    assert_eq!(s.actor_count(), 1025);
    let mut stop = Box::pin(blocked.stop());
    assert!(futures::poll!(&mut stop).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert!(futures::poll!(&mut stop).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(ctx.children().len(), 1024);
    assert_eq!(s.actor_count(), 1025);
    assert!(matches!(
        ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0)).await,
        Err(ActorError::InternalError(message))
            if message == "child capacity exhausted (1024 live children or pending termination notifications)"
    ));
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), stop)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    // The stop receipt may wake us before the separate notification owner
    // releases its capacity. Only that documented rejection permits a retry.
    let replacement = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match ctx
                .spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
                .await
            {
                Ok(actor) => break actor,
                Err(ActorError::InternalError(message))
                    if message == "child capacity exhausted (1024 live children or pending termination notifications)" =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected child admission failure: {error:?}"),
            }
        }
    })
    .await
    .expect("retired child did not release capacity");
    assert_eq!(ctx.children().len(), 1024);
    assert!(replacement.is_alive());
    root.stop().await.unwrap();
    assert_eq!(s.actor_count(), 0);
    s.shutdown_internal().await.unwrap();
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

#[test]
fn named_instance_and_factory_retire_on_original_engine_after_host_exit() {
    use parrot::thread::config::SchedulingMode;
    isolated::run(
        "named_instance_and_factory_retire_on_original_engine_after_host_exit",
        "PARROT_NAMED_TREE_HOST_EXIT",
        Duration::from_secs(40),
        || {
            for mode in [
                SchedulingMode::DedicatedThread,
                SchedulingMode::Sharded {
                    affinity_key: "replacement".into(),
                },
            ] {
                for factory in [false, true] {
                    check_named_host_exit(mode.clone(), factory);
                }
            }
        },
    );
}

fn check_named_host_exit(mode: parrot::thread::config::SchedulingMode, factory: bool) {
    use parrot::system::ParrotActorSystem;
    use parrot_api::address::RetirementStatus;

    let make_system = || {
        ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
            default_scheduling_mode: mode.clone(),
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            ..Default::default()
        })
    };
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let host = runtime();
    let observer = runtime();
    let gate = Arc::new(DropGate::default());
    let _release = ReleaseGate(gate.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let old_stats = Arc::new(Stats::default());
    let (facade, original, root, child) = host.block_on(async {
        let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
        let original = make_system();
        facade
            .register_thread_system("named".into(), original.clone(), false)
            .await
            .unwrap();
        let stats = old_stats.clone();
        let root = if factory {
            facade
                .spawn_typed_factory_on::<_, Op, _>(
                    "named",
                    move || Family::new(stats.clone(), 0),
                    "/same",
                )
                .await
        } else {
            facade
                .spawn_typed_on::<_, Op>("named", Family::new(stats, 0), "/same")
                .await
        }
        .unwrap();
        let child = context(&root)
            .await
            .spawn_typed_child::<_, Ping>(BlockedChild {
                gate: gate.clone(),
                drops: drops.clone(),
            })
            .await
            .unwrap();
        child.ask(Ping).await.unwrap();
        assert_eq!(info(&root).await, ("/same".into(), None, 1));
        (facade, original, root, child)
    });

    let new_stats = Arc::new(Stats::default());
    let replacement = observer.block_on(async {
        facade
            .register_thread_system("named".into(), make_system(), false)
            .await
            .unwrap();
        let new = facade
            .spawn_typed_on::<_, Op>("named", Family::new(new_stats.clone(), 0), "/same")
            .await
            .unwrap();
        assert_eq!(info(&new).await, ("/same".into(), None, 0));
        assert_eq!(info(&root).await.2, 1);
        new
    });
    drop(host);
    observer.block_on(async {
                        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await.unwrap();
                        let root_stop = root.stop();
                        let child_stop = child.stop();
                        tokio::pin!(root_stop, child_stop);
                        assert!(futures::poll!(&mut root_stop).is_pending());
                        assert!(futures::poll!(&mut child_stop).is_pending());
                        assert!(matches!(child.retirement_status(), RetirementStatus::Pending));
                        assert_eq!(drops.load(Ordering::SeqCst), 0);
                        assert_eq!(info(&replacement).await, ("/same".into(), None, 0));
                        gate.release();
                        let (parent_result, child_result) = tokio::time::timeout(Duration::from_secs(4), async {
                            tokio::join!(root_stop, child_stop)
                        }).await.unwrap();
                        for (result, path) in [(parent_result, root.path()), (child_result, child.path())] {
                            let error = result.unwrap_err();
                            let ActorError::Other(source) = error else { panic!("{error:?}") };
                            let Some(parrot::thread::error::SystemError::ActorCreationError(detail)) =
                                source.downcast_ref::<parrot::thread::error::SystemError>() else { panic!("{source:?}") };
                            let prefix = format!("typed actor {path} execution failed without a disposal receipt: Err(JoinError::Cancelled(Id(");
                            assert!(detail.strip_prefix(&prefix).and_then(|text| text.strip_suffix(")))"))
                                .is_some_and(|id| id.parse::<u64>().is_ok()), "{detail}");
                        }
                        assert!(matches!(root.retirement_status(), RetirementStatus::Retired(Ok(()))));
                        assert!(matches!(child.retirement_status(), RetirementStatus::Retired(Ok(()))));
                        assert_eq!(drops.load(Ordering::SeqCst), 1);
                        assert_eq!(old_stats.drops.load(Ordering::SeqCst), 1);
                        assert_eq!(original.actor_count(), 0);
                        assert!(root.ask(Op::Info).await.is_err());
                        assert!(child.ask(Ping).await.is_err());
                        assert_eq!(info(&replacement).await, ("/same".into(), None, 0));
                        assert_eq!(new_stats.drops.load(Ordering::SeqCst), 0);
                        replacement.stop().await.unwrap();
                        assert_eq!(new_stats.drops.load(Ordering::SeqCst), 1);
                        facade.shutdown_runtime().await.unwrap();
                        let shutdown = original.shutdown_internal().await;
                        assert!(matches!(&shutdown,
                            Err(parrot::thread::error::SystemError::ShutdownError(message))
                            if message == "1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{shutdown:?}");
                    });
}

#[tokio::test]
async fn typed_parent_timeout_keeps_both_paths_until_blocked_child_really_retires() {
    let s = ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(80),
        ..Default::default()
    });
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
    assert_eq!(s.actor_count(), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 0);
    assert!(
        ctx.spawn_typed_child::<_, Op>(Family::new(stats.clone(), 0))
            .await
            .is_err()
    );
    gate.release();
    absent(&s, "/blocked").await;
    assert_eq!(s.actor_count(), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    s.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn racing_typed_child_admission_and_parent_stop_leaves_no_orphans() {
    let s = system();
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
    assert_eq!(s.actor_count(), 0);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 17);
    s.shutdown_internal().await.unwrap();
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
#[tokio::test]
async fn typed_parent_retirement_preserves_child_stop_hook_error_and_panic() {
    for hook_panic in [false, true] {
        let s = system();
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
        assert_eq!(s.actor_count(), 0);
        assert!(
            !stats
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|(p, e)| p == "/hook" && *e == "hook")
        );
        s.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn typed_parent_retirement_preserves_child_destructor_failure() {
    let s = system();
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
    assert_eq!(s.actor_count(), 0);
    s.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_grandparent_retirement_preserves_descendant_cleanup_failure() {
    for (hook_panic, drop_panic, depth) in [
        (false, false, 1),
        (true, false, 1),
        (false, true, 1),
        (false, false, 3),
        (true, false, 3),
        (false, true, 3),
    ] {
        let s = system();
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
        assert_eq!(s.actor_count(), 0);
        assert!(
            !stats
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|(_, event)| *event == "hook")
        );
        s.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn typed_factory_cannot_replace_a_generation_with_failed_descendant_retirement() {
    let s = system();
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
    assert_eq!(s.actor_count(), 0);
    s.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_child_escalation_reconstructs_repeatable_parent_through_its_own_policy() {
    let s = system();
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
    assert_eq!(s.actor_count(), 0);
    assert!(
        !stats
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(p, e)| p == "/escalated-factory" && *e == "hook")
    );
    s.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn unsupported_context_rejection_isolates_actor_and_policy_destructors() {
    struct Foreign;
    impl parrot_api::typed_context::TypedContextServices for Foreign {
        fn path(&self) -> &str {
            "/foreign"
        }
        fn actor_ref(&self) -> Option<parrot_api::types::BoxedActorRef> {
            None
        }
        fn parent(&self) -> Option<parrot_api::types::BoxedActorRef> {
            None
        }
        fn children(&self) -> parrot_api::context::ReadOnlyChildrenVec {
            parrot_api::context::ReadOnlyChildrenVec::new(Arc::new(std::sync::RwLock::new(
                Vec::new(),
            )))
        }
        fn stop(&self) {}
        fn set_child_strategy(&self, _: SupervisorStrategyType) {}
    }
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("unaccepted policy destructor");
        }
    }
    let ctx = TypedContext::from_services(Arc::new(Foreign));
    let dropped = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let capture = Capture(captures.clone());
    let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(1),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    });
    assert!(
        ctx.spawn_typed_child_with_strategy::<_, Ping>(
            BadCleanup {
                hook_panic: false,
                drop_panic: true,
                drops: dropped.clone()
            },
            strategy
        )
        .await
        .is_err()
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
}

#[test]
fn host_shutdown_isolates_parent_and_child_policy_capture_panics() {
    const NAME: &str = "host_shutdown_isolates_parent_and_child_policy_capture_panics";
    const CHILD: &str = "PARROT_TYPED_TREE_POLICY_SHUTDOWN_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(NAME) {
        struct Capture(Arc<AtomicUsize>);
        impl Drop for Capture {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("host cancelled supervision capture");
            }
        }
        fn policy(count: &Arc<AtomicUsize>) -> SupervisorStrategyType {
            let capture = Capture(count.clone());
            SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 0,
                within: Duration::from_secs(1),
                decider: BasicDecisionFn::new(move |_| {
                    let _ = &capture;
                    SupervisionDecision::Stop
                }),
            })
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stats = Arc::new(Stats::default());
        let captures = Arc::new(AtomicUsize::new(0));
        let (system, actors, paths) = runtime.block_on(async {
            let system = system();
            let actor = system
                .spawn_typed_with_strategy::<_, Op>(
                    Family::new(stats.clone(), 0),
                    "/policy-shutdown",
                    policy(&captures),
                )
                .await
                .unwrap();
            actor.ask(Op::Policy(policy(&captures))).await.unwrap();
            let branch = child(&actor, 0).await;
            let leaf = child(&branch, 0).await;
            let paths = [
                info(&actor).await.0,
                info(&branch).await.0,
                info(&leaf).await.0,
            ];
            assert_eq!(info(&actor).await.2, 1);
            assert_eq!(info(&branch).await.1, Some(paths[0].clone()));
            assert_eq!(info(&branch).await.2, 1);
            assert_eq!(info(&leaf).await.1, Some(paths[1].clone()));
            assert_eq!(system.actor_count(), 3);
            (system, [actor, branch, leaf], paths)
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
        println!("POLICY_HOST_STOPPED");
        drop(system);
        stats.contexts.lock().unwrap().clear();
        // Final system release hands cleanup to the independent lifecycle
        // owner. Its receipt must follow both isolated policy destructors.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match actors[0].retirement_status() {
                parrot_api::address::RetirementStatus::Retired(result) => {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("host cancelled supervision capture")
                    );
                    break;
                }
                parrot_api::address::RetirementStatus::Pending => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "host exit lost the typed actor's retirement owner"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                parrot_api::address::RetirementStatus::Unsupported => {
                    panic!("managed actor must report actual retirement");
                }
            }
        }
        for actor in &actors[1..] {
            assert!(matches!(
                actor.retirement_status(),
                parrot_api::address::RetirementStatus::Retired(_)
            ));
        }
        assert!(actors.iter().all(|actor| !actor.is_alive()));
        assert_eq!(captures.load(Ordering::SeqCst), 2);
        assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
        let events = stats.events.lock().unwrap().clone();
        // Host cancellation releases finite inputs, but cannot run normal
        // asynchronous stop hooks or impose child-before-parent destruction.
        for path in &paths {
            for event in ["init", "drop"] {
                assert_eq!(
                    events
                        .iter()
                        .filter(|(p, e)| p == path && *e == event)
                        .count(),
                    1,
                    "{path}: expected one {event}, got {events:?}"
                );
            }
        }
        drop(actors);
        println!("POLICY_HOST_RETIRED");
        return;
    }
    use std::process::{Command, Stdio};
    let log = std::env::temp_dir().join(format!("parrot-tree-policy-{}.log", uuid::Uuid::new_v4()));
    let output = std::fs::File::create(&log).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture"])
        .env(CHILD, NAME)
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output))
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
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(&log).unwrap();
    std::fs::remove_file(log).unwrap();
    assert!(!timed_out, "child timed out: {output}");
    assert!(output.contains("POLICY_HOST_STOPPED"), "{output}");
    assert!(status.success(), "child {status}: {output}");
    assert!(output.contains("POLICY_HOST_RETIRED"), "{output}");
}

mod descendant_sources {
    use super::*;
    use std::error::Error;
    enum Backend {
        Actix,
        Shared,
        Dedicated,
        Sharded,
    }
    use Backend::*;
    struct Spec {
        formats: AtomicUsize,
        sources: AtomicUsize,
        hooks: AtomicUsize,
        actor_drops: AtomicUsize,
        fail: bool,
        hostile_display: bool,
        entered: tokio::sync::Notify,
        gate: tokio::sync::Semaphore,
    }
    struct Business(Arc<Spec>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("DescendantBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.0.hostile_display,
                "descendant Display must not execute during retirement"
            );
            f.write_str("original descendant business failure")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("framework must not traverse unknown business sources");
        }
    }
    struct Leaf(Arc<Spec>);
    impl Drop for Leaf {
        fn drop(&mut self) {
            self.0.actor_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl ParrotTypedDispatch for Leaf {
        type Msg = ();
        type Reply = ();
        fn dispatch(&mut self, _: ()) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                self.0.hooks.fetch_add(1, Ordering::SeqCst);
                self.0.entered.notify_one();
                let _permit = self.0.gate.acquire().await.unwrap();
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
    impl ParrotMsgVariant<Leaf> for Ping {
        fn inject(_: Self) {}
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    fn original(error: &ActorError, spec: &Arc<Spec>) {
        let mut source = Some(error as &(dyn Error + 'static));
        let mut found = false;
        for _ in 0..128 {
            let Some(value) = source else { break };
            if let Some(business) = value.downcast_ref::<Business>() {
                assert!(Arc::ptr_eq(&business.0, spec));
                found = true;
                break;
            }
            source = value.source();
        }
        assert!(
            found,
            "descendant's concrete business error/source identity was replaced by text"
        );
    }
    async fn run(
        backend: Backend,
        factory: bool,
        depth: usize,
        hostile_display: bool,
        cancel: bool,
        fail: bool,
    ) {
        let spec = Arc::new(Spec {
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
            hooks: AtomicUsize::new(0),
            actor_drops: AtomicUsize::new(0),
            fail,
            hostile_display,
            entered: tokio::sync::Notify::new(),
            gate: tokio::sync::Semaphore::new(0),
        });
        let stats = Arc::new(Stats::default());
        let mut actix_system = None;
        let thread = match backend {
            Actix => None,
            Shared => Some(system()),
            Dedicated => Some(dedicated_system()),
            Sharded => Some(sharded_system()),
        };
        let root = if let Some(system) = &thread {
            if factory {
                let stats = stats.clone();
                system
                    .spawn_typed_factory::<_, Op, _>(
                        move || Family::new(stats.clone(), depth),
                        "/source-tree",
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed::<_, Op>(Family::new(stats.clone(), depth), "/source-tree")
                    .await
                    .unwrap()
            }
        } else {
            let system = Arc::new(
                parrot::actix::ActixActorSystem::with_arbiter_count(1)
                    .await
                    .unwrap(),
            );
            let root = if factory {
                let stats = stats.clone();
                system
                    .spawn_typed_factory::<_, Op, _>(
                        move || Family::new(stats.clone(), depth),
                        "/source-tree",
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed::<_, Op>(Family::new(stats.clone(), depth), "/source-tree")
                    .await
                    .unwrap()
            };
            actix_system = Some(system);
            root
        };
        info(&root).await;
        let placement = match backend {
            Dedicated => Some(("parrot-dedicated-", depth + 1)),
            Sharded => Some(("parrot-shard-", 1)),
            _ => None,
        };
        if let Some((prefix, count)) = placement {
            let owners = stats.init_threads.lock().unwrap();
            assert_eq!(owners.len(), depth + 1);
            assert!(
                owners.iter().all(|(_, name)| name.starts_with(prefix)),
                "{owners:?}"
            );
            let threads: std::collections::HashSet<_> = owners.iter().map(|(id, _)| *id).collect();
            assert_eq!(threads.len(), count, "{owners:?}");
        }
        let parent = stats.contexts.lock().unwrap().last().unwrap().clone();
        let leaf = if factory {
            let spec = spec.clone();
            parent
                .spawn_typed_child_factory::<_, Ping, _>(move || Leaf(spec.clone()))
                .await
                .unwrap()
        } else {
            parent
                .spawn_typed_child::<_, Ping>(Leaf(spec.clone()))
                .await
                .unwrap()
        };
        leaf.ask(Ping).await.unwrap();
        let mut first = Box::pin(root.stop());
        let mut second = Box::pin(root.stop());
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        tokio::time::timeout(Duration::from_secs(3), spec.entered.notified())
            .await
            .unwrap();
        let first = if cancel {
            drop(first);
            assert!(futures::poll!(&mut second).is_pending());
            assert_eq!(spec.actor_drops.load(Ordering::SeqCst), 0);
            assert_eq!(stats.drops.load(Ordering::SeqCst), 0);
            None
        } else {
            Some(first)
        };
        spec.gate.add_permits(1);
        let (first, second) = tokio::time::timeout(Duration::from_secs(3), async {
            let first = match first {
                Some(first) => Some(first.await),
                None => None,
            };
            (first, second.await)
        })
        .await
        .expect("descendant retirement lost its real completion receipt");
        let late = root.stop().await;
        let leaf_result = leaf.stop().await;
        assert_eq!(spec.hooks.load(Ordering::SeqCst), 1);
        assert_eq!(spec.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(stats.drops.load(Ordering::SeqCst), depth + 1);
        if let Some(system) = &thread {
            assert_eq!(system.actor_count(), 0);
            system.shutdown_internal().await.unwrap();
        }
        if let Some(system) = &actix_system {
            system.as_ref().clone().shutdown().await.unwrap();
        }
        if fail {
            original(
                late.as_ref().expect_err("late root waiter lost failure"),
                &spec,
            );
            original(
                leaf_result.as_ref().expect_err("leaf waiter lost failure"),
                &spec,
            );
            if let Some(result) = &first {
                original(
                    result
                        .as_ref()
                        .expect_err("parent must report failed descendant retirement"),
                    &spec,
                );
            }
            original(
                second
                    .as_ref()
                    .expect_err("parent must report failed descendant retirement"),
                &spec,
            );
        } else {
            assert!(late.is_ok());
            assert!(leaf_result.is_ok());
            assert!(first.as_ref().is_none_or(|r| r.is_ok()));
            assert!(second.is_ok());
        }
        assert_eq!(
            spec.formats.load(Ordering::SeqCst),
            0,
            "lifecycle invoked business Display"
        );
        assert_eq!(
            spec.sources.load(Ordering::SeqCst),
            0,
            "lifecycle traversed unknown business source"
        );
    }
    #[actix::test]
    async fn thread_instance_depth_1_retains_source() {
        run(Shared, false, 1, false, false, true).await;
    }
    #[actix::test]
    async fn thread_instance_depth_3_retains_source() {
        run(Shared, false, 3, false, false, true).await;
    }
    #[actix::test]
    async fn thread_factory_depth_1_retains_source() {
        run(Shared, true, 1, false, false, true).await;
    }
    #[actix::test]
    async fn thread_factory_depth_3_retains_source() {
        run(Shared, true, 3, false, false, true).await;
    }
    #[actix::test]
    async fn thread_hostile_display() {
        run(Shared, false, 2, true, false, true).await;
    }
    #[actix::test]
    async fn thread_cancelled_waiter() {
        run(Shared, false, 2, false, true, true).await;
    }
    #[tokio::test]
    async fn dedicated_instance_cancelled_waiter() {
        run(Dedicated, false, 2, false, true, true).await;
    }
    #[tokio::test]
    async fn sharded_instance_cancelled_waiter() {
        run(Sharded, false, 2, false, true, true).await;
    }
    #[tokio::test]
    async fn dedicated_factory_cancelled_waiter() {
        run(Dedicated, true, 2, false, true, true).await;
    }
    #[tokio::test]
    async fn sharded_factory_cancelled_waiter() {
        run(Sharded, true, 2, false, true, true).await;
    }
    #[actix::test]
    async fn thread_healthy() {
        run(Shared, false, 2, false, false, false).await;
    }
    #[actix::test]
    async fn actix_instance_depth_1_retains_source() {
        run(Actix, false, 1, false, false, true).await;
    }
    #[actix::test]
    async fn actix_instance_depth_3_retains_source() {
        run(Actix, false, 3, false, false, true).await;
    }
    #[actix::test]
    async fn actix_factory_depth_1_retains_source() {
        run(Actix, true, 1, false, false, true).await;
    }
    #[actix::test]
    async fn actix_factory_depth_3_retains_source() {
        run(Actix, true, 3, false, false, true).await;
    }
    #[actix::test]
    async fn actix_hostile_display() {
        run(Actix, false, 2, true, false, true).await;
    }
    #[actix::test]
    async fn actix_cancelled_waiter() {
        run(Actix, false, 2, false, true, true).await;
    }
    #[actix::test]
    async fn actix_healthy() {
        run(Actix, false, 2, false, false, false).await;
    }
    #[actix::test]
    async fn thread_factory_hostile_display() {
        run(Shared, true, 2, true, false, true).await;
    }
    #[actix::test]
    async fn thread_factory_cancelled_waiter() {
        run(Shared, true, 2, false, true, true).await;
    }
    #[actix::test]
    async fn thread_factory_healthy() {
        run(Shared, true, 2, false, false, false).await;
    }
    #[actix::test]
    async fn actix_factory_hostile_display() {
        run(Actix, true, 2, true, false, true).await;
    }
    #[actix::test]
    async fn actix_factory_cancelled_waiter() {
        run(Actix, true, 2, false, true, true).await;
    }
    #[actix::test]
    async fn actix_factory_healthy() {
        run(Actix, true, 2, false, false, false).await;
    }
}
