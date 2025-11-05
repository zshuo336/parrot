//! Owned constructors: admission precedes user code, deadlines do not end ownership.
#[cfg(test)]
#[path = "factory_admission_trace_tests.rs"]
mod admission_trace_contracts;
use super::{
    address::ThreadActorRef,
    config::ThreadActorConfig,
    context::ThreadContext,
    death_watch::ThreadWatches,
    error::{SpawnError, SystemError},
    mailbox::{Mailbox, WeakMailboxRef, mpsc::MpscMailbox},
    supervisor_exec::{Respawn, RestartControl, SupervisionEntry},
    system::{ActorRegistryEntry, StopOutcome, ThreadActorSystem},
};
use parrot_api::{
    actor::Actor,
    address::{ActorPath, ActorRef},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::sync::{Arc, Mutex, Weak};
#[cfg(test)]
use tokio::sync::Semaphore;
use tokio::sync::watch;

pub(crate) trait FactoryDisposal: Send + Sync {
    fn dispose(&self) -> BoxedFuture<'_, Result<(), SystemError>>;
}
use crate::constructor::{ConstructionError, Factory};
impl<E> From<ConstructionError<E>> for SpawnError
where
    E: Into<SpawnError>,
{
    fn from(error: ConstructionError<E>) -> Self {
        match error {
            ConstructionError::Cancelled => Self::Cancelled,
            ConstructionError::ConstructorPanicked(message) => Self::InitializationFailed(message),
            ConstructionError::DisposalPanicked(message) => Self::DisposalFailed(message),
            ConstructionError::WorkerFailed(message) => Self::SchedulerError(message),
            ConstructionError::Publication(error) => error.into(),
        }
    }
}
impl<A: Send + 'static> FactoryDisposal for Factory<A> {
    fn dispose(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async move { self.retire().await.map_err(SystemError::ShutdownError) })
    }
}

pub(crate) struct PendingFactory {
    pub reference: Arc<dyn ActorRef>,
    pub supervision: Arc<SupervisionEntry>,
    pub control: Arc<RestartControl>,
    pub finished: watch::Sender<Option<StopOutcome>>,
    pub retired: watch::Sender<Option<StopOutcome>>,
    pub lifetime: Arc<crate::children::ChildLifetime>,
}
pub(crate) enum Replacement<'a> {
    Generation(&'a Arc<dyn Mailbox>),
    Pending(&'a Arc<PendingFactory>),
    Instance(&'a Arc<super::instance::PendingInstance>),
}
/// Dropping a spawn waiter cancels admission/publication but never the owner.
struct CancelSpawn {
    system: Weak<ThreadActorSystem>,
    path: String,
    pending: Option<Arc<PendingFactory>>,
}
impl Drop for CancelSpawn {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.control.cancel();
            if let Some(system) = self.system.upgrade() {
                let _ = system.request_stop_ref(&self.path, &pending.supervision.mailbox);
            }
        }
    }
}

impl ThreadActorSystem {
    pub(crate) fn spawn_owned_factory<'a, A, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
        config: ThreadActorConfig,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + 'a
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
        F: Fn() -> A + Send + Sync + 'static,
    {
        let factory = crate::owned_payload::OwnedPayload::new(
            factory,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        let config = crate::owned_payload::OwnedPayload::new(
            config,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        parrot_api::trace::capture(async move {
            let factory = Factory::new(
                factory.take(),
                self.factory_slots.clone(),
                self.cleanup_slots.clone(),
            );
            // Keep one protected copy until synchronous admission has returned.
            // Rejected temporary config clones cannot run the final user Drop.
            let lifetime = crate::children::ChildLifetime::new(Weak::new());
            let result = self.start_owned_factory(
                factory.clone(),
                path,
                config.value().clone(),
                None,
                lifetime.clone(),
            );
            match result {
                Ok(ticket) => {
                    // The unactivated ticket retains PendingFactory ->
                    // supervision -> respawn -> a config clone. Its admission
                    // Arc therefore cannot be final here. Release this spare
                    // copy synchronously before activation; unrelated cleanup
                    // capacity must not prevent an admitted constructor running.
                    drop(config.take());
                    ticket.wait().await
                }
                Err(error) => {
                    let factory = factory.retire();
                    let config = config.retire();
                    let (factory, config, policy) =
                        tokio::join!(factory, config, lifetime.dispose_policy());
                    factory.map_err(SpawnError::InitializationFailed)?;
                    config.map_err(|error| SpawnError::DisposalFailed(error.to_string()))?;
                    policy.map_err(|error| SpawnError::DisposalFailed(error.to_string()))?;
                    Err(error)
                }
            }
        })
    }

    fn start_owned_factory<A>(
        self: &Arc<Self>,
        factory: Arc<Factory<A>>,
        path: &str,
        mut config: ThreadActorConfig,
        parent: Option<BoxedActorRef>,
        lifetime: Arc<crate::children::ChildLifetime>,
    ) -> Result<FactoryTicket<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        lifetime.bind_admission(&mut config, &self.runtime_handle, &self.cleanup_slots);
        let restart_lifetime = lifetime.clone();
        let restart_parent = parent.as_ref().map(|parent| parent.clone_boxed());
        let restart_factory = factory.clone();
        let restart_path = path.to_owned();
        let restart_config = config.clone();
        let respawn: Respawn = Arc::new(move |system, entry, old_mailbox, control, group| {
            let lifetime = restart_lifetime.clone();
            let factory = restart_factory.clone();
            let path = restart_path.clone();
            let config = restart_config.clone();
            let parent = restart_parent.as_ref().map(|parent| parent.clone_boxed());
            Box::pin(async move {
                if let Some(group) = group {
                    return system
                        .rebuild_group(
                            factory,
                            path,
                            parent,
                            config,
                            entry,
                            GroupPrevious::Generation(old_mailbox),
                            lifetime,
                            group,
                        )
                        .await
                        .map(|_| ())
                        .map_err(respawn_error);
                }
                factory
                    .construct(control, move |actor| {
                        system
                            .spawn_at_internal(
                                actor,
                                &path,
                                parent,
                                config,
                                Some(entry),
                                Some(Replacement::Generation(&old_mailbox)),
                                lifetime,
                                None,
                            )
                            .map(|_| ())
                    })
                    .await
                    .map_err(SpawnError::from)
                    .map_err(respawn_error)
            })
        });
        let address = Arc::new(Mutex::new(Weak::<MpscMailbox>::new() as WeakMailboxRef));
        let watches = ThreadWatches::dynamic(&address);
        let supervision = Arc::new(SupervisionEntry {
            stop_request: Default::default(),
            watches: watches.clone(),
            strategy: config
                .supervisor_strategy
                .clone()
                .unwrap_or_else(|| self.config().default_supervisor_strategy.clone()),
            restarts: Mutex::new(Vec::new()),
            mailbox: address.clone(),
            respawn,
            factory: factory.clone(),
        });
        let reference = ThreadActorRef::<A>::new(
            ActorPath::for_test(path),
            Weak::<MpscMailbox>::new() as WeakMailboxRef,
            config
                .backpressure_strategy
                .clone()
                .unwrap_or_else(|| self.config().default_backpressure_strategy.clone()),
            config
                .ask_timeout
                .unwrap_or(self.config().default_ask_timeout),
            None,
        )
        .with_system(Arc::downgrade(self))
        .with_mailbox_cell(address)
        .with_stop_request(supervision.stop_request.clone())
        .with_death_watch(watches.common.clone());
        let timeout = self.config().shutdown_timeout;
        let deadline = tokio::time::Instant::now() + timeout;
        let pending = Arc::new(PendingFactory {
            reference: Arc::from(reference.clone_boxed()),
            supervision: supervision.clone(),
            control: Arc::new(RestartControl::with_deadline(deadline)),
            finished: watch::channel(None).0,
            retired: watch::channel(None).0,
            lifetime,
        });
        let admitted = {
            let mut registry = self.registry.write().unwrap();
            if self.is_shutting_down() {
                Err(SpawnError::SystemShutdown)
            } else if config
                .message_batch_size
                .is_some_and(|n| !(1..=256).contains(&n))
            {
                Err(SpawnError::InvalidConfig(
                    "message_batch_size must be in 1..=256".into(),
                ))
            } else if registry.contains_key(path) {
                Err(SpawnError::ActorPathAlreadyExists(path.into()))
            } else if self.self_weak().is_none() {
                Err(SpawnError::SchedulerError(
                    "System self reference not initialized".into(),
                ))
            } else {
                registry.insert(
                    path.to_owned(),
                    ActorRegistryEntry::Pending(pending.clone()),
                );
                Ok(())
            }
        };
        admitted?;
        let recovery_system = Arc::downgrade(self);
        let recovery_path = path.to_owned();
        let recovery_address = supervision.mailbox.clone();
        pending.lifetime.bind_recovery(Arc::new(move |group| {
            let Some(system) = recovery_system.upgrade() else {
                group.abort("Thread system disappeared");
                return;
            };
            system.request_group_retirement(&recovery_path, &recovery_address, group);
        }));
        let weak = Arc::downgrade(self);
        let cancel = CancelSpawn {
            system: weak.clone(),
            path: path.into(),
            pending: Some(pending.clone()),
        };
        let registry = self.registry.clone();
        let path = path.to_owned();
        let cancel_system = weak.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let (activate, activation) = tokio::sync::oneshot::channel();
        self.runtime_handle.spawn(parrot_api::trace::capture(async move {
            // Parent ownership is installed before any constructor can run.
            if activation.await.is_err() { pending.control.cancel(); }
            let mut send = Some(send);
            let work = async {
                let publish_pending = pending.clone();
                let publish_path = path.clone();
                let next_parent = parent.as_ref().map(|p| p.clone_boxed());
                let next_config = config.clone();
                let next_supervision = supervision.clone();
                let publish_system = weak.clone();
                let mut result = factory.construct(pending.control.clone(), move |actor| {
                    let system = publish_system.upgrade().ok_or(SpawnError::SystemShutdown)?;
                    system.spawn_at_internal(actor, &publish_path, next_parent, next_config, Some(next_supervision), Some(Replacement::Pending(&publish_pending)), publish_pending.lifetime.clone(), None)
                }).await.map_err(SpawnError::from);
                if matches!(result, Err(SpawnError::Cancelled)) && let Some(group) = pending.lifetime.recovery() {
                    result = match weak.upgrade() {
                        Some(system) => system.rebuild_group(factory.clone(), path.clone(), parent, config, supervision, GroupPrevious::Pending(pending.clone()), pending.lifetime.clone(), group).await,
                        None => Err(SpawnError::SystemShutdown),
                    };
                }
                if result.is_ok() { return (result, None); }
                let policy = pending.lifetime.dispose_policy().await.map_err(SystemError::policy_disposal);
                let disposal = factory.dispose().await.and(policy).map_err(crate::error_receipt::SystemReceipt::new);
                let retirement = match (&result, &disposal) {
                    (_, Err(error)) => Err(error.actor_receipt()),
                    (Err(SpawnError::DisposalFailed(message)), _) => Err(crate::error_receipt::ActorReceipt::from_error(parrot_api::errors::ActorError::InternalError(message.clone()))),
                    _ => Ok(()),
                };
                let status = match (&result, disposal) {
                    (_, Err(error)) => Err(error.into_error()),
                    (Err(SpawnError::SystemShutdown | SpawnError::Cancelled), Ok(())) => Ok(()),
                    (Err(error), Ok(())) => Err(SystemError::ActorCreationError(error.to_string())),
                    (Ok(_), _) => unreachable!(),
                };
                watches.close();
                let reason = match &status {
                    Ok(()) => parrot_api::supervisor::DeathReason::Normal,
                    Err(error) => parrot_api::supervisor::DeathReason::Failed(crate::error_receipt::system_summary(error)),
                };
                let event = parrot_api::context::ActorTerminated { generation: watches.common.id(), path: path.clone(), reason: reason.clone() };
                {
                    let mut registry = registry.write().unwrap();
                    if matches!(registry.get(&path), Some(ActorRegistryEntry::Pending(current)) if Arc::ptr_eq(current, &pending)) { registry.remove(&path); }
                }
                let notification = pending.lifetime.finish(retirement);
                let observed = watches.clone();
                let observed_path = path.clone();
                let notification: BoxedFuture<'static, ()> = Box::pin(async move {
                    let _ = tokio::join!(
                        observed.common.terminated(event, timeout),
                        super::death_watch::notify_legacy(&observed.legacy, &observed_path, reason, timeout),
                        notification,
                    );
                });
                let outcome = StopOutcome::from_result(status);
                pending.retired.send_replace(Some(outcome.clone()));
                if pending.finished.borrow().is_none() {
                    let public = if tokio::time::Instant::now() >= deadline { StopOutcome::TimedOut(format!("factory {path} exceeded creation deadline")) } else { outcome };
                    pending.finished.send_replace(Some(public));
                }
                let result = if tokio::time::Instant::now() >= deadline {
                    Err(SpawnError::Timeout(format!("factory {path} exceeded creation deadline")))
                } else { result };
                (result, Some(notification))
            };
            tokio::pin!(work);
            let (result, notification) = tokio::select! {
                biased;
                result = &mut work => result,
                _ = tokio::time::sleep_until(deadline) => {
                    pending.control.cancel();
                    if let Some(system) = cancel_system.upgrade() { let _ = system.request_stop_ref(&path, &pending.supervision.mailbox); }
                    pending.finished.send_replace(Some(StopOutcome::TimedOut(format!("factory {path} exceeded creation deadline"))));
                    if let Some(send) = send.take() { let _ = send.send(Err(SpawnError::Timeout(format!("factory {path} exceeded creation deadline")))); }
                    work.await
                }
            };
            if result.is_ok() {
                if pending.finished.borrow().is_none() { pending.finished.send_replace(Some(StopOutcome::Stopped)); }
                pending.retired.send_replace(Some(StopOutcome::Stopped));
            }
            if let Some(send) = send { let _ = send.send(result); }
            if let Some(notification) = notification { notification.await; }
        }));
        Ok(FactoryTicket {
            reference,
            receive,
            activate: Some(activate),
            cancel,
        })
    }
}

struct FactoryTicket<A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static> {
    reference: ThreadActorRef<A>,
    receive: tokio::sync::oneshot::Receiver<Result<ThreadActorRef<A>, SpawnError>>,
    activate: Option<tokio::sync::oneshot::Sender<()>>,
    cancel: CancelSpawn,
}
impl<A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static> FactoryTicket<A> {
    async fn wait(mut self) -> Result<ThreadActorRef<A>, SpawnError> {
        if let Some(activate) = self.activate.take() {
            let _ = activate.send(());
        }
        let result = (&mut self.receive).await.map_err(|_| {
            SpawnError::SchedulerError("factory owner ended without a creation result".into())
        })?;
        self.cancel.pending = None;
        result
    }
}

trait ChildFactory: Send {
    fn spawn(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        parent: BoxedActorRef,
        scope: Arc<crate::children::ChildScope>,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'static, ActorResult<BoxedActorRef>>;
}
pub(crate) struct ErasedChildFactory(Box<dyn ChildFactory>);
struct TypedChildFactory<A>(Box<dyn Fn() -> A + Send + Sync>);
impl<A> ChildFactory for TypedChildFactory<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    fn spawn(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        parent: BoxedActorRef,
        scope: Arc<crate::children::ChildScope>,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
        Box::pin(async move {
            let factory = Factory::new(
                self.0,
                system.factory_slots.clone(),
                system.cleanup_slots.clone(),
            );
            let path = format!(
                "{}/{}",
                parent.path().trim_end_matches('/'),
                uuid::Uuid::new_v4().simple()
            );
            let mut ticket = None;
            let mut strategy = Some(strategy);
            let mut admitted_lifetime = None;
            let admitted = scope.spawn(|lifetime| {
                admitted_lifetime = Some(lifetime.clone());
                lifetime.set_strategy(strategy.take().expect("one admission"));
                let pending = system
                    .start_owned_factory(
                        factory.clone(),
                        &path,
                        ThreadActorConfig {
                            supervisor_strategy: Some(super::config::SupervisorStrategy::Stop),
                            ..Default::default()
                        },
                        Some(parent),
                        lifetime.clone(),
                    )
                    .map_err(parrot_api::errors::ActorError::from)?;
                let child = pending.reference.clone_boxed();
                let address = pending.reference.mailbox_cell();
                let weak = Arc::downgrade(&system);
                let stop: crate::children::StopChild = Arc::new(move || {
                    if let Some(system) = weak.upgrade() {
                        let _ = system.request_stop_ref(&path, &address);
                    }
                    let lifetime = lifetime.clone();
                    Box::pin(async move { lifetime.wait().await })
                });
                ticket = Some(pending);
                Ok((child, stop))
            });
            if let Err(error) = admitted {
                // Even rejection before scope admission may own arbitrary
                // factory and decider captures. Dispose outside the topology
                // lock and retain that work if the caller stops waiting.
                let policy = crate::supervision_policy::PolicyStore::new(
                    strategy.flatten(),
                    system.cleanup_slots.clone(),
                );
                system
                    .runtime_handle
                    .spawn(async move {
                        let lifetime = match admitted_lifetime {
                            Some(lifetime) => lifetime.dispose_policy().await,
                            None => Ok(()),
                        };
                        let policy = policy.dispose().await;
                        let factory = factory
                            .dispose()
                            .await
                            .map_err(parrot_api::errors::ActorError::from);
                        factory.and(policy).and(lifetime)
                    })
                    .await
                    .map_err(|e| parrot_api::errors::ActorError::InternalError(e.to_string()))??;
                return Err(error);
            }
            // Waiting never holds the topology lock. Cancellation revokes this
            // logical child while its independent owner retains cleanup.
            ticket
                .expect("accepted child has a factory ticket")
                .wait()
                .await
                .map(|reference| reference.clone_boxed())
                .map_err(parrot_api::errors::ActorError::from)
        })
    }
}
impl<A> parrot_api::context::ActorFactoryContext<A> for ThreadContext<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    fn wrap_factory<F: Fn() -> A + Send + Sync + 'static>(factory: F) -> BoxedMessage {
        Box::new(ErasedChildFactory(Box::new(TypedChildFactory(Box::new(
            factory,
        )))))
    }
}
impl ErasedChildFactory {
    pub(crate) fn spawn(
        self,
        system: Arc<ThreadActorSystem>,
        parent: BoxedActorRef,
        scope: Arc<crate::children::ChildScope>,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
        self.0.spawn(system, parent, scope, strategy)
    }
}

fn respawn_error(error: SpawnError) -> SystemError {
    match error {
        SpawnError::SystemShutdown | SpawnError::Cancelled => SystemError::ShuttingDown,
        SpawnError::DisposalFailed(message) => SystemError::ShutdownError(message),
        error => SystemError::ActorCreationError(error.to_string()),
    }
}
enum GroupPrevious {
    Generation(Arc<dyn Mailbox>),
    Pending(Arc<PendingFactory>),
}
impl ThreadActorSystem {
    fn request_group_retirement(
        self: &Arc<Self>,
        path: &str,
        address: &Arc<Mutex<WeakMailboxRef>>,
        group: Arc<crate::children::recovery::Recovery>,
    ) {
        let expected = {
            let registry = self.registry.read().unwrap();
            match registry.get(path) {
                Some(ActorRegistryEntry::Pending(entry))
                    if Arc::ptr_eq(address, &entry.supervision.mailbox) =>
                {
                    if entry.lifetime.is_stopping() {
                        group.abort("Thread member already stopping");
                    }
                    entry.control.cancel();
                    return;
                }
                Some(ActorRegistryEntry::Dynamic(entry))
                    if entry
                        .supervision
                        .as_ref()
                        .is_some_and(|supervision| Arc::ptr_eq(address, &supervision.mailbox)) =>
                {
                    if entry.lifetime.is_stopping() {
                        group.abort("Thread member already stopping");
                        return;
                    }
                    if entry.stop_completion.is_some() {
                        return;
                    }
                    Arc::downgrade(&entry.mailbox)
                }
                _ => {
                    group.abort("Thread recovery member disappeared");
                    return;
                }
            }
        };
        let system = self.clone();
        let path = path.to_owned();
        self.runtime_handle.spawn(async move {
            system
                .retire_generation(
                    &path,
                    expected,
                    Arc::new(parrot_api::errors::ActorError::Stopped),
                    parrot_api::supervisor::DeathReason::Normal,
                    true,
                )
                .await;
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn rebuild_group<A>(
        self: &Arc<Self>,
        factory: Arc<Factory<A>>,
        path: String,
        parent: Option<BoxedActorRef>,
        config: ThreadActorConfig,
        supervision: Arc<SupervisionEntry>,
        previous: GroupPrevious,
        lifetime: Arc<crate::children::ChildLifetime>,
        group: Arc<crate::children::recovery::Recovery>,
    ) -> Result<ThreadActorRef<A>, SpawnError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        use crate::children::recovery::Step;
        let fail = |message: String| {
            if group.control.is_cancelled() {
                SpawnError::Cancelled
            } else {
                SpawnError::InitializationFailed(message)
            }
        };
        let work = async {
            lifetime
                .settle_live_decision()
                .await
                .map_err(|error| SpawnError::DisposalFailed(error.to_string()))?;
            group
                .arrive(lifetime.id, Step::Retired)
                .await
                .map_err(fail)?;
            let prepared = parrot_api::trace::in_scope(group.failure_context.clone(), || {
                factory.prepare::<SpawnError>(group.control.clone())
            })
            .await
            .map_err(SpawnError::from)?;
            if let Err(error) = group.arrive(lifetime.id, Step::Prepared).await {
                prepared
                    .discard()
                    .await
                    .map_err(SpawnError::DisposalFailed)?;
                return Err(fail(error));
            }
            let publish_system = self.clone();
            let publish_path = path.clone();
            let publish_group = group.clone();
            let publish_lifetime = lifetime.clone();
            let publish_entry = supervision.clone();
            let publish_previous = match &previous {
                GroupPrevious::Generation(mailbox) => GroupPrevious::Generation(mailbox.clone()),
                GroupPrevious::Pending(pending) => GroupPrevious::Pending(pending.clone()),
            };
            let mode = config
                .scheduling_mode
                .clone()
                .unwrap_or_else(|| self.config().default_scheduling_mode.clone());
            let reference = prepared
                .publish(move |actor| {
                    let replacing = match &publish_previous {
                        GroupPrevious::Generation(mailbox) => Replacement::Generation(mailbox),
                        GroupPrevious::Pending(pending) => Replacement::Pending(pending),
                    };
                    publish_system.spawn_at_internal(
                        actor,
                        &publish_path,
                        parent,
                        config,
                        Some(publish_entry),
                        Some(replacing),
                        publish_lifetime,
                        Some(publish_group),
                    )
                })
                .await
                .map_err(SpawnError::from)?;
            let published: Arc<dyn Mailbox> = supervision
                .mailbox
                .lock()
                .unwrap()
                .upgrade()
                .expect("published mailbox retained by registry");
            let transition = self
                .registry
                .read()
                .unwrap()
                .get(&path)
                .and_then(ActorRegistryEntry::dynamic)
                .and_then(|entry| entry.restart_cancel.clone());
            let weak_mailbox = Arc::downgrade(&published);
            let weak_system = Arc::downgrade(self);
            let commit_path = path.clone();
            let commit_lifetime = lifetime.clone();
            let commit_address = supervision.mailbox.clone();
            group.on_activation(Box::new(move || {
                let Some(system) = weak_system.upgrade() else {
                    return;
                };
                {
                    let mut registry = system.registry.write().unwrap();
                    if let Some(entry) = registry
                        .get_mut(&commit_path)
                        .and_then(ActorRegistryEntry::dynamic_mut)
                        && entry
                            .restart_cancel
                            .as_ref()
                            .zip(transition.as_ref())
                            .is_some_and(|(current, expected)| Arc::ptr_eq(current, expected))
                        && weak_mailbox
                            .upgrade()
                            .is_some_and(|mailbox| Arc::ptr_eq(&mailbox, &entry.mailbox))
                    {
                        entry.stop_completion = None;
                        entry.retirement_completion = None;
                        entry.restart_cancel = None;
                    }
                }
                // A stop racing after the commit vote must still own the new
                // generation; it cannot disappear with the old owner's receipt.
                if commit_lifetime.is_stopping() {
                    let _ = system.request_stop_ref(&commit_path, &commit_address);
                }
            }));
            match group.arrive(lifetime.id, Step::Published).await {
                Ok(()) => Ok(reference),
                Err(error) => {
                    let disposed = self
                        .retire_panicked_actor(&path, published.clone(), mode, group.deadline)
                        .await;
                    let mut registry = self.registry.write().unwrap();
                    if registry
                        .get(&path)
                        .and_then(ActorRegistryEntry::dynamic)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.mailbox, &published))
                    {
                        match &previous {
                            GroupPrevious::Generation(old) => {
                                let entry = registry
                                    .get_mut(&path)
                                    .and_then(ActorRegistryEntry::dynamic_mut)
                                    .unwrap();
                                entry.mailbox = old.clone();
                                *supervision.mailbox.lock().unwrap() =
                                    Arc::downgrade(old) as WeakMailboxRef;
                            }
                            GroupPrevious::Pending(pending) => {
                                registry.insert(
                                    path.clone(),
                                    ActorRegistryEntry::Pending(pending.clone()),
                                );
                                *supervision.mailbox.lock().unwrap() =
                                    Weak::<MpscMailbox>::new() as WeakMailboxRef;
                            }
                        }
                    }
                    disposed.map_err(|e| SpawnError::DisposalFailed(e.to_string()))?;
                    Err(fail(error))
                }
            }
        };
        tokio::pin!(work);
        let result = tokio::select! {
            biased;
            result = &mut work => result,
            _ = tokio::time::sleep_until(group.deadline) => {
                group.abort("Thread cohort recovery deadline elapsed");
                work.await
            },
        };
        if let Err(error) = &result {
            group.abort(error.to_string());
        }
        result
    }
}

impl parrot_api::engine::AsyncFactoryRuntime for ThreadActorSystem {
    fn spawn_factory<'a>(
        &'a self,
        factory: parrot_api::engine::RuntimeFactory,
        options: parrot_api::engine::FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, parrot_api::system::SystemError>> {
        use parrot_api::system::SystemError;
        let factory = crate::owned_payload::OwnedPayload::new(
            factory,
            &self.runtime_handle,
            &self.cleanup_slots,
        );
        Box::pin(parrot_api::trace::capture(async move {
            let admission = options.validate().and_then(|()| {
                self.self_arc().ok_or_else(|| {
                    SystemError::InitializationError(
                        "ThreadActorSystem requires its managed Arc identity".into(),
                    )
                })
            });
            let system = match admission {
                Ok(system) => system,
                Err(error) => {
                    factory.retire().await.map_err(SystemError::ActorError)?;
                    return Err(error);
                }
            };
            let constructor = factory.take();
            let path = options
                .spawn
                .path
                .unwrap_or_else(|| format!("/runtime/{}", uuid::Uuid::new_v4().simple()));
            let reference = system
                .spawn_owned_factory(
                    move || super::system::BoxedThreadActor::new(constructor()),
                    &path,
                    ThreadActorConfig {
                        supervisor_strategy: Some(super::config::SupervisorStrategy::Restart {
                            max_retries: options.max_restarts as usize,
                            within: options.within,
                        }),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| SystemError::ActorCreationError(e.to_string()))?;
            Ok(Box::new(reference) as BoxedActorRef)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    struct Gate {
        entered: tokio::sync::Notify,
        open: Mutex<bool>,
        changed: Condvar,
    }
    impl Gate {
        fn block(&self) {
            self.entered.notify_one();
            let (_guard, timeout) = self
                .changed
                .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(3), |open| {
                    !*open
                })
                .unwrap();
            assert!(!timeout.timed_out());
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
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[tokio::test]
    async fn capture_retirement_waits_for_publication_to_release_its_function_clone() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let captured = Capture(dropped.clone());
        let factory = Factory::new(
            move || {
                let _ = &captured;
            },
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
        );
        let gate = Arc::new(Gate {
            entered: Default::default(),
            open: Mutex::new(false),
            changed: Condvar::new(),
        });
        let _release = Release(gate.clone());
        let publisher = gate.clone();
        let owner = factory.clone();
        let constructor = tokio::spawn(async move {
            owner
                .construct(Arc::new(RestartControl::default()), move |_| {
                    publisher.block();
                    Ok::<(), SpawnError>(())
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        let disposal = factory.dispose();
        tokio::pin!(disposal);
        assert!(futures::poll!(&mut disposal).is_pending());
        let early = tokio::time::timeout(Duration::from_millis(50), &mut disposal)
            .await
            .is_ok();
        let before = dropped.load(Ordering::SeqCst);
        gate.release();
        constructor.await.unwrap().unwrap();
        disposal.await.unwrap();
        assert!(!early, "capture receipt preceded active publication");
        assert_eq!(before, 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
