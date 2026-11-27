//! Supervision must survive its creation host when business workers survive.
use parrot::thread::{
    SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext,
    config::SupervisorStrategy,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus},
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    supervisor::{
        BasicDecisionFn, OneForAllStrategy, OneForOneStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    trace::TraceContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);
const FAILURE: &str = "independent worker business failure";

#[path = "support/isolated.rs"]
mod child;

fn isolated(name: &str, body: impl FnOnce()) {
    child::run(name, "PARROT_SUPERVISION_HOST_CHILD", WAIT * 6, body);
}

#[derive(Default)]
struct State {
    built: AtomicUsize,
    drops: [AtomicUsize; 4],
    decisions: Mutex<Vec<(String, Option<[u8; 16]>)>>,
    policies: AtomicUsize,
}

#[test]
fn supervision_isolation_drains_large_diagnostics() {
    isolated("supervision_isolation_drains_large_diagnostics", || {
        eprintln!("{}", "x".repeat(256 * 1024));
    });
}
struct PolicyCapture(Arc<State>);
impl Drop for PolicyCapture {
    fn drop(&mut self) {
        self.0.policies.fetch_add(1, Ordering::SeqCst);
    }
}
impl PolicyCapture {
    fn decide(&self, error: &ActorError, choice: SupervisionDecision) -> SupervisionDecision {
        let text = match error {
            ActorError::MessageHandlingError(text) | ActorError::Panic(text) => text.clone(),
            _ => panic!("supervision replaced the business failure: {error:?}"),
        };
        assert!(text.contains(FAILURE), "{text}");
        self.0
            .decisions
            .lock()
            .unwrap()
            .push((text, TraceContext::current().map(|c| c.trace_id())));
        choice
    }
}
fn policy(state: &Arc<State>, choice: SupervisionDecision, all: bool) -> SupervisorStrategyType {
    let capture = PolicyCapture(state.clone());
    let decider = BasicDecisionFn::new(move |error| capture.decide(error, choice));
    if all {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 2,
            within: Duration::from_secs(60),
            decider,
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 2,
            within: Duration::from_secs(60),
            decider,
        })
    }
}
struct Generation {
    state: Arc<State>,
    number: usize,
    value: usize,
}
impl Generation {
    fn new(state: Arc<State>) -> Self {
        let number = state.built.fetch_add(1, Ordering::SeqCst);
        Self {
            state,
            number,
            value: 10,
        }
    }
}
impl Drop for Generation {
    fn drop(&mut self) {
        self.state.drops[self.number].fetch_add(1, Ordering::SeqCst);
    }
}
struct Fail(bool);
struct SpawnChild(Arc<State>);
struct SpawnFactoryChild {
    state: Arc<State>,
    capture: Arc<()>,
}
struct View(usize, usize, String);
impl Actor for Generation {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(failure) = message.downcast_ref::<Fail>() {
                self.value += 1;
                assert!(!failure.0, "{FAILURE}");
                return Err(ActorError::MessageHandlingError(FAILURE.into()));
            }
            if message.is::<SpawnChild>() {
                self.value += 1;
                let state = message.downcast::<SpawnChild>().unwrap().0;
                let strategy = policy(&state, SupervisionDecision::Escalate, false);
                let child = ctx
                    .spawner()
                    .spawn_supervised(Self::new(state), EmptyConfig, strategy)
                    .await?;
                return Ok(Box::new(child) as BoxedMessage);
            }
            if message.is::<SpawnFactoryChild>() {
                let SpawnFactoryChild { state, capture } =
                    *message.downcast::<SpawnFactoryChild>().unwrap();
                let child = ctx
                    .spawner()
                    .spawn_factory(move || {
                        let _keep = &capture;
                        Self::new(state.clone())
                    })
                    .await?;
                return Ok(Box::new(child) as BoxedMessage);
            }
            Ok(Box::new(View(
                self.number,
                self.value,
                std::thread::current().name().unwrap_or("unnamed").into(),
            )) as BoxedMessage)
        })
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn system(sharded: bool) -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        max_dedicated_threads: 8,
        default_scheduling_mode: if sharded {
            SchedulingMode::Sharded {
                affinity_key: "surviving-supervision".into(),
            }
        } else {
            SchedulingMode::DedicatedThread
        },
        ..Default::default()
    })
}
async fn query(actor: &dyn ActorRef, expected: usize, sharded: bool) -> View {
    tokio::time::timeout(WAIT, async {
        loop {
            match actor.send(Box::new(())).await {
                Ok(reply) => {
                    let view = *reply.downcast::<View>().unwrap();
                    assert!(view.0 <= expected, "unexpected extra generation");
                    if view.0 == expected {
                        assert!(
                            view.2.starts_with(if sharded {
                                "parrot-shard-"
                            } else {
                                "parrot-dedicated-"
                            }),
                            "business execution moved to {}",
                            view.2
                        );
                        return view;
                    }
                }
                // A query admitted to the retiring generation may lose its
                // reply; only a successful replacement reply proves recovery.
                Err(ActorError::Stopped) => {}
                Err(ActorError::ReplyChannelError(text))
                    if text.starts_with("Reply channel closed for ask to ") => {}
                Err(error) => panic!("generation query lost its business contract: {error}"),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervision did not publish a responsive replacement")
}
async fn fail(actor: &dyn ActorRef, panic: bool, cause: &TraceContext) {
    let work = cause.in_scope(|| actor.send(Box::new(Fail(panic))));
    let error = tokio::time::timeout(WAIT, work)
        .await
        .expect("failed ASK lost its receipt")
        .unwrap_err();
    assert!(
        matches!(&error, ActorError::Panic(text) if panic && text.contains(FAILURE))
            || matches!(&error, ActorError::MessageHandlingError(text) if !panic && text == FAILURE),
        "{error}"
    );
}
async fn retired(actor: &dyn ActorRef) {
    tokio::time::timeout(WAIT, async {
        while !matches!(actor.retirement_status(), RetirementStatus::Retired(Ok(()))) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed generation did not actually retire");
}
async fn shutdown(system: &ThreadActorSystem) {
    let first = tokio::time::timeout(WAIT, system.shutdown_internal())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(&first, parrot::thread::error::SystemError::ShutdownError(text)
        if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"),
        "{first}"
    );
    let repeated = tokio::time::timeout(WAIT, system.shutdown_internal())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(first.to_string(), repeated.to_string());
    assert_eq!(system.actor_count(), 0);
    assert_eq!(
        system.scheduler_metrics().unwrap().status,
        parrot::thread::scheduler::shared::SchedulerStatus::Shutdown
    );
}
fn root(sharded: bool, panic: bool, all: bool) {
    let host = runtime();
    let state = Arc::new(State::default());
    let sibling = Arc::new(State::default());
    let (system, group, actor, other) = host.block_on(async {
        let system = system(sharded);
        let group = if all {
            Some(
                system
                    .root_cohort(policy(&state, SupervisionDecision::Restart, true))
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let config = ThreadActorConfig {
            supervisor_strategy: Some(SupervisorStrategy::Restart {
                max_retries: 2,
                within: Duration::from_secs(60),
            }),
            ..Default::default()
        };
        let source = state.clone();
        let actor: BoxedActorRef = if let Some(group) = &group {
            Box::new(
                system
                    .spawn_supervised_in_cohort(
                        move || Generation::new(source.clone()),
                        "/first",
                        config.clone(),
                        group,
                    )
                    .await
                    .unwrap(),
            )
        } else {
            Box::new(
                system
                    .spawn_supervised_with_config(
                        move || Generation::new(source.clone()),
                        "/first",
                        config.clone(),
                    )
                    .await
                    .unwrap(),
            )
        };
        let source = sibling.clone();
        let other: Option<BoxedActorRef> = if let Some(group) = &group {
            Some(Box::new(
                system
                    .spawn_supervised_in_cohort(
                        move || Generation::new(source.clone()),
                        "/second",
                        config,
                        group,
                    )
                    .await
                    .unwrap(),
            ))
        } else {
            None
        };
        assert_eq!(query(actor.as_ref(), 0, sharded).await.1, 10);
        if let Some(other) = &other {
            query(other.as_ref(), 0, sharded).await;
        }
        (system, group, actor, other)
    });
    let identity = actor.death_watch().unwrap().id();
    drop(host);
    runtime().block_on(async {
        for generation in 0..3 {
            let cause = TraceContext::root();
            fail(actor.as_ref(), panic, &cause).await;
            if generation < 2 {
                assert_eq!(query(actor.as_ref(), generation + 1, sharded).await.1, 10);
                if let Some(other) = &other {
                    query(other.as_ref(), generation + 1, sharded).await;
                }
                assert_eq!(actor.death_watch().unwrap().id(), identity);
            } else {
                retired(actor.as_ref()).await;
                if let Some(other) = &other {
                    retired(other.as_ref()).await;
                }
            }
            assert_eq!(state.drops[generation].load(Ordering::SeqCst), 1);
            if all {
                assert_eq!(sibling.drops[generation].load(Ordering::SeqCst), 1);
                let decisions = state.decisions.lock().unwrap();
                assert_eq!(decisions.len(), generation + 1);
                assert_eq!(decisions.last().unwrap().1, Some(cause.trace_id()));
            }
        }
        assert_eq!(
            state.built.load(Ordering::SeqCst),
            3,
            "restart budget changed"
        );
        assert!(system.get_actor_ref("/first").is_none());
        if let Some(group) = group {
            group.stop().await.unwrap();
        }
        assert_eq!(state.policies.load(Ordering::SeqCst), usize::from(all));
        shutdown(&system).await;
    });
}
async fn spawn_child(parent: &dyn ActorRef, children: &Arc<State>) -> BoxedActorRef {
    *tokio::time::timeout(WAIT, parent.send(Box::new(SpawnChild(children.clone()))))
        .await
        .expect("child creation depended on the departed host")
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap()
}
fn escalation(sharded: bool, panic: bool, decision: SupervisionDecision) {
    escalation_with_child(sharded, panic, decision, false);
}
fn escalation_with_child(
    sharded: bool,
    panic: bool,
    decision: SupervisionDecision,
    existing: bool,
) {
    let host = runtime();
    let state = Arc::new(State::default());
    let children = Arc::new(State::default());
    let (system, group, parent, mut initial) = host.block_on(async {
        let system = system(sharded);
        let group = system
            .root_cohort(policy(&state, decision, false))
            .await
            .unwrap();
        let source = state.clone();
        let parent = system
            .spawn_supervised_in_cohort(
                move || Generation::new(source.clone()),
                "/parent",
                Default::default(),
                &group,
            )
            .await
            .unwrap();
        query(&parent, 0, sharded).await;
        let child = if existing {
            Some(spawn_child(&parent, &children).await)
        } else {
            None
        };
        (system, group, parent, child)
    });
    let identity = parent.death_watch().unwrap().id();
    drop(host);
    runtime().block_on(async {
        for cycle in 0..2 {
            let child = match initial.take() {
                Some(child) => child,
                None => spawn_child(&parent, &children).await,
            };
            query(child.as_ref(), cycle, sharded).await;
            let cause = TraceContext::root();
            fail(child.as_ref(), panic, &cause).await;
            retired(child.as_ref()).await;
            assert!(system.get_actor_ref(&child.path()).is_none());
            match decision {
                SupervisionDecision::Resume => {
                    assert_eq!(query(&parent, 0, sharded).await.1, 11 + cycle);
                    assert_eq!(state.drops[0].load(Ordering::SeqCst), 0);
                }
                SupervisionDecision::Restart => {
                    assert_eq!(query(&parent, cycle + 1, sharded).await.1, 10);
                    assert_eq!(state.drops[cycle].load(Ordering::SeqCst), 1);
                }
                SupervisionDecision::Stop => {
                    retired(&parent).await;
                }
                _ => unreachable!(),
            }
            for stats in [&state, &children] {
                let decisions = stats.decisions.lock().unwrap();
                assert_eq!(decisions.len(), cycle + 1);
                assert_eq!(decisions.last().unwrap().1, Some(cause.trace_id()));
            }
            assert_eq!(children.drops[cycle].load(Ordering::SeqCst), 1);
            assert_eq!(children.policies.load(Ordering::SeqCst), cycle + 1);
            assert_eq!(parent.death_watch().unwrap().id(), identity);
            if decision == SupervisionDecision::Stop {
                break;
            }
        }
        group.stop().await.unwrap();
        assert!(system.get_actor_ref("/parent").is_none());
        assert_eq!(state.policies.load(Ordering::SeqCst), 1);
        let builds = if decision == SupervisionDecision::Restart {
            3
        } else {
            1
        };
        assert_eq!(state.built.load(Ordering::SeqCst), builds);
        for generation in 0..builds {
            assert_eq!(state.drops[generation].load(Ordering::SeqCst), 1);
        }
        shutdown(&system).await;
    });
}
fn retire_mixed_tree(sharded: bool) {
    let host = runtime();
    let root_state = Arc::new(State::default());
    let middle_state = Arc::new(State::default());
    let leaf_state = Arc::new(State::default());
    let capture = Arc::new(());
    let factory_capture = Arc::downgrade(&capture);
    let (system, parent, middle) = host.block_on(async {
        let system = system(sharded);
        let parent = system
            .spawn_at(
                Generation::new(root_state.clone()),
                "/tree",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let reply = parent
            .send(Box::new(SpawnFactoryChild {
                state: middle_state.clone(),
                capture,
            }))
            .await
            .unwrap();
        let middle = *reply.downcast::<BoxedActorRef>().unwrap();
        assert_eq!(query(middle.as_ref(), 0, sharded).await.1, 10);
        (system, parent, middle)
    });
    drop(host);
    runtime().block_on(async {
        let leaf = spawn_child(middle.as_ref(), &leaf_state).await;
        let members: [&dyn ActorRef; 3] = [&parent, middle.as_ref(), leaf.as_ref()];
        for (member, expected) in members.iter().zip([10, 11, 10]) {
            assert_eq!(query(*member, 0, sharded).await.1, expected);
            assert!(system.get_actor_ref(&member.path()).is_some());
        }
        assert!(factory_capture.upgrade().is_some());
        tokio::time::timeout(WAIT, parent.stop())
            .await
            .unwrap()
            .unwrap();
        for (member, state) in members
            .iter()
            .zip([&root_state, &middle_state, &leaf_state])
        {
            assert!(matches!(
                member.retirement_status(),
                RetirementStatus::Retired(Ok(()))
            ));
            assert!(system.get_actor_ref(&member.path()).is_none());
            assert!(member.send(Box::new(())).await.is_err());
            assert_eq!(state.built.load(Ordering::SeqCst), 1);
            assert_eq!(state.drops[0].load(Ordering::SeqCst), 1);
        }
        assert!(factory_capture.upgrade().is_none());
        shutdown(&system).await;
    });
}
macro_rules! contract {
    ($name:ident, $scenario:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || $scenario);
        }
    };
}
contract!(dedicated_legacy_error, root(false, false, false));
contract!(
    dedicated_mixed_tree_stop_after_host_exit,
    retire_mixed_tree(false)
);
contract!(
    sharded_mixed_tree_stop_after_host_exit,
    retire_mixed_tree(true)
);
contract!(dedicated_legacy_panic, root(false, true, false));
contract!(sharded_legacy_error, root(true, false, false));
contract!(sharded_legacy_panic, root(true, true, false));
contract!(dedicated_one_for_all_error, root(false, false, true));
contract!(dedicated_one_for_all_panic, root(false, true, true));
contract!(sharded_one_for_all_error, root(true, false, true));
contract!(sharded_one_for_all_panic, root(true, true, true));
contract!(
    dedicated_escalation_resume_error,
    escalation(false, false, SupervisionDecision::Resume)
);
contract!(
    dedicated_escalation_resume_panic,
    escalation(false, true, SupervisionDecision::Resume)
);
contract!(
    dedicated_escalation_restart_error,
    escalation(false, false, SupervisionDecision::Restart)
);
contract!(
    dedicated_escalation_restart_panic,
    escalation(false, true, SupervisionDecision::Restart)
);
contract!(
    dedicated_escalation_stop_error,
    escalation(false, false, SupervisionDecision::Stop)
);
contract!(
    dedicated_escalation_stop_panic,
    escalation(false, true, SupervisionDecision::Stop)
);
contract!(
    sharded_escalation_resume_error,
    escalation(true, false, SupervisionDecision::Resume)
);
contract!(
    sharded_escalation_resume_panic,
    escalation(true, true, SupervisionDecision::Resume)
);
contract!(
    sharded_escalation_restart_error,
    escalation(true, false, SupervisionDecision::Restart)
);
contract!(
    sharded_escalation_restart_panic,
    escalation(true, true, SupervisionDecision::Restart)
);
contract!(
    sharded_escalation_stop_error,
    escalation(true, false, SupervisionDecision::Stop)
);
contract!(
    sharded_escalation_stop_panic,
    escalation(true, true, SupervisionDecision::Stop)
);
contract!(
    dedicated_existing_resume_error,
    escalation_with_child(false, false, SupervisionDecision::Resume, true)
);
contract!(
    dedicated_existing_resume_panic,
    escalation_with_child(false, true, SupervisionDecision::Resume, true)
);
contract!(
    dedicated_existing_restart_error,
    escalation_with_child(false, false, SupervisionDecision::Restart, true)
);
contract!(
    dedicated_existing_restart_panic,
    escalation_with_child(false, true, SupervisionDecision::Restart, true)
);
contract!(
    dedicated_existing_stop_error,
    escalation_with_child(false, false, SupervisionDecision::Stop, true)
);
contract!(
    dedicated_existing_stop_panic,
    escalation_with_child(false, true, SupervisionDecision::Stop, true)
);
contract!(
    sharded_existing_resume_error,
    escalation_with_child(true, false, SupervisionDecision::Resume, true)
);
contract!(
    sharded_existing_resume_panic,
    escalation_with_child(true, true, SupervisionDecision::Resume, true)
);
contract!(
    sharded_existing_restart_error,
    escalation_with_child(true, false, SupervisionDecision::Restart, true)
);
contract!(
    sharded_existing_restart_panic,
    escalation_with_child(true, true, SupervisionDecision::Restart, true)
);
contract!(
    sharded_existing_stop_error,
    escalation_with_child(true, false, SupervisionDecision::Stop, true)
);
contract!(
    sharded_existing_stop_panic,
    escalation_with_child(true, true, SupervisionDecision::Stop, true)
);
