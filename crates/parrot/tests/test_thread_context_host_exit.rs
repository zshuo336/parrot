//! Actual managed parent inputs must retire without their departed host.
use futures::FutureExt;
use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    message::CloneableMessage,
    supervisor::{
        BasicDecisionFn, OneForAllStrategy, OneForOneStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    trace::TraceContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);
#[derive(Default)]
struct State {
    entered: AtomicUsize,
    drops: AtomicUsize,
    constructed: AtomicUsize,
    initialized: AtomicUsize,
    invoked: AtomicUsize,
    stalled: AtomicUsize,
    started: AtomicBool,
    polled: AtomicBool,
    returned: AtomicBool,
    open: Mutex<bool>,
    changed: Condvar,
    traces: Mutex<Vec<Option<[u8; 16]>>>,
}
impl State {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<State>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[derive(Clone)]
struct Capture(Arc<State>);
impl parrot_api::actor::ActorConfig for Capture {}
impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capture")
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.entered.fetch_add(1, Ordering::SeqCst);
        let (open, timeout) = self
            .0
            .changed
            .wait_timeout_while(self.0.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        if !*open || timeout.timed_out() {
            self.0.stalled.fetch_add(1, Ordering::SeqCst);
        }
        drop(open);
        self.0
            .traces
            .lock()
            .unwrap()
            .push(TraceContext::current().map(|trace| trace.trace_id()));
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Debug, Clone)]
struct Target(Capture);
impl ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.0.invoked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ActorError::Stopped) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.0.invoked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ActorError::Stopped) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/unused-target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> parrot_api::types::BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn policy(state: &Arc<State>) -> SupervisorStrategyType {
    let capture = Capture(state.clone());
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    })
}
struct Child(Capture);
impl Actor for Child {
    type Config = Capture;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.0.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
struct Parent;
struct Seed {
    retiring: Arc<State>,
    sibling: Arc<State>,
    stopping: Arc<State>,
}
struct RecoveringChild {
    capture: Capture,
    stopping: Arc<State>,
    generation: usize,
}
impl Actor for RecoveringChild {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.stopping.entered.fetch_add(1, Ordering::SeqCst);
            while !*self.stopping.open.lock().unwrap() {
                tokio::task::yield_now().await;
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let generation = self.generation;
        let _ = &self.capture;
        Box::pin(async move {
            if message.is::<bool>() {
                return Err(ActorError::MessageHandlingError(
                    "child recovery trigger".into(),
                ));
            }
            Ok(Box::new(generation) as BoxedMessage)
        })
    }
}
struct Operation {
    state: Arc<State>,
    route: usize,
    action: usize,
}
impl Actor for Parent {
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
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Seed>() {
                let seed = *message.downcast::<Seed>().unwrap();
                let mut children = Vec::new();
                for state in [seed.retiring, seed.sibling] {
                    let stopping = seed.stopping.clone();
                    children.push(
                        context
                            .spawner()
                            .spawn_factory_with_strategy(
                                move || RecoveringChild {
                                    capture: Capture(state.clone()),
                                    stopping: stopping.clone(),
                                    generation: state.constructed.fetch_add(1, Ordering::SeqCst),
                                },
                                SupervisorStrategyType::OneForAll(OneForAllStrategy {
                                    max_restarts: 1,
                                    within: WAIT,
                                    decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
                                }),
                            )
                            .await?,
                    );
                }
                return Ok(Box::new(children) as BoxedMessage);
            }
            if !message.is::<Operation>() {
                return Ok(
                    Box::new(std::thread::current().name().unwrap_or("").to_owned())
                        as BoxedMessage,
                );
            }
            let operation = *message.downcast::<Operation>().unwrap();
            let state = operation.state;
            let capture = Capture(state.clone());
            let mut future: BoxedFuture<'_, ActorResult<()>> = match operation.route {
                0 => Box::pin(
                    context
                        .spawner()
                        .spawn_supervised(Child(capture), Capture(state.clone()), policy(&state))
                        .map(|result| result.map(|_| ())),
                ),
                1 | 9 => {
                    let source = state.clone();
                    Box::pin(
                        context
                            .spawner()
                            .spawn_factory_with_strategy(
                                move || {
                                    let _ = &capture;
                                    source.constructed.fetch_add(1, Ordering::SeqCst);
                                    Child(Capture(source.clone()))
                                },
                                policy(&state),
                            )
                            .map(|result| result.map(|_| ())),
                    )
                }
                2 => Box::pin(
                    context
                        .spawner()
                        .spawn_factory_erased(Box::new(capture), Some(policy(&state)))
                        .map(|result| result.map(|_| ())),
                ),
                3 => context.send(Box::new(Target(Capture(state.clone()))), Box::new(capture)),
                4 => Box::pin(
                    context
                        .ask(Box::new(Target(Capture(state.clone()))), Box::new(capture))
                        .map(|result| result.map(|_| ())),
                ),
                5 => context.schedule_once(
                    Box::new(Target(Capture(state.clone()))),
                    Box::new(capture),
                    WAIT,
                ),
                6 => context.schedule_periodic(
                    Box::new(Target(Capture(state.clone()))),
                    CloneableMessage::from_cloneable(capture),
                    WAIT,
                    WAIT,
                ),
                7 => context.watch(Box::new(Target(capture))),
                8 => context.unwatch(Box::new(Target(capture))),
                _ => unreachable!(),
            };
            state.started.store(true, Ordering::SeqCst);
            if operation.action > 0 {
                let pending = futures::poll!(&mut future).is_pending();
                state.polled.store(true, Ordering::SeqCst);
                assert!(pending, "rejection returned before cleanup");
            }
            if operation.action == 2 {
                let error = future.await.unwrap_err();
                if operation.route == 9 {
                    assert!(
                        matches!(error, ActorError::InternalError(ref text)
                        if text == "child group is handling a failure; retry after recovery"),
                        "{error:?}"
                    );
                } else {
                    assert!(
                        matches!(error, ActorError::InitializationError(ref text)
                        if text == "Thread factory payload type mismatch"),
                        "{error:?}"
                    );
                }
            } else {
                drop(future);
            }
            state.returned.store(true, Ordering::SeqCst);
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
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
fn check(sharded: bool, route: usize, action: usize) {
    let host = runtime();
    let system = ThreadActorSystem::try_shared_with_handle(
        ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            default_scheduling_mode: if sharded {
                SchedulingMode::Sharded {
                    affinity_key: "context-host".into(),
                }
            } else {
                SchedulingMode::DedicatedThread
            },
            shutdown_timeout: WAIT,
            ..Default::default()
        },
        host.handle().clone(),
    )
    .unwrap();
    let parent = host
        .block_on(system.spawn_at(Parent, "/parent", None, Default::default()))
        .unwrap();
    drop(host);
    let state = Arc::new(State::default());
    let _release = Release(state.clone());
    let cause = TraceContext::root();
    runtime().block_on(async {
        let worker = parent
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<String>()
            .unwrap();
        assert!(worker.starts_with(if sharded {
            "parrot-shard-"
        } else {
            "parrot-dedicated-"
        }));
        let operation = cause.in_scope(|| {
            let parent = parent.clone();
            let state = state.clone();
            parrot_api::trace::capture(async move {
                parent
                    .send(Box::new(Operation {
                        state,
                        route,
                        action,
                    }))
                    .await
            })
        });
        let task = tokio::spawn(operation);
        until(|| state.started.load(Ordering::SeqCst)).await;
        if action == 2 {
            let polled = tokio::time::timeout(Duration::from_millis(500), async {
                while !state.polled.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok();
            if !polled {
                state.release();
            }
            assert!(polled, "polling rejection blocked the actual parent worker");
            until(|| state.entered.load(Ordering::SeqCst) > 0).await;
            assert!(!state.returned.load(Ordering::SeqCst));
        } else {
            let returned = tokio::time::timeout(Duration::from_millis(500), async {
                while !state.returned.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok();
            state.release();
            assert!(returned, "input cancellation blocked actual parent worker");
        }
        state.release();
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let expected = match route {
            0 => 3,
            1..=6 => 2,
            _ => 1,
        };
        until(|| state.drops.load(Ordering::SeqCst) == expected).await;
        assert_eq!(state.stalled.load(Ordering::SeqCst), 0);
        assert_eq!(state.constructed.load(Ordering::SeqCst), 0);
        assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(state.invoked.load(Ordering::SeqCst), 0);
        assert_eq!(
            *state.traces.lock().unwrap(),
            vec![Some(cause.trace_id()); expected]
        );
        assert_eq!(system.actor_count(), 1);
        parent.stop().await.unwrap();
        assert_eq!(system.actor_count(), 0);
        for _ in 0..2 {
            let error = system.shutdown_internal().await.unwrap_err();
            assert!(
                matches!(error, parrot::thread::error::SystemError::ShutdownError(ref text)
                if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"),
                "{error}"
            );
        }
    });
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_CONTEXT_HOST_CASE").as_deref() == Ok(name) {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_CONTEXT_HOST_CASE", name)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + WAIT * 6;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: watchdog expired");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn topology(sharded: bool, cancel: bool) {
    let host = runtime();
    let system = ThreadActorSystem::try_shared_with_handle(
        ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            default_scheduling_mode: if sharded {
                SchedulingMode::Sharded {
                    affinity_key: "context-host".into(),
                }
            } else {
                SchedulingMode::DedicatedThread
            },
            shutdown_timeout: WAIT,
            ..Default::default()
        },
        host.handle().clone(),
    )
    .unwrap();
    let parent = host
        .block_on(system.spawn_at(Parent, "/parent", None, Default::default()))
        .unwrap();
    let retiring = Arc::new(State::default());
    let _retire_release = Release(retiring.clone());
    let sibling = Arc::new(State::default());
    let _sibling_release = Release(sibling.clone());
    retiring.release();
    sibling.release();
    // The failed instance skips before_stop. A healthy OneForAll sibling holds
    // recovery in an async hook, leaving the shared shard available to Parent.
    let stopping = Arc::new(State::default());
    let _stop_release = Release(stopping.clone());
    let children = host.block_on(async {
        *parent
            .send(Box::new(Seed {
                retiring: retiring.clone(),
                sibling: sibling.clone(),
                stopping: stopping.clone(),
            }))
            .await
            .unwrap()
            .downcast::<Vec<parrot_api::types::BoxedActorRef>>()
            .unwrap()
    });
    let child = &children[0];
    host.block_on(async {
        assert_eq!(
            *child
                .send(Box::new(()))
                .await
                .unwrap()
                .downcast::<usize>()
                .unwrap(),
            0
        );
    });
    drop(host);
    let state = Arc::new(State::default());
    let _release = Release(state.clone());
    let cause = TraceContext::root();
    runtime().block_on(async {
        let error = child.send(Box::new(true)).await.unwrap_err();
        assert!(matches!(error, ActorError::MessageHandlingError(ref text) if text == "child recovery trigger"));
        until(|| stopping.entered.load(Ordering::SeqCst) == 1).await;
        until(|| retiring.drops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(sibling.drops.load(Ordering::SeqCst), 0);
        let operation = cause.in_scope(|| {
            let parent = parent.clone();
            let state = state.clone();
            parrot_api::trace::capture(async move {
                parent.send(Box::new(Operation { state, route: 9, action: if cancel { 1 } else { 2 } })).await
            })
        });
        let task = tokio::spawn(operation);
        until(|| state.started.load(Ordering::SeqCst)).await;
        let progressed = tokio::time::timeout(Duration::from_millis(500), async {
            while !if cancel { state.returned.load(Ordering::SeqCst) } else { state.polled.load(Ordering::SeqCst) } {
                tokio::task::yield_now().await;
            }
        }).await.is_ok();
        if !progressed { state.release(); stopping.release(); retiring.release(); }
        assert!(progressed, "topology rejection blocked the actual parent worker");
        until(|| state.entered.load(Ordering::SeqCst) > 0).await;
        if !cancel { assert!(!state.returned.load(Ordering::SeqCst)); }
        assert_eq!(retiring.drops.load(Ordering::SeqCst), 1);
        assert_eq!(sibling.drops.load(Ordering::SeqCst), 0);
        state.release();
        tokio::time::timeout(WAIT, task).await.unwrap().unwrap().unwrap();
        until(|| state.drops.load(Ordering::SeqCst) == 2).await;
        assert_eq!(state.constructed.load(Ordering::SeqCst), 0);
        assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(state.stalled.load(Ordering::SeqCst), 0);
        assert_eq!(*state.traces.lock().unwrap(), vec![Some(cause.trace_id()); 2]);
        retiring.release();
        stopping.release();
        until(|| retiring.constructed.load(Ordering::SeqCst) == 2).await;
        let generation = tokio::time::timeout(WAIT, async {
            loop {
                match child.send(Box::new(())).await {
                    Ok(reply) => break *reply.downcast::<usize>().unwrap(),
                    Err(ActorError::Stopped) => tokio::task::yield_now().await,
                    Err(error) => panic!("recovery lost its business result: {error}"),
                }
            }
        }).await.unwrap();
        assert_eq!(generation, 1);
        assert_eq!(retiring.drops.load(Ordering::SeqCst), 1);
        until(|| sibling.constructed.load(Ordering::SeqCst) == 2).await;
        assert_eq!(*children[1].send(Box::new(())).await.unwrap().downcast::<usize>().unwrap(), 1);
        parent.stop().await.unwrap();
        assert_eq!(system.actor_count(), 0);
        assert_eq!(retiring.drops.load(Ordering::SeqCst), 2);
        assert_eq!(retiring.stalled.load(Ordering::SeqCst), 0);
        assert_eq!(sibling.drops.load(Ordering::SeqCst), 2);
        assert_eq!(sibling.stalled.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            let error = system.shutdown_internal().await.unwrap_err();
            assert!(matches!(error, parrot::thread::error::SystemError::ShutdownError(ref text)
                if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{error}");
        }
    });
}
macro_rules! case {
    ($name:ident, $sharded:expr, $route:expr, $action:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || check($sharded, $route, $action));
        }
    };
}
case!(dedicated_instance_unpolled, false, 0, 0);
case!(sharded_instance_unpolled, true, 0, 0);
case!(dedicated_factory_unpolled, false, 1, 0);
case!(sharded_factory_unpolled, true, 1, 0);
case!(dedicated_erased_unpolled, false, 2, 0);
case!(sharded_erased_unpolled, true, 2, 0);
case!(dedicated_erased_cancelled, false, 2, 1);
case!(sharded_erased_cancelled, true, 2, 1);
case!(dedicated_erased_rejected, false, 2, 2);
case!(sharded_erased_rejected, true, 2, 2);
case!(dedicated_send_unpolled, false, 3, 0);
case!(sharded_send_unpolled, true, 3, 0);
case!(dedicated_ask_unpolled, false, 4, 0);
case!(sharded_ask_unpolled, true, 4, 0);
case!(dedicated_timer_unpolled, false, 5, 0);
case!(sharded_timer_unpolled, true, 5, 0);
case!(dedicated_periodic_unpolled, false, 6, 0);
case!(sharded_periodic_unpolled, true, 6, 0);
case!(dedicated_watch_unpolled, false, 7, 0);
case!(sharded_watch_unpolled, true, 7, 0);
case!(dedicated_unwatch_unpolled, false, 8, 0);
case!(sharded_unwatch_unpolled, true, 8, 0);
#[test]
fn dedicated_topology_cancelled() {
    isolated("dedicated_topology_cancelled", || topology(false, true));
}
#[test]
fn sharded_topology_cancelled() {
    isolated("sharded_topology_cancelled", || topology(true, true));
}
#[test]
fn dedicated_topology_rejected() {
    isolated("dedicated_topology_rejected", || topology(false, false));
}
#[test]
fn sharded_topology_rejected() {
    isolated("sharded_topology_rejected", || topology(true, false));
}
