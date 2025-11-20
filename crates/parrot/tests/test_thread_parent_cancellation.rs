//! Real parent cancellation retains late outputs, policy tracing and reference identity.
use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::TraceContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);
type TraceEvent = (&'static str, Option<[u8; 16]>);

#[derive(Default)]
struct State {
    construction: AtomicUsize,
    initialized: AtomicUsize,
    messages: AtomicUsize,
    actor_drops: AtomicUsize,
    factory_drops: AtomicUsize,
    policy_drops: AtomicUsize,
    handler_drops: AtomicUsize,
    handler_returns: AtomicUsize,
    parent_drops: AtomicUsize,
    blocked: AtomicUsize,
    stalled: AtomicUsize,
    open: Mutex<bool>,
    changed: Condvar,
    traces: Mutex<Vec<TraceEvent>>,
}
impl State {
    fn block(&self) {
        self.blocked.fetch_add(1, Ordering::SeqCst);
        let (open, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        if !*open || timeout.timed_out() {
            self.stalled.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn trace(&self, kind: &'static str) {
        self.traces
            .lock()
            .unwrap()
            .push((kind, TraceContext::current().map(|t| t.trace_id())));
    }
}
struct Release(Arc<State>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Handler(Arc<State>);
impl Drop for Handler {
    fn drop(&mut self) {
        self.0.handler_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Capture(Arc<State>, bool, bool);
impl Drop for Capture {
    fn drop(&mut self) {
        if self.2 {
            self.0.block();
        }
        let kind = if self.1 { "policy" } else { "factory" };
        self.0.trace(kind);
        if self.1 {
            self.0.policy_drops.fetch_add(1, Ordering::SeqCst);
        } else {
            self.0.factory_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
}
fn policy(state: &Arc<State>) -> SupervisorStrategyType {
    let capture = Capture(state.clone(), true, false);
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    })
}
struct Child(Arc<State>);
impl Drop for Child {
    fn drop(&mut self) {
        self.0.trace("actor");
        self.0.actor_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Child {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.messages.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(message) })
    }
}
struct Operation {
    state: Arc<State>,
    reject: bool,
}
struct Parent(Arc<State>);
impl Drop for Parent {
    fn drop(&mut self) {
        self.0.parent_drops.fetch_add(1, Ordering::SeqCst);
    }
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
            if !message.is::<Operation>() {
                return Ok(
                    Box::new(std::thread::current().name().unwrap_or("").to_owned())
                        as BoxedMessage,
                );
            }
            let request = *message.downcast::<Operation>().unwrap();
            let state = request.state;
            let _handler = Handler(state.clone());
            let capture = Capture(state.clone(), false, request.reject);
            let result = if request.reject {
                context
                    .spawner()
                    .spawn_factory_erased(Box::new(capture), Some(policy(&state)))
                    .await
                    .map(|_| ())
            } else {
                let source = state.clone();
                context
                    .spawner()
                    .spawn_factory_with_strategy(
                        move || {
                            let _ = &capture;
                            source.construction.fetch_add(1, Ordering::SeqCst);
                            source.block();
                            Child(source.clone())
                        },
                        policy(&state),
                    )
                    .await
                    .map(|_| ())
            };
            state.handler_returns.fetch_add(1, Ordering::SeqCst);
            result?;
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
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn check(sharded: bool, host_exit: bool, reject: bool) {
    let host = runtime();
    let system = ThreadActorSystem::try_shared_with_handle(
        ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            default_scheduling_mode: if sharded {
                SchedulingMode::Sharded {
                    affinity_key: "parent-cancel".into(),
                }
            } else {
                SchedulingMode::DedicatedThread
            },
            shutdown_timeout: Duration::from_secs(1),
            ..Default::default()
        },
        host.handle().clone(),
    )
    .unwrap();
    let state = Arc::new(State::default());
    let _release = Release(state.clone());
    let parent = host
        .block_on(system.spawn_at(Parent(state.clone()), "/parent", None, Default::default()))
        .unwrap();
    let neighbour_state = Arc::new(State::default());
    let neighbour = host
        .block_on(system.spawn_at(
            Child(neighbour_state.clone()),
            "/neighbour",
            None,
            Default::default(),
        ))
        .unwrap();
    let host = if host_exit {
        drop(host);
        None
    } else {
        Some(host)
    };
    let cause = TraceContext::root();
    runtime().block_on(async {
        let worker = *parent.send(Box::new(())).await.unwrap().downcast::<String>().unwrap();
        assert!(worker.starts_with(if sharded { "parrot-shard-" } else { "parrot-dedicated-" }));
        let operation = cause.in_scope(|| {
            let parent = parent.clone();
            let state = state.clone();
            parrot_api::trace::capture(async move {
                parent.send(Box::new(Operation { state, reject })).await
            })
        });
        let waiter = tokio::spawn(operation);
        until(|| state.blocked.load(Ordering::SeqCst) == 1).await;
        assert_eq!(state.handler_drops.load(Ordering::SeqCst), 0);
        assert_eq!(state.handler_returns.load(Ordering::SeqCst), 0);
        let child_path = system.actor_paths().into_iter().find(|p| p.starts_with("/parent/"));
        let old = child_path.as_ref().map(|path| system.get_actor_ref(path).unwrap());
        if reject { assert!(child_path.is_none()); }
        else { assert_eq!(system.actor_count(), 3); }
        system.get_mailbox("/parent").unwrap().get_processor().unwrap().cancel_processing();
        until(|| state.handler_drops.load(Ordering::SeqCst) == 1).await;
        let result = tokio::time::timeout(WAIT, parent.stop()).await.unwrap();
        if !reject { assert!(result.is_err(), "blocked constructor cannot be retired"); }
        until(|| state.handler_drops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(state.handler_returns.load(Ordering::SeqCst), 0, "handler completed instead of cancelling");
        assert_eq!(state.actor_drops.load(Ordering::SeqCst), 0);
        assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
        if let Some(path) = &child_path {
            assert!(system.get_actor_ref(path).is_some(), "late constructor lost its path reservation");
            assert!(system.get_actor_ref("/parent").is_some(), "parent retired before child ownership");
            let replacement = Arc::new(State::default());
            let result = system.spawn_at(Child(replacement.clone()), path, None, Default::default()).await;
            assert!(matches!(result, Err(parrot::thread::error::SpawnError::ActorPathAlreadyExists(_))));
            assert_eq!(replacement.actor_drops.load(Ordering::SeqCst), 1);
            assert_eq!(replacement.initialized.load(Ordering::SeqCst), 0);
        }
        let reply = neighbour.send(Box::new(17u32)).await.unwrap();
        assert_eq!(*reply.downcast::<u32>().unwrap(), 17);
        state.release();
        assert!(tokio::time::timeout(WAIT, waiter).await.unwrap().unwrap().is_err());
        until(|| system.get_actor_ref("/parent").is_none()).await;
        until(|| state.factory_drops.load(Ordering::SeqCst) == 1 && state.policy_drops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(state.parent_drops.load(Ordering::SeqCst), 1);
        assert_eq!(state.actor_drops.load(Ordering::SeqCst), usize::from(!reject));
        assert_eq!(state.construction.load(Ordering::SeqCst), usize::from(!reject));
        assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(state.messages.load(Ordering::SeqCst), 0);
        assert_eq!(state.stalled.load(Ordering::SeqCst), 0);
        let traces = state.traces.lock().unwrap().clone();
        assert_eq!(traces.len(), if reject { 2 } else { 3 });
        if let Some(path) = child_path {
            assert!(system.get_actor_ref(&path).is_none());
            let fresh = Arc::new(State::default());
            let replacement = system.spawn_at(Child(fresh.clone()), &path, None, Default::default()).await.unwrap();
            until(|| fresh.initialized.load(Ordering::SeqCst) == 1).await;
            assert_eq!(fresh.initialized.load(Ordering::SeqCst), 1);
            assert!(matches!(old.unwrap().send(Box::new(())).await, Err(ActorError::Stopped)));
            assert_eq!(fresh.messages.load(Ordering::SeqCst), 0, "old child reference reached the replacement");
            replacement.send(Box::new(29u32)).await.unwrap();
            assert_eq!(fresh.messages.load(Ordering::SeqCst), 1);
            replacement.stop().await.unwrap();
            assert_eq!(fresh.actor_drops.load(Ordering::SeqCst), 1);
        }
        neighbour.stop().await.unwrap();
        assert_eq!(neighbour_state.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(system.actor_count(), 0);
        for _ in 0..2 {
            let shutdown = system.shutdown_internal().await;
            if host_exit {
                assert!(matches!(shutdown, Err(parrot::thread::error::SystemError::ShutdownError(ref text))
                    if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{shutdown:?}");
            } else { shutdown.unwrap(); }
        }
        assert!(traces.iter().all(|(_, trace)| *trace == Some(cause.trace_id())), "{traces:?}");
    });
    drop(host);
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_PARENT_CANCEL_PROBE").as_deref() == Ok(name) {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_PARENT_CANCEL_PROBE", name)
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
macro_rules! case {
    ($name:ident, $sharded:expr, $exit:expr, $reject:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || check($sharded, $exit, $reject));
        }
    };
}
case!(dedicated_late_live, false, false, false);
case!(sharded_late_live, true, false, false);
case!(dedicated_late_host_exit, false, true, false);
case!(sharded_late_host_exit, true, true, false);
case!(dedicated_rejection_host_exit, false, true, true);
case!(sharded_rejection_host_exit, true, true, true);
