use actix::{Addr, Context as ActixCtx};
use parrot_api::address::ActorPath;
use parrot_api::context::ActorSpawner;
use parrot_api::context::{
    ActorContext, ActorContextScheduler, ReadOnlyChildrenVec, ScheduledTask,
};
use parrot_api::errors::ActorError;
use parrot_api::message::{CloneableMessage, Message};
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture};
use std::sync::Arc;
use std::sync::RwLock;

#[derive(Clone, Default)]
struct ManagedSpawner {
    service: Option<crate::actix::system::WeakSystem>,
    scope: Option<Arc<crate::children::ChildScope>>,
    parent: Option<Arc<BoxedActorRef>>,
}
trait SpawnPayload: Send {
    fn accepts_config(&self, config: &parrot_api::types::BoxedMessage) -> bool;
    fn spawn(
        self: Box<Self>,
        system: &crate::actix::system::ActixActorSystem,
        parent: BoxedActorRef,
        lifetime: Arc<crate::children::ChildLifetime>,
        config: parrot_api::types::BoxedMessage,
    ) -> Result<super::system::InstanceTicket, super::disposal::RejectedSpawn>;
}
struct ErasedSpawn(Box<dyn SpawnPayload>);
struct TypedSpawn<A>(A);
impl<A> SpawnPayload for TypedSpawn<A>
where
    A: parrot_api::actor::Actor<Context = ActixContext<crate::actix::actor::ActixActor<A>>>
        + parrot_api::actor::ActixEngineExt<
            Context = ActixContext<crate::actix::actor::ActixActor<A>>,
        > + Unpin
        + 'static,
{
    fn accepts_config(&self, config: &parrot_api::types::BoxedMessage) -> bool {
        config.is::<A::Config>()
    }
    fn spawn(
        self: Box<Self>,
        system: &crate::actix::system::ActixActorSystem,
        parent: BoxedActorRef,
        lifetime: Arc<crate::children::ChildLifetime>,
        config: parrot_api::types::BoxedMessage,
    ) -> Result<super::system::InstanceTicket, super::disposal::RejectedSpawn> {
        // The private bridge validated this exact config before admission.
        let config = *config
            .downcast::<A::Config>()
            .expect("validated Actix child config");
        system.spawn_managed(self.0, config, Some(parent), Some(lifetime))
    }
}
impl<A> parrot_api::context::ActorSpawnContext<A>
    for ActixContext<crate::actix::actor::ActixActor<A>>
where
    A: parrot_api::actor::Actor<Context = Self>
        + parrot_api::actor::ActixEngineExt<Context = Self>
        + Unpin
        + 'static,
{
    fn wrap_spawn(actor: A) -> parrot_api::types::BoxedMessage {
        Box::new(ErasedSpawn(Box::new(TypedSpawn(actor))))
    }
}
async fn reject_instance(
    actor: super::disposal::OwnedPayload<parrot_api::types::BoxedMessage>,
    config: super::disposal::OwnedPayload<parrot_api::types::BoxedMessage>,
    strategy: super::disposal::OwnedPayload<Option<parrot_api::supervisor::SupervisorStrategyType>>,
    error: ActorError,
) -> ActorResult<BoxedActorRef> {
    let (actor, config, strategy) =
        tokio::join!(actor.retire(), config.retire(), strategy.retire());
    actor.and(config).and(strategy)?;
    Err(error)
}
impl ManagedSpawner {
    fn own<T: Send + 'static>(&self, value: T) -> super::disposal::OwnedPayload<T> {
        match &self.service {
            Some(service) => {
                super::disposal::OwnedPayload::new(value, &service.runtime, &service.cleanup_slots)
            }
            None => super::disposal::OwnedPayload::unbound(value),
        }
    }
    fn spawn_policy<'a>(
        &'a self,
        actor: parrot_api::types::BoxedMessage,
        config: parrot_api::types::BoxedMessage,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let actor = self.own(actor);
        let config = self.own(config);
        let strategy = self.own(strategy);
        Box::pin(async move {
            use super::disposal::OwnedPayload;
            let Some(service) = self.service.as_ref() else {
                return reject_instance(
                    actor,
                    config,
                    strategy,
                    ActorError::InternalError(
                        "Actix actor spawner is not bound to a managed actor".into(),
                    ),
                )
                .await;
            };
            let system = match service.upgrade() {
                Ok(system) => system,
                Err(error) => return reject_instance(actor, config, strategy, error).await,
            };
            let (Some(scope), Some(parent)) = (&self.scope, &self.parent) else {
                return reject_instance(actor, config, strategy, ActorError::Stopped).await;
            };
            let payload = match actor.take().downcast::<ErasedSpawn>() {
                Ok(payload) => OwnedPayload::new(payload, &service.runtime, &service.cleanup_slots),
                Err(actor) => {
                    return reject_instance(
                        OwnedPayload::new(actor, &service.runtime, &service.cleanup_slots),
                        config,
                        strategy,
                        ActorError::InitializationError("Actix spawn payload type mismatch".into()),
                    )
                    .await;
                }
            };
            if !payload.value().0.accepts_config(config.value()) {
                let actor = OwnedPayload::new(
                    payload.take() as parrot_api::types::BoxedMessage,
                    &service.runtime,
                    &service.cleanup_slots,
                );
                return reject_instance(
                    actor,
                    config,
                    strategy,
                    ActorError::InitializationError("child configuration type mismatch".into()),
                )
                .await;
            }
            let mut payload = Some(payload);
            let mut config = Some(config);
            let mut ticket = None;
            let mut strategy = Some(strategy);
            let mut rejected = None;
            let mut lifetime_owner = None;
            let result = scope.spawn(|lifetime| {
                lifetime_owner = Some(lifetime.clone());
                lifetime.set_strategy(strategy.take().expect("one instance admission").take());
                let pending = match payload
                    .take()
                    .expect("one instance admission")
                    .take()
                    .0
                    .spawn(
                        &system,
                        parent.as_ref().clone_boxed(),
                        lifetime.clone(),
                        config.take().expect("one config admission").take(),
                    ) {
                    Ok(created) => created,
                    Err(failed) => {
                        rejected = Some(failed.retired);
                        return Err(match failed.error {
                            parrot_api::system::SystemError::ShuttingDown => ActorError::Stopped,
                            error => ActorError::InternalError(error.to_string()),
                        });
                    }
                };
                let child = pending.actor_ref.clone_boxed();
                let control = pending.control.clone();
                ticket = Some(pending);
                let stop: crate::children::StopChild = Arc::new(move || {
                    control.request_stop(tokio::time::Instant::now() + control.timeout);
                    let lifetime = lifetime.clone();
                    Box::pin(async move { lifetime.wait().await })
                });
                Ok((child, stop))
            });
            match result {
                Ok(_) => ticket.expect("admitted instance has a ticket").wait().await,
                Err(error) => {
                    let mut cleanup: Vec<BoxedFuture<'static, ActorResult<()>>> = Vec::new();
                    if let Some(payload) = payload {
                        cleanup.push(payload.retire());
                    }
                    if let Some(strategy) = strategy {
                        cleanup.push(strategy.retire());
                    }
                    if let Some(config) = config {
                        cleanup.push(config.retire());
                    }
                    if let Some(rejected) = rejected {
                        cleanup.push(rejected);
                    }
                    if let Some(lifetime) = lifetime_owner {
                        cleanup.push(Box::pin(async move { lifetime.dispose_policy().await }));
                    }
                    let mut result = Ok(());
                    for finished in futures::future::join_all(cleanup).await {
                        result = result.and(finished);
                    }
                    result?;
                    Err(error)
                }
            }
        })
    }
}

async fn reject_factory_payload(
    factory: super::disposal::OwnedPayload<parrot_api::types::BoxedMessage>,
    strategy: super::disposal::OwnedPayload<Option<parrot_api::supervisor::SupervisorStrategyType>>,
    error: ActorError,
) -> ActorResult<BoxedActorRef> {
    let (factory, strategy) = tokio::join!(factory.retire(), strategy.retire());
    factory.and(strategy)?;
    Err(error)
}
impl ActorSpawner for ManagedSpawner {
    fn spawn_factory_erased<'a>(
        &'a self,
        factory: parrot_api::types::BoxedMessage,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let factory = self.own(factory);
        let strategy = self.own(strategy);
        Box::pin(async move {
            let Some(service) = self.service.as_ref() else {
                return reject_factory_payload(factory, strategy, ActorError::Stopped).await;
            };
            let system = match service.upgrade() {
                Ok(system) => system,
                Err(error) => return reject_factory_payload(factory, strategy, error).await,
            };
            let (Some(scope), Some(parent)) = (&self.scope, &self.parent) else {
                return reject_factory_payload(factory, strategy, ActorError::Stopped).await;
            };
            if !factory.value().is::<super::factory::ErasedChildFactory>() {
                return reject_factory_payload(
                    factory,
                    strategy,
                    ActorError::InitializationError("Actix factory payload type mismatch".into()),
                )
                .await;
            }
            let factory = factory
                .take()
                .downcast::<super::factory::ErasedChildFactory>()
                .expect("validated Actix factory payload");
            factory
                .spawn(
                    system,
                    parent.as_ref().clone_boxed(),
                    scope.clone(),
                    strategy.take(),
                )
                .await
        })
    }

    fn spawn<'a>(
        &'a self,
        actor: parrot_api::types::BoxedMessage,
        config: parrot_api::types::BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        self.spawn_policy(actor, config, None)
    }
    fn spawn_with_strategy<'a>(
        &'a self,
        actor: parrot_api::types::BoxedMessage,
        config: parrot_api::types::BoxedMessage,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        self.spawn_policy(actor, config, Some(strategy))
    }
}

/// ActixContext is a wrapper around actix::Context
///
/// # Overview
/// It provides a standardized context interface for Parrot actors
/// using Actix as the underlying engine.
///
/// # Key Responsibilities
/// - Controls actor lifecycle
/// - Provides access to actor address and path
/// - Enables actor supervision
///
/// # Implementation Details
/// - Wraps the Actix context
/// - Translates Parrot API calls to Actix operations
/// - Manages actor lifecycle state
///
/// # Thread Safety
/// - Thread safe through interior Arc reference
pub struct ActixContext<A>
where
    A: actix::Actor<Context = ActixCtx<A>>,
{
    /// The underlying Actix address
    addr: Arc<Addr<A>>,
    stop_control: Option<Arc<crate::actix::lifecycle::ActorControl>>,
    tasks: Arc<crate::context_tasks::ContextTasks>,
    pub(crate) idle: Arc<crate::receive_timeout::ReceiveTimeout>,
    idle_spawner: Option<actix::ArbiterHandle>,
    /// Actor path in the system
    path: ActorPath,
    /// The actor's parent reference
    parent: Option<Arc<BoxedActorRef>>,
    /// The actor's child references
    children: Option<Arc<RwLock<Vec<BoxedActorRef>>>>,
    pub(crate) stream_registry: crate::streams::OwnedStreams,
    spawner: ManagedSpawner,
    /// Default deadline for context initiated asks.  Keep this on the
    /// context so `ActorContext::ask` has the same bounded behavior as the
    /// engine's public actor reference.
    ask_timeout: std::time::Duration,
}

impl<A> Clone for ActixContext<A>
where
    A: actix::Actor<Context = ActixCtx<A>>,
{
    fn clone(&self) -> Self {
        Self {
            addr: self.addr.clone(),
            stop_control: self.stop_control.clone(),
            tasks: self.tasks.clone(),
            idle: self.idle.clone(),
            idle_spawner: self.idle_spawner.clone(),
            path: self.path.clone(),
            parent: self.parent.clone(),
            children: self.children.clone(),
            stream_registry: self.stream_registry.clone(),
            spawner: self.spawner.clone(),
            ask_timeout: self.ask_timeout,
        }
    }
}

/// M1 derive-decouple: the engine-neutral Context alias for this engine.
///
/// The derive macro's generated code references the context through
/// `__parrot_engine::EngineContext<Self>`; this alias provides the
/// engine-specific concrete type behind that neutral face.
///
/// `crate::actix::actor::ActixActor` is the Parrot→Actix adapter wrapper:
/// `EngineContext<UserActor> = ActixContext<ActixActor<UserActor>>` —
/// exactly the shape derive-generated `type Context` used to hardcode.
pub type EngineContext<A> = ActixContext<crate::actix::actor::ActixActor<A>>;

impl<A> ActixContext<A>
where
    A: actix::Actor<Context = ActixCtx<A>>,
{
    /// Create a new ActixContext wrapping an actix::Context
    ///
    /// # Parameters
    /// - `addr`: The Actix address to wrap
    /// - `path`: Actor path in the system
    ///
    /// # Returns
    /// A new ActixContext instance
    pub fn new(addr: Addr<A>, path: ActorPath) -> Self {
        let tasks = crate::context_tasks::ContextTasks::new(
            crate::context_tasks::DEFAULT_MAX_CONTEXT_TASKS,
        );
        let stream_registry = crate::streams::OwnedStreams::new::<Self>(
            tasks.clone(),
            crate::streams::StreamExecutor::Actix(actix::Arbiter::try_current()),
        );
        Self {
            addr: Arc::new(addr),
            stop_control: None,
            idle: crate::receive_timeout::ReceiveTimeout::new(),
            idle_spawner: actix::Arbiter::try_current(),
            tasks,
            path,
            parent: None,
            children: None,
            stream_registry,
            spawner: ManagedSpawner::default(),
            ask_timeout: std::time::Duration::from_secs(5),
        }
    }

    pub(crate) fn with_ask_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.ask_timeout = timeout;
        self
    }

    pub(crate) fn with_control(
        mut self,
        control: Arc<crate::actix::lifecycle::ActorControl>,
    ) -> Self {
        self.tasks = control.tasks.clone();
        self.stream_registry.set_tasks(control.tasks.clone());
        self.stop_control = Some(control);
        self
    }

    pub(crate) fn set_spawn_service(
        &mut self,
        service: Option<crate::actix::system::WeakSystem>,
        parent: Option<BoxedActorRef>,
    ) {
        self.parent = parent.map(Arc::new);
        self.spawner = ManagedSpawner {
            service,
            scope: self
                .stop_control
                .as_ref()
                .map(|control| control.children.clone()),
            parent: Some(Arc::new(self.path.target.clone_boxed())),
        };
    }

    /// Create a new ActixContext with a parent reference
    ///
    /// # Parameters
    /// - `addr`: The Actix address to wrap
    /// - `path`: Actor path in the system
    /// - `parent`: The parent actor reference
    ///
    /// # Returns
    /// A new ActixContext instance
    pub fn new_with_parent(addr: Addr<A>, path: ActorPath, parent: BoxedActorRef) -> Self {
        let mut ctx = Self::new(addr, path);
        ctx.parent = Some(Arc::new(parent));
        ctx
    }

    /// Get the inner Actix address
    pub fn addr(&self) -> Arc<Addr<A>> {
        self.addr.clone()
    }
}

impl<A> ActorContext for ActixContext<A>
where
    A: actix::Actor<Context = ActixCtx<A>>,
{
    /// Get the actor's self reference
    fn get_self_ref(&self) -> BoxedActorRef {
        self.path.target.clone_boxed()
    }

    /// Stop the actor execution
    ///
    /// Requests cancellation of the current dispatch and starts owned cleanup.
    /// Self-stop returns after requesting it; external managed references can join completion.
    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.idle.close();
            self.tasks.close();
            if let Some(control) = &self.stop_control {
                // Self-stop requests cancellation; awaiting own disposal would deadlock.
                control.request_stop(tokio::time::Instant::now() + control.timeout);
                Ok(())
            } else {
                self.path.target.stop().await
            }
        })
    }

    /// Send a message to another actor without waiting for a response
    fn send<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: parrot_api::types::BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.spawner.own(target);
        let msg = self.spawner.own(msg);
        Box::pin(crate::context_message::call(
            target,
            msg,
            &self.tasks,
            |target, msg| target.deliver(msg),
        ))
    }

    /// Send a message to another actor and wait for a response
    fn ask<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: parrot_api::types::BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        let timeout = self.ask_timeout;
        let target = self.spawner.own(target);
        let msg = self.spawner.own(msg);
        Box::pin(crate::context_message::call(
            target,
            msg,
            &self.tasks,
            move |target, msg| target.send_with_timeout(msg, Some(timeout)),
        ))
    }

    /// Schedule a one-time delayed message
    fn schedule_once<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: parrot_api::types::BoxedMessage,
        delay: std::time::Duration,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.spawner.own(target);
        let msg = self.spawner.own(msg);
        Box::pin(parrot_api::trace::capture(async move {
            let task = self.tasks.once(target.take(), msg.take(), delay)?;
            actix::spawn(task);
            Ok(())
        }))
    }

    /// Register a fixed-delay timer owned by this actor generation.
    fn schedule_periodic<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: CloneableMessage,
        initial_delay: std::time::Duration,
        interval: std::time::Duration,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.spawner.own(target);
        let msg = self.spawner.own(msg);
        Box::pin(parrot_api::trace::capture(async move {
            let task = self
                .tasks
                .periodic(target.take(), msg.take(), initial_delay, interval)?;
            actix::spawn(task);
            Ok(())
        }))
    }

    /// Watch another actor for termination
    ///
    /// # Parameters
    /// - `target`: Reference to another actor to watch
    ///
    /// When the watched actor terminates, this actor will
    /// receive a Terminated message.
    fn watch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.spawner.own(target);
        let watcher = self.spawner.own(self.get_self_ref());
        Box::pin(crate::context_message::call(
            target,
            watcher,
            &self.tasks,
            |target, watcher| target.watch(watcher),
        ))
    }

    /// Remove watch for actor termination
    fn unwatch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        let target = self.spawner.own(target);
        let watcher = self.spawner.own(self.get_self_ref());
        Box::pin(crate::context_message::call(
            target,
            watcher,
            &self.tasks,
            |target, watcher| target.unwatch(watcher),
        ))
    }

    /// Set the parent actor reference
    fn set_parent(&mut self, parent: BoxedActorRef) {
        self.parent = Some(Arc::new(parent));
    }

    /// Get parent actor reference
    fn parent(&self) -> Option<BoxedActorRef> {
        self.parent.as_ref().map(|p| p.as_ref().clone_boxed())
    }

    /// Add a child actor to the context
    fn add_child(&mut self, child: BoxedActorRef) {
        if let Some(children) = &mut self.children {
            children.write().unwrap().push(child);
        } else {
            self.children = Some(Arc::new(RwLock::new(vec![child])));
        }
    }

    /// Remove a child actor from the context
    fn remove_child(&mut self, child: BoxedActorRef) {
        if let Some(children) = &mut self.children {
            let mut children = children.write().unwrap();
            children
                .iter()
                .position(|c| c.as_ref().eq(&*child))
                .map(|index| children.remove(index));
        }
    }

    /// Get references to all child actors
    fn children(&self) -> Option<ReadOnlyChildrenVec> {
        if let Some(control) = &self.stop_control {
            return Some(control.children.view());
        }
        self.children
            .as_ref()
            .map(|children| ReadOnlyChildrenVec::new(children.clone()))
    }

    /// Set timeout for receiving messages
    fn set_receive_timeout(&mut self, timeout: Option<std::time::Duration>) {
        if let Some(task) = self.idle.configure(timeout, &self.tasks) {
            let submitted = self.idle_spawner.as_ref().is_some_and(|arbiter| {
                arbiter.spawn(async move {
                    let _ = task.await;
                })
            });
            if !submitted {
                self.idle.close();
                tracing::warn!("receive timeout arbiter unavailable; disabled");
            }
        }
    }

    /// Get current receive timeout
    fn receive_timeout(&self) -> Option<std::time::Duration> {
        self.idle.get()
    }

    /// Set supervision strategy for child actors
    fn set_supervisor_strategy(
        &mut self,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) {
        if let Some(control) = &self.stop_control {
            control.children.set_strategy(strategy);
        }
    }

    /// Get the actor's path
    ///
    /// # Returns
    /// The actor's unique path in the actor system
    fn path(&self) -> &ActorPath {
        &self.path
    }

    /// Get stream registry
    fn stream_registry(&mut self) -> &mut dyn parrot_api::stream::StreamRegistry {
        &mut self.stream_registry
    }

    /// Get actor spawner
    fn async_spawner(&self) -> Option<&dyn parrot_api::engine::AsyncActorSpawner> {
        Some(&self.spawner)
    }
    fn local_spawner(&self) -> Option<&dyn parrot_api::engine::AsyncLocalActorSpawner> {
        (self.spawner.service.is_some()
            && self.spawner.scope.is_some()
            && self.spawner.parent.is_some())
        .then_some(&self.spawner)
    }

    fn spawner(&mut self) -> &mut dyn parrot_api::context::ActorSpawner {
        &mut self.spawner
    }

    // ---------------- K2 receptionist gateway forwarding ----------------

    fn receptionist_register(
        &mut self,
        key: parrot_api::receptionist::ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        let gateway = self
            .spawner
            .service
            .as_ref()
            .and_then(|s| s.receptionist_gateway());
        let scope = self
            .spawner
            .service
            .as_ref()
            .map(|service| self.tasks.registration_scope(&service.runtime));
        let path = self.path.path().to_string();
        Box::pin(async move {
            match (gateway, scope) {
                (Some(gateway), Some(scope)) => gateway.register_scoped(key, path, scope).await,
                _ => Err(ActorError::InternalError("receptionist not enabled".into())),
            }
        })
    }

    fn receptionist_deregister(
        &mut self,
        key: &parrot_api::receptionist::ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        let gateway = self
            .spawner
            .service
            .as_ref()
            .and_then(|s| s.receptionist_gateway());
        let scope = self
            .spawner
            .service
            .as_ref()
            .map(|service| self.tasks.registration_scope(&service.runtime));
        let key = key.clone();
        let path = self.path.path().to_string();
        Box::pin(async move {
            match (gateway, scope) {
                (Some(gateway), Some(scope)) => gateway.deregister_scoped(key, path, scope).await,
                _ => Err(ActorError::InternalError("receptionist not enabled".into())),
            }
        })
    }

    fn receptionist_subscribe<'a>(
        &'a mut self,
        key: parrot_api::receptionist::ReceptionistKey,
    ) -> BoxedFuture<'a, ActorResult<parrot_api::receptionist::ReceptionistStream>> {
        let gateway = self
            .spawner
            .service
            .as_ref()
            .and_then(|s| s.receptionist_gateway());
        Box::pin(async move {
            match gateway {
                Some(gateway) => gateway.subscribe(key).await,
                None => Err(ActorError::InternalError("receptionist not enabled".into())),
            }
        })
    }
}

impl<A> ActorContextScheduler for ActixContext<A>
where
    A: actix::Actor<Context = ActixCtx<A>>,
{
    fn schedule<'life0, 'async_trait, M>(
        &'life0 self,
        msg: M,
        delay: std::time::Duration,
        interval: Option<std::time::Duration>,
    ) -> BoxedFuture<'async_trait, ActorResult<ScheduledTask>>
    where
        M: Message + Clone + Sync + 'async_trait,
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let msg = self.spawner.own(msg);
        Box::pin(parrot_api::trace::capture(async move {
            // Validate both clock domains before reserving a slot or launching
            // a timer. A rejected input must be fully retired before returning.
            let schedule_time = match std::time::SystemTime::now().checked_add(delay) {
                Some(time) => time,
                None => {
                    msg.retire().await?;
                    return Err(ActorError::InternalError(
                        "scheduled time exceeds clock range".into(),
                    ));
                }
            };
            // Generated references are owned inputs too. Keep each one
            // outside callback unwinding, and retire all inputs before reporting
            // a rejected registration. Callback failure is the primary error.
            let target = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.get_self_ref()
            })) {
                Ok(target) => self.spawner.own(target),
                Err(payload) => {
                    let error = ActorError::Panic(crate::owned_payload::panic_message(payload));
                    let _ = msg.retire().await;
                    return Err(error);
                }
            };
            let target_for_task =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    target.value().clone_boxed()
                })) {
                    Ok(target) => target,
                    Err(payload) => {
                        let error = ActorError::Panic(crate::owned_payload::panic_message(payload));
                        let _ = tokio::join!(target.retire(), msg.retire());
                        return Err(error);
                    }
                };
            let registration = match interval {
                Some(interval) => self.tasks.periodic_with_id(
                    target_for_task,
                    CloneableMessage::from_message(msg.take()),
                    delay,
                    interval,
                ),
                None => self
                    .tasks
                    .once_with_id(target_for_task, Box::new(msg.take()), delay),
            };
            let (id, task) = match registration {
                Ok(registered) => registered,
                Err(error) => {
                    let _ = target.retire().await;
                    return Err(error);
                }
            };
            let target: Arc<dyn parrot_api::address::ActorRef> = Arc::from(target.take());
            actix::spawn(task);
            let tasks = self.tasks.clone();
            let cancel = Arc::new(move || tasks.cancel_schedule(id)) as Arc<dyn Fn() + Send + Sync>;
            Ok(ScheduledTask::with_cancel(
                id,
                target,
                schedule_time,
                cancel,
            ))
        }))
    }

    fn cancel_schedule(&self, task: ScheduledTask) {
        if task.target.path() == self.path.path() {
            task.cancel();
        }
    }
}

impl parrot_api::engine::AsyncActorSpawner for ManagedSpawner {
    fn spawn<'a>(
        &'a self,
        actor: parrot_api::engine::RuntimeActor,
        config: parrot_api::engine::InstanceConfig,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let actor = super::system::BoxedActixActor { inner: actor };
        let payload = <ActixContext<super::actor::ActixActor<super::system::BoxedActixActor>> as parrot_api::context::ActorSpawnContext<super::system::BoxedActixActor>>::wrap_spawn(actor);
        match strategy {
            Some(strategy) => {
                ActorSpawner::spawn_with_strategy(self, payload, Box::new(config), strategy)
            }
            None => ActorSpawner::spawn(self, payload, Box::new(config)),
        }
    }
    fn spawn_factory<'a>(
        &'a self,
        factory: parrot_api::engine::RuntimeFactory,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let payload = <ActixContext<super::actor::ActixActor<super::system::BoxedActixActor>> as parrot_api::context::ActorFactoryContext<super::system::BoxedActixActor>>::wrap_factory(move || super::system::BoxedActixActor { inner: factory() });
        ActorSpawner::spawn_factory_erased(self, payload, strategy)
    }
}

impl parrot_api::engine::AsyncLocalActorSpawner for ManagedSpawner {
    fn spawn_local_factory<'a>(
        &'a self,
        factory: parrot_api::engine::LocalRuntimeFactory,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let factory = self.own(factory);
        let policy = self.own(strategy);
        Box::pin(parrot_api::trace::capture(async move {
            if let (Some(service), Some(scope), Some(parent)) =
                (&self.service, &self.scope, &self.parent)
                && let Ok(system) = service.upgrade()
            {
                // A native factory context's path.target is its physical
                // generation. Resolve the registry's stable logical parent,
                // retaining it weakly. The OLD scope still arbitrates admission
                // atomically, so path reuse cannot admit through a stale context.
                let parent_path = parent.path();
                let identity = system
                    .registry
                    .read()
                    .unwrap()
                    .actors
                    .get(&parent_path)
                    .map(|entry| Arc::downgrade(&entry.actor_ref));
                let Some(identity) = identity else {
                    let (factory, policy) = tokio::join!(factory.retire(), policy.retire());
                    factory.and(policy)?;
                    return Err(ActorError::Stopped);
                };
                let path = format!(
                    "{}/{}",
                    parent_path.trim_end_matches('/'),
                    uuid::Uuid::new_v4()
                );
                return Arc::new(system)
                    .spawn_neutral_local_child(
                        factory,
                        policy,
                        &path,
                        super::local_typed::LocalParent {
                            scope,
                            identity: Some(identity),
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
