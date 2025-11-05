//! Managed typed actors run on their owning Actix arbiter. Lifecycle admission,
//! cancellation and completion are shared with all protocol views and dynamic bridges.
//! Performance depends on the workload and must be measured with release benchmarks.
use crate::actix::{
    lifecycle::{ActorControl, Outcome, panic_message},
    system::{ActixActorSystem, Registration},
};
mod factory;
mod ready_budget;
#[cfg(test)]
mod ready_budget_contracts;
use futures::FutureExt;
pub use parrot_api::typed_channel::{DEFAULT_ASK_TIMEOUT, TypedActorRef, TypedEnvelope};
use parrot_api::{
    errors::ActorError,
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive},
    types::ActorResult,
};
use std::sync::Arc;

// Own the live instance and mailbox outside every user future. Tokio may drop
// the whole execution future when an arbiter stops; poll-level catch_unwind
// does not run the cleanup code after an interrupted await.
struct TypedInputs<A: ParrotTypedDispatch> {
    actor: Option<A>,
    creation_context: Option<parrot_api::trace::TraceContext>,
    receiver: Option<flume::Receiver<TypedEnvelope<A>>>,
    factory: Option<Arc<crate::constructor::Factory<A>>>,
    policy_slots: Arc<tokio::sync::Semaphore>,
    cleanup_error: Option<crate::error_receipt::ActorReceipt>,
    dispatch_cancel_failed: bool,
    control: Arc<ActorControl>,
    finished: Option<tokio::sync::oneshot::Sender<Outcome>>,
}
impl<A: ParrotTypedDispatch> TypedInputs<A> {
    fn retire(&mut self) -> ActorResult<()> {
        self.control
            .begin_cleanup(tokio::time::Instant::now() + self.control.timeout);
        let mut cleanup = self
            .cleanup_error
            .take()
            .map_or(Ok(()), |error| Err(error.into_error()));
        if let Some(receiver) = self.receiver.take() {
            // Sender clones can outlive registration, so Receiver::drop alone
            // cannot prove that the queued messages have actually retired.
            while let Ok(envelope) = receiver.try_recv() {
                if let Err(payload) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(envelope)))
                {
                    let message = panic_message(payload);
                    if cleanup.is_ok() {
                        cleanup = Err(ActorError::Panic(message));
                    }
                }
            }
            drop(receiver);
        }
        if let Some(actor) = self.actor.take()
            && let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
        {
            let message = panic_message(payload);
            if cleanup.is_ok() {
                cleanup = Err(ActorError::Panic(message));
            }
        }
        let outcome = Outcome::from_result(cleanup);
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(outcome.clone());
        }
        outcome.result()
    }
}
impl<A: ParrotTypedDispatch> Drop for TypedInputs<A> {
    fn drop(&mut self) {
        if self.finished.is_some() {
            // Separate unwind boundaries keep one destructor from skipping
            // another, including during cancellation of a user future.
            let _ = self.retire();
        }
    }
}

async fn run_actor<A: ParrotTypedDispatch>(
    mut inputs: TypedInputs<A>,
    control: Arc<ActorControl>,
    runtime: tokio::runtime::Handle,
) -> crate::actix::lifecycle::ActorExit {
    if inputs.factory.is_some() {
        return factory::run(inputs, control, runtime).await;
    }
    let context = control.typed_tree().context();
    let mut hook_started = false;
    let mut was_escalated = false;
    let actor = inputs.actor.as_mut().expect("live typed instance");
    let receiver = inputs.receiver.as_ref().expect("live typed mailbox");
    let result = std::panic::AssertUnwindSafe(async {
        if control.channel.is_sealed() {
            return Ok(());
        }
        use crate::typed_supervision::Step;
        match control
            .wait_typed_stage(parrot_api::trace::scope(inputs.creation_context.take(), crate::typed_supervision::lifecycle_stage(|| actor.init_with_context(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed))), &runtime)
            .await
        {
            Step::Stopped => return Ok(()),
            Step::Escalated(error) => {
                was_escalated = true;
                return Err(crate::supervision_policy::reply_error(error));
            }
            Step::Recovery(group) => {
                group.abort("non-repeatable parent cannot recover");
                return Err(ActorError::InternalError(
                    "non-repeatable parent cannot recover".into(),
                ));
            }
            Step::Ready(result) => result.unwrap_or_else(|message| Err(ActorError::Panic(message))).inspect_err(|_| {
                control
                    .supervision_failed
                    .store(true, std::sync::atomic::Ordering::Release);
            })?,
        }
        let mut budget = ready_budget::ReadyBudget::default();
        loop {
            let envelope = match budget
                .wait(control.wait_typed_stage(receiver.recv_async(), &runtime))
                .await
            {
                Step::Stopped => break,
                Step::Escalated(error) => {
                    was_escalated = true;
                    return Err(crate::supervision_policy::reply_error(error));
                }
                Step::Recovery(group) => {
                    group.abort("non-repeatable parent cannot recover");
                    return Err(ActorError::InternalError(
                        "non-repeatable parent cannot recover".into(),
                    ));
                }
                Step::Ready(Ok(envelope)) => envelope,
                Step::Ready(Err(_)) => break,
            };
            control.channel.notify_capacity();
            match budget
                .wait(control.wait_typed_stage(
                    crate::typed_supervision::dispatch(
                        actor,
                        envelope,
                        &control.lifetime,
                        &runtime,
                        control.timeout,
                        Some(&context),
                        crate::typed_supervision::DispatchCleanup::new(
                            &mut inputs.cleanup_error,
                            &mut inputs.dispatch_cancel_failed,
                        ),
                    ),
                    &runtime,
                ))
                .await
            {
                Step::Stopped => break,
                Step::Escalated(error) => {
                    was_escalated = true;
                    return Err(crate::supervision_policy::reply_error(error));
                }
                Step::Recovery(group) => {
                    group.abort("non-repeatable parent cannot recover");
                    return Err(ActorError::InternalError(
                        "non-repeatable parent cannot recover".into(),
                    ));
                }
                Step::Ready(Err(error)) => {
                    control
                        .supervision_failed
                        .store(true, std::sync::atomic::Ordering::Release);
                    return Err(crate::supervision_policy::reply_error(error));
                }
                Step::Ready(Ok(())) => {}
            }
            budget.completed().await;
        }
        // Explicit stop may cancel a handler while its failure decider is
        // still running. Do not enter a normal hook on that failed instance;
        // the system owner still joins the outstanding decision and captures.
        if let Some(error) = control.escalated.lock().unwrap().clone() {
            was_escalated = true;
            return Err(crate::supervision_policy::reply_error(error));
        }
        if control.lifetime.decision_deadline().is_some() {
            return Ok(());
        }
        control.typed_tree().join().await?;
        if control.expired_now() {
            return Err(control.enforce_deadline());
        }
        if inputs.dispatch_cancel_failed {
            return Ok(());
        }
        hook_started = true;
        tokio::select! {
            biased;
            _ = control.expired() => Err(control.enforce_deadline()),
            result = crate::typed_supervision::lifecycle_stage(|| actor.before_stop_with_context(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed)) => result.unwrap_or_else(|message| Err(ActorError::Panic(message))),
        }
    })
    .catch_unwind()
    .await;
    let result = result.unwrap_or_else(|payload| {
        if !hook_started {
            control
                .supervision_failed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Err(ActorError::Panic(panic_message(payload)))
    });
    if was_escalated {
        control
            .supervision_failed
            .store(true, std::sync::atomic::Ordering::Release);
    }
    let result = Outcome::from_result(result);
    let children = control.typed_tree().join().await;
    let cleanup = inputs.retire().and(children).and(if hook_started {
        result.clone().result()
    } else {
        Ok(())
    });
    crate::actix::lifecycle::ActorExit {
        // The monitor uses the original escalation Arc, preserving its reason.
        outcome: if was_escalated {
            Ok(())
        } else {
            result.result()
        },
        cleanup,
    }
}

impl ActixActorSystem {
    /// Register one actor at the exact requested path. The system owns its lifetime.
    /// Stop cancels pending dispatch, runs the stop hook and joins disposal on the
    /// actor's arbiter. A blocking user poll can exceed the deadline; its cleanup
    /// remains owned and no further messages are admitted.
    /// Inputs acquire cleanup ownership when this method is called. Dropping
    /// the returned future before polling retires them without invoking actor
    /// construction or initialization; registration still starts on first poll.
    pub fn spawn_typed<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.spawn_typed_with_strategy(
            actor,
            path,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::StopOnFailure,
            ),
        )
    }

    /// Spawn a static protocol instance with explicit handler supervision.
    /// Resume retains state and queued messages. Restart needs a repeatable
    /// factory; roots have no parent to receive Escalate and stop instead.
    pub fn spawn_typed_with_strategy<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.spawn_typed_source(Some(actor), None, path, strategy)
    }

    fn spawn_typed_source<'a, A, M>(
        self: &'a Arc<Self>,
        actor: Option<A>,
        factory: Option<crate::constructor::owned::OwnedFactory<A>>,
        path: &'a str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let payload = actor.map(|actor| {
            crate::owned_payload::OwnedPayload::new(actor, &self.runtime, &self.cleanup_slots)
        });
        let policy = crate::owned_payload::OwnedPayload::new(
            Some(strategy),
            &self.runtime,
            &self.cleanup_slots,
        );
        parrot_api::trace::capture(
            self.spawn_typed_owned_source(payload, factory, path, policy, None),
        )
    }

    pub(crate) async fn spawn_typed_child_factory_source<A, M, F>(
        self: &Arc<Self>,
        factory: crate::owned_payload::OwnedPayload<F>,
        policy: crate::owned_payload::OwnedPayload<
            Option<parrot_api::supervisor::SupervisorStrategyType>,
        >,
        path: &str,
        parent: &crate::typed_context::ManagedContext,
    ) -> ActorResult<TypedActorRef<A, M>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        let factory = crate::constructor::owned::OwnedFactory::new(
            factory.take(),
            self.constructor_slots.clone(),
            self.cleanup_slots.clone(),
            self.runtime.clone(),
        );
        self.spawn_typed_owned_source(None, Some(factory), path, policy, Some(parent))
            .await
    }

    pub(crate) async fn spawn_typed_child_source<A, M>(
        self: &Arc<Self>,
        actor: crate::owned_payload::OwnedPayload<A>,
        policy: crate::owned_payload::OwnedPayload<
            Option<parrot_api::supervisor::SupervisorStrategyType>,
        >,
        path: &str,
        parent: &crate::typed_context::ManagedContext,
    ) -> ActorResult<TypedActorRef<A, M>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.spawn_typed_owned_source(Some(actor), None, path, policy, Some(parent))
            .await
    }

    async fn spawn_typed_owned_source<A, M>(
        self: &Arc<Self>,
        payload: Option<crate::owned_payload::OwnedPayload<A>>,
        factory: Option<crate::constructor::owned::OwnedFactory<A>>,
        path: &str,
        policy: crate::owned_payload::OwnedPayload<
            Option<parrot_api::supervisor::SupervisorStrategyType>,
        >,
        parent: Option<&crate::typed_context::ManagedContext>,
    ) -> ActorResult<TypedActorRef<A, M>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let creation_context = parrot_api::trace::TraceContext::current();
        let mut factory = factory;
        let mut payload = payload;
        let mut policy = Some(policy);
        let unpublished_actor = payload.as_ref().map(|value| value.receipt());
        let unpublished_policy = policy.as_ref().unwrap().receipt();
        let unpublished = async move {
            let (actor, policy) = tokio::join!(
                async {
                    match unpublished_actor {
                        Some(receipt) => receipt.await,
                        None => Ok(()),
                    }
                },
                unpublished_policy
            );
            actor.and(policy).map_err(|error| error.to_string())
        }
        .boxed()
        .shared();
        let mut admit = |lifetime: Option<Arc<crate::children::ChildLifetime>>| {
            let (sender, receiver) =
                parrot_api::typed_channel::typed_channel::<A>(self.settings.mailbox_capacity);
            let lifetime = lifetime.unwrap_or_else(|| {
                crate::children::ChildLifetime::with_policy(
                    std::sync::Weak::new(),
                    None,
                    self.policy_slots.clone(),
                )
            });
            let (control, finished) = ActorControl::with_lifetime(
                self.settings.shutdown_timeout,
                self.settings.max_context_tasks,
                Some(lifetime),
                self.policy_slots.clone(),
            );
            let own = Arc::new(std::sync::OnceLock::new());
            let weak_stop = Arc::downgrade(&control);
            let weak_escalated = Arc::downgrade(&control);
            let tree = Arc::new(crate::typed_context::TypedTree::new_backend(
                crate::typed_context::Backend::Actix(Box::new(self.downgrade())),
                crate::typed_context::Identity {
                    path: path.to_owned(),
                    own: own.clone(),
                    parent: parent.and_then(|parent| parent.own()),
                },
                crate::typed_context::Hooks {
                    stop: Arc::new(move |scope| {
                        if let Some(control) = weak_stop.upgrade()
                            && control.typed_tree().stop_generation(scope)
                        {
                            control.request_stop(tokio::time::Instant::now() + control.timeout);
                        }
                    }),
                    escalate: Arc::new(move |error| {
                        if let Some(control) = weak_escalated.upgrade()
                            && !control.channel.is_sealed()
                        {
                            let mut signal = control.escalated.lock().unwrap();
                            signal.get_or_insert(error);
                            control
                                .begin_construction(tokio::time::Instant::now() + control.timeout);
                            drop(signal);
                            control.typed_escalation_changed.notify_waiters();
                        }
                    }),
                },
                self.settings.shutdown_timeout,
                self.runtime.clone(),
                self.policy_slots.clone(),
                self.cleanup_slots.clone(),
            ));
            control
                .typed_tree
                .set(tree.clone())
                .unwrap_or_else(|_| unreachable!("one typed tree"));
            if factory.is_some() {
                let weak = Arc::downgrade(&control);
                control.lifetime.bind_recovery(Arc::new(move |group| {
                    if let Some(control) = weak.upgrade() {
                        if control.channel.is_sealed() {
                            group.abort("static factory member is stopping");
                        }
                        control.begin_construction(group.deadline);
                        control.typed_escalation_changed.notify_waiters();
                    } else {
                        group.abort("static factory member owner disappeared");
                    }
                }));
            }
            let actor_ref = TypedActorRef::new(sender, path.to_owned())
                .with_lifecycle(control.clone(), self.settings.ask_timeout);
            actor_ref.install_termination_recipient()?;
            let mut registry = self.registry.write().unwrap();
            if registry.shutting_down {
                return Err(ActorError::Stopped);
            }
            if registry.actors.contains_key(path) {
                return Err(ActorError::InternalError(format!(
                    "actor path already exists: {path}"
                )));
            }
            let erased: Arc<dyn parrot_api::address::ActorRef> =
                Arc::from(actor_ref.clone().into_dyn());
            own.set(Arc::downgrade(&erased))
                .unwrap_or_else(|_| unreachable!("one typed identity"));
            let arbiter = self.arbiter_pool().next_arbiter();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            let (retired_tx, retired_rx) = tokio::sync::oneshot::channel();
            let task_control = control.clone();
            let runtime = self.runtime.clone();
            let payload = payload.take();
            let source_factory = factory.as_ref().map(|owner| owner.factory.clone());
            let policy = policy.take().expect("one admitted typed policy");
            // Own the mailbox before submitting Execute: accepted references
            // may enqueue messages even when the task never reaches first poll.
            let mut inputs = TypedInputs {
                actor: None,
                creation_context: creation_context.clone(),
                receiver: Some(receiver),
                factory: source_factory,
                policy_slots: self.policy_slots.clone(),
                cleanup_error: None,
                dispatch_cancel_failed: false,
                control: task_control.clone(),
                finished: Some(retired_tx),
            };
            // The owner catches both poll panics and panics during normal disposal.
            if !arbiter.spawn(async move {
                let result = std::panic::AssertUnwindSafe(async move {
                    inputs.actor = payload.map(|value| value.take());
                    let strategy = policy.take();
                    task_control.lifetime.set_strategy(strategy);
                    run_actor(inputs, task_control, runtime).await
                })
                .catch_unwind()
                .await
                .unwrap_or_else(|payload| crate::actix::lifecycle::ActorExit {
                    outcome: Err(ActorError::Panic(panic_message(payload))),
                    cleanup: Err(ActorError::InternalError(
                        "typed execution ended without a cleanup receipt".into(),
                    )),
                });
                let _ = result_tx.send(result);
            }) {
                return Err(ActorError::InternalError(
                    "Actix arbiter rejected actor execution".into(),
                ));
            }
            registry.actors.insert(
                path.to_owned(),
                Registration {
                    actor_ref: erased,
                    control: control.clone(),
                },
            );
            let unpublished = unpublished.clone();
            let factory = factory.take();
            self.monitor_actor_with_disposal(
                path.to_owned(),
                control,
                finished,
                result_rx,
                Some(Box::pin(async move {
                    // Executor cancellation still retains the real descendant
                    // retirement operation on the independent system runtime.
                    let (unpublished, retired, children) =
                        tokio::join!(unpublished, retired_rx, tree.join());
                    let retirement = match retired {
                        Ok(outcome) => outcome.result(),
                        Err(_) => Err(ActorError::InternalError(
                            "typed execution ended without an input retirement receipt".into(),
                        )),
                    };
                    let factory = match factory {
                        Some(factory) => factory.retire().await,
                        None => Ok(()),
                    };
                    unpublished
                        .map_err(ActorError::InternalError)
                        .and(retirement)
                        .and(children)
                        .and(factory)
                })),
                None,
            );
            Ok(actor_ref)
        };
        let admission = if let Some(parent) = parent {
            let mut reference = None;
            let result = parent.scope.spawn(|lifetime| {
                let actor = admit(Some(lifetime.clone()))?;
                let erased = actor.clone().into_dyn();
                let stop = erased.clone_boxed();
                reference = Some(actor);
                Ok((
                    erased,
                    Arc::new(move || {
                        let stop = stop.clone_boxed();
                        let lifetime = lifetime.clone();
                        Box::pin(async move {
                            let _ = stop.stop().await;
                            lifetime.wait().await
                        })
                    }),
                ))
            });
            result.map(|_| reference.expect("admitted typed child"))
        } else {
            admit(None)
        };
        match admission {
            Ok(reference) => Ok(reference),
            Err(error) => {
                // Independent owners isolate both user destructors, including
                // rejection by the arbiter before the task's first poll.
                drop(payload);
                drop(policy);
                let factory = async {
                    match factory {
                        Some(factory) => factory.retire().await,
                        None => Ok(()),
                    }
                };
                let (inputs, factory) = tokio::join!(unpublished, factory);
                inputs.map_err(ActorError::InternalError).and(factory)?;
                Err(error)
            }
        }
    }

    /// M4: 静态轨 spawn（单一协议便捷入口，无需 derive 枚举信封）。
    ///
    /// 与 thread 引擎的 `spawn_typed_single` 完全同构：适配器
    /// `SingleDispatch` 把单个 `TypedReceive<M>` 包装成
    /// `ParrotTypedDispatch`；用户消息经 `SingleVariant::wrap` 包装。
    pub fn spawn_typed_single<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
    ) -> impl std::future::Future<
        Output = ActorResult<
            TypedActorRef<
                crate::thread::typed::SingleDispatch<A, M>,
                crate::thread::typed::SingleVariant<A, M>,
            >,
        >,
    > + Send
    + 'a
    where
        A: TypedReceive<M>,
        M: parrot_api::message::Message,
    {
        use crate::thread::typed::{SingleDispatch, SingleVariant};
        self.spawn_typed::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
        )
    }

    /// Single-protocol static instance with an explicit handler failure strategy.
    pub fn spawn_typed_single_with_strategy<'a, A, M>(
        self: &'a Arc<Self>,
        actor: A,
        path: &'a str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> impl std::future::Future<
        Output = ActorResult<
            TypedActorRef<
                crate::thread::typed::SingleDispatch<A, M>,
                crate::thread::typed::SingleVariant<A, M>,
            >,
        >,
    > + Send
    + 'a
    where
        A: TypedReceive<M>,
        M: parrot_api::message::Message,
    {
        use crate::thread::typed::{SingleDispatch, SingleVariant};
        self.spawn_typed_with_strategy::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
            strategy,
        )
    }
}

#[cfg(test)]
mod unpublished_tests {
    use super::*;
    use parrot_api::{
        message::Message,
        supervisor::{
            BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
        },
        types::BoxedFuture,
    };
    use std::{
        sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        wake: Condvar,
        entered: tokio::sync::Notify,
    }
    impl Gate {
        fn block(&self) {
            self.entered.notify_one();
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.wake.wait(open).unwrap();
            }
        }
        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    struct Unpolled {
        gate: Arc<Gate>,
        initialized: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for Unpolled {
        fn drop(&mut self) {
            self.gate.block();
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl ParrotTypedDispatch for Unpolled {
        type Msg = ();
        type Reply = ();
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.initialized.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn dispatch(&mut self, _: ()) -> BoxedFuture<'_, ActorResult<()>> {
            panic!("unpublished actor must not dispatch")
        }
    }
    struct Ping;
    impl Message for Ping {
        type Result = ();
    }
    impl ParrotMsgVariant<Unpolled> for Ping {
        fn inject(_: Self) {}
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[actix::test]
    async fn accepted_but_unpolled_typed_task_keeps_registration_until_inputs_retire() {
        let system = Arc::new(
            ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_millis(80),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let worker = system.arbiter_pool().next_arbiter();
        let blocked = Arc::new(Gate::default());
        let _unblock = Release(blocked.clone());
        let gate = blocked.clone();
        assert!(worker.spawn(async move {
            gate.block();
        }));
        tokio::time::timeout(Duration::from_secs(2), blocked.entered.notified())
            .await
            .unwrap();
        // Stop precedes Execute in this same arbiter command queue, while its
        // OS thread is blocked. Admission succeeds but the actor cannot poll.
        assert!(worker.stop());
        let disposal = Arc::new(Gate::default());
        let _dispose = Release(disposal.clone());
        let initialized = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let capture = Capture(captures.clone());
        let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(1),
            decider: BasicDecisionFn::new(move |_| {
                let _keep = &capture;
                SupervisionDecision::Stop
            }),
        });
        let actor = system
            .spawn_typed_with_strategy::<_, Ping>(
                Unpolled {
                    gate: disposal.clone(),
                    initialized: initialized.clone(),
                    dropped: dropped.clone(),
                },
                "/unpolled",
                policy,
            )
            .await
            .unwrap();
        blocked.release();
        tokio::time::timeout(Duration::from_secs(2), disposal.entered.notified())
            .await
            .unwrap();
        let stopped = tokio::time::timeout(Duration::from_secs(2), actor.stop())
            .await
            .unwrap();
        assert!(
            matches!(
                stopped,
                Err(ActorError::TimeoutDetail(_)) | Err(ActorError::Timeout)
            ),
            "not a disposal receipt: {stopped:?}"
        );
        assert!(
            system.get_actor(&"/unpolled".into()).await.is_some(),
            "lost executor sender is not proof of input disposal"
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        disposal.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            while system.get_actor(&"/unpolled".into()).await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(initialized.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        system.as_ref().clone().shutdown().await.unwrap();
    }
    #[actix::test]
    async fn started_typed_task_keeps_registration_until_actor_retires() {
        let system = Arc::new(
            ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_millis(80),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let worker = system.arbiter_pool().next_arbiter();
        let disposal = Arc::new(Gate::default());
        let _dispose = Release(disposal.clone());
        let initialized = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let capture = Capture(captures.clone());
        let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(1),
            decider: BasicDecisionFn::new(move |_| {
                let _keep = &capture;
                SupervisionDecision::Stop
            }),
        });
        let actor = system
            .spawn_typed_with_strategy::<_, Ping>(
                Unpolled {
                    gate: disposal.clone(),
                    initialized: initialized.clone(),
                    dropped: dropped.clone(),
                },
                "/started",
                policy,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while initialized.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(worker.stop());
        tokio::time::timeout(Duration::from_secs(2), disposal.entered.notified())
            .await
            .unwrap();
        let stopped = tokio::time::timeout(Duration::from_secs(2), actor.stop())
            .await
            .unwrap();
        assert!(
            matches!(
                stopped,
                Err(ActorError::TimeoutDetail(_)) | Err(ActorError::Timeout)
            ),
            "not a disposal receipt: {stopped:?}"
        );
        assert!(
            system.get_actor(&"/started".into()).await.is_some(),
            "lost executor sender is not proof of input disposal"
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        disposal.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            while system.get_actor(&"/started".into()).await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(initialized.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        system.as_ref().clone().shutdown().await.unwrap();
    }
    struct Queued(Arc<AtomicUsize>);
    impl Drop for Queued {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Message for Queued {
        type Result = ();
    }
    struct Cancelled {
        initialized: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for Cancelled {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl ParrotTypedDispatch for Cancelled {
        type Msg = Queued;
        type Reply = ();
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async {
                self.initialized.notify_one();
                futures::future::pending().await
            })
        }
        fn dispatch(&mut self, _: Queued) -> BoxedFuture<'_, ActorResult<()>> {
            panic!("initialization has not completed")
        }
    }
    impl ParrotMsgVariant<Cancelled> for Queued {
        fn inject(msg: Self) -> Self {
            msg
        }
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    #[actix::test]
    async fn cancelled_typed_task_drains_mailbox_while_references_survive() {
        let system = Arc::new(
            ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_millis(80),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let initialized = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let messages = Arc::new(AtomicUsize::new(0));
        let actor = system
            .spawn_typed::<_, Queued>(
                Cancelled {
                    initialized: initialized.clone(),
                    dropped: dropped.clone(),
                },
                "/cancelled-queue",
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), initialized.notified())
            .await
            .unwrap();
        actor.tell(Queued(messages.clone())).await.unwrap();
        actor.tell(Queued(messages.clone())).await.unwrap();
        assert!(system.arbiter_pool().next_arbiter().stop());
        // Observe arbiter cancellation before requesting a public stop; a
        // cooperative stop must not win the race and drain the queue for us.
        tokio::time::timeout(Duration::from_secs(2), async {
            while dropped.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), actor.stop())
                .await
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while system.get_actor(&"/cancelled-queue".into()).await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            messages.load(Ordering::SeqCst),
            2,
            "retirement must drain queued payloads even while old references own senders"
        );
        assert!(!actor.is_alive());
        system.as_ref().clone().shutdown().await.unwrap();
    }
    struct CancelMessage {
        drops: Arc<AtomicUsize>,
        gate: Option<Arc<Gate>>,
        panic: bool,
    }
    impl Drop for CancelMessage {
        fn drop(&mut self) {
            if let Some(gate) = &self.gate {
                gate.block();
            }
            self.drops.fetch_add(1, Ordering::SeqCst);
            if self.panic {
                panic!("cancelled queued message destructor");
            }
        }
    }
    impl Message for CancelMessage {
        type Result = ();
    }
    struct PendingInit {
        entered: Arc<tokio::sync::Notify>,
        drops: Arc<AtomicUsize>,
        panic: bool,
    }
    impl std::future::Future for PendingInit {
        type Output = ActorResult<()>;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            self.entered.notify_one();
            std::task::Poll::Pending
        }
    }
    impl Drop for PendingInit {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if self.panic {
                panic!("cancelled initialization future destructor");
            }
        }
    }
    struct CancelActor {
        entered: Arc<tokio::sync::Notify>,
        future_drops: Arc<AtomicUsize>,
        actor_drops: Arc<AtomicUsize>,
        panic: bool,
        handler: bool,
    }
    impl Drop for CancelActor {
        fn drop(&mut self) {
            self.actor_drops.fetch_add(1, Ordering::SeqCst);
            if self.panic {
                panic!("cancelled actor destructor");
            }
        }
    }
    impl CancelActor {
        fn pending(&self) -> PendingInit {
            PendingInit {
                entered: self.entered.clone(),
                drops: self.future_drops.clone(),
                panic: self.panic,
            }
        }
    }
    impl ParrotTypedDispatch for CancelActor {
        type Msg = CancelMessage;
        type Reply = ();
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            if self.handler {
                Box::pin(async { Ok(()) })
            } else {
                Box::pin(self.pending())
            }
        }
        fn dispatch(&mut self, msg: CancelMessage) -> BoxedFuture<'_, ActorResult<()>> {
            assert!(self.handler, "cancelled initializer never dispatches");
            let pending = self.pending();
            Box::pin(async move {
                let _held_message = msg;
                pending.await
            })
        }
    }
    impl ParrotMsgVariant<CancelActor> for CancelMessage {
        fn inject(msg: Self) -> Self {
            msg
        }
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    async fn cancelled_ownership(panic: bool, gate: Option<Arc<Gate>>, handler: bool) {
        let system = Arc::new(
            ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
                arbiter_count: 1,
                // This case tests error precedence, not the short-deadline contract.
                shutdown_timeout: if panic {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(80)
                },
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let entered = Arc::new(tokio::sync::Notify::new());
        let future_drops = Arc::new(AtomicUsize::new(0));
        let actor_drops = Arc::new(AtomicUsize::new(0));
        let message_drops = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let capture = Capture(captures.clone());
        let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(1),
            decider: BasicDecisionFn::new(move |_| {
                let _keep = &capture;
                SupervisionDecision::Stop
            }),
        });
        let actor = system
            .spawn_typed_with_strategy::<_, CancelMessage>(
                CancelActor {
                    entered: entered.clone(),
                    future_drops: future_drops.clone(),
                    actor_drops: actor_drops.clone(),
                    panic,
                    handler,
                },
                "/cancelled-inputs",
                policy,
            )
            .await
            .unwrap();
        if handler {
            actor
                .tell(CancelMessage {
                    drops: message_drops.clone(),
                    gate: None,
                    panic: false,
                })
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        actor
            .tell(CancelMessage {
                drops: message_drops.clone(),
                gate: gate.clone(),
                panic,
            })
            .await
            .unwrap();
        actor
            .tell(CancelMessage {
                drops: message_drops.clone(),
                gate: None,
                panic: false,
            })
            .await
            .unwrap();
        assert!(system.arbiter_pool().next_arbiter().stop());
        if let Some(gate) = &gate {
            tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
                .await
                .unwrap();
            let error = tokio::time::timeout(Duration::from_secs(2), actor.stop())
                .await
                .unwrap()
                .unwrap_err();
            assert!(
                matches!(error, ActorError::Timeout | ActorError::TimeoutDetail(_)),
                "premature completion: {error}"
            );
            assert!(
                system
                    .get_actor(&"/cancelled-inputs".into())
                    .await
                    .is_some()
            );
            assert_eq!(message_drops.load(Ordering::SeqCst), usize::from(handler));
            assert_eq!(captures.load(Ordering::SeqCst), 0);
            gate.release();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while system
                .get_actor(&"/cancelled-inputs".into())
                .await
                .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), actor.stop())
            .await
            .unwrap()
            .unwrap_err();
        if panic {
            // Initialization and dispatch now both preserve the failed user
            // future disposal before the later queued-message destructor.
            let expected = "cancelled initialization future destructor";
            assert!(
                matches!(&error, ActorError::Panic(message) if message == expected),
                "first cleanup receipt changed: expected {expected}, got {error}"
            );
        }
        assert_eq!(future_drops.load(Ordering::SeqCst), 1);
        assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            message_drops.load(Ordering::SeqCst),
            2 + usize::from(handler)
        );
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert!(!actor.is_alive());
        system.as_ref().clone().shutdown().await.unwrap();
    }
    #[actix::test]
    async fn cancelled_typed_mailbox_keeps_registration_until_payloads_retire() {
        for handler in [false, true] {
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            cancelled_ownership(false, Some(gate), handler).await;
        }
    }
    #[actix::test]
    async fn cancelled_typed_destructor_panics_are_isolated() {
        if std::env::var_os("PARROT_TYPED_CANCEL_DROP_CHILD").is_some() {
            // Only this --exact child owns the hook. Count deliberately injected
            // panics without synchronous backtrace IO consuming the lifecycle
            // watchdog; unexpected failures retain the ordinary diagnostic hook.
            let observed = Arc::new([
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ]);
            let counted = observed.clone();
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let index = match info.payload().downcast_ref::<&str>().copied() {
                    Some("cancelled initialization future destructor") => Some(0),
                    Some("cancelled queued message destructor") => Some(1),
                    Some("cancelled actor destructor") => Some(2),
                    _ => None,
                };
                if let Some(index) = index {
                    counted[index].fetch_add(1, Ordering::SeqCst);
                } else {
                    previous(info);
                }
            }));
            for handler in [false, true] {
                cancelled_ownership(true, None, handler).await;
            }
            assert_eq!(
                observed.each_ref().map(|n| n.load(Ordering::SeqCst)),
                [2, 2, 2]
            );
            println!("PARROT_TYPED_CANCEL_DROP_COUNTS=2,2,2");
            return;
        }
        let output = tokio::time::timeout(Duration::from_secs(20), tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "actix::typed::unpublished_tests::cancelled_typed_destructor_panics_are_isolated", "--nocapture"])
            .env("PARROT_TYPED_CANCEL_DROP_CHILD", "1").kill_on_drop(true).output()).await.unwrap().unwrap();
        assert!(
            output.status.success(),
            "cancelled destructor child contract failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("PARROT_TYPED_CANCEL_DROP_COUNTS=2,2,2"),
            "child exited without executing all panic/disposal contracts"
        );
    }
    #[actix::test]
    async fn unpolled_typed_task_drains_mailbox_before_retirement() {
        let system = Arc::new(
            ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_millis(80),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let worker = system.arbiter_pool().next_arbiter();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        assert!(worker.spawn(async move {
            blocked.block();
        }));
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        assert!(worker.stop());
        let initialized = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let messages = Arc::new(AtomicUsize::new(0));
        let actor = system
            .spawn_typed::<_, Queued>(
                Cancelled {
                    initialized: initialized.clone(),
                    dropped: dropped.clone(),
                },
                "/unpolled-queue",
            )
            .await
            .unwrap();
        actor.tell(Queued(messages.clone())).await.unwrap();
        actor.tell(Queued(messages.clone())).await.unwrap();
        gate.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            while system.get_actor(&"/unpolled-queue".into()).await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            initialized.notified().now_or_never().is_none(),
            "task must never poll"
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            messages.load(Ordering::SeqCst),
            2,
            "unpolled task also owns admitted messages"
        );
        assert!(actor.stop().await.is_err());
        system.as_ref().clone().shutdown().await.unwrap();
    }
    struct FactoryCapture {
        gate: Arc<Gate>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for FactoryCapture {
        fn drop(&mut self) {
            self.gate.block();
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[actix::test]
    async fn unpolled_factory_keeps_path_until_constructor_captures_retire() {
        let system = Arc::new(
            ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_millis(80),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let worker = system.arbiter_pool().next_arbiter();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        assert!(worker.spawn(async move {
            blocked.block();
        }));
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        assert!(worker.stop());
        let cleanup = Arc::new(Gate::default());
        let _clean = Release(cleanup.clone());
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = FactoryCapture {
            gate: cleanup.clone(),
            dropped: drops.clone(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let actor = system
            .spawn_typed_factory::<Unpolled, Ping, _>(
                move || {
                    let _ = &capture;
                    invoked.fetch_add(1, Ordering::SeqCst);
                    panic!("unpolled factory must not construct")
                },
                "/factory-unpolled",
            )
            .await
            .unwrap();
        gate.release();
        tokio::time::timeout(Duration::from_secs(2), cleanup.entered.notified())
            .await
            .unwrap();
        assert!(actor.stop().await.is_err());
        assert!(
            system
                .get_actor(&"/factory-unpolled".into())
                .await
                .is_some()
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        cleanup.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            while system
                .get_actor(&"/factory-unpolled".into())
                .await
                .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        system.as_ref().clone().shutdown().await.unwrap();
    }
    #[actix::test]
    async fn cancelled_arbiter_retains_late_factory_output_until_real_disposal() {
        let system = Arc::new(
            ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_millis(80),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let worker = system.arbiter_pool().next_arbiter();
        let constructor = Arc::new(Gate::default());
        let _release = Release(constructor.clone());
        let blocked = constructor.clone();
        let disposal = Arc::new(Gate::default());
        let _dispose = Release(disposal.clone());
        let drop_gate = disposal.clone();
        let initialized = Arc::new(AtomicUsize::new(0));
        let init = initialized.clone();
        let dropped = Arc::new(AtomicUsize::new(0));
        let drops = dropped.clone();
        let captures = Arc::new(AtomicUsize::new(0));
        let capture = Capture(captures.clone());
        let returned = Arc::new(tokio::sync::Notify::new());
        let produced = returned.clone();
        let actor = system
            .spawn_typed_factory::<_, Ping, _>(
                move || {
                    let _ = &capture;
                    blocked.block();
                    produced.notify_one();
                    Unpolled {
                        gate: drop_gate.clone(),
                        initialized: init.clone(),
                        dropped: drops.clone(),
                    }
                },
                "/factory-cancelled",
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), constructor.entered.notified())
            .await
            .unwrap();
        let cleanup_permit = system.cleanup_slots.clone().acquire_owned().await.unwrap();
        assert!(worker.stop());
        assert!(actor.stop().await.is_err());
        assert!(
            system
                .get_actor(&"/factory-cancelled".into())
                .await
                .is_some()
        );
        constructor.release();
        tokio::time::timeout(Duration::from_secs(2), returned.notified())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), disposal.entered.notified())
                .await
                .is_err(),
            "cancelled output bypassed the system cleanup budget"
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(cleanup_permit);
        tokio::time::timeout(Duration::from_secs(2), disposal.entered.notified())
            .await
            .unwrap();
        assert_eq!(initialized.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert!(
            system
                .get_actor(&"/factory-cancelled".into())
                .await
                .is_some()
        );
        disposal.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            while system
                .get_actor(&"/factory-cancelled".into())
                .await
                .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(initialized.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        system.as_ref().clone().shutdown().await.unwrap();
    }
}
