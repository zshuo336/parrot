// SPDX-License-Identifier: AGPL-3.0-only
//! Captured engines remain protected after their catalog registration is removed.
use futures::FutureExt;
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    address::ActorRef,
    context::{ActorContext, ReadOnlyChildrenVec},
    engine::{
        AsyncCohortRuntime, AsyncEngineRuntime, AsyncFactoryRuntime, AsyncIoRuntime,
        AsyncLocalRuntime, AsyncRootCohort, FactoryOptions, InstanceConfig, LocalActor,
        LocalRuntimeFactory, RuntimeActor, RuntimeFactory, SpawnOptions,
    },
    local_typed::LocalBoxedFuture,
    message::Message,
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    system::{SystemError, SystemResources, SystemState, SystemStatus},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Counts {
    runtimes: AtomicUsize,
    inputs: AtomicUsize,
    outputs: AtomicUsize,
    stops: AtomicUsize,
    entries: AtomicUsize,
    traces: Mutex<Vec<Option<parrot_api::trace::TraceContext>>>,
}
struct Input(Arc<Counts>);
impl Drop for Input {
    fn drop(&mut self) {
        self.0.inputs.fetch_add(1, Ordering::SeqCst);
    }
}
struct Probe(Input);
struct Query;
impl Message for Query {
    type Result = ();
}
impl ParrotTypedDispatch for Probe {
    type Msg = Query;
    type Reply = ();
    fn dispatch(&mut self, _: Query) -> BoxedFuture<'_, ActorResult<()>> {
        let _ = &self.0;
        Box::pin(async { Ok(()) })
    }
}
impl ParrotMsgVariant<Probe> for Query {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(value: ()) -> ActorResult<()> {
        Ok(value)
    }
}
impl Actor for Probe {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
impl LocalActor for Probe {
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}

struct RecursivePanic;
impl Drop for RecursivePanic {
    fn drop(&mut self) {
        std::panic::panic_any(RecursivePanic);
    }
}
#[derive(Clone, Copy)]
enum Behavior {
    Error,
    Pending,
    Success,
}
#[derive(Default)]
struct Gate {
    entered: AtomicUsize,
    released: Mutex<bool>,
    ready: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.ready.notify_all();
    }
}
struct Hostile {
    counts: Arc<Counts>,
    behavior: Behavior,
    recursive: bool,
    output_panics: bool,
    gate: Option<Arc<Gate>>,
    callback: Option<Box<dyn Fn() + Send + Sync>>,
    query_callback: bool,
    engine: Option<Arc<parrot::thread::ThreadActorSystem>>,
}
impl Drop for Hostile {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.entered.fetch_add(1, Ordering::SeqCst);
            let lock = gate.released.lock().unwrap();
            drop(gate.ready.wait_while(lock, |released| !*released).unwrap());
        }
        self.counts.runtimes.fetch_add(1, Ordering::SeqCst);
        self.counts
            .traces
            .lock()
            .unwrap()
            .push(parrot_api::trace::TraceContext::current());
        if self.recursive {
            std::panic::panic_any(RecursivePanic);
        }
        panic!("selected engine destructor witness");
    }
}
impl Hostile {
    fn query(&self) {
        if self.query_callback {
            self.enter();
        }
    }
    fn enter(&self) {
        self.counts.entries.fetch_add(1, Ordering::SeqCst);
        if let Some(callback) = &self.callback {
            callback();
        }
    }
    async fn result<T>(&self, make: impl FnOnce() -> T) -> Result<T, SystemError> {
        self.enter();
        match self.behavior {
            Behavior::Error => Err(SystemError::ActorCreationError(
                "original business error".into(),
            )),
            Behavior::Pending => std::future::pending().await,
            Behavior::Success => Ok(make()),
        }
    }
    fn output(&self) -> Output {
        Output(self.counts.clone(), self.output_panics)
    }
}
struct Output(Arc<Counts>, bool);
struct StopWitness(Arc<Counts>, bool);
impl std::future::Future for StopWitness {
    type Output = ActorResult<()>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.0.stops.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            std::panic::panic_any(RecursivePanic);
        }
        std::task::Poll::Ready(Ok(()))
    }
}
impl Drop for StopWitness {
    fn drop(&mut self) {
        if self.1 {
            std::panic::panic_any(RecursivePanic);
        }
    }
}
impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("owner output")
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.0.outputs.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            std::panic::panic_any(RecursivePanic);
        }
    }
}
impl ActorRef for Output {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver(&self, _: BoxedMessage) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(StopWitness(self.0.clone(), self.1))
    }
    fn path(&self) -> String {
        "/owner/output".into()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self(self.0.clone(), self.1))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
impl AsyncRootCohort for Output {
    fn members(&self) -> ReadOnlyChildrenVec {
        ReadOnlyChildrenVec::new(Arc::new(std::sync::RwLock::new(Vec::new())))
    }
    fn stop(&self) -> BoxedFuture<'static, ActorResult<()>> {
        Box::pin(StopWitness(self.0.clone(), self.1))
    }
    fn spawn<'a>(
        &'a self,
        _: RuntimeActor,
        _: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        unreachable!()
    }
    fn spawn_factory<'a>(
        &'a self,
        _: RuntimeFactory,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        unreachable!()
    }
}
impl AsyncEngineRuntime for Hostile {
    fn shutdown_would_wait_for_current_task(&self) -> bool {
        self.query();
        false
    }
    fn io_runtime(&self) -> Option<&dyn AsyncIoRuntime> {
        self.query();
        Some(self)
    }
    fn cohort_runtime(&self) -> Option<&dyn AsyncCohortRuntime> {
        self.query();
        Some(self)
    }
    fn factory_runtime(&self) -> Option<&dyn AsyncFactoryRuntime> {
        self.query();
        Some(self)
    }
    fn local_runtime(&self) -> Option<&dyn AsyncLocalRuntime> {
        self.query();
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        Box::pin(async move {
            if let Some(engine) = &self.engine {
                self.enter();
                return AsyncEngineRuntime::spawn(
                    engine.as_ref(),
                    actor,
                    config,
                    SpawnOptions::at("/owner/live"),
                )
                .await;
            }
            let _inputs = (actor, config);
            self.result(|| Box::new(self.output()) as BoxedActorRef)
                .await
        })
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async move {
            self.result(|| Box::new(self.output()) as BoxedActorRef)
                .await
                .ok()
        })
    }
    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: SystemState::Running,
            active_actors: 0,
            uptime: Duration::ZERO,
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        }
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
}
impl AsyncFactoryRuntime for Hostile {
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        _: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        Box::pin(async move {
            let _input = factory;
            self.result(|| Box::new(self.output()) as BoxedActorRef)
                .await
        })
    }
}
impl AsyncLocalRuntime for Hostile {
    fn spawn_local_factory<'a>(
        &'a self,
        factory: LocalRuntimeFactory,
        _: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        Box::pin(async move {
            let _input = factory;
            self.result(|| Box::new(self.output()) as BoxedActorRef)
                .await
        })
    }
}
impl AsyncCohortRuntime for Hostile {
    fn root_cohort(
        &self,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, SystemError>> {
        Box::pin(async move {
            let _input = strategy;
            self.result(|| Arc::new(self.output()) as Arc<dyn AsyncRootCohort>)
                .await
        })
    }
}
impl AsyncIoRuntime for Hostile {
    fn run_io(
        &self,
        future: BoxedFuture<'static, ActorResult<BoxedMessage>>,
    ) -> BoxedFuture<'static, ActorResult<BoxedMessage>> {
        // The public I/O trait deliberately returns a future independent of self.
        self.enter();
        let behavior = self.behavior;
        let counts = self.counts.clone();
        let output_panics = self.output_panics;
        Box::pin(async move {
            let _input = future;
            match behavior {
                Behavior::Error => Err(parrot_api::errors::ActorError::InternalError(
                    "original business error".into(),
                )),
                Behavior::Pending => std::future::pending().await,
                Behavior::Success => Ok(Box::new(Output(counts, output_panics)) as BoxedMessage),
            }
        })
    }
}
fn policy() -> SupervisorStrategyType {
    SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure)
}

fn selected<'a>(
    facade: &'a ParrotActorSystem,
    named: bool,
    route: usize,
    counts: Arc<Counts>,
    invalid: bool,
) -> BoxedFuture<'a, Result<(), String>> {
    let path = if invalid { "" } else { "/owner/probe" };
    let options = SpawnOptions::at(path);
    let factory_options = FactoryOptions {
        spawn: options.clone(),
        ..Default::default()
    };
    macro_rules! erase {
        ($future:expr) => {
            Box::pin($future.map(|result| result.map(|_| ()).map_err(|error| error.to_string())))
        };
    }
    let input = Input(counts.clone());
    match route {
        0 => {
            if named {
                erase!(facade.spawn_typed_on::<_, Query>("hostile", Probe(input), path))
            } else {
                erase!(facade.spawn_typed::<_, Query>(Probe(input), path))
            }
        }
        1 => {
            let factory = move || {
                let _ = &input;
                Probe(Input(counts.clone()))
            };
            if named {
                erase!(facade.spawn_typed_factory_on::<_, Query, _>("hostile", factory, path))
            } else {
                erase!(facade.spawn_typed_factory::<_, Query, _>(factory, path))
            }
        }
        2 => {
            drop(input);
            if named {
                erase!(facade.native_root_cohort_on("hostile", policy()))
            } else {
                erase!(facade.native_root_cohort(policy()))
            }
        }
        3 => {
            drop(input);
            if named {
                erase!(facade.root_cohort_on("hostile", policy()))
            } else {
                erase!(AsyncCohortRuntime::root_cohort(facade, policy()))
            }
        }
        4 => {
            if named {
                erase!(facade.spawn_on("hostile", Box::new(Probe(input)), Box::new(()), options))
            } else {
                erase!(AsyncEngineRuntime::spawn(
                    facade,
                    Box::new(Probe(input)),
                    Box::new(()),
                    options
                ))
            }
        }
        5 => {
            let factory: RuntimeFactory = Box::new(move || {
                let _ = &input;
                Box::new(Probe(Input(counts.clone())))
            });
            if named {
                erase!(facade.spawn_factory_on("hostile", factory, factory_options))
            } else {
                erase!(AsyncFactoryRuntime::spawn_factory(
                    facade,
                    factory,
                    factory_options
                ))
            }
        }
        6 => {
            let factory: LocalRuntimeFactory = Box::new(move || {
                let _ = &input;
                Box::new(Probe(Input(counts.clone())))
            });
            if named {
                erase!(facade.spawn_local_factory_on("hostile", factory, factory_options))
            } else {
                erase!(AsyncLocalRuntime::spawn_local_factory(
                    facade,
                    factory,
                    factory_options
                ))
            }
        }
        7 => {
            let work = Box::pin(async move {
                let _ = input;
                Ok(Box::new(()) as BoxedMessage)
            });
            if named {
                erase!(facade.run_io_on("hostile", work))
            } else {
                erase!(AsyncIoRuntime::run_io(facade, work))
            }
        }
        _ => unreachable!(),
    }
}
async fn wait(value: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while value.load(Ordering::SeqCst) != expected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("actual retirement did not finish");
}
async fn setup(
    behavior: Behavior,
    recursive: bool,
    output_panics: bool,
    gate: Option<Arc<Gate>>,
) -> (Arc<ParrotActorSystem>, Arc<Counts>) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let counts = Arc::new(Counts::default());
    facade
        .register_runtime(
            "hostile".into(),
            Arc::new(Hostile {
                counts: counts.clone(),
                behavior,
                recursive,
                output_panics,
                gate,
                callback: None,
                query_callback: false,
                engine: None,
            }),
            true,
        )
        .unwrap();
    (facade, counts)
}
fn remove(facade: &ParrotActorSystem) {
    drop(
        facade
            .unregister_system("hostile")
            .unwrap()
            .expect("registered owner"),
    );
}
#[path = "support/isolated.rs"]
mod isolated_child;

fn isolated(name: &str, body: impl FnOnce()) {
    isolated_child::run(
        name,
        "PARROT_SELECTED_OWNER_CHILD",
        Duration::from_secs(30),
        body,
    );
}
fn run(future: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}
macro_rules! contract {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || run($body));
        }
    };
}

contract!(
    unpolled_default_and_named_creation_drop_isolates_last_engine_owner,
    async {
        for named in [false, true] {
            for route in 0..8 {
                let (facade, counts) = setup(Behavior::Error, true, false, None).await;
                let pending = selected(&facade, named, route, counts.clone(), false);
                remove(&facade);
                assert_eq!(
                    counts.runtimes.load(Ordering::SeqCst),
                    0,
                    "route {route} must retain selected owner"
                );
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending)))
                        .is_ok(),
                    "route {route}"
                );
                wait(&counts.runtimes, 1).await;
                wait(&counts.inputs, 1).await;
                facade.shutdown_runtime().await.unwrap();
            }
        }
    }
);

contract!(
    rejected_and_business_errors_survive_engine_cleanup_failure,
    async {
        for named in [false, true] {
            for route in 0..8 {
                let (facade, counts) = setup(Behavior::Error, true, false, None).await;
                let pending = selected(&facade, named, route, counts.clone(), false);
                remove(&facade);
                let error = pending.await.expect_err("selected runtime cannot succeed");
                assert!(
                    !error.contains("disposal"),
                    "original error lost for route {route}: {error}"
                );
                if route >= 3 {
                    assert!(error.contains("original business error"), "{error}");
                }
                assert_eq!(counts.runtimes.load(Ordering::SeqCst), 1);
                wait(&counts.inputs, 1).await;
                facade.shutdown_runtime().await.unwrap();
            }
        }
    }
);

contract!(
    validation_rejection_retires_inputs_and_preserves_path_error,
    async {
        for named in [false, true] {
            for route in [4, 5, 6] {
                let (facade, counts) = setup(Behavior::Success, false, false, None).await;
                let pending = selected(&facade, named, route, counts.clone(), true);
                remove(&facade);
                let error = pending.await.unwrap_err();
                assert!(error.contains("path"), "{error}");
                assert_eq!(counts.entries.load(Ordering::SeqCst), 0);
                assert_eq!(counts.runtimes.load(Ordering::SeqCst), 1);
                wait(&counts.inputs, 1).await;
                facade.shutdown_runtime().await.unwrap();
            }
        }
    }
);

contract!(
    cancellation_of_polled_work_keeps_last_engine_and_inputs_protected,
    async {
        for named in [false, true] {
            for route in 3..8 {
                let (facade, counts) = setup(Behavior::Pending, true, false, None).await;
                let mut pending = selected(&facade, named, route, counts.clone(), false);
                remove(&facade);
                assert!(futures::poll!(pending.as_mut()).is_pending());
                assert_eq!(counts.entries.load(Ordering::SeqCst), 1);
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending)))
                        .is_ok()
                );
                wait(&counts.runtimes, 1).await;
                wait(&counts.inputs, 1).await;
                facade.shutdown_runtime().await.unwrap();
            }
        }
    }
);

contract!(
    owner_failure_retires_successful_unpublished_outputs_even_when_they_panic,
    async {
        for output_panics in [false, true] {
            for named in [false, true] {
                for route in 3..8 {
                    let (facade, counts) =
                        setup(Behavior::Success, true, output_panics, None).await;
                    let pending = selected(&facade, named, route, counts.clone(), false);
                    remove(&facade);
                    let error = pending
                        .await
                        .expect_err("failed owner cleanup cannot publish success");
                    assert!(error.contains("disposal"), "{error}");
                    assert_eq!(counts.runtimes.load(Ordering::SeqCst), 1);
                    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
                    assert_eq!(counts.stops.load(Ordering::SeqCst), usize::from(route != 7));
                    wait(&counts.inputs, 1).await;
                    facade.shutdown_runtime().await.unwrap();
                }
            }
        }
    }
);

contract!(
    blocking_engine_cleanup_delays_publication_and_survives_waiter_cancellation,
    async {
        for cancel in [false, true] {
            let gate = Arc::new(Gate::default());
            let (facade, counts) = setup(Behavior::Success, true, true, Some(gate.clone())).await;
            let mut pending = selected(&facade, true, 4, counts.clone(), false);
            remove(&facade);
            assert!(futures::poll!(pending.as_mut()).is_pending());
            wait(&gate.entered, 1).await;
            assert_eq!(
                counts.outputs.load(Ordering::SeqCst),
                0,
                "output must remain unpublished and retained"
            );
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if cancel {
                drop(pending);
                gate.release();
            } else {
                gate.release();
                assert!(pending.await.unwrap_err().contains("disposal"));
            }
            wait(&counts.runtimes, 1).await;
            wait(&counts.outputs, 1).await;
            assert_eq!(counts.stops.load(Ordering::SeqCst), 1);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    nested_leaf_selection_remains_protected_after_both_catalogs_are_removed,
    async {
        for route in 0..4 {
            let (inner, counts) = setup(Behavior::Error, true, false, None).await;
            let outer = ParrotActorSystem::new(Default::default()).await.unwrap();
            outer
                .register_runtime("hostile".into(), inner.clone(), true)
                .unwrap();
            let pending = selected(&outer, true, route, counts.clone(), false);
            remove(&inner);
            remove(&outer);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending))).is_ok()
            );
            wait(&counts.runtimes, 1).await;
            wait(&counts.inputs, 1).await;
            inner.shutdown_runtime().await.unwrap();
            outer.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    shutdown_rejection_still_retires_captured_engine_and_original_inputs,
    async {
        for named in [false, true] {
            for route in 0..7 {
                let (facade, counts) = setup(Behavior::Error, true, false, None).await;
                let pending = selected(&facade, named, route, counts.clone(), false);
                remove(&facade);
                facade.shutdown_runtime().await.unwrap();
                let error = pending.await.unwrap_err();
                assert!(!error.contains("disposal"), "{error}");
                assert_eq!(counts.entries.load(Ordering::SeqCst), 0);
                assert_eq!(counts.runtimes.load(Ordering::SeqCst), 1);
                wait(&counts.inputs, 1).await;
            }
        }
    }
);

contract!(
    owner_cleanup_uses_creation_trace_after_cancellation_in_another_scope,
    async {
        use parrot_api::trace::{TraceContext, in_scope};
        let (facade, counts) = setup(Behavior::Error, true, false, None).await;
        let creation = TraceContext::root();
        let cancellation = TraceContext::root();
        let pending = in_scope(Some(creation.clone()), || {
            selected(&facade, true, 4, counts.clone(), false)
        });
        remove(&facade);
        in_scope(Some(cancellation), || drop(pending));
        wait(&counts.runtimes, 1).await;
        while counts.traces.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        let trace = counts.traces.lock().unwrap()[0]
            .clone()
            .expect("creation trace must survive");
        assert_eq!(trace.trace_id(), creation.trace_id());
        assert_eq!(trace.span_id(), creation.span_id());
        facade.shutdown_runtime().await.unwrap();
    }
);

#[derive(Debug)]
struct OriginalCallbackPanic;

contract!(
    creation_retirement_reservations_bound_pending_work_and_cancelled_admission,
    async {
        let (facade, counts) = setup(Behavior::Pending, true, false, None).await;
        // The facade's one disposal slot permits 1024 pending terminal owners.
        let mut pending = Vec::new();
        for _ in 0..1024 {
            tokio::task::yield_now().await;
            let mut work = selected(&facade, true, 4, counts.clone(), false);
            assert!(futures::poll!(work.as_mut()).is_pending());
            pending.push(work);
        }
        assert_eq!(counts.entries.load(Ordering::SeqCst), 1024);
        let mut blocked = selected(&facade, true, 4, counts.clone(), false);
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        assert_eq!(
            counts.entries.load(Ordering::SeqCst),
            1024,
            "saturated cleanup admission must precede underlying creation"
        );
        drop(pending.pop());
        tokio::task::yield_now().await;
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        assert_eq!(
            counts.entries.load(Ordering::SeqCst),
            1025,
            "cancelled waiter releases its terminal-owner reservation"
        );
        remove(&facade);
        drop(blocked);
        drop(pending);
        wait(&counts.inputs, 1025).await;
        wait(&counts.runtimes, 1).await;
        facade.shutdown_runtime().await.unwrap();
    }
);

struct ReentrantStop {
    facade: Arc<ParrotActorSystem>,
    hooks: Arc<AtomicUsize>,
    input: Input,
}
impl Actor for ReentrantStop {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                self.facade.spawn_on(
                    "live",
                    Box::new(Probe(Input(self.input.0.clone()))),
                    Box::new(()),
                    SpawnOptions::at("/owner/reentry"),
                ),
            )
            .await
            .map_err(|_| {
                parrot_api::errors::ActorError::InternalError(
                    "cleanup slot prevented valid stop-hook reentry".into(),
                )
            })?
            .map_err(|error| parrot_api::errors::ActorError::InternalError(error.to_string()))?;
            result.stop().await?;
            self.hooks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

contract!(
    discarded_creation_stop_hook_can_reenter_the_same_facade,
    async {
        let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
        let engine = parrot::thread::ThreadActorSystem::shared(Default::default());
        let counts = Arc::new(Counts::default());
        let hooks = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Gate::default());
        facade
            .register_runtime("live".into(), engine.clone(), false)
            .unwrap();
        facade
            .register_runtime(
                "hostile".into(),
                Arc::new(Hostile {
                    counts: counts.clone(),
                    behavior: Behavior::Success,
                    recursive: true,
                    output_panics: false,
                    gate: Some(gate.clone()),
                    callback: None,
                    query_callback: false,
                    engine: Some(engine.clone()),
                }),
                true,
            )
            .unwrap();
        let pending = facade.spawn_on(
            "hostile",
            Box::new(ReentrantStop {
                facade: facade.clone(),
                hooks: hooks.clone(),
                input: Input(counts.clone()),
            }),
            Box::new(()),
            SpawnOptions::at("/owner/live"),
        );
        remove(&facade);
        let mut pending = Box::pin(pending);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(futures::poll!(pending.as_mut()).is_pending());
                if gate.entered.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let reference = AsyncEngineRuntime::get_actor(engine.as_ref(), "/owner/live")
            .await
            .unwrap();
        // Admission may precede initialization. A completed business reply makes
        // the before_stop expectation independent of scheduler timing.
        tokio::time::timeout(Duration::from_secs(3), reference.send(Box::new(())))
            .await
            .unwrap()
            .unwrap();
        drop(reference);
        gate.release();
        assert!(pending.await.unwrap_err().to_string().contains("disposal"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while counts.inputs.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "reentrant retirement failed: inputs={} hooks={} runtime_drops={} original={error}",
                counts.inputs.load(Ordering::SeqCst),
                hooks.load(Ordering::SeqCst),
                counts.runtimes.load(Ordering::SeqCst)
            )
        });
        assert_eq!(
            hooks.load(Ordering::SeqCst),
            1,
            "real stop hook must finish its nested facade creation"
        );
        facade.shutdown_runtime().await.unwrap();
    }
);

contract!(
    discarded_success_stops_the_real_native_actor_while_its_engine_remains_alive,
    async {
        for cancel in [false, true] {
            let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
            let engine = parrot::thread::ThreadActorSystem::shared(Default::default());
            let counts = Arc::new(Counts::default());
            let gate = Arc::new(Gate::default());
            facade
                .register_runtime(
                    "hostile".into(),
                    Arc::new(Hostile {
                        counts: counts.clone(),
                        behavior: Behavior::Success,
                        recursive: true,
                        output_panics: false,
                        gate: Some(gate.clone()),
                        callback: None,
                        query_callback: false,
                        engine: Some(engine.clone()),
                    }),
                    true,
                )
                .unwrap();
            let task_facade = facade.clone();
            let task_counts = counts.clone();
            // Capture selection before unregistering, but start native admission on polling.
            let pending = facade.spawn_on(
                "hostile",
                Box::new(Probe(Input(task_counts))),
                Box::new(()),
                SpawnOptions::at("/owner/live"),
            );
            remove(&facade);
            let mut pending = Box::pin(pending);
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    assert!(futures::poll!(pending.as_mut()).is_pending());
                    if gate.entered.load(Ordering::SeqCst) == 1 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let reference = AsyncEngineRuntime::get_actor(engine.as_ref(), "/owner/live")
                .await
                .expect("actual native actor admitted");
            assert!(reference.is_alive().await);
            if cancel {
                drop(pending);
                gate.release();
            } else {
                gate.release();
                assert!(pending.await.unwrap_err().to_string().contains("disposal"));
            }
            wait(&counts.inputs, 1).await;
            assert!(
                !reference.is_alive().await,
                "discarding the result must stop the actor, even with a live engine owner"
            );
            drop(reference);
            task_facade.shutdown_runtime().await.unwrap();
            AsyncEngineRuntime::shutdown(engine.as_ref()).await.unwrap();
        }
    }
);

contract!(
    capability_queries_preserve_callback_panic_identity_after_self_unregister,
    async {
        for query in 0..5 {
            for callback_panics in [false, true] {
                let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
                let counts = Arc::new(Counts::default());
                let weak = Arc::downgrade(&facade);
                facade
                    .register_runtime(
                        "hostile".into(),
                        Arc::new(Hostile {
                            counts: counts.clone(),
                            behavior: Behavior::Error,
                            recursive: true,
                            output_panics: false,
                            gate: None,
                            query_callback: true,
                            engine: None,
                            callback: Some(Box::new(move || {
                                remove(&weak.upgrade().unwrap());
                                if callback_panics {
                                    std::panic::panic_any(OriginalCallbackPanic);
                                }
                            })),
                        }),
                        true,
                    )
                    .unwrap();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match query {
                        0 => {
                            let _ = AsyncEngineRuntime::io_runtime(facade.as_ref());
                        }
                        1 => {
                            let _ = AsyncEngineRuntime::factory_runtime(facade.as_ref());
                        }
                        2 => {
                            let _ = AsyncEngineRuntime::local_runtime(facade.as_ref());
                        }
                        3 => {
                            let _ = AsyncEngineRuntime::cohort_runtime(facade.as_ref());
                        }
                        _ => {
                            let _ = AsyncEngineRuntime::shutdown_would_wait_for_current_task(
                                facade.as_ref(),
                            );
                        }
                    }));
                let payload = result.expect_err("query owner cleanup must not silently succeed");
                assert_eq!(payload.is::<OriginalCallbackPanic>(), callback_panics);
                assert_eq!(counts.runtimes.load(Ordering::SeqCst), 1);
                facade.shutdown_runtime().await.unwrap();
            }
        }
    }
);

contract!(
    async_lookup_snapshot_owners_and_unpublished_result_are_individually_protected,
    async {
        for callback_panics in [false, true] {
            for pending in [false, true] {
                let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
                let counts = Arc::new(Counts::default());
                let weak = Arc::downgrade(&facade);
                facade
                    .register_runtime(
                        "hostile".into(),
                        Arc::new(Hostile {
                            counts: counts.clone(),
                            behavior: if pending {
                                Behavior::Pending
                            } else {
                                Behavior::Success
                            },
                            recursive: true,
                            output_panics: true,
                            gate: None,
                            query_callback: false,
                            engine: None,
                            callback: Some(Box::new(move || {
                                let facade = weak.upgrade().unwrap();
                                remove(&facade);
                                drop(facade.unregister_system("second").unwrap());
                                if callback_panics {
                                    std::panic::panic_any(OriginalCallbackPanic);
                                }
                            })),
                        }),
                        true,
                    )
                    .unwrap();
                facade
                    .register_runtime(
                        "second".into(),
                        Arc::new(Hostile {
                            counts: counts.clone(),
                            behavior: Behavior::Error,
                            recursive: true,
                            output_panics: false,
                            gate: None,
                            query_callback: false,
                            engine: None,
                            callback: None,
                        }),
                        false,
                    )
                    .unwrap();
                let mut lookup = std::panic::AssertUnwindSafe(AsyncEngineRuntime::get_actor(
                    facade.as_ref(),
                    "/owner/output",
                ))
                .catch_unwind();
                if pending && !callback_panics {
                    assert!(futures::poll!(&mut lookup).is_pending());
                    assert!(
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(lookup)))
                            .is_ok()
                    );
                } else {
                    let payload = lookup
                        .await
                        .expect_err("failed snapshot owner cleanup cannot publish result");
                    assert_eq!(payload.is::<OriginalCallbackPanic>(), callback_panics);
                }
                wait(&counts.runtimes, 2).await;
                wait(&counts.outputs, usize::from(!pending && !callback_panics)).await;
                facade.shutdown_runtime().await.unwrap();
            }
        }
    }
);
