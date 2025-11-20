//! Cancelled and expired constructors retain late outputs and exact path ownership.
use futures::FutureExt;
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    system::ParrotActorSystem,
    thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    actor::{Actor, ActorState},
    address::{ActorRef, RetirementStatus},
    context::ActorContext,
    engine::{
        AsyncEngineRuntime, AsyncFactoryRuntime, FactoryOptions, RuntimeFactory, SpawnOptions,
    },
    errors::ActorError,
    system::SystemError,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(5);
const DEADLINE: Duration = Duration::from_millis(350);
const PATH: &str = "/pending-handoff";
type Event = (&'static str, Option<[u8; 16]>);

#[derive(Default)]
struct Gate {
    entered: AtomicUsize,
    open: Mutex<bool>,
    changed: Condvar,
    expired: AtomicUsize,
}
impl Gate {
    fn block(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let (open, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        if !*open || timeout.timed_out() {
            self.expired.fetch_add(1, Ordering::SeqCst);
        }
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
#[derive(Default)]
struct State {
    events: Mutex<Vec<Event>>,
    constructed: AtomicUsize,
    initialized: AtomicUsize,
    stopped: AtomicUsize,
    outputs: AtomicUsize,
    captures: AtomicUsize,
    construct: Arc<Gate>,
    output: Arc<Gate>,
    capture: Arc<Gate>,
}
impl State {
    fn note(&self, event: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((event, TraceContext::current().map(|t| t.trace_id())));
    }
}
struct Capture(Arc<State>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.capture.block();
        self.0.note("capture");
        self.0.captures.fetch_add(1, Ordering::SeqCst);
    }
}
struct Probe {
    state: Arc<State>,
    value: u64,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.state.output.block();
        self.state.note("output");
        self.state.outputs.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Probe {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.state.note("init");
        self.state.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.state.stopped.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let request = *message.downcast::<u64>().unwrap();
            Ok(Box::new((
                self.value + request,
                TraceContext::current().map(|t| t.trace_id()),
                std::thread::current().name().unwrap_or("").to_owned(),
            )) as BoxedMessage)
        })
    }
}
fn factory(state: &Arc<State>, value: u64) -> RuntimeFactory {
    let capture = Capture(state.clone());
    Box::new(move || {
        let state = &capture.0;
        state.note("constructor");
        state.constructed.fetch_add(1, Ordering::SeqCst);
        state.construct.block();
        Box::new(Probe {
            state: state.clone(),
            value,
        })
    })
}
fn options() -> FactoryOptions {
    FactoryOptions {
        spawn: SpawnOptions::at(PATH),
        max_restarts: 0,
        ..Default::default()
    }
}
#[derive(Clone, Copy)]
enum Mode {
    Dedicated,
    Sharded,
    Actix,
}
enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
    Facade(Arc<ParrotActorSystem>),
}
impl Engine {
    async fn new(mode: Mode, facade: bool, deadline: bool) -> Self {
        let timeout = if deadline { DEADLINE } else { WAIT };
        let native = match mode {
            Mode::Dedicated | Mode::Sharded => {
                Self::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
                    shared_pool_size: 1,
                    shared_burst_workers_max: 0,
                    shutdown_timeout: timeout,
                    default_scheduling_mode: match mode {
                        Mode::Dedicated => SchedulingMode::DedicatedThread,
                        _ => SchedulingMode::Sharded {
                            affinity_key: "pending-handoff".into(),
                        },
                    },
                    ..Default::default()
                }))
            }
            Mode::Actix => Self::Actix(Arc::new(
                ActixActorSystem::with_settings(ActixSystemSettings {
                    arbiter_count: 1,
                    shutdown_timeout: timeout,
                    ..Default::default()
                })
                .await
                .unwrap(),
            )),
        };
        if !facade {
            return native;
        }
        let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
        match native {
            Self::Thread(system) => facade
                .register_thread_system("handoff".into(), system, true)
                .await
                .unwrap(),
            Self::Actix(system) => facade
                .register_actix_system("handoff".into(), system.as_ref().clone(), true)
                .await
                .unwrap(),
            Self::Facade(_) => unreachable!(),
        }
        Self::Facade(facade)
    }
    fn runtime(&self) -> &dyn AsyncEngineRuntime {
        match self {
            Self::Thread(system) => system.as_ref(),
            Self::Actix(system) => system.as_ref(),
            Self::Facade(system) => system.as_ref(),
        }
    }
    fn create(
        &self,
        state: &Arc<State>,
        value: u64,
    ) -> BoxedFuture<'_, Result<BoxedActorRef, SystemError>> {
        let factory = factory(state, value);
        match self {
            Self::Thread(system) => system.spawn_factory(factory, options()),
            Self::Actix(system) => system.spawn_factory(factory, options()),
            Self::Facade(system) => system.spawn_factory_on("handoff", factory, options()),
        }
    }
}
fn observer() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
async fn until(message: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect(message);
}
async fn retired(reference: &dyn ActorRef) {
    until("late factory resources did not retire", || {
        !matches!(reference.retirement_status(), RetirementStatus::Pending)
    })
    .await;
    assert!(matches!(
        reference.retirement_status(),
        RetirementStatus::Retired(Ok(()))
    ));
}
fn assert_timeout(mode: Mode, result: Result<BoxedActorRef, SystemError>) {
    match (mode, result) {
        (Mode::Dedicated | Mode::Sharded, Err(SystemError::ActorCreationError(text))) => {
            assert_eq!(
                text,
                format!("Actor creation timed out: factory {PATH} exceeded creation deadline")
            );
        }
        (Mode::Actix, Err(SystemError::ActorError(ActorError::TimeoutDetail(text)))) => {
            assert_eq!(text, "Actix factory creation deadline elapsed");
        }
        (_, result) => panic!("creation did not preserve its deadline error: {result:?}"),
    }
}
async fn blocked_path(engine: &Engine, old: &dyn ActorRef, state: &Arc<State>) {
    assert!(matches!(old.retirement_status(), RetirementStatus::Pending));
    assert_eq!(state.outputs.load(Ordering::SeqCst), 0);
    assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(state.stopped.load(Ordering::SeqCst), 0);
    assert!(engine.runtime().get_actor(PATH).await.is_some());
    let rejected = Arc::new(State::default());
    rejected.capture.release();
    rejected.output.release();
    rejected.construct.release();
    let error = engine.create(&rejected, 99).await.unwrap_err();
    assert!(
        matches!(error, SystemError::ActorCreationError(ref message) if message == &format!("Actor path already exists: {PATH}"))
            || matches!(error, SystemError::ActorError(ActorError::InitializationError(ref message)) if message == "actor path already registered"),
        "reserved constructor path was admitted: {error}"
    );
    assert_eq!(rejected.constructed.load(Ordering::SeqCst), 0);
    assert_eq!(rejected.captures.load(Ordering::SeqCst), 1);
}
fn contract(mode: Mode, facade: bool, deadline: bool, last_owner: bool) {
    for traced in [false, true] {
        let host = actix::System::new();
        let engine = host.block_on(Engine::new(mode, facade, deadline));
        let state = Arc::new(State::default());
        let _release_construct = Release(state.construct.clone());
        let _release_output = Release(state.output.clone());
        let _release_capture = Release(state.capture.clone());
        let cause = traced.then(TraceContext::root);
        let expected = cause.as_ref().map(TraceContext::trace_id);
        let mut pending = trace::in_scope(cause, || engine.create(&state, 1));
        let old = host.block_on(trace::scope(Some(TraceContext::root()), async {
            assert!(pending.as_mut().now_or_never().is_none());
            until("constructor did not enter", || {
                state.construct.entered.load(Ordering::SeqCst) == 1
            })
            .await;
            engine.runtime().get_actor(PATH).await.unwrap()
        }));
        // The OS host really exits while the independent constructor remains held.
        drop(host);
        let observer = observer();
        observer.block_on(async {
            if deadline {
                assert_timeout(
                    mode,
                    tokio::time::timeout(WAIT, &mut pending).await.unwrap(),
                );
            }
        });
        drop(pending);
        observer.block_on(blocked_path(&engine, old.as_ref(), &state));
        state.construct.release();
        observer.block_on(until("late output did not reach disposal", || {
            state.output.entered.load(Ordering::SeqCst) == 1
        }));
        observer.block_on(blocked_path(&engine, old.as_ref(), &state));
        let engine = if last_owner {
            drop(engine);
            None
        } else {
            Some(engine)
        };
        state.output.release();
        observer.block_on(until("factory captures did not reach disposal", || {
            state.capture.entered.load(Ordering::SeqCst) == 1
        }));
        assert_eq!(state.outputs.load(Ordering::SeqCst), 1);
        assert_eq!(state.captures.load(Ordering::SeqCst), 0);
        assert!(matches!(old.retirement_status(), RetirementStatus::Pending));
        if let Some(engine) = &engine {
            observer.block_on(async {
                assert!(engine.runtime().get_actor(PATH).await.is_some());
            });
        }
        state.capture.release();
        observer.block_on(async {
            retired(old.as_ref()).await;
            assert_eq!(state.constructed.load(Ordering::SeqCst), 1);
            assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
            assert_eq!(state.stopped.load(Ordering::SeqCst), 0);
            assert_eq!(state.outputs.load(Ordering::SeqCst), 1);
            assert_eq!(state.captures.load(Ordering::SeqCst), 1);
            assert_eq!(
                *state.events.lock().unwrap(),
                vec![
                    ("constructor", expected),
                    ("output", expected),
                    ("capture", expected)
                ]
            );
            for gate in [&state.construct, &state.output, &state.capture] {
                assert_eq!(
                    gate.expired.load(Ordering::SeqCst),
                    0,
                    "a gate watchdog released the test"
                );
            }
            let stopped = old.stop().await.map_err(|error| error.to_string());
            assert_eq!(old.stop().await.map_err(|error| error.to_string()), stopped);
            assert!(old.send(Box::new(1u64)).await.is_err());
            let Some(engine) = engine else { return };
            assert!(engine.runtime().get_actor(PATH).await.is_none());
            let replacement = Arc::new(State::default());
            replacement.construct.release();
            replacement.output.release();
            replacement.capture.release();
            let current = tokio::time::timeout(WAIT, engine.create(&replacement, 40))
                .await
                .unwrap()
                .unwrap();
            let command = TraceContext::root();
            let reply = command
                .in_scope(|| current.send(Box::new(2u64)))
                .await
                .unwrap();
            let reply = reply.downcast::<(u64, Option<[u8; 16]>, String)>().unwrap();
            assert_eq!((reply.0, reply.1), (42, Some(command.trace_id())));
            match mode {
                Mode::Dedicated => assert!(reply.2.starts_with("parrot-dedicated-")),
                Mode::Sharded => assert!(reply.2.starts_with("parrot-shard-")),
                Mode::Actix => assert!(!reply.2.is_empty()),
            }
            assert_eq!(old.stop().await.map_err(|error| error.to_string()), stopped);
            assert!(
                current.send(Box::new(2u64)).await.is_ok(),
                "old generation stopped its replacement"
            );
            current.stop().await.unwrap();
            retired(current.as_ref()).await;
            assert_eq!(replacement.outputs.load(Ordering::SeqCst), 1);
            assert_eq!(replacement.captures.load(Ordering::SeqCst), 1);
            assert_eq!(replacement.initialized.load(Ordering::SeqCst), 1);
            assert_eq!(replacement.stopped.load(Ordering::SeqCst), 1);
            assert!(engine.runtime().get_actor(PATH).await.is_none());
            let first = engine
                .runtime()
                .shutdown()
                .await
                .map_err(|error| error.to_string());
            assert_eq!(
                engine
                    .runtime()
                    .shutdown()
                    .await
                    .map_err(|error| error.to_string()),
                first
            );
            if matches!(mode, Mode::Actix) {
                first.unwrap();
            } else {
                assert!(
                    first
                        .unwrap_err()
                        .contains("shared task unexpectedly cancelled")
                );
            }
        });
    }
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_HANDOFF_CHILD").as_deref() == Ok(name) {
        body();
        let path = std::env::var_os("PARROT_HANDOFF_RECEIPT").unwrap();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut file, name.as_bytes()).unwrap();
        return;
    }
    let receipt = std::env::temp_dir().join(format!("parrot-handoff-{}", uuid::Uuid::new_v4()));
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_HANDOFF_CHILD", name)
            .env("PARROT_HANDOFF_RECEIPT", &receipt)
            .spawn()
            .unwrap(),
    );
    let end = Instant::now() + WAIT * 12;
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "pending handoff contract failed: {name}: {status}"
            );
            assert_eq!(
                std::fs::read_to_string(&receipt).unwrap(),
                name,
                "requested child did not complete"
            );
            std::fs::remove_file(&receipt).unwrap();
            return;
        }
        assert!(
            Instant::now() < end,
            "host join or constructor disposal blocked: {name}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident, $mode:ident, $facade:expr, $deadline:expr, $last:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                contract(Mode::$mode, $facade, $deadline, $last)
            });
        }
    };
}
case!(dedicated_cancel_reuse, Dedicated, false, false, false);
case!(dedicated_deadline_reuse, Dedicated, false, true, false);
case!(dedicated_cancel_last_owner, Dedicated, false, false, true);
case!(dedicated_deadline_last_owner, Dedicated, false, true, true);
case!(sharded_cancel_reuse, Sharded, false, false, false);
case!(sharded_deadline_reuse, Sharded, false, true, false);
case!(sharded_cancel_last_owner, Sharded, false, false, true);
case!(sharded_deadline_last_owner, Sharded, false, true, true);
case!(actix_cancel_reuse, Actix, false, false, false);
case!(actix_deadline_reuse, Actix, false, true, false);
case!(actix_cancel_last_owner, Actix, false, false, true);
case!(actix_deadline_last_owner, Actix, false, true, true);
case!(facade_dedicated_cancel_reuse, Dedicated, true, false, false);
case!(
    facade_dedicated_deadline_reuse,
    Dedicated,
    true,
    true,
    false
);
case!(
    facade_dedicated_cancel_last_owner,
    Dedicated,
    true,
    false,
    true
);
case!(
    facade_dedicated_deadline_last_owner,
    Dedicated,
    true,
    true,
    true
);
case!(facade_sharded_cancel_reuse, Sharded, true, false, false);
case!(facade_sharded_deadline_reuse, Sharded, true, true, false);
case!(facade_sharded_cancel_last_owner, Sharded, true, false, true);
case!(
    facade_sharded_deadline_last_owner,
    Sharded,
    true,
    true,
    true
);
case!(facade_actix_cancel_reuse, Actix, true, false, false);
case!(facade_actix_deadline_reuse, Actix, true, true, false);
case!(facade_actix_cancel_last_owner, Actix, true, false, true);
case!(facade_actix_deadline_last_owner, Actix, true, true, true);

mod creation_traces {
    use super::*;
    use parrot_api::{
        actor::ActixEngineExt,
        engine::{AsyncCohortRuntime, AsyncLocalRuntime, LocalActor, LocalRuntimeFactory},
        local_typed::LocalBoxedFuture,
        supervisor::{
            BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
        },
        system::ActorSystem,
        typed_context::TypedContext,
    };
    use std::rc::Rc;

    struct NativeThread(Probe);
    impl Actor for NativeThread {
        type Config = Box<dyn Any + Send>;
        type Context = parrot::thread::ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            self.0.init(ctx)
        }
        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.0.receive_message(message, ctx)
        }
    }
    struct NativeActix(Probe);
    impl ActixEngineExt for NativeActix {}
    impl Actor for NativeActix {
        type Config = Box<dyn Any + Send>;
        type Context = parrot::actix::context::ActixContext<parrot::actix::actor::ActixActor<Self>>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn use_async_handler(&self) -> bool {
            true
        }
        fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            self.0.init(ctx)
        }
        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.0.receive_message(message, ctx)
        }
    }
    struct Local(Probe, Rc<u64>, std::thread::ThreadId);
    impl LocalActor for Local {
        fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
            self.0.state.note("init");
            self.0.state.initialized.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn receive<'a>(
            &'a mut self,
            message: BoxedMessage,
            _: &'a TypedContext,
        ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                let local = self.1.clone();
                tokio::task::yield_now().await;
                assert_eq!(std::thread::current().id(), self.2);
                assert!(Rc::ptr_eq(&local, &self.1));
                Ok(Box::new((
                    *local + *message.downcast::<u64>().unwrap(),
                    TraceContext::current().map(|t| t.trace_id()),
                )) as BoxedMessage)
            })
        }
    }
    impl Drop for Local {
        fn drop(&mut self) {
            assert_eq!(std::thread::current().id(), self.2);
        }
    }
    fn ready() -> Arc<State> {
        let state = Arc::new(State::default());
        state.construct.release();
        state.output.release();
        state.capture.release();
        state
    }
    struct Static(Probe);
    struct Query(u64);
    type StaticReply = (u64, Option<[u8; 16]>, String);
    impl parrot_api::Message for Query {
        type Result = StaticReply;
    }
    impl parrot_api::typed::ParrotMsgVariant<Static> for Query {
        fn inject(message: Self) -> Self {
            message
        }
        fn extract(reply: StaticReply) -> ActorResult<StaticReply> {
            Ok(reply)
        }
    }
    impl parrot_api::typed::ParrotTypedDispatch for Static {
        type Msg = Query;
        type Reply = StaticReply;
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.0.state.note("init");
            self.0.state.initialized.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn dispatch(&mut self, message: Query) -> BoxedFuture<'_, ActorResult<StaticReply>> {
            Box::pin(async move {
                Ok((
                    self.0.value + message.0,
                    TraceContext::current().map(|t| t.trace_id()),
                    std::thread::current().name().unwrap_or("").to_owned(),
                ))
            })
        }
    }
    fn local_factory(state: &Arc<State>) -> LocalRuntimeFactory {
        let capture = Capture(state.clone());
        Box::new(move || {
            capture.0.note("constructor");
            Box::new(Local(
                Probe {
                    state: capture.0.clone(),
                    value: 40,
                },
                Rc::new(40),
                std::thread::current().id(),
            ))
        })
    }
    fn create<'a>(
        engine: &'a Engine,
        route: usize,
        state: &Arc<State>,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        if route == 0 {
            return engine.runtime().spawn(
                Box::new(Probe {
                    state: state.clone(),
                    value: 40,
                }),
                Box::new(Capture(state.clone())),
                SpawnOptions::at(PATH),
            );
        }
        if route == 10 {
            let Engine::Actix(system) = engine else {
                unreachable!()
            };
            return system.spawn_local_factory(local_factory(state), options());
        }
        if route == 14 {
            let Engine::Actix(system) = engine else {
                unreachable!()
            };
            let owned = Capture(state.clone());
            return Box::pin(
                system
                    .spawn_typed_factory::<Static, Query, _>(
                        move || {
                            owned.0.note("constructor");
                            Static(Probe {
                                state: owned.0.clone(),
                                value: 40,
                            })
                        },
                        PATH,
                    )
                    .map(|result| {
                        result
                            .map(|reference| reference.into_dyn())
                            .map_err(SystemError::ActorError)
                    }),
            );
        }
        let Engine::Facade(system) = engine else {
            unreachable!()
        };
        let probe = || Probe {
            state: state.clone(),
            value: 40,
        };
        match route {
            1 => AsyncEngineRuntime::spawn(
                system.as_ref(),
                Box::new(probe()),
                Box::new(Capture(state.clone())),
                SpawnOptions::at(PATH),
            ),
            2 => system.spawn_on(
                "handoff",
                Box::new(probe()),
                Box::new(Capture(state.clone())),
                SpawnOptions::at(PATH),
            ),
            3 => AsyncFactoryRuntime::spawn_factory(system.as_ref(), factory(state, 40), options()),
            4 => system.spawn_factory_on("handoff", factory(state, 40), options()),
            5 => system.spawn_root_thread(NativeThread(probe()), Box::new(Capture(state.clone()))),
            6 => system.spawn_root_thread_in_system(
                "handoff",
                NativeThread(probe()),
                Box::new(Capture(state.clone())),
            ),
            7 => ActorSystem::spawn_root_boxed(
                system.as_ref(),
                Box::new(probe()),
                Box::new(Capture(state.clone())),
            ),
            8 => system.internal_spawn_actor(probe(), Box::new(Capture(state.clone()))),
            9 => system.spawn_actor_in_system("handoff", probe(), Box::new(Capture(state.clone()))),
            11 => AsyncLocalRuntime::spawn_local_factory(
                system.as_ref(),
                local_factory(state),
                options(),
            ),
            12 => system.spawn_local_factory_on("handoff", local_factory(state), options()),
            13 => system.spawn_root_actix(NativeActix(probe()), Box::new(Capture(state.clone()))),
            _ => unreachable!(),
        }
    }
    fn contract(mode: Mode, route: usize, exit: bool) {
        for (capture, poll) in [
            (Some(TraceContext::root()), Some(TraceContext::root())),
            (None, Some(TraceContext::root())),
            (Some(TraceContext::root()), None),
        ] {
            let expected = capture.as_ref().map(TraceContext::trace_id);
            let host = actix::System::new();
            let engine = host.block_on(Engine::new(mode, !matches!(route, 0 | 10 | 14), false));
            let state = ready();
            let pending = trace::in_scope(capture, || create(&engine, route, &state));
            let work = trace::scope(poll, async {
                let result = tokio::time::timeout(WAIT, pending).await.unwrap();
                if matches!(route, 8 | 9) {
                    assert!(
                        matches!(result, Err(SystemError::ActorCreationError(ref text)) if text.starts_with("Generic actor creation not supported directly.")),
                        "{result:?}"
                    );
                    assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
                    assert_eq!(state.outputs.load(Ordering::SeqCst), 1);
                    assert_eq!(state.captures.load(Ordering::SeqCst), 1);
                    let mut events = state.events.lock().unwrap().clone();
                    events.sort();
                    assert_eq!(events, vec![("capture", expected), ("output", expected)]);
                } else {
                    let actor = result.unwrap();
                    let command = TraceContext::root();
                    let message: BoxedMessage = if route == 14 {
                        Box::new(Query(2))
                    } else {
                        Box::new(2u64)
                    };
                    let reply = command.in_scope(|| actor.send(message)).await.unwrap();
                    if matches!(route, 10..=12) {
                        assert_eq!(
                            *reply.downcast::<(u64, Option<[u8; 16]>)>().unwrap(),
                            (42, Some(command.trace_id()))
                        );
                    } else {
                        let reply = reply.downcast::<(u64, Option<[u8; 16]>, String)>().unwrap();
                        assert_eq!((reply.0, reply.1), (42, Some(command.trace_id())));
                    }
                    assert_eq!(state.initialized.load(Ordering::SeqCst), 1);
                    let events = state.events.lock().unwrap().clone();
                    assert!(
                        events.contains(&("init", expected)),
                        "instance init used the polling trace: {events:?}"
                    );
                    if matches!(route, 3 | 4 | 10..=12 | 14) {
                        assert!(
                            events.contains(&("constructor", expected)),
                            "factory used the polling trace: {events:?}"
                        );
                    }
                    actor.stop().await.unwrap();
                    retired(actor.as_ref()).await;
                    assert_eq!(state.outputs.load(Ordering::SeqCst), 1);
                    assert_eq!(state.captures.load(Ordering::SeqCst), 1);
                    assert!(
                        state
                            .events
                            .lock()
                            .unwrap()
                            .contains(&("capture", expected)),
                        "config/factory disposal used the polling trace"
                    );
                }
                let result = engine.runtime().shutdown().await;
                if exit && !matches!(mode, Mode::Actix) {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("shared task unexpectedly cancelled")
                    );
                } else {
                    result.unwrap();
                }
            });
            if exit {
                drop(host);
                observer().block_on(work);
            } else {
                host.block_on(work);
            }
        }
    }
    fn cohort(mode: Mode, route: usize) {
        for (capture, poll) in [
            (Some(TraceContext::root()), Some(TraceContext::root())),
            (None, Some(TraceContext::root())),
            (Some(TraceContext::root()), None),
        ] {
            let expected = capture.as_ref().map(TraceContext::trace_id);
            let host = actix::System::new();
            let engine = host.block_on(Engine::new(mode, route != 0, false));
            let state = ready();
            let owned = Capture(state.clone());
            let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 1,
                within: WAIT,
                decider: BasicDecisionFn::new(move |_| {
                    std::hint::black_box(&owned);
                    SupervisionDecision::Stop
                }),
            });
            let pending = trace::in_scope(capture, || {
                if route == 0 {
                    engine
                        .runtime()
                        .cohort_runtime()
                        .unwrap()
                        .root_cohort(strategy)
                } else {
                    let Engine::Facade(system) = &engine else {
                        unreachable!()
                    };
                    if route == 1 {
                        AsyncCohortRuntime::root_cohort(system.as_ref(), strategy)
                    } else {
                        system.root_cohort_on("handoff", strategy)
                    }
                }
            });
            host.block_on(trace::scope(poll, async {
                let cohort = tokio::time::timeout(WAIT, pending).await.unwrap().unwrap();
                cohort.stop().await.unwrap();
                assert_eq!(state.captures.load(Ordering::SeqCst), 1);
                assert_eq!(
                    *state.events.lock().unwrap(),
                    vec![("capture", expected)],
                    "cohort policy disposal used the polling trace"
                );
                engine.runtime().shutdown().await.unwrap();
            }));
        }
    }
    macro_rules! route {
        ($name:ident, $mode:ident, $route:expr) => {
            #[test]
            fn $name() {
                isolated(concat!("creation_traces::", stringify!($name)), || {
                    contract(Mode::$mode, $route, false)
                });
            }
        };
    }
    macro_rules! group {
        ($name:ident, $mode:ident, $route:expr) => {
            #[test]
            fn $name() {
                isolated(concat!("creation_traces::", stringify!($name)), || {
                    cohort(Mode::$mode, $route)
                });
            }
        };
    }
    macro_rules! after_exit {
        ($name:ident, $mode:ident, $route:expr) => {
            #[test]
            fn $name() {
                isolated(concat!("creation_traces::", stringify!($name)), || {
                    contract(Mode::$mode, $route, true)
                });
            }
        };
    }
    route!(thread_native_instance_trace, Dedicated, 0);
    route!(thread_default_instance_trace, Dedicated, 1);
    route!(thread_named_instance_trace, Dedicated, 2);
    route!(thread_default_factory_trace, Dedicated, 3);
    route!(thread_named_factory_trace, Dedicated, 4);
    route!(thread_legacy_default_trace, Dedicated, 5);
    route!(thread_legacy_named_trace, Dedicated, 6);
    route!(thread_legacy_boxed_trace, Dedicated, 7);
    route!(thread_generic_default_trace, Dedicated, 8);
    route!(thread_generic_named_trace, Dedicated, 9);
    route!(actix_native_instance_trace, Actix, 0);
    route!(actix_default_instance_trace, Actix, 1);
    route!(actix_named_instance_trace, Actix, 2);
    route!(actix_default_factory_trace, Actix, 3);
    route!(actix_named_factory_trace, Actix, 4);
    route!(actix_legacy_boxed_trace, Actix, 7);
    route!(actix_generic_default_trace, Actix, 8);
    route!(actix_generic_named_trace, Actix, 9);
    route!(actix_native_local_trace, Actix, 10);
    route!(actix_default_local_trace, Actix, 11);
    route!(actix_named_local_trace, Actix, 12);
    route!(actix_legacy_default_trace, Actix, 13);
    route!(actix_native_static_trace, Actix, 14);
    after_exit!(thread_instance_after_host_exit, Dedicated, 0);
    after_exit!(sharded_instance_after_host_exit, Sharded, 0);
    after_exit!(actix_instance_after_host_exit, Actix, 0);
    after_exit!(facade_thread_instance_after_host_exit, Dedicated, 1);
    after_exit!(facade_sharded_instance_after_host_exit, Sharded, 2);
    after_exit!(facade_actix_instance_after_host_exit, Actix, 1);
    after_exit!(actix_local_after_host_exit, Actix, 10);
    after_exit!(facade_actix_local_after_host_exit, Actix, 12);
    after_exit!(actix_static_factory_after_host_exit, Actix, 14);
    group!(thread_native_cohort_trace, Dedicated, 0);
    group!(thread_default_cohort_trace, Dedicated, 1);
    group!(thread_named_cohort_trace, Dedicated, 2);
    group!(actix_native_cohort_trace, Actix, 0);
    group!(actix_default_cohort_trace, Actix, 1);
    group!(actix_named_cohort_trace, Actix, 2);
}
