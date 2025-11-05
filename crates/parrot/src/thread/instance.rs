//! Instance creation owns configuration retirement before publishing runnable state.
use super::{
    address::ThreadActorRef,
    config::ThreadActorConfig,
    context::ThreadContext,
    death_watch::ThreadWatches,
    error::{SpawnError, SystemError},
    mailbox::{WeakMailboxRef, mpsc::MpscMailbox},
    system::{ActorRegistryEntry, StopOutcome, ThreadActorSystem},
};
use crate::constructor::ConstructionControl;
use crate::owned_payload::OwnedPayload;
use parrot_api::{actor::Actor, address::ActorRef};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{oneshot, watch};

type CreationSender = oneshot::Sender<Result<(), SpawnError>>;
enum CreationState {
    Waiting(CreationSender),
    Succeeded,
    Finished,
}
/// Publication and the deadline owner compete for one creation result. Taking
/// the sender commits the outcome; notification occurs after releasing the lock.
struct CreationReply(Mutex<CreationState>);
impl CreationReply {
    fn new(sender: CreationSender) -> Self {
        Self(Mutex::new(CreationState::Waiting(sender)))
    }
    fn take_success(&self, control: &ConstructionControl) -> Option<CreationSender> {
        let mut state = self.0.lock().unwrap();
        if !matches!(*state, CreationState::Waiting(_)) || control.is_cancelled() {
            return None;
        }
        match std::mem::replace(&mut *state, CreationState::Succeeded) {
            CreationState::Waiting(sender) => Some(sender),
            _ => unreachable!("checked waiting creation result"),
        }
    }
    fn take_failure(&self) -> Option<CreationSender> {
        let mut state = self.0.lock().unwrap();
        if !matches!(*state, CreationState::Waiting(_)) {
            return None;
        }
        match std::mem::replace(&mut *state, CreationState::Finished) {
            CreationState::Waiting(sender) => Some(sender),
            _ => unreachable!("checked waiting creation result"),
        }
    }
    fn succeeded(&self) -> bool {
        matches!(*self.0.lock().unwrap(), CreationState::Succeeded)
    }
}

pub(crate) struct PendingInstance {
    pub stop_request: super::address::StopRequest,
    pub reference: Arc<dyn ActorRef>,
    pub address: Arc<Mutex<WeakMailboxRef>>,
    pub watches: ThreadWatches,
    pub control: Arc<ConstructionControl>,
    pub finished: watch::Sender<Option<StopOutcome>>,
    pub retired: watch::Sender<Option<StopOutcome>>,
    pub lifetime: Arc<crate::children::ChildLifetime>,
}
struct CancelInstance {
    system: Weak<ThreadActorSystem>,
    path: String,
    pending: Option<Arc<PendingInstance>>,
}
impl Drop for CancelInstance {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.control.cancel();
            if let Some(system) = self.system.upgrade() {
                let _ = system.request_stop_ref(&self.path, &pending.address);
            }
        }
    }
}

async fn retire_inputs<A: Send + 'static>(
    actor: OwnedPayload<A>,
    parent: OwnedPayload<Option<parrot_api::types::BoxedActorRef>>,
    engine: OwnedPayload<ThreadActorConfig>,
) -> Result<(), SpawnError> {
    let (actor, parent, engine) = tokio::join!(actor.retire(), parent.retire(), engine.retire());
    actor
        .and(parent)
        .and(engine)
        .map_err(|e| SpawnError::DisposalFailed(e.to_string()))
}

impl ThreadActorSystem {
    /// Both admission paths use the same bounded blocking publication owner.
    #[allow(clippy::too_many_arguments)]
    fn submit_instance_publication<A>(
        self: &Arc<Self>,
        actor: OwnedPayload<A>,
        parent: OwnedPayload<Option<parrot_api::types::BoxedActorRef>>,
        engine: OwnedPayload<ThreadActorConfig>,
        publication: Arc<PendingInstance>,
        name: String,
        publication_reply: Arc<CreationReply>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> tokio::task::JoinHandle<Result<(), SpawnError>>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        let system = self.clone();
        let trace = parrot_api::trace::TraceContext::current();
        self.runtime_handle.spawn_blocking(move || {
            parrot_api::trace::in_scope(trace, || {
                let result = {
                    let _permit = permit;
                    system.spawn_at_internal(
                        actor.take(),
                        &name,
                        parent.take(),
                        engine.take(),
                        None,
                        Some(super::factory::Replacement::Instance(&publication)),
                        publication.lifetime.clone(),
                        None,
                    )
                }
                .map(|_| ());
                if result.is_ok()
                    && let Some(sender) = publication_reply.take_success(&publication.control)
                {
                    publication.retired.send_replace(Some(StopOutcome::Stopped));
                    publication
                        .finished
                        .send_replace(Some(StopOutcome::Stopped));
                    let _ = sender.send(Ok(()));
                }
                result
            })
        })
    }
    pub(crate) fn spawn_owned_instance<A>(
        self: &Arc<Self>,
        actor: A,
        config: A::Config,
        engine: ThreadActorConfig,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + '_
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        self.spawn_owned_instance_at(actor, config, engine, None)
    }

    pub(crate) fn spawn_owned_instance_at<A>(
        self: &Arc<Self>,
        actor: A,
        config: A::Config,
        engine: ThreadActorConfig,
        path: Option<String>,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + '_
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        self.spawn_owned_instance_with_parent(actor, Some(config), engine, path, None)
    }

    pub(crate) fn spawn_owned_instance_with_parent<A>(
        self: &Arc<Self>,
        actor: A,
        config: Option<A::Config>,
        engine: ThreadActorConfig,
        path: Option<String>,
        parent: Option<parrot_api::types::BoxedActorRef>,
    ) -> impl std::future::Future<Output = Result<ThreadActorRef<A>, SpawnError>> + Send + '_
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        let actor = OwnedPayload::new(actor, &self.runtime_handle, &self.cleanup_slots);
        let config = OwnedPayload::new(config, &self.runtime_handle, &self.cleanup_slots);
        let engine = OwnedPayload::new(engine, &self.runtime_handle, &self.cleanup_slots);
        let parent = OwnedPayload::new(parent, &self.runtime_handle, &self.cleanup_slots);
        parrot_api::trace::capture(async move {
            let path = path.unwrap_or_else(|| format!("/user/{}", uuid::Uuid::new_v4().simple()));
            let address = Arc::new(Mutex::new(Weak::<MpscMailbox>::new() as WeakMailboxRef));
            let watches = ThreadWatches::dynamic(&address);
            let reference = ThreadActorRef::<A>::registered(
                path.clone(),
                address.clone(),
                Default::default(),
                Arc::downgrade(self),
                engine
                    .value()
                    .backpressure_strategy
                    .clone()
                    .unwrap_or_else(|| self.config().default_backpressure_strategy.clone()),
                engine
                    .value()
                    .ask_timeout
                    .unwrap_or(self.config().default_ask_timeout),
            )
            .with_death_watch(watches.common.clone());
            let timeout = self.config().shutdown_timeout;
            let deadline = tokio::time::Instant::now() + timeout;
            let pending = Arc::new(PendingInstance {
                stop_request: reference.stop_request(),
                reference: Arc::new(reference.clone()),
                address,
                watches,
                control: Arc::new(ConstructionControl::with_deadline(deadline)),
                finished: watch::channel(None).0,
                retired: watch::channel(None).0,
                lifetime: crate::children::ChildLifetime::new(Weak::new()),
            });
            let admission = {
                let mut registry = self.registry.write().unwrap();
                if self.is_shutting_down() {
                    Err(SpawnError::SystemShutdown)
                } else if engine
                    .value()
                    .message_batch_size
                    .is_some_and(|n| !(1..=256).contains(&n))
                {
                    Err(SpawnError::InvalidConfig(
                        "message_batch_size must be in 1..=256".into(),
                    ))
                } else if self.self_weak().is_none() {
                    Err(SpawnError::SchedulerError(
                        "System self reference not initialized".into(),
                    ))
                } else if registry.contains_key(&path) {
                    Err(SpawnError::ActorPathAlreadyExists(path.clone()))
                } else {
                    registry.insert(path.clone(), ActorRegistryEntry::Instance(pending.clone()));
                    Ok(())
                }
            };
            if let Err(error) = admission {
                let (actor, config, parent, engine) = tokio::join!(
                    actor.retire(),
                    config.retire(),
                    parent.retire(),
                    engine.retire()
                );
                actor
                    .and(config)
                    .and(parent)
                    .and(engine)
                    .map_err(|e| SpawnError::DisposalFailed(e.to_string()))?;
                return Err(error);
            }
            let weak = Arc::downgrade(self);
            let registry = self.registry.clone();
            let mut cancel = CancelInstance {
                system: weak.clone(),
                path: path.clone(),
                pending: Some(pending.clone()),
            };
            let (send, receive) = oneshot::channel();
            let reply = Arc::new(CreationReply::new(send));
            let mut inputs = Some((actor, config, parent, engine));
            // No user destructor can run here. Reserve capacity without waiting;
            // all fallbacks retain their independently scheduled cleanup owner.
            let submitted = if !std::mem::needs_drop::<Option<A::Config>>()
                && !pending.control.is_cancelled()
            {
                self.factory_slots
                    .clone()
                    .try_acquire_owned()
                    .ok()
                    .map(|permit| {
                        let (actor, config, parent, engine) = inputs.take().unwrap();
                        drop(config);
                        self.submit_instance_publication(
                            actor,
                            parent,
                            engine,
                            pending.clone(),
                            path.clone(),
                            reply.clone(),
                            permit,
                        )
                    })
            } else {
                None
            };
            self.runtime_handle.spawn(parrot_api::trace::capture(async move {
            let work = async {
                let result = if let Some(submitted) = submitted {
                    submitted.await.map_err(|e| SpawnError::DisposalFailed(crate::owned_payload::join_error(e))).and_then(|result| result)
                } else {
                let (actor, config, parent, engine) = inputs.take().expect("deferred publication inputs");
                let config_result = config.retire().await.map_err(|e| SpawnError::DisposalFailed(e.to_string()));
                if let Err(error) = config_result {
                    // The configuration failed first; cleanup must finish but must
                    // not replace that failure with a later destructor panic.
                    let _ = retire_inputs(actor, parent, engine).await;
                    Err(error)
                } else if pending.control.is_cancelled() {
                    retire_inputs(actor, parent, engine).await.and(Err(SpawnError::Cancelled))
                } else if let Some(system) = weak.upgrade() {
                    let publication = pending.clone();
                    let name = path.clone();
                    // Bound publication/rejection work as well as configuration
                    // cleanup. Waiting for a slot never abandons owned inputs.
                    let permit = tokio::select! {
                        biased;
                        _ = pending.control.cancelled() => None,
                        permit = system.factory_slots.clone().acquire_owned() => permit.ok(),
                    };
                    if let Some(permit) = permit {
                        system.submit_instance_publication(actor, parent, engine, publication, name, reply.clone(), permit)
                            .await.map_err(|e| SpawnError::DisposalFailed(crate::owned_payload::join_error(e))).and_then(|result| result)
                    } else {
                        retire_inputs(actor, parent, engine).await.and(Err(SpawnError::Cancelled))
                    }
                } else {
                    retire_inputs(actor, parent, engine).await.and(Err(SpawnError::SystemShutdown))
                }
                };
                if result.is_ok() {
                    // Successful publication already owns the creation result.
                    // A delayed join must not expire an actor returned to its caller.
                    if reply.succeeded() { return result; }
                    if pending.control.is_cancelled() {
                        // Cancellation can race the final synchronous publication.
                        // The same logical address now identifies the live actor;
                        // retain ownership until its actual lifecycle has retired.
                        if let Some(system) = weak.upgrade() {
                            let _ = system.request_stop_ref(&path, &pending.address);
                        }
                        let retirement = pending.lifetime.wait().await;
                        let outcome = StopOutcome::from_result(retirement.as_ref().map(|_| ()).map_err(|e| SystemError::ShutdownError(e.to_string())));
                        pending.retired.send_replace(Some(outcome.clone()));
                        if pending.finished.borrow().is_none() { pending.finished.send_replace(Some(outcome)); }
                        retirement.map_err(|e| SpawnError::DisposalFailed(e.to_string()))?;
                        return Err(if tokio::time::Instant::now() >= deadline {
                            SpawnError::Timeout("instance creation exceeded deadline".into())
                        } else { SpawnError::Cancelled });
                    }
                    pending.retired.send_replace(Some(StopOutcome::Stopped));
                    pending.finished.send_replace(Some(StopOutcome::Stopped));
                    return result;
                }
                pending.watches.close();
                let disposal = pending.lifetime.dispose_policy().await;
                let result = match result {
                    // Preserve a first configuration/actor disposal failure;
                    // admission cleanup still completes and cannot overwrite it.
                    Err(error @ SpawnError::DisposalFailed(_)) => Err(error),
                    result => disposal.map_err(|e| SpawnError::DisposalFailed(e.to_string())).and(result),
                };
                let retirement = match &result { Err(SpawnError::DisposalFailed(message)) => Err(message.clone()), _ => Ok(()) };
                {
                    let mut registry = registry.write().unwrap();
                    if matches!(registry.get(&path), Some(ActorRegistryEntry::Instance(current)) if Arc::ptr_eq(current, &pending)) { registry.remove(&path); }
                }
                let notification = pending.lifetime.finish(retirement.map_err(|message| crate::error_receipt::ActorReceipt::from_error(parrot_api::errors::ActorError::InternalError(message))));
                let stopped = match &result {
                    Err(SpawnError::DisposalFailed(message)) => Err(SystemError::ShutdownError(message.clone())),
                    _ => Ok(()),
                };
                let outcome = StopOutcome::from_result(stopped);
                pending.retired.send_replace(Some(outcome.clone()));
                if pending.finished.borrow().is_none() { pending.finished.send_replace(Some(outcome)); }
                notification.await;
                result
            };
            tokio::pin!(work);
            let result = tokio::select! {
                biased;
                result = &mut work => result,
                _ = tokio::time::sleep_until(deadline) => {
                    if let Some(sender) = reply.take_failure() {
                        pending.control.cancel();
                        pending.finished.send_replace(Some(StopOutcome::TimedOut("instance configuration retirement exceeded creation deadline".into())));
                        let _ = sender.send(Err(SpawnError::Timeout("instance configuration retirement exceeded creation deadline".into())));
                    }
                    work.await
                }
            };
            let reason = result.as_ref().err().map(|error| match error {
                SpawnError::Cancelled | SpawnError::SystemShutdown => parrot_api::supervisor::DeathReason::Normal,
                SpawnError::Timeout(_) => parrot_api::supervisor::DeathReason::Killed,
                error => parrot_api::supervisor::DeathReason::Failed(error.to_string()),
            });
            if let Some(sender) = reply.take_failure() { let _ = sender.send(result); }
            if let Some(reason) = reason {
                // Closing admission is not a termination event. Notify existing
                // observers only after both unpublished inputs really retire.
                let event = parrot_api::context::ActorTerminated {
                    generation: pending.watches.common.id(), path: path.clone(), reason: reason.clone(),
                };
                let _ = tokio::join!(
                    pending.watches.common.terminated(event, timeout),
                    super::death_watch::notify_legacy(&pending.watches.legacy, &path, reason, timeout),
                );
            }
        }));
            let result = receive.await.map_err(|_| {
                SpawnError::SchedulerError("instance owner ended without creation receipt".into())
            })?;
            cancel.pending = None;
            result.map(|()| reference)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::{
        actor::ActorState,
        types::{ActorResult, BoxedFuture, BoxedMessage},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Actor for Probe {
        type Config = parrot_api::actor::EmptyConfig;
        type Context = ThreadContext<Self>;
        fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            self.0.fetch_add(100, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Ok(msg) })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    async fn waiting_publication(stop_by_reference: bool) {
        let system = ThreadActorSystem::shared(Default::default());
        let count = Arc::new(AtomicUsize::new(0));
        let permit = system
            .factory_slots
            .clone()
            .acquire_many_owned(system.factory_slots.available_permits() as u32)
            .await
            .unwrap();
        let owner = system.clone();
        let actor = Probe(count.clone());
        let caller = tokio::spawn(async move {
            owner
                .spawn_owned_instance(actor, parrot_api::actor::EmptyConfig, Default::default())
                .await
        });
        let reference =
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    let reference = system.registry.read().unwrap().values().next().and_then(
                        |entry| match entry {
                            ActorRegistryEntry::Instance(pending) => {
                                Some(pending.reference.clone())
                            }
                            _ => None,
                        },
                    );
                    if let Some(reference) = reference {
                        break reference;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        // Keep all publication permits held until retirement has completed:
        // stopping a queued creation must not depend on a publication slot.
        if stop_by_reference {
            tokio::time::timeout(std::time::Duration::from_secs(3), reference.stop())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(caller.await.unwrap(), Err(SpawnError::Cancelled)));
        } else {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while system.actor_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation alone must retire the queued instance");
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            system.shutdown_internal(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "queued actor must retire without init"
        );
        assert_eq!(system.actor_count(), 0);
        drop(permit);
    }
    #[tokio::test]
    async fn reference_stop_retires_instance_waiting_for_publication_capacity() {
        waiting_publication(true).await;
    }
    #[tokio::test]
    async fn cancelled_caller_retires_instance_waiting_for_publication_capacity() {
        waiting_publication(false).await;
    }

    #[tokio::test]
    async fn pending_instance_stop_notifies_common_and_legacy_observers_after_retirement() {
        use parrot_api::{
            context::ActorTerminated,
            death_watch::{DeathWatch, TerminationRecipient},
        };
        struct Recipient(tokio::sync::mpsc::UnboundedSender<ActorTerminated>);
        impl TerminationRecipient for Recipient {
            fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
                let result = self
                    .0
                    .send(event)
                    .map_err(|_| parrot_api::errors::ActorError::Stopped);
                Box::pin(async { result })
            }
        }
        let system = ThreadActorSystem::shared(Default::default());
        let count = Arc::new(AtomicUsize::new(0));
        let permit = system
            .factory_slots
            .clone()
            .acquire_many_owned(system.factory_slots.available_permits() as u32)
            .await
            .unwrap();
        let owner = system.clone();
        let actor = Probe(count.clone());
        let caller = tokio::spawn(async move {
            owner
                .spawn_owned_instance(actor, parrot_api::actor::EmptyConfig, Default::default())
                .await
        });
        let pending = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let pending =
                    system
                        .registry
                        .read()
                        .unwrap()
                        .values()
                        .find_map(|entry| match entry {
                            ActorRegistryEntry::Instance(pending) => Some(pending.clone()),
                            _ => None,
                        });
                if let Some(pending) = pending {
                    break pending;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let observer = DeathWatch::new(4);
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        observer.set_recipient(Arc::new(Recipient(send))).unwrap();
        pending.watches.common.watch(&observer).unwrap();
        pending.watches.legacy.watch(&observer).unwrap();
        pending.reference.stop().await.unwrap();
        assert!(matches!(caller.await.unwrap(), Err(SpawnError::Cancelled)));
        let events = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let first = receive.recv().await.unwrap();
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "termination preceded actor retirement"
            );
            let second = receive.recv().await.unwrap();
            [first, second]
        })
        .await;
        system.shutdown_internal().await.unwrap();
        drop(permit);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let events = events.expect("pending instance never notified both observers");
        for event in &events {
            assert_eq!(event.path, pending.reference.path());
        }
        let ids: std::collections::HashSet<_> =
            events.iter().map(|event| event.generation).collect();
        assert_eq!(
            ids,
            [pending.watches.common.id(), pending.watches.legacy.id()]
                .into_iter()
                .collect()
        );
        assert_eq!(observer.subscriptions(), (0, 0));
    }

    // Hold every publication slot to make caller cancellation deterministic.
    // Cleanup must use the independent cleanup pool and retain the path until
    // the policy destructor has really finished.
    async fn explicit_path_waiting_publication(abort: bool) {
        #[derive(Debug)]
        struct Policy {
            entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            dropped: Arc<AtomicUsize>,
        }
        impl super::super::mailbox::admission::MessageAdmission for Policy {
            fn prepare(
                &self,
                message: BoxedMessage,
            ) -> Result<BoxedMessage, super::super::error::MailboxError> {
                Ok(message)
            }
        }
        impl Drop for Policy {
            fn drop(&mut self) {
                self.entered
                    .get_mut()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                self.release
                    .get_mut()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                self.dropped.fetch_add(1, Ordering::SeqCst);
                panic!("queued engine policy disposal");
            }
        }
        let system = ThreadActorSystem::shared(Default::default());
        let permit = system
            .factory_slots
            .clone()
            .acquire_many_owned(system.factory_slots.available_permits() as u32)
            .await
            .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let policy_count = Arc::new(AtomicUsize::new(0));
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, receive) = std::sync::mpsc::channel();
        let engine = ThreadActorConfig {
            message_admission: Some(Arc::new(Policy {
                entered: std::sync::Mutex::new(Some(entered)),
                release: std::sync::Mutex::new(receive),
                dropped: policy_count.clone(),
            })),
            ..Default::default()
        };
        let owner = system.clone();
        let actor = Probe(count.clone());
        let caller = tokio::spawn(async move {
            owner
                .spawn_at(actor, "/queued-explicit", None, engine)
                .await
        });
        let pending = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(ActorRegistryEntry::Instance(entry)) =
                    system.registry.read().unwrap().get("/queued-explicit")
                {
                    break entry.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let stop = if abort {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            None
        } else {
            let reference = pending.reference.clone();
            Some((caller, tokio::spawn(async move { reference.stop().await })))
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(system.actor_count(), 1);
        assert!(
            pending.retired.borrow().is_none(),
            "retirement receipt preceded policy Drop"
        );
        assert_eq!(policy_count.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let retired = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            super::super::system::await_stop(pending.retired.subscribe()),
        )
        .await
        .unwrap();
        assert!(
            retired
                .unwrap_err()
                .to_string()
                .contains("queued engine policy disposal")
        );
        if let Some((caller, stop)) = stop {
            assert!(
                stop.await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("queued engine policy disposal")
            );
            assert!(matches!(
                caller.await.unwrap(),
                Err(SpawnError::DisposalFailed(_))
            ));
        }
        assert_eq!(system.actor_count(), 0);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(policy_count.load(Ordering::SeqCst), 1);
        // Publication permits remain held through real retirement.
        system.shutdown_internal().await.unwrap();
        drop(permit);
    }
    #[tokio::test]
    async fn explicit_path_cancelled_waiter_retires_engine_without_publication_capacity() {
        explicit_path_waiting_publication(true).await;
    }
    #[tokio::test]
    async fn explicit_path_stop_retires_engine_without_publication_capacity() {
        explicit_path_waiting_publication(false).await;
    }
}

#[cfg(test)]
mod direct_receipt_contracts {
    use super::*;
    use parrot_api::{
        actor::{ActorState, EmptyConfig},
        types::{ActorResult, BoxedFuture, BoxedMessage},
    };
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
        time::{Duration, Instant},
    };
    #[derive(Default)]
    struct Counts {
        init: AtomicUsize,
        dropped: AtomicUsize,
    }
    struct Echo(Arc<Counts>);
    impl Drop for Echo {
        fn drop(&mut self) {
            self.0.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Actor for Echo {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            self.0.init.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Ok(msg) })
        }
    }
    fn setup() -> (
        tokio::runtime::Runtime,
        Arc<ThreadActorSystem>,
        std::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let system = {
            let _entered = runtime.enter();
            tokio::time::pause();
            ThreadActorSystem::shared(crate::thread::config::ThreadActorSystemConfig {
                shutdown_timeout: Duration::from_secs(10),
                ..Default::default()
            })
        };
        let (release, blocked) = std::sync::mpsc::channel();
        let (entered, started) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            entered.send(()).unwrap();
            blocked
                .recv_timeout(Duration::from_secs(15))
                .expect("test must release publication executor");
        });
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        (runtime, system, release, blocker)
    }
    fn queue_mode<F: Future<Output = Result<ThreadActorRef<Echo>, SpawnError>>>(
        runtime: &tokio::runtime::Runtime,
        system: &Arc<ThreadActorSystem>,
        creation: &mut Pin<Box<F>>,
        immediate_capacity: bool,
    ) -> Weak<PendingInstance> {
        runtime.block_on(async {
            let capacity = system.factory_slots.available_permits();
            assert!(capacity > 0);
            let slots = if immediate_capacity {
                None
            } else {
                Some(
                    system
                        .factory_slots
                        .clone()
                        .acquire_many_owned(capacity as u32)
                        .await
                        .unwrap(),
                )
            };
            assert!(futures::poll!(creation.as_mut()).is_pending());
            let pending = {
                let registry = system.registry.read().unwrap();
                match registry.get("/direct-receipt").unwrap() {
                    ActorRegistryEntry::Instance(pending) => Arc::downgrade(pending),
                    _ => panic!("publication must be pending"),
                }
            };
            drop(slots);
            // Current-thread executor: once this task resumes after capacity was
            // acquired, the owner's same poll has queued spawn_blocking and yielded.
            let wall_deadline = Instant::now() + Duration::from_secs(3);
            while system.factory_slots.available_permits() == capacity {
                assert!(
                    Instant::now() < wall_deadline,
                    "owner did not queue publication"
                );
                tokio::task::yield_now().await;
            }
            assert_eq!(
                system.factory_slots.available_permits(),
                capacity - 1,
                "queued publication must retain exactly one permit"
            );
            assert!(futures::poll!(creation.as_mut()).is_pending());
            pending
        })
    }
    fn expire_without_driving(runtime: &tokio::runtime::Runtime) {
        let _entered = runtime.enter();
        let before = tokio::time::Instant::now();
        let mut advance = Box::pin(tokio::time::advance(Duration::from_secs(20)));
        // advance changes the clock before yielding; no executor task is polled.
        let _ = advance
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(tokio::time::Instant::now() - before >= Duration::from_secs(20));
        drop(advance);
        tokio::time::resume();
    }
    #[test]
    fn successful_publication_returns_without_owner_repoll_and_survives_late_deadline() {
        successful_publication_returns_without_owner_repoll_and_survives_late_deadline_mode(false);
    }
    #[test]
    fn immediate_successful_publication_returns_without_owner_repoll_and_survives_late_deadline() {
        successful_publication_returns_without_owner_repoll_and_survives_late_deadline_mode(true);
    }
    fn successful_publication_returns_without_owner_repoll_and_survives_late_deadline_mode(
        immediate_capacity: bool,
    ) {
        let (runtime, system, release, blocker) = setup();
        let counts = Arc::new(Counts::default());
        let mut creation = Box::pin(system.spawn_at(
            Echo(counts.clone()),
            "/direct-receipt",
            None,
            Default::default(),
        ));
        let pending = queue_mode(&runtime, &system, &mut creation, immediate_capacity);
        release.send(()).unwrap();
        let mut returned = None;
        let wall_deadline = Instant::now() + Duration::from_secs(3);
        {
            let _entered = runtime.enter();
            while Instant::now() < wall_deadline {
                if let Poll::Ready(result) = creation
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                {
                    returned = Some(result);
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let direct = returned.is_some();
        expire_without_driving(&runtime);
        runtime.block_on(async {
            blocker.await.unwrap();
            let result = match returned {
                Some(result) => result,
                None => creation.as_mut().await,
            };
            tokio::time::timeout(Duration::from_secs(3), async {
                while pending.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("creation owner did not retire");
            let usable = match result {
                Ok(actor) => actor
                    .send_with_timeout(Box::new(42u64), Some(Duration::from_secs(3)))
                    .await
                    .ok()
                    .and_then(|reply| reply.downcast::<u64>().ok())
                    .is_some_and(|value| *value == 42),
                Err(_) => false,
            };
            system.shutdown_internal().await.unwrap();
            assert_eq!(system.actor_count(), 0);
            assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
            assert!(
                direct,
                "published creation still requires owner executor repoll"
            );
            assert!(
                usable,
                "already successful creation was expired by a delayed owner"
            );
            assert_eq!(counts.init.load(Ordering::SeqCst), 1);
        });
    }
    #[test]
    fn queued_publication_timeout_keeps_path_and_actor_until_real_retirement() {
        queued_publication_timeout_keeps_path_and_actor_until_real_retirement_mode(false);
    }
    #[test]
    fn immediate_queued_publication_timeout_keeps_path_and_actor_until_real_retirement() {
        queued_publication_timeout_keeps_path_and_actor_until_real_retirement_mode(true);
    }
    fn queued_publication_timeout_keeps_path_and_actor_until_real_retirement_mode(
        immediate_capacity: bool,
    ) {
        let (runtime, system, release, blocker) = setup();
        let counts = Arc::new(Counts::default());
        let mut creation = Box::pin(system.spawn_at(
            Echo(counts.clone()),
            "/direct-receipt",
            None,
            Default::default(),
        ));
        let pending = queue_mode(&runtime, &system, &mut creation, immediate_capacity);
        expire_without_driving(&runtime);
        runtime.block_on(async {
            let result = tokio::time::timeout(Duration::from_secs(3), creation.as_mut())
                .await
                .unwrap();
            let timed_out = matches!(result, Err(SpawnError::Timeout(_)));
            let retained = system.actor_count() == 1
                && pending.upgrade().is_some()
                && counts.init.load(Ordering::SeqCst) == 0
                && counts.dropped.load(Ordering::SeqCst) == 0;
            release.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                while pending.upgrade().is_some() || system.actor_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("timed-out publication did not retire");
            system.shutdown_internal().await.unwrap();
            assert!(
                timed_out,
                "blocked publication must report its creation deadline"
            );
            assert!(
                retained,
                "timeout released pending ownership before actual cleanup"
            );
            assert_eq!(counts.init.load(Ordering::SeqCst), 0);
            assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
        });
    }
    #[test]
    fn dropping_unconsumed_success_still_retires_the_published_actor() {
        dropping_unconsumed_success_still_retires_the_published_actor_mode(false);
    }
    #[test]
    fn immediate_dropping_unconsumed_success_still_retires_the_published_actor() {
        dropping_unconsumed_success_still_retires_the_published_actor_mode(true);
    }
    fn dropping_unconsumed_success_still_retires_the_published_actor_mode(
        immediate_capacity: bool,
    ) {
        let (runtime, system, release, blocker) = setup();
        let counts = Arc::new(Counts::default());
        let mut creation = Box::pin(system.spawn_at(
            Echo(counts.clone()),
            "/direct-receipt",
            None,
            Default::default(),
        ));
        let pending = queue_mode(&runtime, &system, &mut creation, immediate_capacity);
        release.send(()).unwrap();
        let wall_deadline = Instant::now() + Duration::from_secs(3);
        let committed = loop {
            let committed = pending.upgrade().is_some_and(|pending| {
                matches!(&*pending.finished.borrow(), Some(StopOutcome::Stopped))
            });
            if committed || Instant::now() >= wall_deadline {
                break committed;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        // The result was committed but has never been polled by its caller.
        drop(creation);
        {
            let _entered = runtime.enter();
            tokio::time::resume();
        }
        runtime.block_on(async {
            blocker.await.unwrap();
            let retired_without_shutdown = tokio::time::timeout(Duration::from_secs(3), async {
                while pending.upgrade().is_some() || system.actor_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok();
            system.shutdown_internal().await.unwrap();
            assert!(
                committed,
                "publication did not commit without owner progress"
            );
            assert!(
                retired_without_shutdown,
                "dropping an unconsumed success lost published actor retirement"
            );
            assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
            assert!(counts.init.load(Ordering::SeqCst) <= 1);
        });
    }
}

#[cfg(test)]
mod creation_reply_contracts {
    use super::*;
    #[test]
    fn success_cannot_be_replaced_by_late_timeout_or_error() {
        let (send, mut receive) = oneshot::channel();
        let reply = CreationReply::new(send);
        let sender = reply.take_success(&ConstructionControl::default()).unwrap();
        assert!(reply.take_failure().is_none());
        assert!(
            reply
                .take_success(&ConstructionControl::default())
                .is_none()
        );
        sender.send(Ok(())).unwrap();
        receive.try_recv().unwrap().unwrap();
    }
    #[test]
    fn cancelled_and_expired_creations_cannot_claim_success() {
        let cancelled = ConstructionControl::default();
        cancelled.cancel();
        let expired = ConstructionControl::with_deadline(
            tokio::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        for control in [cancelled, expired] {
            let (send, mut receive) = oneshot::channel();
            let reply = CreationReply::new(send);
            assert!(reply.take_success(&control).is_none());
            assert!(matches!(
                receive.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            reply
                .take_failure()
                .unwrap()
                .send(Err(SpawnError::Cancelled))
                .unwrap();
            assert!(matches!(
                receive.try_recv().unwrap(),
                Err(SpawnError::Cancelled)
            ));
        }
    }
    #[test]
    fn failure_cannot_be_overwritten_by_success() {
        let (send, mut receive) = oneshot::channel();
        let reply = CreationReply::new(send);
        let failure = reply.take_failure().unwrap();
        assert!(
            reply
                .take_success(&ConstructionControl::default())
                .is_none()
        );
        failure
            .send(Err(SpawnError::Timeout("creation deadline".into())))
            .unwrap();
        assert!(matches!(
            receive.try_recv().unwrap(),
            Err(SpawnError::Timeout(_))
        ));
    }
    #[test]
    fn competing_terminal_owners_send_exactly_one_result() {
        for _ in 0..64 {
            let (send, mut receive) = oneshot::channel();
            let reply = CreationReply::new(send);
            let barrier = std::sync::Barrier::new(2);
            let winners = std::thread::scope(|scope| {
                let success = scope.spawn(|| {
                    barrier.wait();
                    if let Some(sender) = reply.take_success(&ConstructionControl::default()) {
                        sender.send(Ok(())).unwrap();
                        1
                    } else {
                        0
                    }
                });
                let failure = scope.spawn(|| {
                    barrier.wait();
                    if let Some(sender) = reply.take_failure() {
                        sender
                            .send(Err(SpawnError::Timeout("contended deadline".into())))
                            .unwrap();
                        1
                    } else {
                        0
                    }
                });
                success.join().unwrap() + failure.join().unwrap()
            });
            assert_eq!(winners, 1);
            let result = receive.try_recv().unwrap();
            assert!(result.is_ok() || matches!(result, Err(SpawnError::Timeout(_))));
            assert!(reply.take_failure().is_none());
        }
    }
}
