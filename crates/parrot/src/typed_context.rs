//! Static child admission uses the same concrete engine path as static roots.
use crate::owned_payload::OwnedPayload;
use crate::{children::ChildScope, thread::ThreadActorSystem};
pub use parrot_api::typed_context::TypedContext;
use parrot_api::{
    address::ActorRef,
    context::ReadOnlyChildrenVec,
    errors::ActorError,
    local_typed::{LocalProtocol, LocalTypedActorRef, LocalTypedDispatch},
    supervisor::SupervisorStrategyType,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    typed_context::TypedContextServices,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct Identity {
    pub path: String,
    pub own: Arc<OnceLock<Weak<dyn ActorRef>>>,
    pub parent: Option<Weak<dyn ActorRef>>,
}
type StopGeneration = Arc<dyn Fn(&Arc<ChildScope>) + Send + Sync>;
#[derive(Clone)]
pub(crate) struct Hooks {
    pub stop: StopGeneration,
    pub escalate: crate::children::Escalate,
}
#[derive(Clone)]
pub(crate) enum Backend {
    Thread(Weak<ThreadActorSystem>),
    Actix(Box<crate::actix::system::WeakSystem>),
}
pub(crate) struct ManagedContext {
    pub system: Backend,
    pub scope: Arc<ChildScope>,
    identity: Identity,
    hooks: Hooks,
    runtime: tokio::runtime::Handle,
    cleanup: Arc<tokio::sync::Semaphore>,
}
impl ManagedContext {
    pub(crate) fn own(&self) -> Option<Weak<dyn ActorRef>> {
        self.identity.own.get().cloned()
    }
}
impl TypedContextServices for ManagedContext {
    fn local_spawner(&self) -> Option<&dyn parrot_api::engine::AsyncLocalActorSpawner> {
        matches!(&self.system, Backend::Actix(_)).then_some(self)
    }
    fn path(&self) -> &str {
        &self.identity.path
    }
    fn actor_ref(&self) -> Option<BoxedActorRef> {
        self.identity
            .own
            .get()?
            .upgrade()
            .map(|actor| actor.clone_boxed())
    }
    fn parent(&self) -> Option<BoxedActorRef> {
        self.identity
            .parent
            .as_ref()?
            .upgrade()
            .map(|actor| actor.clone_boxed())
    }
    fn children(&self) -> ReadOnlyChildrenVec {
        self.scope.view()
    }
    fn stop(&self) {
        if !self.scope.is_closed() {
            (self.hooks.stop)(&self.scope);
        }
    }
    fn set_child_strategy(&self, strategy: SupervisorStrategyType) {
        self.scope.set_strategy(strategy);
    }
}

/// A logical actor retains the current generation's actual child owner.
pub(crate) struct TypedTree {
    current: Mutex<Arc<ManagedContext>>,
    stopped: AtomicBool,
    pending_failure: Mutex<Option<(Arc<ActorError>, crate::children::FailurePolicy)>>,
    timeout: Duration,
    slots: Arc<tokio::sync::Semaphore>,
}
impl TypedTree {
    pub(crate) fn new(system: &Arc<ThreadActorSystem>, identity: Identity, hooks: Hooks) -> Self {
        Self::new_backend(
            Backend::Thread(Arc::downgrade(system)),
            identity,
            hooks,
            system.config().shutdown_timeout,
            system.runtime_handle.clone(),
            system.cleanup_slots.clone(),
            system.cleanup_slots.clone(),
        )
    }
    pub(crate) fn new_backend(
        system: Backend,
        identity: Identity,
        hooks: Hooks,
        timeout: Duration,
        runtime: tokio::runtime::Handle,
        slots: Arc<tokio::sync::Semaphore>,
        cleanup: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        let scope = ChildScope::new(timeout, slots.clone());
        scope.bind_escalator(hooks.escalate.clone());
        Self {
            current: Mutex::new(Arc::new(ManagedContext {
                system,
                scope,
                identity,
                hooks,
                runtime,
                cleanup,
            })),
            stopped: AtomicBool::new(false),
            pending_failure: Mutex::new(None),
            timeout,
            slots,
        }
    }
    pub(crate) fn defer_failure(
        &self,
        error: Arc<ActorError>,
        policy: crate::children::FailurePolicy,
    ) {
        *self.pending_failure.lock().unwrap() = Some((error, policy));
    }
    pub(crate) fn take_failure(&self) -> Option<(Arc<ActorError>, crate::children::FailurePolicy)> {
        self.pending_failure.lock().unwrap().take()
    }
    pub(crate) fn context(&self) -> TypedContext {
        TypedContext::from_services(self.current.lock().unwrap().clone())
    }
    pub(crate) fn stop_generation(&self, scope: &Arc<ChildScope>) -> bool {
        let current = self.current.lock().unwrap();
        if !Arc::ptr_eq(&current.scope, scope) || scope.is_closed() {
            return false;
        }
        // Atomic with next_generation: a stop authorized by the old generation
        // cannot be delayed until after a replacement has been installed.
        self.stopped.store(true, Ordering::Release);
        scope.close();
        true
    }
    pub(crate) fn close(&self) {
        self.stopped.store(true, Ordering::Release);
        self.current.lock().unwrap().scope.close();
    }
    pub(crate) async fn join(&self) -> ActorResult<()> {
        let scope = self.current.lock().unwrap().scope.clone();
        scope.join().await
    }
    /// Called only after the old scope has joined and old instance has retired.
    pub(crate) fn next_generation(&self) -> ActorResult<()> {
        let mut current = self.current.lock().unwrap();
        if self.stopped.load(Ordering::Acquire) {
            return Err(ActorError::Stopped);
        }
        let scope = ChildScope::new(self.timeout, self.slots.clone());
        scope.bind_escalator(current.hooks.escalate.clone());
        *current = Arc::new(ManagedContext {
            system: current.system.clone(),
            scope,
            identity: current.identity.clone(),
            hooks: current.hooks.clone(),
            runtime: current.runtime.clone(),
            cleanup: current.cleanup.clone(),
        });
        Ok(())
    }
}

/// Static child creation. No business message is converted to `Any` here.
/// Actor/factory and strategy inputs acquire independent cleanup ownership at
/// call time, including when the returned creation future is never polled.
/// Foreign contexts use the current executor when available, otherwise isolated
/// synchronous disposal. Admission and construction remain lazy.
/// Returned futures retain their invocation's trace context across polling on
/// another task. Factory construction and initialization use that identity;
/// subsequent messages establish their own independent contexts.
pub trait TypedContextExt {
    /// Admit a repeatable Local child. Every instance is constructed and
    /// retired on one chosen arbiter. Only the Send constructor crosses threads;
    /// it is invoked serially and need not be Sync. Inherits the parent's policy.
    fn spawn_local_typed_child_factory<A, M, F>(
        &self,
        constructor: F,
    ) -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<A, M>>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static;

    /// Repeatable Local child with an explicit policy override. Failed messages
    /// are not replayed, and old contexts cannot admit children after replacement.
    fn spawn_local_typed_child_factory_with_strategy<A, M, F>(
        &self,
        constructor: F,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<A, M>>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static;

    /// Construct a non-Send child on an Actix arbiter owned by this parent
    /// generation. Only the Send constructor crosses threads. Child state and
    /// futures remain on their chosen arbiter, which need not be the parent's.
    /// This is one-shot construction, not a restartable factory. Admission is
    /// lazy, but constructor captures are owned immediately, even if unpolled.
    /// Thread and foreign contexts reject this operation and retire its inputs.
    fn spawn_local_typed_child<A, M, F>(
        &self,
        constructor: F,
    ) -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<A, M>>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static;

    /// Local child with a policy override; otherwise the parent's live child
    /// policy is inherited. Parent retirement joins actual child cleanup.
    fn spawn_local_typed_child_with_strategy<A, M, F>(
        &self,
        constructor: F,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<A, M>>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static;
    /// Admit a repeatable child using the parent's live child strategy.
    fn spawn_typed_child_factory<A, M, F>(
        &self,
        factory: F,
    ) -> BoxedFuture<'_, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static;
    /// Admit a repeatable child with an explicit strategy override.
    fn spawn_typed_child_factory_with_strategy<A, M, F>(
        &self,
        factory: F,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static;

    fn spawn_typed_child<A, M>(
        &self,
        actor: A,
    ) -> BoxedFuture<'_, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>;
    fn spawn_typed_child_with_strategy<A, M>(
        &self,
        actor: A,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>;
}
fn own<T: Send + 'static>(context: &TypedContext, value: T) -> OwnedPayload<T> {
    match context.services().downcast_ref::<ManagedContext>() {
        Some(services) => OwnedPayload::new(value, &services.runtime, &services.cleanup),
        None => OwnedPayload::unbound(value),
    }
}
async fn spawn<A: ParrotTypedDispatch, M: ParrotMsgVariant<A>>(
    context: &TypedContext,
    actor: OwnedPayload<A>,
    strategy: OwnedPayload<Option<SupervisorStrategyType>>,
) -> ActorResult<TypedActorRef<A, M>> {
    let Some(services) = context.services().downcast_ref::<ManagedContext>() else {
        let (actor, strategy) = tokio::join!(actor.retire(), strategy.retire());
        actor
            .and(strategy)
            .map_err(|error| ActorError::Panic(error.to_string()))?;
        return Err(ActorError::InitializationError(
            "this context has no static child creation bridge".into(),
        ));
    };
    let path = format!(
        "{}/{}",
        services.path().trim_end_matches('/'),
        uuid::Uuid::new_v4()
    );
    match &services.system {
        Backend::Thread(system) => {
            if let Some(system) = system.upgrade() {
                return system
                    .spawn_typed_child_source(actor, strategy, &path, services)
                    .await;
            }
        }
        Backend::Actix(system) => {
            if let Ok(system) = system.upgrade() {
                return Arc::new(system)
                    .spawn_typed_child_source(actor, strategy, &path, services)
                    .await;
            }
        }
    }
    let (actor, strategy) = tokio::join!(actor.retire(), strategy.retire());
    actor.and(strategy)?;
    Err(ActorError::Stopped)
}
async fn spawn_factory<A, M, F>(
    context: &TypedContext,
    factory: OwnedPayload<F>,
    policy: OwnedPayload<Option<SupervisorStrategyType>>,
) -> ActorResult<TypedActorRef<A, M>>
where
    A: ParrotTypedDispatch,
    M: ParrotMsgVariant<A>,
    F: Fn() -> A + Send + Sync + 'static,
{
    let Some(services) = context.services().downcast_ref::<ManagedContext>() else {
        let (factory, policy) = tokio::join!(factory.retire(), policy.retire());
        factory
            .and(policy)
            .map_err(|error| ActorError::Panic(error.to_string()))?;
        return Err(ActorError::InitializationError(
            "this context has no static child creation bridge".into(),
        ));
    };
    let path = format!(
        "{}/{}",
        services.path().trim_end_matches('/'),
        uuid::Uuid::new_v4()
    );
    match &services.system {
        Backend::Thread(system) => {
            if let Some(system) = system.upgrade() {
                return system
                    .spawn_typed_child_factory_source(factory, policy, &path, services)
                    .await;
            }
        }
        Backend::Actix(system) => {
            if let Ok(system) = system.upgrade() {
                return Arc::new(system)
                    .spawn_typed_child_factory_source(factory, policy, &path, services)
                    .await;
            }
        }
    }
    let (factory, policy) = tokio::join!(factory.retire(), policy.retire());
    factory.and(policy)?;
    Err(ActorError::Stopped)
}
impl TypedContextExt for TypedContext {
    fn spawn_local_typed_child_factory<A, M, F>(
        &self,
        constructor: F,
    ) -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<A, M>>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        Box::pin(parrot_api::trace::capture(spawn_local(
            self,
            own(self, constructor),
            own(self, None),
            true,
        )))
    }
    fn spawn_local_typed_child_factory_with_strategy<A, M, F>(
        &self,
        constructor: F,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<A, M>>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        Box::pin(parrot_api::trace::capture(spawn_local(
            self,
            own(self, constructor),
            own(self, Some(strategy)),
            true,
        )))
    }

    fn spawn_local_typed_child<A, M, F>(
        &self,
        constructor: F,
    ) -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<A, M>>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        Box::pin(parrot_api::trace::capture(spawn_local(
            self,
            own(self, constructor),
            own(self, None),
            false,
        )))
    }
    fn spawn_local_typed_child_with_strategy<A, M, F>(
        &self,
        constructor: F,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<A, M>>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        Box::pin(parrot_api::trace::capture(spawn_local(
            self,
            own(self, constructor),
            own(self, Some(strategy)),
            false,
        )))
    }
    fn spawn_typed_child_factory<A, M, F>(
        &self,
        factory: F,
    ) -> BoxedFuture<'_, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        Box::pin(parrot_api::trace::capture(spawn_factory(
            self,
            own(self, factory),
            own(self, None),
        )))
    }
    fn spawn_typed_child_factory_with_strategy<A, M, F>(
        &self,
        factory: F,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        Box::pin(parrot_api::trace::capture(spawn_factory(
            self,
            own(self, factory),
            own(self, Some(strategy)),
        )))
    }

    fn spawn_typed_child<A, M>(&self, actor: A) -> BoxedFuture<'_, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        Box::pin(parrot_api::trace::capture(spawn(
            self,
            own(self, actor),
            own(self, None),
        )))
    }
    fn spawn_typed_child_with_strategy<A, M>(
        &self,
        actor: A,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        Box::pin(parrot_api::trace::capture(spawn(
            self,
            own(self, actor),
            own(self, Some(strategy)),
        )))
    }
}

async fn spawn_local<A, M, F>(
    context: &TypedContext,
    constructor: OwnedPayload<F>,
    policy: OwnedPayload<Option<SupervisorStrategyType>>,
    repeatable: bool,
) -> ActorResult<LocalTypedActorRef<A, M>>
where
    A: LocalTypedDispatch,
    M: ParrotMsgVariant<LocalProtocol<A>>,
    F: Fn() -> A + Send + 'static,
{
    if let Some(services) = context.services().downcast_ref::<ManagedContext>()
        && let Backend::Actix(system) = &services.system
    {
        if let Ok(system) = system.upgrade() {
            let path = format!(
                "{}/{}",
                services.path().trim_end_matches('/'),
                uuid::Uuid::new_v4()
            );
            return Arc::new(system)
                .spawn_local_typed_owned_source(
                    constructor,
                    policy,
                    &path,
                    Some(services),
                    repeatable,
                )
                .await;
        }
        let (constructor, policy) = tokio::join!(constructor.retire(), policy.retire());
        constructor.and(policy)?;
        return Err(ActorError::Stopped);
    }
    let (constructor, policy) = tokio::join!(constructor.retire(), policy.retire());
    constructor.and(policy)?;
    Err(ActorError::InitializationError(
        "this context has no Local child execution backend".into(),
    ))
}

impl parrot_api::engine::AsyncLocalActorSpawner for ManagedContext {
    fn spawn_local_factory<'a>(
        &'a self,
        factory: parrot_api::engine::LocalRuntimeFactory,
        strategy: Option<SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let factory = OwnedPayload::new(factory, &self.runtime, &self.cleanup);
        let policy = OwnedPayload::new(strategy, &self.runtime, &self.cleanup);
        Box::pin(parrot_api::trace::capture(async move {
            if let Backend::Actix(system) = &self.system
                && let Ok(system) = system.upgrade()
            {
                let path = format!(
                    "{}/{}",
                    self.path().trim_end_matches('/'),
                    uuid::Uuid::new_v4()
                );
                return Arc::new(system)
                    .spawn_neutral_local_child(
                        factory,
                        policy,
                        &path,
                        crate::actix::local_typed::LocalParent {
                            scope: &self.scope,
                            identity: self.own(),
                        },
                    )
                    .await;
            }
            let (factory, policy) = tokio::join!(factory.retire(), policy.retire());
            factory.and(policy)?;
            Err(ActorError::Stopped)
        }))
    }
}
