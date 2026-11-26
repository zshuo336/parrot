// SPDX-License-Identifier: AGPL-3.0-only
//! Registry snapshots protect each engine independently from user callbacks.
use futures::FutureExt;
use parrot::{
    mirror::{MirrorCopier, MirrorPolicy},
    system::{ActorSystemImpl, ParrotActorSystem},
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState},
    address::{ActorPath, ActorRef},
    context::ActorContext,
    engine::{AsyncEngineRuntime, AsyncIoRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    system::{ActorSystem, SystemError, SystemResources, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

type Callback = Box<dyn Fn() + Send + Sync>;
#[derive(Debug)]
struct Original;
struct Recursive;
impl Drop for Recursive {
    fn drop(&mut self) {
        std::panic::panic_any(Recursive);
    }
}
#[derive(Default)]
struct State {
    engines: AtomicUsize,
    calls: AtomicUsize,
    outputs: AtomicUsize,
    stops: AtomicUsize,
    inputs: AtomicUsize,
}
#[derive(Default)]
struct Gate {
    entered: AtomicUsize,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Engine {
    state: Arc<State>,
    callback: Callback,
    status_callback: bool,
    pending: bool,
    output_panics: bool,
    future_panics: bool,
    drop_future_only: bool,
    missing: bool,
    drop_panics: bool,
    gate: Option<Arc<Gate>>,
}
impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.entered.fetch_add(1, Ordering::SeqCst);
            drop(
                gate.changed
                    .wait_while(gate.open.lock().unwrap(), |open| !*open)
                    .unwrap(),
            );
        }
        self.state.engines.fetch_add(1, Ordering::SeqCst);
        if self.drop_panics {
            std::panic::panic_any(Recursive);
        }
    }
}
struct LookupFuture<'a> {
    engine: &'a Engine,
}
impl std::future::Future for LookupFuture<'_> {
    type Output = Option<BoxedActorRef>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let engine = self.engine;
        engine.state.calls.fetch_add(1, Ordering::SeqCst);
        (engine.callback)();
        if engine.future_panics {
            std::panic::panic_any(Original);
        }
        if engine.pending {
            return std::task::Poll::Pending;
        }
        if engine.missing {
            return std::task::Poll::Ready(None);
        }
        std::task::Poll::Ready(Some(Box::new(Reference(
            engine.state.clone(),
            engine.output_panics,
        ))))
    }
}
impl Drop for LookupFuture<'_> {
    fn drop(&mut self) {
        if self.engine.future_panics || self.engine.drop_future_only {
            std::panic::panic_any(Recursive);
        }
    }
}
impl AsyncEngineRuntime for Engine {
    fn io_runtime(&self) -> Option<&dyn AsyncIoRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        _: RuntimeActor,
        _: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        if self.future_panics {
            return Box::pin(PanicFuture::<Result<BoxedActorRef, SystemError>>(
                std::marker::PhantomData,
            ));
        }
        Box::pin(async {
            Err(SystemError::ActorCreationError(
                "unsupported test engine".into(),
            ))
        })
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(LookupFuture { engine: self })
    }
    fn status(&self) -> SystemStatus {
        if self.status_callback {
            self.state.calls.fetch_add(1, Ordering::SeqCst);
            (self.callback)();
        }
        SystemStatus {
            state: SystemState::Running,
            active_actors: 7,
            uptime: Duration::ZERO,
            resources: SystemResources {
                cpu_usage: 1.0,
                memory_usage: 11,
                thread_count: 2,
            },
        }
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
}
struct PanicFuture<T>(std::marker::PhantomData<fn() -> T>);
impl<T> std::future::Future for PanicFuture<T> {
    type Output = T;
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<T> {
        std::panic::panic_any(Original)
    }
}
impl<T> Drop for PanicFuture<T> {
    fn drop(&mut self) {
        std::panic::panic_any(Recursive);
    }
}
impl AsyncIoRuntime for Engine {
    fn run_io(
        &self,
        _: BoxedFuture<'static, ActorResult<BoxedMessage>>,
    ) -> BoxedFuture<'static, ActorResult<BoxedMessage>> {
        Box::pin(PanicFuture(std::marker::PhantomData))
    }
}
#[derive(Debug)]
struct Reference(Arc<State>, bool);
impl Drop for Reference {
    fn drop(&mut self) {
        self.0.outputs.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            std::panic::panic_any(Recursive);
        }
    }
}
impl ActorRef for Reference {
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
        Box::pin(async {
            self.0.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn path(&self) -> String {
        format!("/snapshot/reference-{:p}", Arc::as_ptr(&self.0))
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
impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("snapshot state")
    }
}
async fn wait(value: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while value.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("snapshot retirement did not finish");
}
fn remove_all(facade: &ParrotActorSystem) {
    for name in facade.list_registered_systems().unwrap() {
        drop(facade.unregister_system(&name).unwrap());
    }
}
async fn facade() -> Arc<ParrotActorSystem> {
    Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap())
}
fn engine(state: Arc<State>, callback: Callback) -> Engine {
    Engine {
        state,
        callback,
        status_callback: false,
        pending: false,
        output_panics: false,
        future_panics: false,
        drop_future_only: false,
        missing: false,
        drop_panics: true,
        gate: None,
    }
}
fn check_payload(payload: Box<dyn Any + Send>, original: bool) {
    let correct = payload.is::<Original>();
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload)))
        && let Err(last) = catch_unwind(AssertUnwindSafe(|| drop(payload)))
    {
        std::mem::forget(last);
    }
    assert_eq!(
        correct, original,
        "original callback panic identity was replaced"
    );
}
#[path = "support/isolated.rs"]
mod isolated_child;

fn isolated(name: &str, body: impl FnOnce()) {
    isolated_child::run(name, "PARROT_SNAPSHOT_CHILD", Duration::from_secs(12), body);
}
#[test]
fn missing_snapshot_child_cannot_pass_the_parent_contract() {
    let failure = std::panic::catch_unwind(|| {
        isolated("__missing_snapshot_contract__", || unreachable!());
    })
    .expect_err("the parent accepted an empty exact child selection");
    let message = failure.downcast_ref::<String>().unwrap();
    assert!(
        message.contains("child did not execute its body exactly once"),
        "{message}"
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
    ($name:ident,$body:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || run($body));
        }
    };
}

contract!(
    status_callback_and_each_snapshot_owner_have_independent_unwind_scopes,
    async {
        for panics in [false, true] {
            let facade = facade().await;
            let state = Arc::new(State::default());
            for name in ["first", "second"] {
                let weak = Arc::downgrade(&facade);
                let mut runtime = engine(
                    state.clone(),
                    Box::new(move || {
                        remove_all(&weak.upgrade().unwrap());
                        if panics {
                            std::panic::panic_any(Original);
                        }
                    }),
                );
                runtime.status_callback = true;
                facade
                    .register_runtime(name.into(), Arc::new(runtime), true)
                    .unwrap();
            }
            let outcome = catch_unwind(AssertUnwindSafe(|| ActorSystem::status(facade.as_ref())));
            check_payload(outcome.unwrap_err(), panics);
            assert_eq!(state.engines.load(Ordering::SeqCst), 2);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    status_aliases_sample_once_and_can_reenter_the_registry,
    async {
        let facade = facade().await;
        let state = Arc::new(State::default());
        let weak = Arc::downgrade(&facade);
        let mut runtime = engine(
            state.clone(),
            Box::new(move || {
                let facade = weak.upgrade().unwrap();
                assert_eq!(facade.list_registered_systems().unwrap().len(), 2);
                facade.set_default_system("alias").unwrap();
            }),
        );
        runtime.status_callback = true;
        runtime.drop_panics = false;
        let runtime = Arc::new(runtime);
        facade
            .register_runtime("first".into(), runtime.clone(), true)
            .unwrap();
        facade
            .register_runtime("alias".into(), runtime, false)
            .unwrap();
        let status = ActorSystem::status(facade.as_ref());
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        assert_eq!(status.active_actors, 7);
        assert_eq!(status.resources.memory_usage, 11);
        remove_all(&facade);
        wait(&state.engines, 1).await;
        facade.shutdown_runtime().await.unwrap();
    }
);

async fn lookup(facade: &ParrotActorSystem, route: u8) -> Option<BoxedActorRef> {
    match route {
        0 => {
            facade
                .internal_get_actor(&ActorPath::placeholder("/snapshot/reference"))
                .await
        }
        1 => ActorSystem::get_actor(facade, &ActorPath::placeholder("/snapshot/reference")).await,
        _ => AsyncEngineRuntime::get_actor(facade, "/snapshot/reference").await,
    }
}
contract!(
    legacy_lookup_owner_failure_retires_results_without_stopping_shared_actors,
    async {
        for route in 0..3 {
            for result_panics in [false, true] {
                let facade = facade().await;
                let state = Arc::new(State::default());
                let weak = Arc::downgrade(&facade);
                let mut runtime = engine(
                    state.clone(),
                    Box::new(move || remove_all(&weak.upgrade().unwrap())),
                );
                runtime.output_panics = result_panics;
                facade
                    .register_runtime("first".into(), Arc::new(runtime), true)
                    .unwrap();
                let outcome = AssertUnwindSafe(lookup(&facade, route))
                    .catch_unwind()
                    .await;
                check_payload(outcome.unwrap_err(), false);
                wait(&state.engines, 1).await;
                wait(&state.outputs, 1).await;
                assert_eq!(
                    state.stops.load(Ordering::SeqCst),
                    0,
                    "a lookup result is not an unpublished creation"
                );
                facade.shutdown_runtime().await.unwrap();
            }
        }
    }
);

contract!(
    legacy_lookup_callback_panic_keeps_identity_after_self_unregister,
    async {
        for route in 0..3 {
            let facade = facade().await;
            let state = Arc::new(State::default());
            let weak = Arc::downgrade(&facade);
            facade
                .register_runtime(
                    "first".into(),
                    Arc::new(engine(
                        state.clone(),
                        Box::new(move || {
                            remove_all(&weak.upgrade().unwrap());
                            std::panic::panic_any(Original);
                        }),
                    )),
                    true,
                )
                .unwrap();
            let outcome = AssertUnwindSafe(lookup(&facade, route))
                .catch_unwind()
                .await;
            check_payload(outcome.unwrap_err(), true);
            wait(&state.engines, 1).await;
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    lookup_delegate_poll_and_drop_panics_do_not_double_unwind,
    async {
        for route in 0..3 {
            let facade = facade().await;
            let state = Arc::new(State::default());
            let mut runtime = engine(state.clone(), Box::new(|| {}));
            runtime.future_panics = true;
            runtime.drop_panics = false;
            facade
                .register_runtime("first".into(), Arc::new(runtime), true)
                .unwrap();
            let outcome = AssertUnwindSafe(lookup(&facade, route))
                .catch_unwind()
                .await;
            check_payload(outcome.unwrap_err(), true);
            remove_all(&facade);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    legacy_lookup_cancellation_never_runs_a_blocking_engine_drop_on_caller,
    async {
        for route in 0..3 {
            let facade = facade().await;
            let state = Arc::new(State::default());
            let weak = Arc::downgrade(&facade);
            let gate = Arc::new(Gate::default());
            let mut runtime = engine(
                state.clone(),
                Box::new(move || remove_all(&weak.upgrade().unwrap())),
            );
            runtime.pending = true;
            runtime.drop_future_only = true;
            runtime.gate = Some(gate.clone());
            facade
                .register_runtime("first".into(), Arc::new(runtime), true)
                .unwrap();
            let mut pending = Box::pin(lookup(&facade, route));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            assert!(catch_unwind(AssertUnwindSafe(|| drop(pending))).is_ok());
            wait(&gate.entered, 1).await;
            assert_eq!(state.engines.load(Ordering::SeqCst), 0);
            gate.release();
            wait(&state.engines, 1).await;
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

struct Input(Callback, Arc<State>);
impl Drop for Input {
    fn drop(&mut self) {
        self.1.inputs.fetch_add(1, Ordering::SeqCst);
        (self.0)();
    }
}
struct Generic(Input);
struct ThreadInput(Input);
struct ActixInput(Input);
macro_rules! actor {
    ($actor:ident,$context:ty) => {
        impl Actor for $actor {
            type Config = Box<dyn Any + Send>;
            type Context = $context;
            fn state(&self) -> ActorState {
                let _ = &self.0;
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
    };
}
actor!(Generic, dyn ActorContext);
actor!(
    ThreadInput,
    parrot::thread::context::ThreadContext<ThreadInput>
);
actor!(
    ActixInput,
    parrot::actix::context::ActixContext<parrot::actix::actor::ActixActor<ActixInput>>
);
impl ActixEngineExt for ActixInput {}
contract!(
    legacy_creation_rejection_protects_engine_released_by_input_destructor,
    async {
        for route in 0..5 {
            let facade = facade().await;
            let state = Arc::new(State::default());
            let weak = Arc::downgrade(&facade);
            facade
                .register_runtime(
                    "first".into(),
                    Arc::new(engine(state.clone(), Box::new(|| {}))),
                    true,
                )
                .unwrap();
            let input = Input(
                Box::new(move || remove_all(&weak.upgrade().unwrap())),
                state.clone(),
            );
            let pending = async {
                match route {
                    0 => {
                        facade
                            .internal_spawn_actor(Generic(input), Box::new(()) as InstanceConfig)
                            .await
                    }
                    1 => {
                        facade
                            .spawn_actor_in_system(
                                "first",
                                Generic(input),
                                Box::new(()) as InstanceConfig,
                            )
                            .await
                    }
                    2 => {
                        facade
                            .spawn_root_thread(ThreadInput(input), Box::new(()) as InstanceConfig)
                            .await
                    }
                    3 => {
                        facade
                            .spawn_root_thread_in_system(
                                "first",
                                ThreadInput(input),
                                Box::new(()) as InstanceConfig,
                            )
                            .await
                    }
                    _ => {
                        facade
                            .spawn_root_actix(ActixInput(input), Box::new(()) as InstanceConfig)
                            .await
                    }
                }
            };
            let result = AssertUnwindSafe(pending)
                .catch_unwind()
                .await
                .expect("engine cleanup must not unwind a rejected creation");
            assert!(matches!(result, Err(SystemError::ActorCreationError(_))));
            wait(&state.engines, 1).await;
            assert_eq!(state.inputs.load(Ordering::SeqCst), 1);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    mirror_shadow_lookup_failure_does_not_abort_or_replace_primary_results,
    async {
        for callback_panics in [false, true] {
            let facade = facade().await;
            let primary = Arc::new(State::default());
            facade
                .register_prefix_handler("/primary/", Arc::new(Reference(primary.clone(), false)))
                .unwrap();
            facade
                .register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
                .unwrap();
            let handle = facade
                .register_mirror_policy(MirrorPolicy::new("/primary/", "/shadow/value"))
                .unwrap();
            let reference =
                ActorSystem::get_actor(facade.as_ref(), &ActorPath::placeholder("/primary/value"))
                    .await
                    .unwrap();
            let state = Arc::new(State::default());
            let weak = Arc::downgrade(&facade);
            let mut runtime = engine(
                state.clone(),
                Box::new(move || {
                    remove_all(&weak.upgrade().unwrap());
                    if callback_panics {
                        std::panic::panic_any(Original);
                    }
                }),
            );
            runtime.output_panics = true;
            facade
                .register_runtime("shadow".into(), Arc::new(runtime), true)
                .unwrap();
            assert_eq!(
                *reference
                    .send(Box::new(42_u64))
                    .await
                    .unwrap()
                    .downcast::<u64>()
                    .unwrap(),
                42
            );
            wait(&state.engines, 1).await;
            tokio::time::timeout(Duration::from_secs(3), async {
                while handle.stats().delivery_failed == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("shadow failure must be recorded before closing the worker");
            handle.shutdown().await;
            assert!(handle.stats().delivery_failed >= 1);
            assert_eq!(handle.stats().worker_failed, 0);
            assert_eq!(state.stops.load(Ordering::SeqCst), 0);
            drop(reference);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    direct_engine_lookup_preserves_original_poll_panic_after_future_drop_panics,
    async {
        let state = Arc::new(State::default());
        let mut runtime = engine(state.clone(), Box::new(|| {}));
        runtime.future_panics = true;
        runtime.drop_panics = false;
        let system = ActorSystemImpl::Runtime(Arc::new(runtime));
        let outcome =
            AssertUnwindSafe(system.get_actor(&ActorPath::placeholder("/snapshot/reference")))
                .catch_unwind()
                .await;
        check_payload(outcome.unwrap_err(), true);
        drop(system);
        assert_eq!(state.engines.load(Ordering::SeqCst), 1);
    }
);

contract!(
    engine_creation_and_io_delegate_poll_drop_panics_keep_original_payload,
    async {
        for io in [false, true] {
            let facade = facade().await;
            let state = Arc::new(State::default());
            let mut runtime = engine(state.clone(), Box::new(|| {}));
            runtime.future_panics = true;
            runtime.drop_panics = false;
            facade
                .register_runtime("first".into(), Arc::new(runtime), true)
                .unwrap();
            let pending = async {
                if io {
                    let _ = facade
                        .run_io_on(
                            "first",
                            Box::pin(async { Ok(Box::new(()) as BoxedMessage) }),
                        )
                        .await;
                } else {
                    let _ = facade
                        .spawn_on(
                            "first",
                            Box::new(Generic(Input(Box::new(|| {}), state.clone()))),
                            Box::new(()),
                            SpawnOptions::default(),
                        )
                        .await;
                }
            };
            let outcome = AssertUnwindSafe(pending).catch_unwind().await;
            check_payload(outcome.unwrap_err(), true);
            remove_all(&facade);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    successful_lookup_future_disposal_failure_retires_its_unpublished_reference,
    async {
        for route in 0..3 {
            let facade = facade().await;
            let state = Arc::new(State::default());
            let mut runtime = engine(state.clone(), Box::new(|| {}));
            runtime.drop_future_only = true;
            runtime.drop_panics = false;
            runtime.output_panics = true;
            facade
                .register_runtime("first".into(), Arc::new(runtime), true)
                .unwrap();
            let outcome = AssertUnwindSafe(lookup(&facade, route))
                .catch_unwind()
                .await;
            check_payload(outcome.unwrap_err(), false);
            assert_eq!(state.outputs.load(Ordering::SeqCst), 1);
            assert_eq!(state.stops.load(Ordering::SeqCst), 0);
            remove_all(&facade);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    legacy_lookup_preserves_live_registry_fallback_after_default_callback_registration,
    async {
        for route in 0..2 {
            let facade = facade().await;
            let state = Arc::new(State::default());
            let fallback = Arc::new(State::default());
            let weak = Arc::downgrade(&facade);
            let created = fallback.clone();
            let mut first = engine(
                state.clone(),
                Box::new(move || {
                    let mut next = engine(created.clone(), Box::new(|| {}));
                    next.drop_panics = false;
                    weak.upgrade()
                        .unwrap()
                        .register_runtime("added-during-lookup".into(), Arc::new(next), false)
                        .unwrap();
                }),
            );
            first.missing = true;
            first.drop_panics = false;
            facade
                .register_runtime("first".into(), Arc::new(first), true)
                .unwrap();
            let reference = lookup(&facade, route)
                .await
                .expect("legacy fallback samples live names after default misses");
            assert_eq!(state.calls.load(Ordering::SeqCst), 1);
            assert_eq!(fallback.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                reference.path(),
                format!("/snapshot/reference-{:p}", Arc::as_ptr(&fallback))
            );
            drop(reference);
            remove_all(&facade);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);

contract!(
    mirror_healthy_shadow_retains_prefix_fallback_and_primary_reply,
    async {
        let facade = facade().await;
        let primary = Arc::new(State::default());
        let shadow = Arc::new(State::default());
        facade
            .register_prefix_handler("/primary/", Arc::new(Reference(primary, false)))
            .unwrap();
        facade
            .register_prefix_handler("/shadow/", Arc::new(Reference(shadow.clone(), false)))
            .unwrap();
        facade
            .register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
            .unwrap();
        let handle = facade
            .register_mirror_policy(MirrorPolicy::new("/primary/", "/shadow/value"))
            .unwrap();
        let reference =
            ActorSystem::get_actor(facade.as_ref(), &ActorPath::placeholder("/primary/value"))
                .await
                .unwrap();
        assert_eq!(
            *reference
                .send(Box::new(99_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            99
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while handle.stats().admitted != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle.shutdown().await;
        assert_eq!(handle.stats().worker_failed, 0);
        assert_eq!(shadow.stops.load(Ordering::SeqCst), 0);
        drop(reference);
        facade.shutdown_runtime().await.unwrap();
    }
);

contract!(
    mirror_shadow_cancellation_retires_each_snapshot_owner_after_worker_shutdown,
    async {
        let facade = facade().await;
        let primary = Arc::new(State::default());
        facade
            .register_prefix_handler("/primary/", Arc::new(Reference(primary, false)))
            .unwrap();
        facade
            .register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
            .unwrap();
        let handle = facade
            .register_mirror_policy(MirrorPolicy::new("/primary/", "/shadow/value"))
            .unwrap();
        let reference =
            ActorSystem::get_actor(facade.as_ref(), &ActorPath::placeholder("/primary/value"))
                .await
                .unwrap();
        let state = Arc::new(State::default());
        let weak = Arc::downgrade(&facade);
        let gate = Arc::new(Gate::default());
        let mut runtime = engine(
            state.clone(),
            Box::new(move || remove_all(&weak.upgrade().unwrap())),
        );
        runtime.pending = true;
        runtime.drop_future_only = true;
        runtime.gate = Some(gate.clone());
        facade
            .register_runtime("shadow".into(), Arc::new(runtime), true)
            .unwrap();
        reference.deliver(Box::new(1_u64)).await.unwrap();
        wait(&state.calls, 1).await;
        tokio::time::timeout(Duration::from_secs(3), handle.shutdown())
            .await
            .expect("mirror worker must not run runtime destructor inline");
        wait(&gate.entered, 1).await;
        assert_eq!(state.engines.load(Ordering::SeqCst), 0);
        gate.release();
        wait(&state.engines, 1).await;
        drop(reference);
        facade.shutdown_runtime().await.unwrap();
    }
);

struct Uninitialized {
    started: Arc<AtomicUsize>,
    hooks: Arc<AtomicUsize>,
    state: Arc<State>,
}
impl Drop for Uninitialized {
    fn drop(&mut self) {
        self.state.inputs.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Uninitialized {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.started.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.hooks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        unreachable!("pending initialization cannot process messages")
    }
}
contract!(
    native_instance_admission_does_not_promise_before_stop_before_initialization,
    async {
        let facade = facade().await;
        let engine = parrot::thread::ThreadActorSystem::shared(Default::default());
        facade
            .register_runtime("live".into(), engine, true)
            .unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let hooks = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(State::default());
        let actor = facade
            .spawn_on(
                "live",
                Box::new(Uninitialized {
                    started: started.clone(),
                    hooks: hooks.clone(),
                    state: state.clone(),
                }),
                Box::new(()),
                SpawnOptions::at("/uninitialized/control"),
            )
            .await
            .unwrap();
        wait(&started, 1).await;
        tokio::time::timeout(Duration::from_secs(3), actor.stop())
            .await
            .unwrap()
            .unwrap();
        wait(&state.inputs, 1).await;
        assert_eq!(hooks.load(Ordering::SeqCst), 0);
        facade.shutdown_runtime().await.unwrap();
    }
);
