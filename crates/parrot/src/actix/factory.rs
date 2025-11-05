//! Logical Actix factories own construction, generations and final retirement.
use super::{
    actor::ActixActor,
    context::ActixContext,
    lifecycle::{ActorControl, ActorExit, Outcome},
    reference::ActixActorRef,
    system::{ActixActorSystem, Registration, remove_registration},
};
use crate::{
    children::ChildLifetime,
    constructor::{ConstructionControl, ConstructionError, Factory},
};
use futures::FutureExt;
use parrot_api::{
    actor::{ActixEngineExt, Actor},
    address::ActorRef,
    context::ActorTerminated,
    death_watch::{DeathWatch, TerminationRecipient},
    errors::ActorError,
    message::MessageOptions,
    supervisor::{SupervisionDecision, SupervisorStrategyType},
    typed_channel::TypedActorLifecycle,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{Arc, Mutex, Weak, atomic::Ordering},
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    time::Instant,
};

struct Current {
    actor: Arc<dyn ActorRef>,
    control: Arc<ActorControl>,
}
#[derive(Default)]
struct State {
    current: Option<Current>,
    construction: Option<Arc<ConstructionControl>>,
}
struct Logical {
    path: String,
    control: Arc<ActorControl>,
    state: Mutex<State>,
}
impl Logical {
    fn current(&self) -> ActorResult<Arc<dyn ActorRef>> {
        if self.control.channel.is_sealed() {
            return Err(ActorError::Stopped);
        }
        self.state
            .lock()
            .unwrap()
            .current
            .as_ref()
            .map(|current| current.actor.clone())
            .ok_or(ActorError::Stopped)
    }
    fn stop_generation(&self, deadline: Instant) {
        self.control.lifetime.cancel_recovery();
        let state = self.state.lock().unwrap();
        if let Some(construction) = &state.construction {
            construction.cancel();
        }
        if let Some(current) = &state.current {
            current.control.request_stop(deadline);
        }
    }
    fn reference(self: &Arc<Self>) -> BoxedActorRef {
        Box::new(FactoryRef(self.clone()))
    }
}
struct FactoryRef(Arc<Logical>);
impl std::fmt::Debug for FactoryRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ActixFactoryRef")
            .field(&self.0.path)
            .finish()
    }
}
#[async_trait::async_trait]
impl ActorRef for FactoryRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send_with_timeout(message, None)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.0.current()?.send_with_timeout(message, timeout).await
        }))
    }
    fn send_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.0.current()?.send_with_options(message, options).await
        }))
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.0.current()?.deliver(message).await
        }))
    }
    fn deliver_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.0
                .current()?
                .deliver_with_options(message, options)
                .await
        }))
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.control.stop()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move {
            match self.0.current() {
                Ok(actor) => actor.is_alive().await,
                Err(_) => false,
            }
        })
    }
    fn path(&self) -> String {
        self.0.path.clone()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        self.0.reference()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn death_watch(&self) -> Option<DeathWatch> {
        Some(self.0.control.death_watch.clone())
    }
}
struct LogicalRecipient(Weak<Logical>);
impl TerminationRecipient for LogicalRecipient {
    fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let logical = self.0.upgrade().ok_or(ActorError::Stopped)?;
            logical.current()?.send(Box::new(event)).await.map(|_| ())
        })
    }
}
struct Generation {
    control: Arc<ActorControl>,
    finished: watch::Sender<Option<Outcome>>,
    result: oneshot::Receiver<ActorExit>,
}
fn spawn_generation<A>(
    system: ActixActorSystem,
    actor: A,
    logical: Arc<Logical>,
    parent: Option<BoxedActorRef>,
    gate: Option<Arc<crate::children::recovery::Recovery>>,
) -> ActorResult<Generation>
where
    A: Actor<Context = ActixContext<ActixActor<A>>>
        + ActixEngineExt<Context = ActixContext<ActixActor<A>>>
        + Unpin
        + 'static,
{
    let (mut control, finished) = ActorControl::with_lifetime(
        system.settings.shutdown_timeout,
        system.settings.max_context_tasks,
        Some(logical.control.lifetime.clone()),
        system.policy_slots.clone(),
    );
    Arc::get_mut(&mut control)
        .expect("unpublished control")
        .upstream_stop = Some(Arc::downgrade(&logical.control));
    Arc::get_mut(&mut control)
        .expect("unpublished control")
        .group_activation = gate.clone();
    let outcome: super::actor::ActorOutcome = Default::default();
    let actor_base = ActixActor::managed(
        actor,
        logical.path.clone(),
        control.clone(),
        outcome.clone(),
        system.settings.mailbox_capacity,
        system.settings.ask_timeout,
    )
    .with_spawn_service(system.downgrade(), parent);
    let (sender, receiver) = actix::dev::channel::channel(system.settings.mailbox_capacity);
    let reference: Arc<dyn ActorRef> = Arc::new(
        ActixActorRef::new(actix::Addr::new(sender), logical.path.clone())
            .with_control(control.clone()),
    );
    {
        let mut state = logical.state.lock().unwrap();
        if logical.control.channel.is_sealed()
            || (gate.is_none() && logical.control.lifetime.recovery().is_some())
            || gate
                .as_ref()
                .is_some_and(|group| group.control.is_cancelled())
        {
            return Err(ActorError::Stopped);
        }
        // Install cancellation before the arbiter can run initialization.
        state.current = Some(Current {
            actor: reference,
            control: control.clone(),
        });
    }
    let (send, result) = oneshot::channel();
    let tasks = control.tasks.clone();
    let children = control.children.clone();
    let cleanup_control = control.clone();
    if !system.arbiter_pool().next_arbiter().spawn_fn(move || {
        actix::spawn(async move {
            let execution = std::panic::AssertUnwindSafe(async move {
                actix::Context::with_receiver(receiver)
                    .into_future(actor_base)
                    .await;
            })
            .catch_unwind()
            .await;
            let cleanup = execution
                .map_err(|payload| ActorError::Panic(super::lifecycle::panic_message(payload)));
            let outcome = outcome.lock().unwrap().take().unwrap_or_else(|| {
                Err(ActorError::InternalError(
                    "Actix factory generation ended without lifecycle completion".into(),
                ))
            });
            tasks.close();
            let tasks_result = tasks.join().await;
            let children_result = children.join().await;
            let stop_result = cleanup_control
                .stop_failure
                .lock()
                .unwrap()
                .take()
                .map_or(Ok(()), |error| Err(error.into_error()));
            let _ = send.send(ActorExit {
                outcome,
                cleanup: cleanup
                    .and(tasks_result)
                    .and(children_result)
                    .and(stop_result),
            });
        });
    }) {
        logical.state.lock().unwrap().current = None;
        return Err(ActorError::InternalError(
            "Actix arbiter rejected factory generation".into(),
        ));
    }
    Ok(Generation {
        control,
        finished,
        result,
    })
}
fn construction_error(error: ConstructionError<ActorError>) -> ActorError {
    match error {
        ConstructionError::Cancelled => ActorError::Stopped,
        ConstructionError::ConstructorPanicked(message) => ActorError::InitializationError(message),
        ConstructionError::DisposalPanicked(message) => ActorError::Panic(message),
        ConstructionError::WorkerFailed(message) => ActorError::InternalError(message),
        ConstructionError::Publication(error) => error,
    }
}
struct GroupBuildError {
    cancelled: bool,
    outcome: ActorError,
    retirement: Option<crate::error_receipt::ActorReceipt>,
}
impl GroupBuildError {
    fn stopped(error: String) -> Self {
        Self {
            cancelled: true,
            outcome: ActorError::InternalError(error),
            retirement: None,
        }
    }
    fn construction(error: ConstructionError<ActorError>) -> Self {
        let retirement = matches!(
            &error,
            ConstructionError::DisposalPanicked(_) | ConstructionError::WorkerFailed(_)
        )
        .then(|| {
            crate::error_receipt::ActorReceipt::from_error(ActorError::InternalError(format!(
                "group construction cleanup failed: {error:?}"
            )))
        });
        Self {
            cancelled: matches!(
                &error,
                ConstructionError::Cancelled | ConstructionError::Publication(ActorError::Stopped)
            ),
            outcome: construction_error(error),
            retirement,
        }
    }
}
async fn group_generation<A>(
    system: super::system::WeakSystem,
    factory: &Arc<Factory<A>>,
    logical: Arc<Logical>,
    parent: Option<BoxedActorRef>,
    recovery: Arc<crate::children::recovery::Recovery>,
) -> Result<Generation, GroupBuildError>
where
    A: Actor<Context = ActixContext<ActixActor<A>>>
        + ActixEngineExt<Context = ActixContext<ActixActor<A>>>
        + Unpin
        + 'static,
{
    use crate::children::recovery::Step;
    let member = logical.control.lifetime.id;
    let work = async {
        logical
            .control
            .lifetime
            .settle_live_decision()
            .await
            .map_err(|error| {
                let error = crate::error_receipt::ActorReceipt::from_error(error);
                GroupBuildError {
                    cancelled: false,
                    retirement: Some(error.clone()),
                    outcome: error.into_error(),
                }
            })?;
        recovery
            .arrive(member, Step::Retired)
            .await
            .map_err(GroupBuildError::stopped)?;
        let prepared = parrot_api::trace::in_scope(recovery.failure_context.clone(), || {
            factory.prepare::<ActorError>(recovery.control.clone())
        })
        .await
        .map_err(GroupBuildError::construction)?;
        if let Err(error) = recovery.arrive(member, Step::Prepared).await {
            prepared.discard().await.map_err(|error| GroupBuildError {
                cancelled: false,
                outcome: ActorError::InternalError(error.clone()),
                retirement: Some(crate::error_receipt::ActorReceipt::from_error(
                    ActorError::InternalError(error),
                )),
            })?;
            return Err(GroupBuildError::stopped(error));
        }
        let owner = logical.clone();
        let gate = recovery.clone();
        let generation = prepared
            .publish(move |actor| {
                spawn_generation(system.upgrade()?, actor, owner, parent, Some(gate))
            })
            .await
            .map_err(GroupBuildError::construction)?;
        match recovery.arrive(member, Step::Published).await {
            Ok(()) => Ok(generation),
            Err(error) => {
                // This unpublished-to-business generation remains engine-owned
                // until ContextFut, children and tasks have really been disposed.
                generation.control.begin_cleanup(recovery.deadline);
                let exit = generation.result.await.map_err(|lost| GroupBuildError {
                    cancelled: false,
                    outcome: ActorError::InternalError(lost.to_string()),
                    retirement: Some(crate::error_receipt::ActorReceipt::from_error(
                        ActorError::InternalError(lost.to_string()),
                    )),
                })?;
                let cleanup = exit.cleanup;
                generation.finished.send_replace(Some(Outcome::Stopped));
                generation.control.death_watch.abandon();
                logical.state.lock().unwrap().current = None;
                cleanup.map_err(|error| {
                    let error = crate::error_receipt::ActorReceipt::from_error(error);
                    GroupBuildError {
                        cancelled: false,
                        retirement: Some(error.clone()),
                        outcome: error.into_error(),
                    }
                })?;
                Err(GroupBuildError::stopped(error))
            }
        }
    };
    tokio::pin!(work);
    let result = tokio::select! {
        biased;
        result = &mut work => result,
        _ = logical.control.channel.closed() => {
            recovery.abort("explicit stop cancelled Actix cohort");
            work.await
        },
        _ = tokio::time::sleep_until(recovery.deadline) => {
            recovery.abort("Actix cohort recovery deadline elapsed");
            logical.control.request_stop(recovery.deadline);
            work.await
        },
    };
    if let Err(error) = &result {
        recovery.abort(error.outcome.to_string());
    }
    result
}

type SpawnReply = Arc<Mutex<Option<oneshot::Sender<ActorResult<BoxedActorRef>>>>>;
fn reply(send: &SpawnReply, result: ActorResult<BoxedActorRef>) {
    if let Some(send) = send.lock().unwrap().take() {
        let _ = send.send(result);
    }
}
struct Ticket {
    logical: Arc<Logical>,
    activate: Option<oneshot::Sender<()>>,
    receive: oneshot::Receiver<ActorResult<BoxedActorRef>>,
    cancel: bool,
}
impl Ticket {
    async fn wait(mut self) -> ActorResult<BoxedActorRef> {
        if let Some(activate) = self.activate.take() {
            let _ = activate.send(());
        }
        let result = (&mut self.receive).await.map_err(|_| {
            ActorError::InternalError("Actix factory owner lost creation result".into())
        })?;
        self.cancel = false;
        result
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if self.cancel {
            self.logical
                .control
                .request_stop(Instant::now() + self.logical.control.timeout);
        }
    }
}
impl ActixActorSystem {
    /// Create a root logical actor using the OneForOne policy of three restarts per 60 seconds.
    /// Construction is bounded and cancellation-safe; initialization is asynchronous.
    /// Factory captures and explicit engine configuration/policy are owned at
    /// call time. Dropping an unpolled creation future retires inputs without
    /// invoking the constructor or registering an actor.
    pub fn spawn_root_factory<'a, A, F>(
        &'a self,
        factory: F,
    ) -> impl std::future::Future<Output = ActorResult<BoxedActorRef>> + Send + 'a
    where
        A: Actor<Context = ActixContext<ActixActor<A>>>
            + ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_root_factory_with_strategy(
            factory,
            parrot_api::supervisor::DefaultSupervisorStrategyFactory::one_for_one(
                3,
                Duration::from_secs(60),
            ),
        )
    }

    /// Create a root logical actor with an explicit per-actor restart budget.
    /// The root has no parent and is not a sibling cohort. Configuration belongs
    /// in the repeatable constructor. Stop/shutdown await actual resource retirement.
    pub fn spawn_root_factory_with_strategy<'a, A, F>(
        &'a self,
        factory: F,
        strategy: parrot_api::supervisor::OneForOneStrategy,
    ) -> impl std::future::Future<Output = ActorResult<BoxedActorRef>> + Send + 'a
    where
        A: Actor<Context = ActixContext<ActixActor<A>>>
            + ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
        F: Fn() -> A + Send + Sync + 'static,
    {
        let factory =
            crate::owned_payload::OwnedPayload::new(factory, &self.runtime, &self.cleanup_slots);
        let strategy =
            crate::owned_payload::OwnedPayload::new(strategy, &self.runtime, &self.cleanup_slots);
        parrot_api::trace::capture(async move {
            let factory = Factory::new(
                factory.take(),
                self.constructor_slots.clone(),
                self.cleanup_slots.clone(),
            );
            let lifetime = ChildLifetime::with_policy(
                Weak::new(),
                Some(SupervisorStrategyType::OneForOne(strategy.take())),
                self.cleanup_slots.clone(),
            );
            match self.start_factory(factory.clone(), None, lifetime.clone(), None) {
                Ok(ticket) => ticket.wait().await,
                Err(error) => {
                    // Rejection owns arbitrary captures even when the caller cancels.
                    self.runtime
                        .spawn(async move {
                            let policy = lifetime.dispose_policy().await;
                            let factory = factory.retire().await.map_err(ActorError::InternalError);
                            factory.and(policy)
                        })
                        .await
                        .map_err(|error| ActorError::InternalError(error.to_string()))??;
                    Err(error)
                }
            }
        })
    }

    fn start_factory<A>(
        &self,
        factory: Arc<Factory<A>>,
        parent: Option<BoxedActorRef>,
        lifetime: Arc<ChildLifetime>,
        path: Option<String>,
    ) -> ActorResult<Ticket>
    where
        A: Actor<Context = ActixContext<ActixActor<A>>>
            + ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
    {
        let (control, finished) = ActorControl::with_lifetime(
            self.settings.shutdown_timeout,
            self.settings.max_context_tasks,
            Some(lifetime),
            self.policy_slots.clone(),
        );
        let logical = Arc::new(Logical {
            path: path.unwrap_or_else(|| {
                parent.as_ref().map_or_else(
                    || {
                        format!(
                            "actix://{}/{}",
                            std::any::type_name::<A>()
                                .rsplit("::")
                                .next()
                                .unwrap_or("unknown"),
                            uuid::Uuid::new_v4().simple()
                        )
                    },
                    |parent| format!("{}/{}", parent.path(), uuid::Uuid::new_v4().simple()),
                )
            }),
            control,
            state: Mutex::default(),
        });
        let recovering = Arc::downgrade(&logical);
        logical
            .control
            .lifetime
            .bind_recovery(Arc::new(move |recovery| {
                if let Some(logical) = recovering.upgrade() {
                    let state = logical.state.lock().unwrap();
                    if logical.control.channel.is_sealed() {
                        recovery.abort("Actix member already stopping");
                    }
                    if let Some(construction) = &state.construction {
                        construction.cancel();
                    }
                    if let Some(current) = &state.current {
                        current.control.begin_cleanup(recovery.deadline);
                    }
                } else {
                    recovery.abort("Actix logical owner disappeared");
                }
            }));
        let weak = Arc::downgrade(&logical);
        logical
            .control
            .stop_hook
            .set(Arc::new(move |deadline| {
                if let Some(logical) = weak.upgrade() {
                    logical.stop_generation(deadline);
                }
            }))
            .map_err(|_| {
                ActorError::InternalError("factory stop endpoint already installed".into())
            })?;
        logical
            .control
            .death_watch
            .set_recipient(Arc::new(LogicalRecipient(Arc::downgrade(&logical))))?;
        {
            let mut registry = self.registry.write().unwrap();
            if registry.shutting_down {
                return Err(ActorError::Stopped);
            }
            if registry.actors.contains_key(&logical.path) {
                return Err(ActorError::InitializationError(
                    "actor path already registered".into(),
                ));
            }
            registry.actors.insert(
                logical.path.clone(),
                Registration {
                    actor_ref: Arc::from(logical.reference()),
                    control: logical.control.clone(),
                },
            );
        }
        let (send, receive) = oneshot::channel();
        let send = Arc::new(Mutex::new(Some(send)));
        let (activate, activation) = oneshot::channel();
        let weak_system = self.downgrade();
        let registry = Arc::downgrade(&self.registry);
        let pool = self.arbiter_pool();
        let slots = self.policy_slots.clone();
        let notification_slots = self.notification_slots.clone();
        let owner = logical.clone();
        let initial_deadline = Instant::now() + logical.control.timeout;
        let mut construction_context = parrot_api::trace::TraceContext::current();
        self.runtime.spawn(async move {
            let _pool_owner = pool;
            if activation.await.is_err() { owner.control.request_stop(initial_deadline); }
            let work = async {
                let mut first = true;
                let mut deadline = initial_deadline;
                let mut retirement = Ok(());
                let mut staged_generation = None;
                let (mut status, mut reason) = loop {
                    let constructed = if let Some(generation) = staged_generation.take() { Ok(generation) } else {
                    let token = Arc::new(ConstructionControl::with_deadline(deadline));
                    {
                        let mut state = owner.state.lock().unwrap();
                        state.construction = Some(token.clone());
                        if owner.control.channel.is_sealed() { token.cancel(); }
                    }
                    let next_parent = parent.as_ref().map(|parent| parent.clone_boxed());
                    let next_owner = owner.clone();
                    let next_system = weak_system.clone();
                    // Each construction consumes its own admission or failure identity.
                    let construction = parrot_api::trace::in_scope(construction_context.take(), || {
                        factory.construct(token.clone(), move |actor| {
                            spawn_generation(next_system.upgrade()?, actor, next_owner, next_parent, None)
                        })
                    });
                    tokio::pin!(construction);
                    let result = tokio::select! {
                        biased;
                        result = &mut construction => result,
                        _ = owner.control.channel.closed() => { token.cancel(); construction.await },
                        _ = tokio::time::sleep_until(deadline) => {
                            owner.control.request_stop(deadline);
                            reply(&send, Err(ActorError::TimeoutDetail("Actix factory creation deadline elapsed".into())));
                            construction.await
                        },
                    };
                    owner.state.lock().unwrap().construction = None;
                    result
                    };
                    if matches!(constructed, Err(ConstructionError::Cancelled | ConstructionError::Publication(ActorError::Stopped)))
                        && let Some(recovery) = owner.control.lifetime.recovery() {
                        match group_generation(weak_system.clone(), &factory, owner.clone(), parent.as_ref().map(|p| p.clone_boxed()), recovery).await {
                            Ok(generation) => { staged_generation = Some(generation); continue; },
                            Err(error) => {
                                let cancelled = error.cancelled && error.retirement.is_none() && owner.control.channel.is_sealed() && !owner.control.expired_now();
                                if let Some(error) = error.retirement { retirement = Err(error); }
                                break (if cancelled { Outcome::Stopped } else { Outcome::from_result(Err(error.outcome)) }, None);
                            },
                        }
                    }
                    let generation = match constructed {
                        Ok(generation) => generation,
                        Err(error) => {
                            if matches!(&error, ConstructionError::DisposalPanicked(_) | ConstructionError::WorkerFailed(_)) { retirement = Err(crate::error_receipt::ActorReceipt::from_error(ActorError::InternalError(format!("factory cleanup failed: {error:?}")))); }
                            let status = match error {
                                ConstructionError::Cancelled | ConstructionError::Publication(ActorError::Stopped) => Outcome::Stopped,
                                error => Outcome::from_result(Err(construction_error(error))),
                            };
                            break (status, None);
                        }
                    };
                    owner.control.death_watch.resume().ok();
                    if first {
                        first = false;
                        reply(&send, Ok(owner.reference()));
                    }
                    let Generation { control, finished, result } = generation;
                    let exit = result.await;
                    owner.state.lock().unwrap().current = None;
                    owner.control.death_watch.suspend();
                    let ActorExit { outcome, cleanup } = match exit {
                        Ok(exit) => exit,
                        Err(_) => {
                            let error = ActorError::InternalError("Actix factory generation execution was cancelled".into());
                            control.death_watch.abandon();
                            finished.send_replace(Some(Outcome::Failed(crate::error_receipt::ActorReceipt::new(Arc::new(error)))));
                            let error = crate::error_receipt::ActorReceipt::from_error(ActorError::InternalError("Actix factory generation execution was cancelled".into()));
                            retirement = Err(error.clone());
                            break (Outcome::Failed(error), None);
                        }
                    };
                    let failure_context = control.failure_context();
                    let escalated = control.escalated.lock().unwrap().take();
                    let failure = match outcome { Err(error) => Some(Arc::new(error)), Ok(()) => escalated.clone() };
                    let native_status = match cleanup {
                        Err(error) => { let error = crate::error_receipt::ActorReceipt::from_error(error); retirement = Err(error.clone()); Outcome::Failed(error) },
                        Ok(()) => failure.as_ref().map_or(Outcome::Stopped, |error| Outcome::Failed(crate::error_receipt::ActorReceipt::new(error.clone()))),
                    };
                    finished.send_replace(Some(native_status.clone()));
                    // Native references are generation bound. Slow generation
                    // observers cannot consume the logical recovery deadline.
                    let native_reason = match (&native_status, escalated.as_ref()) {
                        (Outcome::Failed(current), Some(original)) if current.same_error(original) => parrot_api::supervisor::DeathReason::Escalated(crate::error_receipt::actor_summary(original)),
                        _ => super::lifecycle::death_reason_from_result(&native_status.clone().result(), control.deadline_was_enforced()),
                    };
                    if let Ok(permit) = notification_slots.clone().try_acquire_owned() {
                        let observed = control.death_watch.clone();
                        let path = owner.path.clone();
                        let timeout = control.timeout;
                        let reason = native_reason.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            observed.terminated(ActorTerminated { generation: observed.id(), path, reason }, timeout).await;
                        });
                    } else { control.death_watch.abandon(); }
                    if retirement.is_err() || owner.control.channel.is_sealed() || control.explicit_stop.load(Ordering::Acquire) || control.expired_now() {
                        if let Some(group) = owner.control.lifetime.recovery() { group.abort("Actix member cannot complete old-generation retirement"); }
                        break (native_status, Some(native_reason.clone()));
                    }
                    deadline = control.cleanup_deadline().unwrap_or_else(|| Instant::now() + owner.control.timeout);
                    let policy = if let Some(recovery) = owner.control.lifetime.recovery() {
                        crate::children::FailurePolicy { decision: SupervisionDecision::Restart, group: None, failure_context: recovery.failure_context.clone(), recovery: Some(recovery) }
                    } else {
                        if !control.supervision_failed.load(Ordering::Acquire) { break (native_status, Some(native_reason.clone())); }
                        let Some(error) = failure.clone() else { break (native_status, Some(native_reason.clone())); };
                        let policy = parrot_api::trace::scope(failure_context.clone(), owner.control.lifetime.failure_policy(error, slots.clone(), deadline));
                        tokio::pin!(policy);
                        let policy = tokio::select! {
                            biased;
                            result = &mut policy => result,
                            _ = tokio::time::sleep_until(deadline) => { owner.control.request_stop(deadline); policy.await },
                        };
                        match policy {
                            Ok(Some(policy)) => policy,
                            Ok(None) => break (native_status, Some(native_reason.clone())),
                            Err(error) => { let error = crate::error_receipt::ActorReceipt::from_error(error); retirement = Err(error.clone()); break (Outcome::Failed(error), None); }
                        }
                    };
                    if owner.control.channel.is_sealed() { break (native_status, Some(native_reason.clone())); }
                    if let Some(recovery) = &policy.recovery {
                        deadline = recovery.deadline;
                        match group_generation(weak_system.clone(), &factory, owner.clone(), parent.as_ref().map(|p| p.clone_boxed()), recovery.clone()).await {
                            Ok(generation) => { staged_generation = Some(generation); continue; },
                            Err(error) => {
                                let cancelled = error.cancelled && error.retirement.is_none() && owner.control.channel.is_sealed() && !owner.control.expired_now();
                                if let Some(error) = error.retirement { retirement = Err(error); }
                                break (if cancelled { Outcome::Stopped } else { Outcome::from_result(Err(error.outcome)) }, None);
                            },
                        }
                    }
                    if let Err(error) = policy.stop_siblings().await { let error = crate::error_receipt::ActorReceipt::from_error(error); retirement = Err(error.clone()); break (Outcome::Failed(error), None); }
                    if Instant::now() >= deadline || owner.control.channel.is_sealed() { owner.control.request_stop(deadline); break (native_status, Some(native_reason.clone())); }
                    match policy.decision {
                        SupervisionDecision::Restart => {
                            let settle = owner.control.lifetime.settle_live_decision();
                            tokio::pin!(settle);
                            let settled = tokio::select! {
                                biased;
                                result = &mut settle => result,
                                _ = tokio::time::sleep_until(deadline) => {
                                    owner.control.request_stop(deadline);
                                    settle.await
                                },
                            };
                            if let Err(error) = settled {
                                let error = crate::error_receipt::ActorReceipt::from_error(error);
                                retirement = Err(error.clone());
                                break (Outcome::Failed(error), None);
                            }
                            if owner.control.channel.is_sealed() || Instant::now() >= deadline {
                                owner.control.request_stop(deadline);
                                break (native_status, Some(native_reason.clone()));
                            }
                            construction_context = failure_context;
                            continue;
                        },
                        SupervisionDecision::Escalate => { if let Some(error) = failure { parrot_api::trace::in_scope(failure_context, || owner.control.lifetime.escalate(error)); } break (native_status, Some(native_reason.clone())); },
                        _ => break (native_status, Some(native_reason.clone())),
                    }
                };
                owner.control.begin_cleanup(Instant::now() + owner.control.timeout);
                let policy = owner.control.lifetime.dispose_policy().await;
                let disposal = factory.retire().await;
                if let Err(error) = disposal.map_err(ActorError::InternalError).and(policy) {
                    let error = crate::error_receipt::ActorReceipt::from_error(error);
                    retirement = Err(error.clone());
                    status = Outcome::Failed(error);
                    reason = None;
                }
                remove_registration(&registry, &owner.path, &owner.control);
                let notification = owner.control.lifetime.finish(retirement);
                (status, notification, reason)
            };
            tokio::pin!(work);
            let (status, notification, reason) = tokio::select! {
                biased;
                result = &mut work => result,
                _ = owner.control.expired() => {
                    let timeout = Outcome::Timeout(format!("Actix factory {} exceeded configured deadline", owner.path));
                    finished.send_replace(Some(timeout.clone()));
                    reply(&send, Err(timeout.result().unwrap_err()));
                    work.await
                }
            };
            if finished.borrow().is_none() { finished.send_replace(Some(status.clone())); }
            reply(&send, Err(status.clone().result().err().unwrap_or(ActorError::Stopped)));
            let observed = owner.control.death_watch.clone();
            let event = ActorTerminated { generation: observed.id(), path: owner.path.clone(), reason: reason.unwrap_or_else(|| super::lifecycle::death_reason_from_result(&status.result(), false)) };
            let _ = tokio::join!(notification, observed.terminated(event, owner.control.timeout));
        });
        Ok(Ticket {
            logical,
            activate: Some(activate),
            receive,
            cancel: true,
        })
    }
}

trait ChildFactory: Send {
    fn spawn(
        self: Box<Self>,
        system: ActixActorSystem,
        parent: BoxedActorRef,
        scope: Arc<crate::children::ChildScope>,
        strategy: Option<SupervisorStrategyType>,
    ) -> BoxedFuture<'static, ActorResult<BoxedActorRef>>;
}
pub(super) struct ErasedChildFactory(Box<dyn ChildFactory>);
struct TypedChildFactory<A>(Box<dyn Fn() -> A + Send + Sync>);
impl<A> ChildFactory for TypedChildFactory<A>
where
    A: Actor<Context = ActixContext<ActixActor<A>>>
        + ActixEngineExt<Context = ActixContext<ActixActor<A>>>
        + Unpin
        + 'static,
{
    fn spawn(
        self: Box<Self>,
        system: ActixActorSystem,
        parent: BoxedActorRef,
        scope: Arc<crate::children::ChildScope>,
        strategy: Option<SupervisorStrategyType>,
    ) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
        Box::pin(async move {
            let factory = Factory::new(
                self.0,
                system.constructor_slots.clone(),
                system.cleanup_slots.clone(),
            );
            let mut ticket = None;
            let mut strategy = Some(strategy);
            let mut admitted_lifetime = None;
            let admitted = scope.spawn(|lifetime| {
                admitted_lifetime = Some(lifetime.clone());
                lifetime.set_strategy(strategy.take().expect("one factory admission"));
                let pending =
                    system.start_factory(factory.clone(), Some(parent), lifetime.clone(), None)?;
                let logical = pending.logical.clone();
                let child = logical.reference();
                let stop: crate::children::StopChild = Arc::new(move || {
                    logical
                        .control
                        .request_stop(Instant::now() + logical.control.timeout);
                    let lifetime = lifetime.clone();
                    Box::pin(async move { lifetime.wait().await })
                });
                ticket = Some(pending);
                Ok((child, stop))
            });
            if let Err(error) = admitted {
                let policy = crate::supervision_policy::PolicyStore::new(
                    strategy.flatten(),
                    system.cleanup_slots.clone(),
                );
                system
                    .runtime
                    .spawn(async move {
                        let lifetime = match admitted_lifetime {
                            Some(lifetime) => lifetime.dispose_policy().await,
                            None => Ok(()),
                        };
                        let policy = policy.dispose().await;
                        let factory = factory.retire().await.map_err(ActorError::InternalError);
                        factory.and(policy).and(lifetime)
                    })
                    .await
                    .map_err(|error| ActorError::InternalError(error.to_string()))??;
                return Err(error);
            }
            ticket.expect("accepted factory has a ticket").wait().await
        })
    }
}
impl<A> parrot_api::context::ActorFactoryContext<A> for ActixContext<ActixActor<A>>
where
    A: Actor<Context = Self> + ActixEngineExt<Context = Self> + Unpin + 'static,
{
    fn wrap_factory<F: Fn() -> A + Send + Sync + 'static>(factory: F) -> BoxedMessage {
        Box::new(ErasedChildFactory(Box::new(TypedChildFactory(Box::new(
            factory,
        )))))
    }
}
impl ErasedChildFactory {
    pub(super) fn spawn(
        self,
        system: ActixActorSystem,
        parent: BoxedActorRef,
        scope: Arc<crate::children::ChildScope>,
        strategy: Option<SupervisorStrategyType>,
    ) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
        self.0.spawn(system, parent, scope, strategy)
    }
}

impl parrot_api::engine::AsyncFactoryRuntime for ActixActorSystem {
    fn spawn_factory<'a>(
        &'a self,
        factory: parrot_api::engine::RuntimeFactory,
        options: parrot_api::engine::FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, parrot_api::system::SystemError>> {
        use parrot_api::system::SystemError;
        let factory =
            crate::owned_payload::OwnedPayload::new(factory, &self.runtime, &self.cleanup_slots);
        Box::pin(parrot_api::trace::capture(async move {
            if let Err(error) = options.validate() {
                factory.retire().await.map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let constructor = factory.take();
            let factory = Factory::new(
                move || super::system::BoxedActixActor {
                    inner: constructor(),
                },
                self.constructor_slots.clone(),
                self.cleanup_slots.clone(),
            );
            let lifetime = ChildLifetime::with_policy(
                Weak::new(),
                Some(SupervisorStrategyType::OneForOne(
                    parrot_api::supervisor::DefaultSupervisorStrategyFactory::one_for_one(
                        options.max_restarts,
                        options.within,
                    ),
                )),
                self.cleanup_slots.clone(),
            );
            match self.start_factory(factory.clone(), None, lifetime.clone(), options.spawn.path) {
                Ok(ticket) => ticket.wait().await.map_err(SystemError::ActorError),
                Err(error) => {
                    // Install independent retirement before awaiting either input.
                    self.runtime
                        .spawn(async move {
                            let policy = lifetime.dispose_policy().await;
                            let factory = factory.retire().await.map_err(ActorError::InternalError);
                            factory.and(policy)
                        })
                        .await
                        .map_err(|e| {
                            SystemError::ActorCreationError(crate::owned_payload::join_error(e))
                        })?
                        .map_err(SystemError::ActorError)?;
                    Err(SystemError::ActorError(error))
                }
            }
        }))
    }
}

#[cfg(test)]
mod group_publication_tests {
    use super::*;
    use parrot_api::actor::{ActorState, EmptyConfig};
    use std::sync::atomic::AtomicUsize;
    struct Output {
        initialized: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for Output {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl ActixEngineExt for Output {}
    impl Actor for Output {
        type Config = EmptyConfig;
        type Context = ActixContext<ActixActor<Self>>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.initialized.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
        fn receive_message<'a>(
            &'a mut self,
            _: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Ok(Box::new(()) as BoxedMessage) })
        }
    }
    #[actix::test]
    async fn partial_arbiter_rejection_discards_entire_prepared_cohort_without_initialization() {
        let system = ActixActorSystem::with_arbiter_count(2).await.unwrap();
        let pool = system.arbiter_pool();
        let _healthy = pool.next_arbiter();
        let failed = pool.next_arbiter();
        failed.stop();
        tokio::time::timeout(Duration::from_secs(2), async {
            while failed.spawn_fn(|| {}) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // next_arbiter now selects the live worker, then the stopped worker.
        // Both constructors must finish before either publication is attempted.
        let initialized = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut members = Vec::new();
        let mut factories = Vec::new();
        for i in 0..2 {
            let (control, _) = ActorControl::new(Duration::from_secs(2));
            members.push(Arc::new(Logical {
                path: format!("/publication/{i}"),
                control,
                state: Mutex::default(),
            }));
            let initialized = initialized.clone();
            let dropped = dropped.clone();
            factories.push(Factory::new(
                move || Output {
                    initialized: initialized.clone(),
                    dropped: dropped.clone(),
                },
                system.constructor_slots.clone(),
                system.cleanup_slots.clone(),
            ));
        }
        let group = crate::children::recovery::Recovery::new(
            uuid::Uuid::new_v4(),
            members.iter().map(|member| member.control.lifetime.id),
            Instant::now() + Duration::from_secs(2),
            Box::new(|_| {}),
        );
        let (a, b) = tokio::join!(
            group_generation(
                system.downgrade(),
                &factories[0],
                members[0].clone(),
                None,
                group.clone()
            ),
            group_generation(
                system.downgrade(),
                &factories[1],
                members[1].clone(),
                None,
                group.clone()
            ),
        );
        assert!(a.is_err() && b.is_err());
        assert!(!group.is_committed());
        assert_eq!(initialized.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        for member in &members {
            assert!(member.state.lock().unwrap().current.is_none());
            group.retired_after_abort(member.control.lifetime.id);
        }
        for factory in factories {
            factory.retire().await.unwrap();
        }
        system.shutdown().await.unwrap();
    }
}
