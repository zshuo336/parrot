//! Static root reconstruction preserves the original concrete mailbox.
use super::*;
use crate::constructor::{ConstructionControl, ConstructionError};
use parrot_api::supervisor::{
    BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
};

pub(super) use crate::constructor::owned::OwnedFactory;
impl ThreadActorSystem {
    /// Admit a repeatable static root, preserving its typed mailbox and original
    /// references across replacement. Construction and initialization are async
    /// after admission; use ASK for readiness and stop for retirement diagnostics.
    /// The default policy is the system's configured policy and restart budget.
    /// Inputs acquire cleanup ownership when this method is called. Dropping
    /// the returned future before polling retires them without invoking actor
    /// construction or initialization; registration still starts on first poll.
    pub fn spawn_typed_factory<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        use crate::thread::config::SupervisorStrategy;
        let (maximum, within, decision) = match self.config().default_supervisor_strategy {
            SupervisorStrategy::Restart {
                max_retries,
                within,
            } => (
                u32::try_from(max_retries).unwrap_or(u32::MAX),
                within,
                SupervisionDecision::Restart,
            ),
            SupervisorStrategy::Resume => (0, Duration::from_secs(60), SupervisionDecision::Resume),
            SupervisorStrategy::Stop => (0, Duration::from_secs(60), SupervisionDecision::Stop),
            SupervisorStrategy::Escalate => {
                (0, Duration::from_secs(60), SupervisionDecision::Escalate)
            }
        };
        self.spawn_typed_factory_with_strategy(
            factory,
            path,
            OneForOneStrategy {
                max_restarts: maximum,
                within,
                decider: BasicDecisionFn::new(move |_| decision),
            },
        )
    }

    /// A bounded OneForOne policy for an independent static root. Unread typed
    /// messages survive restart; the failed message is not replayed. Resume only
    /// applies to handlers, and Escalate terminates this parentless root.
    pub fn spawn_typed_factory_with_strategy<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
        strategy: OneForOneStrategy,
    ) -> impl std::future::Future<Output = ActorResult<TypedActorRef<A, M>>> + Send + 'a
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_typed_source(
            None,
            Some(OwnedFactory::new(
                factory,
                self.factory_slots.clone(),
                self.cleanup_slots.clone(),
                self.runtime_handle.clone(),
            )),
            path,
            SupervisorStrategyType::OneForOne(strategy),
        )
    }
}
impl ThreadActorSystem {
    /// Single-protocol convenience form of `spawn_typed_factory`.
    pub fn spawn_typed_single_factory<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
    ) -> impl std::future::Future<
        Output = ActorResult<TypedActorRef<SingleDispatch<A, M>, SingleVariant<A, M>>>,
    > + Send
    + 'a
    where
        A: TypedReceive<M>,
        M: Message,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_typed_factory(move || SingleDispatch::new(factory()), path)
    }

    /// Single-protocol repeatable root with an explicit bounded failure policy.
    pub fn spawn_typed_single_factory_with_strategy<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
        strategy: OneForOneStrategy,
    ) -> impl std::future::Future<
        Output = ActorResult<TypedActorRef<SingleDispatch<A, M>, SingleVariant<A, M>>>,
    > + Send
    + 'a
    where
        A: TypedReceive<M>,
        M: Message,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_typed_factory_with_strategy(
            move || SingleDispatch::new(factory()),
            path,
            strategy,
        )
    }
}

impl TypedControl {
    pub(super) fn begin_construction(
        &self,
        deadline: tokio::time::Instant,
    ) -> tokio::time::Instant {
        let mut current = self.deadline.lock().unwrap();
        let deadline = current.map_or(deadline, |old| old.min(deadline));
        *current = Some(deadline);
        self.deadline_changed.notify_waiters();
        deadline
    }
    pub(super) fn construction_complete(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<bool, SystemError> {
        let mut current = self.deadline.lock().unwrap();
        if self.lifetime.is_stopping()
            || self
                .explicit_stop
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(false);
        }
        if tokio::time::Instant::now() >= deadline {
            drop(current);
            self.request_stop(deadline);
            return Err(SystemError::Timeout(
                "typed factory publication stopped or expired".into(),
            ));
        }
        if *current == Some(deadline) {
            *current = None;
        }
        self.deadline_changed.notify_waiters();
        Ok(true)
    }
}
struct CancelConstruction(Arc<ConstructionControl>);
impl Drop for CancelConstruction {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
fn construction_error(error: ConstructionError<()>) -> SystemError {
    match error {
        ConstructionError::Cancelled => {
            SystemError::Timeout("typed factory construction cancelled or expired".into())
        }
        ConstructionError::ConstructorPanicked(message) => SystemError::ActorCreationError(message),
        ConstructionError::DisposalPanicked(message) => SystemError::Panic(message),
        ConstructionError::WorkerFailed(message) => SystemError::WorkerStateError(message),
        ConstructionError::Publication(()) => unreachable!("prepare has no publication callback"),
    }
}
async fn construct<A: ParrotTypedDispatch>(
    inputs: &mut TypedInputs<A>,
    deadline: tokio::time::Instant,
    runtime: &tokio::runtime::Handle,
) -> Result<bool, SystemError> {
    if let Some(group) = inputs.control.lifetime.recovery() {
        return rebuild_group(inputs, group, runtime).await;
    }
    let deadline = inputs.control.begin_construction(deadline);
    let factory = inputs.factory.as_ref().expect("repeatable source").clone();
    let cancel = CancelConstruction(Arc::new(ConstructionControl::with_deadline(deadline)));
    let construction = cancel.0.clone();
    let mut work = runtime.spawn(parrot_api::trace::capture(async move {
        factory.prepare::<()>(construction).await
    }));
    let prepared = tokio::select! {
        biased;
        _ = inputs.control.channel.closed() => return Ok(false),
        group = inputs.control.group_recovery() => {
            cancel.0.cancel();
            // A cancelled old constructor may still own a blocking poll or an
            // unpublished output. Retired means that work actually finished.
            match work.await.map_err(|error| SystemError::ShutdownError(error.to_string()))? {
                Ok(prepared) => prepared.discard().await.map_err(SystemError::ShutdownError)?,
                Err(ConstructionError::Cancelled) => {},
                Err(error) => return Err(construction_error(error)),
            }
            return rebuild_group(inputs,group,runtime).await;
        },
        _ = tokio::time::sleep_until(deadline) => { inputs.control.request_stop(deadline); return Err(SystemError::Timeout("typed construction exceeded configured deadline".into())); },
        prepared = &mut work => prepared.map_err(|error| SystemError::ShutdownError(error.to_string()))?.map_err(construction_error)?,
    };
    inputs.actor = Some(prepared.adopt());
    inputs.control.construction_complete(deadline)
}

struct AbortGroup(Arc<crate::children::recovery::Recovery>);
impl Drop for AbortGroup {
    fn drop(&mut self) {
        self.0
            .abort("static cohort participant exited before activation");
    }
}
async fn retire_generation<A: ParrotTypedDispatch>(
    inputs: &mut TypedInputs<A>,
) -> Result<(), SystemError> {
    let control = &inputs.control;
    control
        .tree
        .join()
        .await
        .map_err(|error| SystemError::Other(anyhow::Error::new(error)))?;
    if let Some(actor) = inputs.actor.take()
        && let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
    {
        let message = typed_panic(panic);
        let first = inputs
            .cleanup_error
            .get_or_insert_with(|| {
                crate::error_receipt::ActorReceipt::from_error(ActorError::Panic(message))
            })
            .clone();
        return Err(retirement_error(first));
    }
    if let Err(error) = control.lifetime.settle_live_decision().await {
        let error = crate::error_receipt::ActorReceipt::from_error(error);
        let first = inputs.cleanup_error.get_or_insert(error).clone();
        return Err(retirement_error(first));
    }
    // A dispatcher destructor failure belongs to this generation. Retire the
    // old actor, but do not reconstruct over an unsuccessful cleanup receipt.
    match &inputs.cleanup_error {
        Some(error) => Err(retirement_error(error.clone())),
        None => Ok(()),
    }
}
async fn rebuild_group<A: ParrotTypedDispatch>(
    inputs: &mut TypedInputs<A>,
    group: Arc<crate::children::recovery::Recovery>,
    runtime: &tokio::runtime::Handle,
) -> Result<bool, SystemError> {
    use crate::children::recovery::Step;
    let _abort = AbortGroup(group.clone());
    let control = inputs.control.clone();
    control.begin_construction(group.deadline);
    let member = control.lifetime.id;
    let work = async {
        retire_generation(inputs).await?;
        group
            .arrive(member, Step::Retired)
            .await
            .map_err(SystemError::ShutdownError)?;
        let factory = inputs.factory.as_ref().expect("repeatable source").clone();
        let construction = group.control.clone();
        let prepared = runtime
            .spawn(parrot_api::trace::scope(
                group.failure_context.clone(),
                async move { factory.prepare::<()>(construction).await },
            ))
            .await
            .map_err(|error| SystemError::ShutdownError(error.to_string()))?
            .map_err(construction_error)?;
        if let Err(error) = group.arrive(member, Step::Prepared).await {
            prepared
                .discard()
                .await
                .map_err(SystemError::ShutdownError)?;
            return Err(SystemError::ShutdownError(error));
        }
        if group.control.is_cancelled() || control.channel.is_sealed() {
            prepared
                .discard()
                .await
                .map_err(SystemError::ShutdownError)?;
            return Err(SystemError::ShutdownError(
                "static cohort stopped before publication".into(),
            ));
        }
        control.escalated.lock().unwrap().take();
        control
            .tree
            .next_generation()
            .map_err(|error| SystemError::Other(anyhow::Error::new(error)))?;
        // Adoption happens on the owning executor; no user init or dispatch is
        // allowed until every sibling has installed its replacement input.
        inputs.actor = Some(prepared.adopt());
        inputs.creation_context = group.failure_context.clone();
        group
            .arrive(member, Step::Published)
            .await
            .map_err(SystemError::ShutdownError)?;
        control.construction_complete(group.deadline)
    };
    tokio::pin!(work);
    tokio::select! {
        biased;
        result=&mut work => result,
        _=control.channel.closed()=>{group.abort("explicit stop cancelled static cohort");work.await},
        _=tokio::time::sleep_until(group.deadline)=>{group.abort("static cohort recovery deadline elapsed");control.begin_cleanup(group.deadline);work.await},
    }
}

fn actor_error(error: &Arc<ActorError>) -> SystemError {
    operation_error(error, false)
}
async fn restart<A: ParrotTypedDispatch>(
    inputs: &mut TypedInputs<A>,
    error: Arc<ActorError>,
    deadline: tokio::time::Instant,
    runtime: &tokio::runtime::Handle,
) -> Result<bool, SystemError> {
    let control = inputs.control.clone();
    if control.channel.is_sealed() || tokio::time::Instant::now() >= deadline {
        control.begin_cleanup(deadline);
        return Ok(false);
    }
    let policy = control
        .lifetime
        .failure_policy(error.clone(), control.policy_slots.clone(), deadline)
        .await
        .map_err(|error| SystemError::Other(anyhow::Error::new(error)))?;
    let failure_context = if let Some(policy) = policy {
        if let Some(group) = policy.recovery.clone() {
            return rebuild_group(inputs, group, runtime).await;
        }
        if policy.decision != SupervisionDecision::Restart {
            control.tree.defer_failure(error, policy);
            return Ok(false);
        }
        policy.failure_context
    } else {
        return Ok(false);
    };
    parrot_api::trace::scope(failure_context.clone(), async move {
        let deadline = control.begin_construction(deadline);
        retire_generation(inputs).await?;
        if control.channel.is_sealed() || tokio::time::Instant::now() >= deadline {
            control.begin_cleanup(deadline);
            return Ok(false);
        }
        control.escalated.lock().unwrap().take();
        control
            .tree
            .next_generation()
            .map_err(|error| SystemError::Other(anyhow::Error::new(error)))?;
        inputs.creation_context = failure_context;
        construct(inputs, deadline, runtime).await
    })
    .await
}

pub(super) async fn run<A: ParrotTypedDispatch>(
    mut inputs: TypedInputs<A>,
    control: Arc<TypedControl>,
    batch_size: usize,
    runtime: tokio::runtime::Handle,
) -> anyhow::Result<TypedExit> {
    let mut hook_started = false;
    let mut execution = std::panic::AssertUnwindSafe(async {
        if control.channel.is_sealed() { return Ok(()); }
        if !parrot_api::trace::scope(inputs.creation_context.clone(), construct(&mut inputs, tokio::time::Instant::now() + control.timeout, &runtime)).await? { return Ok(()); }
        let mut reductions = 0;
        'generation: loop {
            let context=control.tree.context();
            if control.channel.is_sealed() { return Ok(()); }
            use crate::typed_supervision::Step;
            let initialization_context = inputs.creation_context.take();
            let initialized=match control.wait_typed_stage(parrot_api::trace::scope(initialization_context.clone(), crate::typed_supervision::lifecycle_stage(|| inputs.actor.as_mut().expect("constructed actor").init_with_context(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed))), &runtime).await {
                Step::Stopped=>return Ok(()),
                Step::Recovery(group)=>{if rebuild_group(&mut inputs,group,&runtime).await? {continue 'generation;} return Ok(());},
                Step::Escalated(error)=>{let deadline=control.deadline();if restart(&mut inputs,error.clone(),deadline,&runtime).await?{continue 'generation;}return Err(actor_error(&error));},
                Step::Ready(result)=>result.unwrap_or_else(|message|Err(ActorError::Panic(message))),
            };
            if let Err(error) = initialized {
                let error = Arc::new(error);
                let deadline = tokio::time::Instant::now() + control.timeout;
                // Own the decider across explicit stop/cancellation, just as for
                // handler failures. Initialization Resume never activates a failed instance.
                let resume = parrot_api::trace::scope(initialization_context, control.lifetime.resume_message(error.clone(), deadline, false, &runtime)).await;
                if !resume && restart(&mut inputs, error.clone(), deadline, &runtime).await? { continue; }
                return Err(actor_error(&error));
            }
            drop(initialization_context);
            loop {
                let message=match control.wait_typed_stage(inputs.receiver.as_ref().expect("typed mailbox").recv_async(), &runtime).await {
                    Step::Stopped=>break 'generation,
                    Step::Recovery(group)=>{if rebuild_group(&mut inputs,group,&runtime).await? {continue 'generation;} return Ok(());},
                    Step::Escalated(error)=>{let deadline=control.deadline();if restart(&mut inputs,error.clone(),deadline,&runtime).await?{continue 'generation;}return Err(actor_error(&error));},
                    Step::Ready(Ok(message))=>message,
                    Step::Ready(Err(_))=>break 'generation,
                };
                control.channel.notify_capacity();
                let dispatched=match control.wait_typed_stage(crate::typed_supervision::dispatch(inputs.actor.as_mut().expect("live actor"),message,&control.lifetime,&runtime,control.timeout,Some(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed)), &runtime).await {
                    Step::Stopped=>break 'generation,
                    Step::Recovery(group)=>{if rebuild_group(&mut inputs,group,&runtime).await? {continue 'generation;} return Ok(());},
                    Step::Escalated(error)=>Err(error),
                    Step::Ready(result)=>result,
                };
                if let Err(error) = dispatched {
                    let deadline = control.lifetime.decision_deadline().unwrap_or_else(|| tokio::time::Instant::now() + control.timeout);
                    if restart(&mut inputs, error.clone(), deadline, &runtime).await? { continue 'generation; }
                    return Err(actor_error(&error));
                }
                reductions += 1;
                if reductions >= batch_size.max(1) { reductions = 0; tokio::task::yield_now().await; }
            }
        }
        if let Some(error)=control.escalated.lock().unwrap().clone() { return Err(escalation_error(&error)); }
        if control.lifetime.decision_deadline().is_some() { return Ok(()); }
        control.tree.join().await.map_err(|error| SystemError::Other(anyhow::Error::new(error)))?;
        let context=control.tree.context();
        if control.deadline() <= tokio::time::Instant::now() { return Err(SystemError::Timeout("typed stop deadline elapsed".into())); }
        if inputs.dispatch_cancel_failed { return Ok(()); }
        hook_started=true;
        tokio::select! {
            biased;
            _ = control.expired() => Err(SystemError::Timeout("typed stop hook exceeded configured deadline".into())),
            result = crate::typed_supervision::lifecycle_stage(|| inputs.actor.as_mut().expect("initialized actor").before_stop_with_context(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed)) => result.map_err(SystemError::Panic).and_then(|result| result.map_err(|error| SystemError::Other(anyhow::Error::new(error)))),
        }
    }).catch_unwind().await.unwrap_or_else(|panic| Err(SystemError::Panic(typed_panic(panic))));
    let children = control
        .tree
        .join()
        .await
        .map_err(|error| SystemError::Other(anyhow::Error::new(error)));
    let cleanup = inputs.retire().map_err(retirement_error).and(children);
    let cleanup_failed = cleanup.is_err() || (hook_started && execution.is_err());
    if let Err(error) = cleanup {
        execution = Err(error);
    }
    let reason = match &execution {
        Ok(()) => DeathReason::Normal,
        Err(SystemError::Timeout(_)) => DeathReason::Killed,
        Err(SystemError::Panic(message)) => DeathReason::Panic(message.clone()),
        Err(error) => DeathReason::Failed(crate::error_receipt::system_summary(error)),
    };
    Ok(TypedExit {
        result: execution,
        reason,
        cleanup_failed,
        failure: None,
    })
}
