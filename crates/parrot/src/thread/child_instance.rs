//! Child instance admission precedes validation, input retirement and publication.
#[cfg(test)]
#[path = "child_instance_trace_tests.rs"]
mod initialization_trace_contracts;

use super::{
    address::ThreadActorRef,
    config::ThreadActorConfig,
    context::ThreadContext,
    death_watch::ThreadWatches,
    error::{SpawnError, SystemError},
    instance::PendingInstance,
    mailbox::{WeakMailboxRef, mpsc::MpscMailbox},
    system::{
        ActorRegistryEntry, ChildSpawnReceipt, ErasedSpawnBox, StopOutcome, ThreadActorSystem,
    },
};
use crate::{constructor::ConstructionControl, owned_payload::OwnedPayload};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    errors::ActorError,
    supervisor::{DeathReason, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{oneshot, watch};

// Only supplies the erased logical reference's marker; never scheduled.
struct PendingChild;
impl Actor for PendingChild {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Starting
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Err(ActorError::Stopped) })
    }
}
struct CancelChild {
    system: Weak<ThreadActorSystem>,
    pending: Arc<PendingInstance>,
    armed: bool,
}
impl Drop for CancelChild {
    fn drop(&mut self) {
        if self.armed {
            self.pending.control.cancel();
            if let Some(system) = self.system.upgrade() {
                let _ =
                    system.request_stop_ref(&self.pending.reference.path(), &self.pending.address);
            }
        }
    }
}
fn failure(error: impl std::fmt::Display) -> SpawnError {
    SpawnError::DisposalFailed(error.to_string())
}

impl ThreadActorSystem {
    pub(crate) async fn spawn_child_erased(
        self: &Arc<Self>,
        actor: BoxedMessage,
        config: BoxedMessage,
        strategy: Option<SupervisorStrategyType>,
        parent: BoxedActorRef,
        scope: &Arc<crate::children::ChildScope>,
    ) -> ActorResult<BoxedActorRef> {
        let actor = OwnedPayload::new(actor, &self.runtime_handle, &self.cleanup_slots);
        let config = OwnedPayload::new(config, &self.runtime_handle, &self.cleanup_slots);
        let mut strategy = Some(OwnedPayload::new(
            strategy,
            &self.runtime_handle,
            &self.cleanup_slots,
        ));
        let path = format!(
            "{}/{}",
            parent.path().trim_end_matches('/'),
            uuid::Uuid::new_v4().simple()
        );
        let address = Arc::new(Mutex::new(Weak::<MpscMailbox>::new() as WeakMailboxRef));
        let watches = ThreadWatches::dynamic(&address);
        let reference = ThreadActorRef::<PendingChild>::new(
            ActorPath::placeholder(&path),
            Weak::<MpscMailbox>::new() as WeakMailboxRef,
            self.config().default_backpressure_strategy.clone(),
            self.config().default_ask_timeout,
            None,
        )
        .with_system(Arc::downgrade(self))
        .with_mailbox_cell(address.clone())
        .with_death_watch(watches.common.clone());
        let timeout = self.config().shutdown_timeout;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut pending = None;
        let admitted = scope.spawn(|lifetime| {
            // No user callback/destructor executes under the topology lock.
            let entry = Arc::new(PendingInstance {
                stop_request: reference.stop_request(),
                reference: Arc::from(reference.clone_boxed()),
                address: address.clone(),
                watches: watches.clone(),
                control: Arc::new(ConstructionControl::with_deadline(deadline)),
                finished: watch::channel(None).0,
                retired: watch::channel(None).0,
                lifetime: lifetime.clone(),
            });
            {
                let mut registry = self.registry.write().unwrap();
                if self.is_shutting_down() {
                    return Err(ActorError::Stopped);
                }
                registry.insert(path.clone(), ActorRegistryEntry::Instance(entry.clone()));
            }
            lifetime.set_strategy(strategy.take().expect("single admission").take());
            lifetime.hold_publication();
            let weak = Arc::downgrade(self);
            let stopped = entry.clone();
            let stop: crate::children::StopChild = Arc::new(move || {
                stopped.control.cancel();
                if let Some(system) = weak.upgrade() {
                    let _ = system.request_stop_ref(&stopped.reference.path(), &stopped.address);
                }
                let lifetime = stopped.lifetime.clone();
                let retired = stopped.retired.subscribe();
                Box::pin(async move {
                    let creation = super::system::await_stop(retired)
                        .await
                        .map_err(ActorError::from);
                    let lifetime = lifetime.wait().await;
                    creation.and(lifetime)
                })
            });
            pending = Some(entry);
            Ok((reference.clone_boxed(), stop))
        });
        if let Err(error) = admitted {
            let (actor, config, strategy) = tokio::join!(actor.retire(), config.retire(), async {
                match strategy {
                    Some(strategy) => strategy.retire().await,
                    None => Ok(()),
                }
            });
            actor.and(config).and(strategy)?;
            return Err(error);
        }
        let pending = pending.expect("accepted child has a lifecycle");
        let mut cancel = CancelChild {
            system: Arc::downgrade(self),
            pending: pending.clone(),
            armed: true,
        };
        let (send, receive) = oneshot::channel();
        let system = self.clone();
        let owner = async move {
            let mut send = Some(send);
            let published = Arc::new(AtomicBool::new(false));
            let mut child_notification: Option<BoxedFuture<'static, ()>> = None;
            let result = {
                let work = async {
                    let result = async {
                    let validation_slots = system.factory_slots.clone();
                    let permit = tokio::select! {
                        biased;
                        _ = pending.control.cancelled() => None,
                        permit = validation_slots.acquire_owned() => permit.ok(),
                    };
                    // Validation is user code for a custom erased bridge. Move both
                    // protected inputs back out of the worker even when it panics.
                    let (actor, config, validation) = if let Some(permit) = permit {
                        let actor_retired = actor.receipt();
                        let config_retired = config.receipt();
                        let trace = parrot_api::trace::TraceContext::current();
                        match tokio::task::spawn_blocking(move || {
                            parrot_api::trace::in_scope(trace, || {
                            let _permit = permit;
                            let validation =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    let payload = actor
                                        .value()
                                        .downcast_ref::<ErasedSpawnBox>()
                                        .ok_or_else(|| {
                                            SpawnError::InvalidConfig(
                                                "Spawn payload is not a boxed spawnable actor"
                                                    .into(),
                                            )
                                        })?;
                                    if !payload.0.accepts_child_config(config.value()) {
                                        return Err(SpawnError::InvalidConfig(
                                            "child configuration type mismatch".into(),
                                        ));
                                    }
                                    Ok(())
                                }))
                                .unwrap_or_else(|panic| {
                                    Err(failure(crate::owned_payload::panic_message(panic)))
                                });
                            (actor, config, validation)
                            })
                        })
                        .await
                        {
                            Ok(inputs) => inputs,
                            Err(error) => {
                                let error = failure(crate::owned_payload::join_error(error));
                                let (actor, config) = tokio::join!(actor_retired, config_retired);
                                return actor.and(config).map_err(failure).and(Err(error));
                            }
                        }
                    } else {
                        (actor, config, Err(SpawnError::Cancelled))
                    };
                    let config_result = config.retire().await.map_err(failure);
                    let ready = config_result.and(validation);
                    if let Err(error) = ready {
                        actor.retire().await.map_err(failure).and(Err(error))
                    } else {
                        let permit = tokio::select! {
                            biased;
                            _ = pending.control.cancelled() => None,
                            permit = system.factory_slots.clone().acquire_owned() => permit.ok(),
                        };
                        if let Some(permit) = permit {
                            let publication = pending.clone();
                            let committed = published.clone();
                            let owner = system.clone();
                            let retired = actor.receipt();
                            let trace = parrot_api::trace::TraceContext::current();
                            let published = tokio::task::spawn_blocking(move || {
                                parrot_api::trace::in_scope(trace, || {
                                let _permit = permit;
                                let payload = actor
                                    .take()
                                    .downcast::<ErasedSpawnBox>()
                                    .expect("validated spawn payload");
                                payload
                                    .0
                                    .spawn_child_on(
                                        owner,
                                        ThreadActorConfig::default(),
                                        parent,
                                        ChildSpawnReceipt(publication, committed),
                                    )
                                    .map_err(|error| {
                                        SpawnError::InitializationFailed(error.to_string())
                                    })
                                })
                            })
                            .await
                            .map_err(|error| failure(crate::owned_payload::join_error(error)))
                            .and_then(|result| result);
                            retired.await.map_err(failure).and(published)
                        } else {
                            actor
                                .retire()
                                .await
                                .map_err(failure)
                                .and(Err(SpawnError::Cancelled))
                        }
                    }
                }
                .await;
                    // A custom bridge can publish successfully and then fail while
                    // retiring its own captures. Failure must still stop that live
                    // generation, never finish its lifetime as an unpublished input.
                    if let Ok(reference) = &result {
                        pending.lifetime.publish_reference(reference.clone_boxed());
                    }
                    let published = published.load(Ordering::Acquire);
                    let publication_result = if published {
                        result.as_ref().map(|_| ()).map_err(ToString::to_string)
                    } else {
                        Ok(())
                    };
                    pending
                        .lifetime
                        .release_publication(publication_result.map_err(|message| {
                            crate::error_receipt::ActorReceipt::from_error(
                                parrot_api::errors::ActorError::InternalError(message),
                            )
                        }));
                    if result.is_err() && published {
                        let _ = system.request_stop_ref(&path, &pending.address);
                        let result = pending.lifetime.wait().await.map_err(failure).and(result);
                        let outcome = StopOutcome::from_result(Err(SystemError::ShutdownError(
                            result.as_ref().expect_err("creation failed").to_string(),
                        )));
                        pending.retired.send_replace(Some(outcome.clone()));
                        if pending.finished.borrow().is_none() {
                            pending.finished.send_replace(Some(outcome));
                        }
                        return result;
                    }
                    if result.is_ok() {
                        if pending.control.is_cancelled() {
                            let _ = system.request_stop_ref(&path, &pending.address);
                            let retirement = pending.lifetime.wait().await;
                            let outcome =
                                StopOutcome::from_result(retirement.as_ref().map(|_| ()).map_err(
                                    |error| SystemError::ShutdownError(error.to_string()),
                                ));
                            pending.retired.send_replace(Some(outcome.clone()));
                            if pending.finished.borrow().is_none() {
                                pending.finished.send_replace(Some(outcome));
                            }
                            return retirement.map_err(failure).and(Err(SpawnError::Cancelled));
                        }
                        pending.finished.send_replace(Some(StopOutcome::Stopped));
                        pending.retired.send_replace(Some(StopOutcome::Stopped));
                        return result;
                    }
                    pending.watches.close();
                    let result = pending
                        .lifetime
                        .dispose_policy()
                        .await
                        .map_err(failure)
                        .and(result);
                    let retirement = match &result {
                        Err(SpawnError::DisposalFailed(message)) => Err(message.clone()),
                        _ => Ok(()),
                    };
                    {
                        let mut registry = system.registry.write().unwrap();
                        if matches!(registry.get(&path), Some(ActorRegistryEntry::Instance(current)) if Arc::ptr_eq(current, &pending))
                        {
                            registry.remove(&path);
                        }
                    }
                    let notification =
                        pending
                            .lifetime
                            .finish(retirement.clone().map_err(|message| {
                                crate::error_receipt::ActorReceipt::from_error(
                                    parrot_api::errors::ActorError::InternalError(message),
                                )
                            }));
                    let outcome =
                        StopOutcome::from_result(retirement.map_err(SystemError::ShutdownError));
                    pending.retired.send_replace(Some(outcome.clone()));
                    if pending.finished.borrow().is_none() {
                        pending.finished.send_replace(Some(outcome));
                    }
                    child_notification = Some(notification);
                    result
                };
                tokio::pin!(work);
                tokio::select! {
                    biased;
                    result = &mut work => result,
                    _ = tokio::time::sleep_until(deadline) => {
                        pending.control.cancel();
                        let error = "child instance creation exceeded deadline".to_string();
                        pending.finished.send_replace(Some(StopOutcome::TimedOut(error.clone())));
                        if let Some(send) = send.take() { let _ = send.send(Err(SpawnError::Timeout(error))); }
                        work.await
                    }
                }
            };
            let result = match result {
                Err(SpawnError::Cancelled) if tokio::time::Instant::now() >= deadline => Err(
                    SpawnError::Timeout("child instance creation exceeded deadline".into()),
                ),
                result => result,
            };
            let reason = result.as_ref().err().map(|error| match error {
                SpawnError::Cancelled => DeathReason::Normal,
                SpawnError::Timeout(_) => DeathReason::Killed,
                error => DeathReason::Failed(error.to_string()),
            });
            if let Some(send) = send {
                let _ = send.send(result);
            }
            let watchers = async {
                if let Some(reason) = reason {
                    let event = parrot_api::context::ActorTerminated {
                        generation: pending.watches.common.id(),
                        path: path.clone(),
                        reason: reason.clone(),
                    };
                    let _ = tokio::join!(
                        pending.watches.common.terminated(event, timeout),
                        super::death_watch::notify_legacy(
                            &pending.watches.legacy,
                            &path,
                            reason,
                            timeout
                        )
                    );
                }
            };
            tokio::join!(watchers, async {
                if let Some(notification) = child_notification {
                    notification.await;
                }
            });
        };
        self.runtime_handle.spawn(parrot_api::trace::capture(owner));
        let result = receive.await.map_err(|_| {
            ActorError::InternalError("child creation owner lost its receipt".into())
        })?;
        cancel.armed = false;
        result.map_err(ActorError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::super::system::{ErasedSpawnable, TypedSpawnPayload};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct DropProbe(Arc<AtomicUsize>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("child rejected input destructor");
        }
    }
    struct Block {
        entered: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    }
    struct Echo;
    impl Actor for Echo {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                match msg.downcast::<Block>() {
                    Ok(block) => {
                        let _ = block.entered.send(());
                        let _ = block.release.await;
                        Ok(Box::new(()) as BoxedMessage)
                    }
                    Err(msg) => Ok(msg),
                }
            })
        }
    }
    async fn setup() -> (
        Arc<ThreadActorSystem>,
        ThreadActorRef<Echo>,
        Arc<crate::children::ChildScope>,
    ) {
        let system = ThreadActorSystem::shared(Default::default());
        let parent = system
            .spawn_root_typed_thread(Echo, EmptyConfig)
            .await
            .unwrap();
        let scope = match system.registry.read().unwrap().get(&parent.path()).unwrap() {
            ActorRegistryEntry::Dynamic(entry) => entry.children.clone(),
            _ => panic!("parent not published"),
        };
        (system, parent, scope)
    }
    struct Bridge {
        panic_validation: bool,
        panic_drop: bool,
        drop_gate: Option<(
            tokio::sync::oneshot::Sender<()>,
            Mutex<std::sync::mpsc::Receiver<()>>,
        )>,
        payload: Option<TypedSpawnPayload<Echo>>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for Bridge {
        fn drop(&mut self) {
            if let Some((entered, release)) = self.drop_gate.take() {
                let _ = entered.send(());
                release
                    .into_inner()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("bridge drop gate lost");
            }
            self.dropped.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.panic_drop,
                "child bridge destructor after publication"
            );
        }
    }
    #[async_trait::async_trait]
    impl ErasedSpawnable for Bridge {
        fn accepts_child_config(&self, config: &BoxedMessage) -> bool {
            assert!(!self.panic_validation, "custom child validation panic");
            config.is::<EmptyConfig>()
        }
        fn spawn_child_on(
            mut self: Box<Self>,
            system: Arc<ThreadActorSystem>,
            engine: ThreadActorConfig,
            parent: BoxedActorRef,
            lifetime: ChildSpawnReceipt,
        ) -> ActorResult<BoxedActorRef> {
            Box::new(self.payload.take().unwrap()).spawn_child_on(system, engine, parent, lifetime)
        }
        async fn spawn_on(
            self: Box<Self>,
            _: Arc<ThreadActorSystem>,
            _: BoxedMessage,
            _: ThreadActorConfig,
        ) -> ActorResult<BoxedActorRef> {
            Err(ActorError::Stopped)
        }
    }
    #[tokio::test]
    async fn custom_child_validation_panic_retires_every_input_and_parent_remains_live() {
        let (system, parent, scope) = setup().await;
        let actor = Arc::new(AtomicUsize::new(0));
        let config = Arc::new(AtomicUsize::new(0));
        let payload = Box::new(ErasedSpawnBox(Box::new(Bridge {
            panic_validation: true,
            panic_drop: false,
            drop_gate: None,
            payload: Some(TypedSpawnPayload::new(Echo)),
            dropped: actor.clone(),
        })));
        let result = system
            .spawn_child_erased(
                payload,
                Box::new(DropProbe(config.clone())),
                None,
                parent.clone_boxed(),
                &scope,
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("destructor"));
        assert_eq!(actor.load(Ordering::SeqCst), 1);
        assert_eq!(config.load(Ordering::SeqCst), 1);
        assert_eq!(system.actor_count(), 1);
        assert_eq!(
            *parent
                .send(Box::new(42_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        system.shutdown_internal().await.unwrap();
    }
    #[tokio::test]
    async fn custom_child_bridge_keeps_existing_signature_and_real_reference() {
        let (system, parent, scope) = setup().await;
        let dropped = Arc::new(AtomicUsize::new(0));
        let payload = Box::new(ErasedSpawnBox(Box::new(Bridge {
            panic_validation: false,
            panic_drop: false,
            drop_gate: None,
            payload: Some(TypedSpawnPayload::new(Echo)),
            dropped: dropped.clone(),
        })));
        let child = system
            .spawn_child_erased(
                payload,
                Box::new(EmptyConfig),
                None,
                parent.clone_boxed(),
                &scope,
            )
            .await
            .unwrap();
        assert!(child.as_any().is::<ThreadActorRef<Echo>>());
        assert!(
            scope
                .view()
                .get(0)
                .unwrap()
                .as_any()
                .is::<ThreadActorRef<Echo>>(),
            "parent tree retained the pending reference marker"
        );
        assert!(child.path().starts_with(&(parent.path() + "/")));
        assert_eq!(
            *child
                .send(Box::new(42_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        parent.stop().await.unwrap();
        assert_eq!(system.actor_count(), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        system.shutdown_internal().await.unwrap();
    }
    #[tokio::test]
    async fn closed_child_scope_retires_actor_config_and_strategy_separately() {
        use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision};
        let (system, parent, scope) = setup().await;
        scope.close();
        let actor = Arc::new(AtomicUsize::new(0));
        let config = Arc::new(AtomicUsize::new(0));
        let policy = Arc::new(AtomicUsize::new(0));
        let capture = DropProbe(policy.clone());
        let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: std::time::Duration::from_secs(1),
            decider: BasicDecisionFn::new(move |_| {
                let _ = &capture;
                SupervisionDecision::Stop
            }),
        });
        let result = system
            .spawn_child_erased(
                Box::new(DropProbe(actor.clone())),
                Box::new(DropProbe(config.clone())),
                Some(strategy),
                parent.clone_boxed(),
                &scope,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(actor.load(Ordering::SeqCst), 1);
        assert_eq!(config.load(Ordering::SeqCst), 1);
        assert_eq!(policy.load(Ordering::SeqCst), 1);
        assert_eq!(system.actor_count(), 1);
        system.shutdown_internal().await.unwrap();
    }
    #[tokio::test]
    async fn parent_stop_cancels_child_queued_behind_all_validation_slots() {
        let (system, parent, scope) = setup().await;
        let permit = system
            .factory_slots
            .clone()
            .acquire_many_owned(system.factory_slots.available_permits() as u32)
            .await
            .unwrap();
        let owner = system.clone();
        let creator = parent.clone_boxed();
        let caller = tokio::spawn(async move {
            owner
                .spawn_child_erased(
                    TypedSpawnPayload::new(Echo).into_boxed(),
                    Box::new(EmptyConfig),
                    None,
                    creator,
                    &scope,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while system.actor_count() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), parent.stop())
            .await
            .unwrap()
            .unwrap();
        assert!(caller.await.unwrap().is_err());
        assert_eq!(system.actor_count(), 0);
        drop(permit);
        system.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn bridge_destructor_failure_after_publication_retires_the_real_child() {
        let (system, parent, scope) = setup().await;
        let dropped = Arc::new(AtomicUsize::new(0));
        let payload = Box::new(ErasedSpawnBox(Box::new(Bridge {
            panic_validation: false,
            panic_drop: true,
            drop_gate: None,
            payload: Some(TypedSpawnPayload::new(Echo)),
            dropped: dropped.clone(),
        })));
        let result = system
            .spawn_child_erased(
                payload,
                Box::new(EmptyConfig),
                None,
                parent.clone_boxed(),
                &scope,
            )
            .await;
        let count = system.actor_count();
        system.shutdown_internal().await.unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("child bridge destructor")
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(count, 1, "failed creation leaked a published child");
    }

    #[tokio::test]
    async fn stopped_child_still_retains_parent_ownership_until_bridge_capture_retires() {
        let (system, parent, scope) = setup().await;
        let dropped = Arc::new(AtomicUsize::new(0));
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let payload = Box::new(ErasedSpawnBox(Box::new(Bridge {
            panic_validation: false,
            panic_drop: true,
            drop_gate: Some((entered, Mutex::new(held))),
            payload: Some(TypedSpawnPayload::new(Echo)),
            dropped: dropped.clone(),
        })));
        let owner = system.clone();
        let ancestor = parent.clone_boxed();
        let children = scope.clone();
        let caller = tokio::spawn(async move {
            owner
                .spawn_child_erased(payload, Box::new(EmptyConfig), None, ancestor, &children)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), observed)
            .await
            .unwrap()
            .unwrap();
        let child = system
            .registry
            .read()
            .unwrap()
            .keys()
            .find(|path| *path != &parent.path())
            .unwrap()
            .clone();
        system.stop_actor(&child).await.unwrap();
        assert_eq!(system.actor_count(), 1);
        assert_eq!(
            scope.view().len(),
            1,
            "live child stop removed an unfinished creation from its parent"
        );
        let stop = parent.stop();
        tokio::pin!(stop);
        assert!(futures::poll!(&mut stop).is_pending());
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), stop)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("child bridge destructor"),
            "{error}"
        );
        assert!(caller.await.unwrap().is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(system.actor_count(), 0);
        system.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn failed_child_creation_does_not_wait_for_parent_notification_capacity() {
        let system = ThreadActorSystem::shared(Default::default());
        let parent = system
            .spawn_typed_with_config(
                Echo,
                EmptyConfig,
                ThreadActorConfig {
                    mailbox_capacity: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let scope = match system.registry.read().unwrap().get(&parent.path()).unwrap() {
            ActorRegistryEntry::Dynamic(entry) => entry.children.clone(),
            _ => panic!("parent not published"),
        };
        let (entered, observed) = oneshot::channel();
        let (release, held) = oneshot::channel();
        let actor = parent.clone_boxed();
        let blocked = tokio::spawn(async move {
            actor
                .send(Box::new(Block {
                    entered,
                    release: held,
                }))
                .await
        });
        observed.await.unwrap();
        parent.deliver(Box::new(42_u64)).await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            system.spawn_child_erased(
                Box::new(()),
                Box::new(EmptyConfig),
                None,
                parent.clone_boxed(),
                &scope,
            ),
        )
        .await;
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        system.shutdown_internal().await.unwrap();
        assert!(
            result
                .expect("creation receipt waited for parent notification mailbox")
                .is_err()
        );
    }
}
