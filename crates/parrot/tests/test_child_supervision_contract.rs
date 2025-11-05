//! Existing public strategy APIs must execute their advertised policy.
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt},
    supervisor::{
        BasicDecisionFn, OneForAllStrategy, OneForOneStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Crash;
struct Child;
impl Actor for Child {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(barrier) = msg.downcast_ref::<Arc<tokio::sync::Barrier>>() {
                barrier.wait().await;
                panic!("simultaneous child failure");
            }
            assert!(!msg.is::<Crash>(), "child crash");
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
struct Parent {
    calls: Arc<AtomicUsize>,
    group: bool,
}
impl Actor for Parent {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let calls = self.calls.clone();
            let decider = BasicDecisionFn::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                SupervisionDecision::Stop
            });
            let mut children = Vec::<BoxedActorRef>::new();
            if self.group {
                ctx.set_supervisor_strategy(SupervisorStrategyType::OneForAll(OneForAllStrategy {
                    max_restarts: 3,
                    within: Duration::from_secs(30),
                    decider,
                }));
                for _ in 0..2 {
                    children.push(ctx.spawner().spawn_typed(Child, EmptyConfig).await?);
                }
            } else {
                children.push(
                    ctx.spawner()
                        .spawn_supervised(
                            Child,
                            EmptyConfig,
                            SupervisorStrategyType::OneForOne(OneForOneStrategy {
                                max_restarts: 3,
                                within: Duration::from_secs(30),
                                decider,
                            }),
                        )
                        .await?,
                );
            }
            for child in &children {
                child.send(Box::new(())).await?;
            }
            Ok(Box::new(children) as BoxedMessage)
        })
    }
}
async fn removed(sys: &parrot::thread::ThreadActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.get_actor_ref(path).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor did not retire");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_for_one_calls_the_supplied_decider() {
    let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let parent = sys
        .spawn_at(
            Parent {
                calls: calls.clone(),
                group: false,
            },
            "/parent",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let children = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<Vec<BoxedActorRef>>()
        .unwrap();
    let _ = children[0].send(Box::new(Crash)).await;
    removed(&sys, &children[0].path()).await;
    let count = calls.load(Ordering::SeqCst);
    sys.shutdown_internal().await.unwrap();
    assert_eq!(
        count, 1,
        "real failure must invoke the configured decider exactly once"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_for_all_stop_policy_also_stops_the_healthy_sibling() {
    let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = sys
        .spawn_at(
            Parent {
                calls: Arc::new(AtomicUsize::new(0)),
                group: true,
            },
            "/parent",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let children = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<Vec<BoxedActorRef>>()
        .unwrap();
    let _ = children[0].send(Box::new(Crash)).await;
    removed(&sys, &children[0].path()).await;
    let sibling_stopped = tokio::time::timeout(Duration::from_secs(1), async {
        while sys.get_actor_ref(&children[1].path()).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    sys.shutdown_internal().await.unwrap();
    assert!(sibling_stopped, "OneForAll Stop must retire every sibling");
}

use parrot_api::actor::ActixEngineExt;
use std::sync::{Condvar, Mutex};
struct ActixChild;
impl ActixEngineExt for ActixChild {}
impl Actor for ActixChild {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(barrier) = msg.downcast_ref::<Arc<tokio::sync::Barrier>>() {
                barrier.wait().await;
                panic!("simultaneous child failure");
            }
            assert!(!msg.is::<Crash>(), "child crash");
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
struct ThreadPolicyParent(Option<SupervisorStrategyType>);
struct ActixPolicyParent(Option<SupervisorStrategyType>);
impl ActixEngineExt for ActixPolicyParent {}
macro_rules! policy_parent {
    ($actor:ident, $context:ty, $child:ident) => {
        impl Actor for $actor {
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
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    ctx.set_supervisor_strategy(self.0.take().unwrap());
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    let count = *msg.downcast::<u32>().unwrap();
                    let mut children = Vec::<BoxedActorRef>::new();
                    for _ in 0..count {
                        children.push(ctx.spawner().spawn_typed($child, EmptyConfig).await?);
                    }
                    for child in &children {
                        child.send(Box::new(())).await?;
                    }
                    Ok(Box::new(children) as BoxedMessage)
                })
            }
        }
    };
}
policy_parent!(
    ThreadPolicyParent,
    parrot::thread::ThreadContext<Self>,
    Child
);
policy_parent!(
    ActixPolicyParent,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>,
    ActixChild
);
enum Engine {
    Thread(Arc<parrot::thread::ThreadActorSystem>),
    Actix(parrot::actix::ActixActorSystem),
}
impl Engine {
    async fn new(actix: bool, timeout: Duration) -> Self {
        if actix {
            Self::Actix(
                parrot::actix::ActixActorSystem::with_settings(
                    parrot::actix::ActixSystemSettings {
                        arbiter_count: 1,
                        shutdown_timeout: timeout,
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            )
        } else {
            Self::Thread(parrot::thread::ThreadActorSystem::shared(
                parrot::thread::config::ThreadActorSystemConfig {
                    shutdown_timeout: timeout,
                    ..Default::default()
                },
            ))
        }
    }
    async fn parent(&self, strategy: SupervisorStrategyType) -> BoxedActorRef {
        match self {
            Self::Thread(sys) => Box::new(
                sys.spawn_at(
                    ThreadPolicyParent(Some(strategy)),
                    "/parent",
                    None,
                    Default::default(),
                )
                .await
                .unwrap(),
            ),
            Self::Actix(sys) => sys
                .spawn_root_typed(ActixPolicyParent(Some(strategy)), EmptyConfig)
                .await
                .unwrap(),
        }
    }
    async fn exists(&self, path: &str) -> bool {
        match self {
            Self::Thread(sys) => sys.get_actor_ref(path).is_some(),
            Self::Actix(sys) => sys.get_actor(&path.to_owned()).await.is_some(),
        }
    }
    async fn absent(&self, path: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.exists(path).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("policy cleanup retained actor indefinitely");
    }
    async fn shutdown(self) {
        match self {
            Self::Thread(sys) => sys.shutdown_internal().await.unwrap(),
            Self::Actix(sys) => sys.shutdown().await.unwrap(),
        }
    }
}
fn stop_policy(calls: Arc<AtomicUsize>, all: bool) -> SupervisorStrategyType {
    let decider = BasicDecisionFn::new(move |error| {
        assert!(matches!(error, parrot_api::errors::ActorError::Panic(_)));
        calls.fetch_add(1, Ordering::SeqCst);
        SupervisionDecision::Stop
    });
    policy(decider, all)
}
fn policy(decider: BasicDecisionFn, all: bool) -> SupervisorStrategyType {
    if all {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 3,
            within: Duration::from_secs(30),
            decider,
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 3,
            within: Duration::from_secs(30),
            decider,
        })
    }
}
async fn children(parent: &dyn ActorRef, count: u32) -> Vec<BoxedActorRef> {
    *parent
        .send(Box::new(count))
        .await
        .unwrap()
        .downcast::<Vec<BoxedActorRef>>()
        .unwrap()
}
#[actix::test]
async fn both_engines_execute_inherited_decider_and_one_for_all_stop() {
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_secs(2)).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let parent = sys.parent(stop_policy(calls.clone(), true)).await;
        let children = children(&*parent, 2).await;
        let _ = children[0].send(Box::new(Crash)).await;
        for child in &children {
            sys.absent(&child.path()).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        parent.send(Box::new(0_u32)).await.unwrap();
        sys.shutdown().await;
    }
}
#[actix::test]
async fn one_for_one_keeps_healthy_sibling_alive() {
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_secs(2)).await;
        let parent = sys
            .parent(stop_policy(Arc::new(AtomicUsize::new(0)), false))
            .await;
        let children = children(&*parent, 2).await;
        let _ = children[0].send(Box::new(Crash)).await;
        sys.absent(&children[0].path()).await;
        children[1].send(Box::new(())).await.unwrap();
        sys.shutdown().await;
    }
}
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    wake: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.wake.wait(open).unwrap();
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.wake.notify_all();
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.entered.notified())
            .await
            .unwrap();
    }
}
#[actix::test]
async fn blocked_decider_preserves_stop_deadline_and_retirement_ownership() {
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_millis(100)).await;
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGate(gate.clone());
        let captured = gate.clone();
        let parent = sys
            .parent(policy(
                BasicDecisionFn::new(move |_| {
                    captured.block();
                    SupervisionDecision::Stop
                }),
                true,
            ))
            .await;
        let children = children(&*parent, 2).await;
        let _ = children[0].send(Box::new(Crash)).await;
        gate.entered().await;
        // Decider must hold neither actor dispatch nor registry locks.
        parent.send(Box::new(0_u32)).await.unwrap();
        let stopped = parent.stop().await;
        let retained = sys.exists(&parent.path()).await && sys.exists(&children[0].path()).await;
        gate.release();
        assert!(stopped.is_err());
        assert!(retained);
        for child in &children {
            sys.absent(&child.path()).await;
        }
        sys.absent(&parent.path()).await;
        sys.shutdown().await;
    }
}
#[actix::test]
async fn decider_panic_does_not_abandon_actor_or_poison_the_parent() {
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_secs(2)).await;
        let parent = sys
            .parent(policy(
                BasicDecisionFn::new(|_| panic!("decider panic")),
                false,
            ))
            .await;
        let children = children(&*parent, 1).await;
        let _ = children[0].send(Box::new(Crash)).await;
        sys.absent(&children[0].path()).await;
        parent.send(Box::new(0_u32)).await.unwrap();
        sys.shutdown().await;
    }
}
struct ReleaseGate(Arc<Gate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Capture {
    gate: Option<Arc<Gate>>,
    panic: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        assert!(!self.panic, "policy destructor panic");
    }
}
#[actix::test]
async fn policy_capture_disposal_is_part_of_parent_retirement() {
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_millis(100)).await;
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGate(gate.clone());
        let captured = Capture {
            gate: Some(gate.clone()),
            panic: false,
        };
        let parent = sys
            .parent(policy(
                BasicDecisionFn::new(move |_| {
                    let _ = &captured;
                    SupervisionDecision::Stop
                }),
                false,
            ))
            .await;
        parent.send(Box::new(0_u32)).await.unwrap();
        let stopped = parent.stop().await;
        let retained = sys.exists(&parent.path()).await;
        gate.release();
        assert!(stopped.is_err());
        assert!(retained);
        sys.absent(&parent.path()).await;
        sys.shutdown().await;
    }
}
#[actix::test]
async fn policy_capture_destructor_panic_is_reported_without_losing_cleanup() {
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_secs(2)).await;
        let captured = Capture {
            gate: None,
            panic: true,
        };
        let parent = sys
            .parent(policy(
                BasicDecisionFn::new(move |_| {
                    let _ = &captured;
                    SupervisionDecision::Stop
                }),
                false,
            ))
            .await;
        parent.send(Box::new(0_u32)).await.unwrap();
        assert!(parent.stop().await.is_err());
        sys.absent(&parent.path()).await;
        sys.shutdown().await;
    }
}
#[actix::test]
async fn simultaneous_one_for_all_failures_do_not_join_each_other() {
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_secs(2)).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let parent = sys.parent(stop_policy(calls.clone(), true)).await;
        let children = children(&*parent, 2).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let (first, second, _) = tokio::join!(
            children[0].send(Box::new(barrier.clone())),
            children[1].send(Box::new(barrier.clone())),
            barrier.wait()
        );
        assert!(first.is_err() && second.is_err());
        for child in &children {
            sys.absent(&child.path()).await;
        }
        assert!((1..=2).contains(&calls.load(Ordering::SeqCst)));
        sys.shutdown().await;
    }
}

struct Branch {
    initialization: u8,
    strategy: Option<SupervisorStrategyType>,
    terminated: Arc<AtomicUsize>,
}
struct ThreadTree(Branch);
struct ActixTree(Branch);
impl ActixEngineExt for ActixTree {}
macro_rules! tree_actor {
    ($actor:ident, $context:ty) => {
        impl Actor for $actor {
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
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    if let Some(strategy) = self.0.strategy.take() {
                        ctx.set_supervisor_strategy(strategy);
                    }
                    assert_ne!(self.0.initialization, 2, "tree init panic");
                    if self.0.initialization == 1 {
                        return Err(parrot_api::errors::ActorError::InitializationError(
                            "tree init error".into(),
                        ));
                    }
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    assert!(!msg.is::<Crash>(), "tree child crash");
                    if let Ok(branch) = msg.downcast::<Branch>() {
                        let failing = branch.initialization != 0;
                        let child = ctx
                            .spawner()
                            .spawn_typed($actor(*branch), EmptyConfig)
                            .await?;
                        if !failing {
                            child.send(Box::new(())).await?;
                        }
                        return Ok(Box::new(child) as BoxedMessage);
                    }
                    Ok(Box::new(()) as BoxedMessage)
                })
            }
            fn handle_child_terminated<'a>(
                &'a mut self,
                _: BoxedActorRef,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.terminated.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }
        }
    };
}
tree_actor!(ThreadTree, parrot::thread::ThreadContext<Self>);
tree_actor!(
    ActixTree,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
impl Engine {
    async fn tree(&self, branch: Branch) -> BoxedActorRef {
        match self {
            Self::Thread(sys) => sys
                .spawn_at(ThreadTree(branch), "/tree", None, Default::default())
                .await
                .unwrap()
                .clone_boxed(),
            Self::Actix(sys) => sys
                .spawn_root_typed(ActixTree(branch), EmptyConfig)
                .await
                .unwrap(),
        }
    }
}
async fn branch(
    parent: &dyn ActorRef,
    strategy: Option<SupervisorStrategyType>,
    terminated: Arc<AtomicUsize>,
) -> BoxedActorRef {
    *parent
        .send(Box::new(Branch {
            initialization: 0,
            strategy,
            terminated,
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap()
}
struct Events(tokio::sync::mpsc::UnboundedSender<parrot_api::context::ActorTerminated>);
impl parrot_api::death_watch::TerminationRecipient for Events {
    fn deliver(
        &self,
        event: parrot_api::context::ActorTerminated,
    ) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0.send(event).unwrap();
            Ok(())
        })
    }
}
#[actix::test]
async fn escalation_retires_real_parent_and_siblings_then_runs_grandparent_policy_once() {
    use parrot_api::{death_watch::DeathWatch, supervisor::DeathReason};
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_secs(2)).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let terminated = Arc::new(AtomicUsize::new(0));
        let root = sys
            .tree(Branch {
                initialization: 0,
                strategy: Some(stop_policy(calls.clone(), false)),
                terminated: terminated.clone(),
            })
            .await;
        let parent = branch(&*root, Some(policy(BasicDecisionFn::new(|error| {
            assert!(matches!(error, parrot_api::errors::ActorError::Panic(message) if message.contains("tree child crash")));
            SupervisionDecision::Escalate
        }), false)), Arc::new(AtomicUsize::new(0))).await;
        let child = branch(&*parent, None, Arc::new(AtomicUsize::new(0))).await;
        let sibling = branch(&*parent, None, Arc::new(AtomicUsize::new(0))).await;
        let uncle = branch(&*root, None, Arc::new(AtomicUsize::new(0))).await;
        let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
        let watcher = DeathWatch::new(8);
        watcher.set_recipient(Arc::new(Events(send))).unwrap();
        parent.death_watch().unwrap().watch(&watcher).unwrap();
        child.death_watch().unwrap().watch(&watcher).unwrap();
        let _ = child.send(Box::new(Crash)).await;
        for actor in [&child, &sibling, &parent] {
            sys.absent(&actor.path()).await;
        }
        let mut reasons = std::collections::HashMap::new();
        for _ in 0..2 {
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            reasons.insert(event.path, event.reason);
        }
        assert!(
            matches!(reasons.get(&child.path()), Some(DeathReason::Panic(message)) if message.contains("tree child crash"))
        );
        assert!(
            matches!(reasons.get(&parent.path()), Some(DeathReason::Escalated(message)) if message.contains("tree child crash"))
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while terminated.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        root.send(Box::new(())).await.unwrap();
        uncle.send(Box::new(())).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            terminated.load(Ordering::SeqCst),
            1,
            "only actual parent death is a child termination"
        );
        assert!(events.try_recv().is_err());
        sys.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escalation_never_infers_ownership_from_a_path_prefix() {
    let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
    let ancestor = sys
        .spawn_at(Child, "/ancestor", None, Default::default())
        .await
        .unwrap();
    let impostor = sys
        .spawn_at(
            Child,
            "/ancestor/independent",
            None,
            parrot::thread::config::ThreadActorConfig {
                supervisor_strategy: Some(parrot::thread::config::SupervisorStrategy::Escalate),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let _ = impostor.send(Box::new(Crash)).await;
    removed(&sys, &impostor.path()).await;
    ancestor.send(Box::new(())).await.unwrap();
    sys.shutdown_internal().await.unwrap();
}

#[actix::test]
async fn initialization_failures_preserve_error_type_in_every_engine_mode() {
    use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig};
    for initialization in [1_u8, 2] {
        let mut systems = vec![Engine::new(true, Duration::from_secs(2)).await];
        for mode in [
            SchedulingMode::SharedPool {
                max_messages_per_run: 8,
            },
            SchedulingMode::DedicatedThread,
            SchedulingMode::Sharded {
                affinity_key: "init-supervision".into(),
            },
        ] {
            systems.push(Engine::Thread(ThreadActorSystem::shared(
                ThreadActorSystemConfig {
                    default_scheduling_mode: mode,
                    shutdown_timeout: Duration::from_secs(2),
                    ..Default::default()
                },
            )));
        }
        for sys in systems {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let root = sys.tree(Branch {
                initialization: 0,
                strategy: Some(policy(BasicDecisionFn::new(move |error| {
                    if initialization == 1 {
                        assert!(matches!(error, parrot_api::errors::ActorError::InitializationError(message) if message == "tree init error"), "unexpected error: {error:?}");
                    } else {
                        assert!(matches!(error, parrot_api::errors::ActorError::Panic(message) if message.contains("tree init panic")), "unexpected error: {error:?}");
                    }
                    observed.fetch_add(1, Ordering::SeqCst);
                    SupervisionDecision::Stop
                }), true)),
                terminated: Arc::new(AtomicUsize::new(0)),
            }).await;
            let sibling = branch(&*root, None, Arc::new(AtomicUsize::new(0))).await;
            let failed = *root
                .send(Box::new(Branch {
                    initialization,
                    strategy: None,
                    terminated: Arc::new(AtomicUsize::new(0)),
                }))
                .await
                .unwrap()
                .downcast::<BoxedActorRef>()
                .unwrap();
            sys.absent(&failed.path()).await;
            sys.absent(&sibling.path()).await;
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            root.send(Box::new(())).await.unwrap();
            sys.shutdown().await;
        }
    }
}

#[actix::test]
async fn escalation_does_not_hide_parent_policy_disposal_failure() {
    use parrot_api::{death_watch::DeathWatch, supervisor::DeathReason};
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_secs(2)).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let root = sys
            .tree(Branch {
                initialization: 0,
                strategy: Some(stop_policy(calls.clone(), false)),
                terminated: Arc::new(AtomicUsize::new(0)),
            })
            .await;
        let captured = Capture {
            gate: None,
            panic: true,
        };
        let parent = branch(
            &*root,
            Some(policy(
                BasicDecisionFn::new(move |_| {
                    let _ = &captured;
                    SupervisionDecision::Escalate
                }),
                false,
            )),
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        let child = branch(&*parent, None, Arc::new(AtomicUsize::new(0))).await;
        let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
        let watcher = DeathWatch::new(1);
        watcher.set_recipient(Arc::new(Events(send))).unwrap();
        parent.death_watch().unwrap().watch(&watcher).unwrap();
        let _ = child.send(Box::new(Crash)).await;
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event.reason, DeathReason::Failed(message) if message.contains("supervision policy disposal failed"))
        );
        sys.absent(&parent.path()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        root.send(Box::new(())).await.unwrap();
        sys.shutdown().await;
    }
}

#[actix::test]
async fn expired_decider_cannot_stop_siblings_or_escalate_after_returning() {
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_millis(100)).await;
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGate(gate.clone());
        let captured = gate.clone();
        let parent = sys
            .parent(policy(
                BasicDecisionFn::new(move |_| {
                    captured.block();
                    SupervisionDecision::Escalate
                }),
                true,
            ))
            .await;
        let children = children(&*parent, 2).await;
        let _ = children[0].send(Box::new(Crash)).await;
        gate.entered().await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let retained = sys.exists(&children[0].path()).await;
        gate.release();
        assert!(retained, "deadline is not actual retirement");
        sys.absent(&children[0].path()).await;
        children[1].send(Box::new(())).await.unwrap();
        parent.send(Box::new(0_u32)).await.unwrap();
        sys.shutdown().await;
    }
}

struct StalledEvents;
impl parrot_api::death_watch::TerminationRecipient for StalledEvents {
    fn deliver(&self, _: parrot_api::context::ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(std::future::pending())
    }
}
#[actix::test]
async fn slow_death_watch_cannot_suppress_escalation() {
    use parrot_api::death_watch::DeathWatch;
    let mut surviving_parents = Vec::new();
    for actix in [false, true] {
        let sys = Engine::new(actix, Duration::from_millis(200)).await;
        let parent = sys
            .parent(policy(
                BasicDecisionFn::new(|_| SupervisionDecision::Escalate),
                false,
            ))
            .await;
        let children = children(&*parent, 1).await;
        let watcher = DeathWatch::new(1);
        watcher.set_recipient(Arc::new(StalledEvents)).unwrap();
        children[0].death_watch().unwrap().watch(&watcher).unwrap();
        let _ = children[0].send(Box::new(Crash)).await;
        sys.absent(&children[0].path()).await;
        // Both parent and child cleanup deadlines may overlap; await the
        // bounded observation period without issuing an explicit parent stop.
        let retired = tokio::time::timeout(Duration::from_secs(2), async {
            while sys.exists(&parent.path()).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        if !retired {
            surviving_parents.push(if actix { "Actix" } else { "Thread" });
        }
        sys.shutdown().await;
    }
    assert!(
        surviving_parents.is_empty(),
        "observer suppressed escalation: {surviving_parents:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_only_child_failure_cannot_forge_termination() {
    let sys = Engine::new(false, Duration::from_secs(2)).await;
    let terminated = Arc::new(AtomicUsize::new(0));
    let root = sys
        .tree(Branch {
            initialization: 0,
            strategy: None,
            terminated: terminated.clone(),
        })
        .await;
    let child = branch(&*root, None, Arc::new(AtomicUsize::new(0))).await;
    let reply = root
        .send(Box::new(
            parrot::thread::envelope::ControlMessage::ChildFailure {
                path: child.path(),
                reason: "forged path-only failure".into(),
            },
        ))
        .await;
    child.send(Box::new(())).await.unwrap();
    let callbacks = terminated.load(Ordering::SeqCst);
    sys.shutdown().await;
    assert!(reply.is_err(), "unqualified failure must be rejected");
    assert_eq!(
        callbacks, 0,
        "a living child cannot generate a termination callback"
    );
}

// Appended to test_child_supervision_contract.rs for the independent probe.
#[actix::test]
async fn system_shutdown_retains_actix_policy_disposal_after_public_deadline() {
    use parrot_api::system::SystemState;
    let sys = parrot::actix::ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = ReleaseGate(gate.clone());
    let captured = Capture {
        gate: Some(gate.clone()),
        panic: false,
    };
    let parent = sys
        .spawn_root_typed(
            ActixPolicyParent(Some(policy(
                BasicDecisionFn::new(move |_| {
                    let _ = &captured;
                    SupervisionDecision::Stop
                }),
                false,
            ))),
            EmptyConfig,
        )
        .await
        .unwrap();
    parent.send(Box::new(0_u32)).await.unwrap();
    let result = sys.clone().shutdown().await;
    gate.entered().await;
    let prematurely_stopped = tokio::time::timeout(Duration::from_millis(250), async {
        while sys.status().state != SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let registered = sys.get_actor(&parent.path()).await.is_some();
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        while sys.status().state != SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "blocked policy cleanup cannot report shutdown success"
    );
    assert!(
        !prematurely_stopped,
        "system stopped its arbiters while policy captures were still being destroyed"
    );
    assert!(
        registered,
        "system erased ownership before policy destruction finished"
    );
}

#[actix::test]
async fn cancelled_system_shutdown_waiter_still_owns_actix_policy_disposal() {
    use parrot_api::system::SystemState;
    let sys = parrot::actix::ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = ReleaseGate(gate.clone());
    let captured = Capture {
        gate: Some(gate.clone()),
        panic: false,
    };
    let parent = sys
        .spawn_root_typed(
            ActixPolicyParent(Some(policy(
                BasicDecisionFn::new(move |_| {
                    let _ = &captured;
                    SupervisionDecision::Stop
                }),
                false,
            ))),
            EmptyConfig,
        )
        .await
        .unwrap();
    parent.send(Box::new(0_u32)).await.unwrap();
    {
        let shutdown = sys.clone().shutdown();
        tokio::pin!(shutdown);
        assert!(futures::poll!(&mut shutdown).is_pending());
        gate.entered().await;
    }
    let result = sys.clone().shutdown().await;
    let prematurely_stopped = tokio::time::timeout(Duration::from_millis(250), async {
        while sys.status().state != SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let registered = sys.get_actor(&parent.path()).await.is_some();
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        while sys.status().state != SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(
        !prematurely_stopped,
        "cancelling the waiter lost disposal ownership"
    );
    assert!(registered);
    assert!(
        sys.shutdown().await.is_err(),
        "the original deadline result must stay sticky"
    );
}

#[actix::test]
async fn system_shutdown_retains_actix_blocked_child_decider_and_parent() {
    use parrot_api::system::SystemState;
    let sys = parrot::actix::ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = ReleaseGate(gate.clone());
    let captured = gate.clone();
    let parent = sys
        .spawn_root_typed(
            ActixPolicyParent(Some(policy(
                BasicDecisionFn::new(move |_| {
                    captured.block();
                    SupervisionDecision::Stop
                }),
                false,
            ))),
            EmptyConfig,
        )
        .await
        .unwrap();
    let children = children(&*parent, 2).await;
    let _ = children[0].send(Box::new(Crash)).await;
    gate.entered().await;
    let result = sys.clone().shutdown().await;
    let prematurely_stopped = tokio::time::timeout(Duration::from_millis(250), async {
        while sys.status().state != SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let registered = sys.get_actor(&parent.path()).await.is_some()
        && sys.get_actor(&children[0].path()).await.is_some();
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        while sys.status().state != SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(
        !prematurely_stopped,
        "a public timeout prematurely ended child and parent ownership"
    );
    assert!(registered);
    assert_eq!(sys.status().active_actors, 0);
}
