//! A completed extension future must not orphan its unpublished native result.
use futures::FutureExt;
use parrot::system::{ActorSystemImpl, ParrotActorSystem};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{
        AsyncCohortRuntime, AsyncEngineRuntime, AsyncFactoryRuntime, AsyncLocalRuntime,
        AsyncRootCohort, FactoryOptions, InstanceConfig, LocalActor, LocalRuntimeFactory,
        RuntimeActor, RuntimeFactory, SpawnOptions,
    },
    local_typed::LocalBoxedFuture,
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    system::{ActorSystem, SystemError, SystemStatus},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Default)]
struct State {
    drops: AtomicUsize,
    future_drops: AtomicUsize,
    hooks: AtomicUsize,
    reference: Mutex<Option<BoxedActorRef>>,
    group: Mutex<Option<Arc<dyn AsyncRootCohort>>>,
    block_stop: bool,
    stop_entered: tokio::sync::Notify,
    stop_release: tokio::sync::Notify,
}
struct Member(Arc<State>);
impl Drop for Member {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Member {
    async fn stopping(&self) -> ActorResult<()> {
        self.0.hooks.fetch_add(1, Ordering::SeqCst);
        if self.0.block_stop {
            self.0.stop_entered.notify_one();
            self.0.stop_release.notified().await;
        }
        Ok(())
    }
}
impl Actor for Member {
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
        Box::pin(self.stopping())
    }
}
impl LocalActor for Member {
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.stopping())
    }
}
struct Original;
struct Recursive;
impl Drop for Recursive {
    fn drop(&mut self) {
        std::panic::panic_any(Recursive);
    }
}
#[derive(Clone, Copy)]
enum Behavior {
    Success,
    Error,
    PollPanic,
}
struct DelegateFuture<'a, T> {
    inner: BoxedFuture<'a, T>,
    state: Arc<State>,
    drop_panics: bool,
    poll_panics: bool,
}
impl<T> std::future::Future for DelegateFuture<'_, T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        if self.poll_panics {
            std::panic::panic_any(Original);
        }
        self.inner.as_mut().poll(cx)
    }
}
impl<T> Drop for DelegateFuture<'_, T> {
    fn drop(&mut self) {
        self.state.future_drops.fetch_add(1, Ordering::SeqCst);
        if self.drop_panics {
            std::panic::panic_any(Recursive);
        }
    }
}
struct Extension {
    native: Arc<dyn AsyncEngineRuntime>,
    state: Arc<State>,
    behavior: Behavior,
    drop_panics: bool,
}
impl Extension {
    fn wrap<'a, T: Send + 'a>(
        &self,
        future: BoxedFuture<'a, Result<T, SystemError>>,
    ) -> BoxedFuture<'a, Result<T, SystemError>> {
        let fail = matches!(self.behavior, Behavior::Error);
        Box::pin(DelegateFuture {
            inner: Box::pin(async move {
                if fail {
                    drop(future);
                    Err(SystemError::ShuttingDown)
                } else {
                    future.await
                }
            }),
            state: self.state.clone(),
            drop_panics: self.drop_panics,
            poll_panics: matches!(self.behavior, Behavior::PollPanic),
        })
    }
    async fn remember(&self, reference: BoxedActorRef) -> Result<BoxedActorRef, SystemError> {
        // Spawn acknowledges admission. A real reply proves init before expecting a stop hook.
        let reply = reference
            .send(Box::new(37usize))
            .await
            .map_err(SystemError::ActorError)?;
        assert_eq!(*reply.downcast::<usize>().unwrap(), 37);
        *self.state.reference.lock().unwrap() = Some(reference.clone_boxed());
        Ok(reference)
    }
}
impl AsyncEngineRuntime for Extension {
    fn factory_runtime(&self) -> Option<&dyn AsyncFactoryRuntime> {
        Some(self)
    }
    fn local_runtime(&self) -> Option<&dyn AsyncLocalRuntime> {
        Some(self)
    }
    fn cohort_runtime(&self) -> Option<&dyn AsyncCohortRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.wrap(Box::pin(async move {
            self.remember(self.native.spawn(actor, config, options).await?)
                .await
        }))
    }
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(DelegateFuture {
            inner: self.native.get_actor(path),
            state: self.state.clone(),
            drop_panics: self.drop_panics,
            poll_panics: false,
        })
    }
    fn status(&self) -> SystemStatus {
        self.native.status()
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        self.native.shutdown()
    }
}
impl AsyncFactoryRuntime for Extension {
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.wrap(Box::pin(async move {
            let reference = self
                .native
                .factory_runtime()
                .unwrap()
                .spawn_factory(factory, options)
                .await?;
            self.remember(reference).await
        }))
    }
}
impl AsyncLocalRuntime for Extension {
    fn spawn_local_factory<'a>(
        &'a self,
        factory: LocalRuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.wrap(Box::pin(async move {
            let reference = self
                .native
                .local_runtime()
                .unwrap()
                .spawn_local_factory(factory, options)
                .await?;
            self.remember(reference).await
        }))
    }
}
impl AsyncCohortRuntime for Extension {
    fn root_cohort(
        &self,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, SystemError>> {
        self.wrap(Box::pin(async move {
            let group = self
                .native
                .cohort_runtime()
                .unwrap()
                .root_cohort(strategy)
                .await?;
            let reference = group
                .spawn(
                    Box::new(Member(self.state.clone())),
                    Box::new(()),
                    SpawnOptions::at("/delegate/member"),
                )
                .await?;
            self.remember(reference).await?;
            *self.state.group.lock().unwrap() = Some(group.clone());
            Ok(group)
        }))
    }
}
fn policy() -> SupervisorStrategyType {
    SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure)
}
async fn setup(
    actix: bool,
    behavior: Behavior,
    drop_panics: bool,
    block_stop: bool,
) -> (ParrotActorSystem, Arc<Extension>) {
    let native: Arc<dyn AsyncEngineRuntime> = if actix {
        Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        )
    } else {
        parrot::thread::ThreadActorSystem::shared(Default::default())
    };
    let extension = Arc::new(Extension {
        native,
        state: Arc::new(State {
            block_stop,
            ..Default::default()
        }),
        behavior,
        drop_panics,
    });
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    facade
        .register_runtime("extension".into(), extension.clone(), true)
        .unwrap();
    (facade, extension)
}
fn operation<'a>(
    facade: &'a ParrotActorSystem,
    extension: &'a Arc<Extension>,
    route: usize,
    named: bool,
) -> BoxedFuture<'a, Result<(), SystemError>> {
    let state = extension.state.clone();
    let options = SpawnOptions::at("/delegate/member");
    let factory_options = FactoryOptions {
        spawn: options.clone(),
        ..Default::default()
    };
    macro_rules! erase {
        ($work:expr) => {
            Box::pin($work.map(|result| result.map(|_| ())))
        };
    }
    match route {
        0 if named => {
            erase!(facade.spawn_on("extension", Box::new(Member(state)), Box::new(()), options))
        }
        0 => erase!(AsyncEngineRuntime::spawn(
            facade,
            Box::new(Member(state)),
            Box::new(()),
            options
        )),
        1 => {
            let factory: RuntimeFactory = Box::new(move || Box::new(Member(state.clone())));
            if named {
                erase!(facade.spawn_factory_on("extension", factory, factory_options))
            } else {
                erase!(AsyncFactoryRuntime::spawn_factory(
                    facade,
                    factory,
                    factory_options
                ))
            }
        }
        2 => {
            let factory: LocalRuntimeFactory = Box::new(move || Box::new(Member(state.clone())));
            if named {
                erase!(facade.spawn_local_factory_on("extension", factory, factory_options))
            } else {
                erase!(AsyncLocalRuntime::spawn_local_factory(
                    facade,
                    factory,
                    factory_options
                ))
            }
        }
        3 if named => erase!(facade.root_cohort_on("extension", policy())),
        3 => erase!(AsyncCohortRuntime::root_cohort(facade, policy())),
        4 => erase!(ActorSystem::spawn_root_boxed(
            facade,
            Box::new(Member(state)),
            Box::new(())
        )),
        5 => Box::pin(async move {
            ActorSystemImpl::Runtime(extension.clone())
                .spawn_root_boxed(Box::new(Member(state)), Box::new(()))
                .await
                .map(|_| ())
        }),
        _ => unreachable!(),
    }
}
fn reference(state: &State) -> BoxedActorRef {
    state
        .reference
        .lock()
        .unwrap()
        .as_ref()
        .expect("real reply recorded")
        .clone_boxed()
}
async fn wait_dropped(state: &State) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.drops.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unpublished native actor was not retired");
}
async fn rejects_live_result(route: usize) {
    for actix in [false, true] {
        if route == 2 && !actix {
            continue;
        }
        for named in [false, true] {
            let (facade, extension) = setup(actix, Behavior::Success, true, false).await;
            let result = operation(&facade, &extension, route, named).await;
            assert!(
                matches!(result, Err(SystemError::ActorError(_))),
                "cleanup must be returned as an error"
            );
            wait_dropped(&extension.state).await;
            assert!(!reference(&extension.state).is_alive().await);
            assert_eq!(extension.state.hooks.load(Ordering::SeqCst), 1);
            assert_eq!(extension.state.future_drops.load(Ordering::SeqCst), 1);
            if let Some(group) = extension.state.group.lock().unwrap().as_ref() {
                assert!(group.members().is_empty());
            }
            facade.shutdown_runtime().await.unwrap();
        }
    }
}
#[path = "support/isolated.rs"]
mod isolated_child;

fn isolated(name: &str, work: impl std::future::Future<Output = ()>) {
    isolated_child::run(
        name,
        "PARROT_CREATION_FUTURE_CHILD",
        Duration::from_secs(45),
        || {
            actix::System::new().block_on(work);
        },
    );
}
macro_rules! contract {
    ($name:ident, $work:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), $work);
        }
    };
}
contract!(
    instance_future_drop_failure_stops_unpublished_native_actor,
    rejects_live_result(0)
);
contract!(
    factory_future_drop_failure_stops_unpublished_native_actor,
    rejects_live_result(1)
);
contract!(
    local_future_drop_failure_stops_unpublished_native_actor,
    rejects_live_result(2)
);
contract!(
    group_future_drop_failure_stops_unpublished_native_members,
    rejects_live_result(3)
);
contract!(
    boxed_facade_future_drop_failure_stops_unpublished_actor,
    rejects_live_result(4)
);
contract!(
    direct_runtime_future_drop_failure_stops_unpublished_actor,
    rejects_live_result(5)
);
contract!(business_error_survives_recursive_future_drop_panic, async {
    for route in 0..6 {
        let (facade, extension) = setup(true, Behavior::Error, true, false).await;
        assert!(matches!(
            operation(&facade, &extension, route, true).await,
            Err(SystemError::ShuttingDown)
        ));
        assert_eq!(extension.state.future_drops.load(Ordering::SeqCst), 1);
        assert!(extension.state.reference.lock().unwrap().is_none());
        facade.shutdown_runtime().await.unwrap();
    }
});
contract!(healthy_delegate_keeps_delivered_native_actor_alive, async {
    for route in 0..6 {
        let (facade, extension) = setup(true, Behavior::Success, false, false).await;
        operation(&facade, &extension, route, false).await.unwrap();
        assert!(reference(&extension.state).is_alive().await);
        assert_eq!(extension.state.drops.load(Ordering::SeqCst), 0);
        assert_eq!(extension.state.hooks.load(Ordering::SeqCst), 0);
        facade.shutdown_runtime().await.unwrap();
        wait_dropped(&extension.state).await;
    }
});
contract!(
    cancelling_failed_creation_waiter_preserves_actual_retirement,
    async {
        for actix in [false, true] {
            let (facade, extension) = setup(actix, Behavior::Success, true, true).await;
            let mut work = operation(&facade, &extension, 0, true);
            tokio::select! {
                result = &mut work => panic!("creation returned before stop: {result:?}"),
                _ = extension.state.stop_entered.notified() => {}
            }
            assert_eq!(extension.state.drops.load(Ordering::SeqCst), 0);
            drop(work);
            extension.state.stop_release.notify_one();
            wait_dropped(&extension.state).await;
            assert!(!reference(&extension.state).is_alive().await);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);
contract!(
    creation_poll_panic_identity_survives_recursive_drop_panic,
    async {
        for route in 0..6 {
            let (facade, extension) = setup(true, Behavior::PollPanic, true, false).await;
            let payload = AssertUnwindSafe(operation(&facade, &extension, route, true))
                .catch_unwind()
                .await
                .unwrap_err();
            assert!(payload.is::<Original>());
            assert_eq!(extension.state.future_drops.load(Ordering::SeqCst), 1);
            facade.shutdown_runtime().await.unwrap();
        }
    }
);
contract!(
    lookup_future_drop_failure_does_not_stop_preexisting_actor,
    async {
        let (facade, extension) = setup(false, Behavior::Success, true, false).await;
        let original = extension
            .native
            .spawn(
                Box::new(Member(extension.state.clone())),
                Box::new(()),
                SpawnOptions::at("/delegate/member"),
            )
            .await
            .unwrap();
        original.send(Box::new(())).await.unwrap();
        assert!(
            AssertUnwindSafe(AsyncEngineRuntime::get_actor(&facade, "/delegate/member"))
                .catch_unwind()
                .await
                .is_err()
        );
        assert!(original.is_alive().await);
        assert_eq!(extension.state.hooks.load(Ordering::SeqCst), 0);
        original.stop().await.unwrap();
        wait_dropped(&extension.state).await;
        facade.shutdown_runtime().await.unwrap();
    }
);
