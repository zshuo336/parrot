//! Constructors belong to the system runtime; live instances stay on one arbiter.
use super::*;
use crate::thread::typed::{SingleDispatch, SingleVariant};
use crate::{
    actix::lifecycle::ActorExit,
    constructor::{ConstructionControl, ConstructionError, owned::OwnedFactory},
};
use parrot_api::{
    message::Message,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
};
use std::time::Duration;

impl ActixActorSystem {
    /// Admit a repeatable static root. The concrete channel and original refs
    /// survive replacement. Construction/initialization follow admission; ASK
    /// observes readiness and stop joins retirement. Defaults to three restarts
    /// in 60 seconds, independently of other roots.
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
        self.spawn_typed_factory_with_strategy(
            factory,
            path,
            OneForOneStrategy {
                max_restarts: 3,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
            },
        )
    }
    /// An independent static root with a bounded OneForOne policy. The failed
    /// message is not replayed; unread messages and incoming DeathWatch survive.
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
                self.constructor_slots.clone(),
                self.cleanup_slots.clone(),
                self.runtime.clone(),
            )),
            path,
            SupervisorStrategyType::OneForOne(strategy),
        )
    }
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
    /// Single-protocol static root with an explicit bounded failure policy.
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
struct CancelConstruction(Arc<ConstructionControl>);
impl Drop for CancelConstruction {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
fn construction_error(error: ConstructionError<()>) -> ActorError {
    match error {
        ConstructionError::Cancelled => {
            ActorError::TimeoutDetail("typed factory construction cancelled or expired".into())
        }
        ConstructionError::ConstructorPanicked(message) => ActorError::InitializationError(message),
        ConstructionError::DisposalPanicked(message) => ActorError::Panic(message),
        ConstructionError::WorkerFailed(message) => ActorError::InternalError(message),
        ConstructionError::Publication(()) => unreachable!("prepare has no publication callback"),
    }
}
async fn construct<A: ParrotTypedDispatch>(
    inputs: &mut TypedInputs<A>,
    deadline: tokio::time::Instant,
    runtime: &tokio::runtime::Handle,
) -> ActorResult<bool> {
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
            match work.await.map_err(|error| ActorError::InternalError(error.to_string()))? {
                Ok(prepared) => prepared.discard().await.map_err(ActorError::InternalError)?,
                Err(ConstructionError::Cancelled) => {},
                Err(error) => return Err(construction_error(error)),
            }
            return rebuild_group(inputs,group,runtime).await;
        },
        _ = tokio::time::sleep_until(deadline) => { inputs.control.begin_cleanup(deadline); return Err(inputs.control.enforce_deadline()); },
        prepared = &mut work => prepared.map_err(|error| ActorError::InternalError(error.to_string()))?.map_err(construction_error)?,
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
) -> Result<(), ActorError> {
    let control = &inputs.control;
    control.typed_tree().join().await?;
    if let Some(actor) = inputs.actor.take()
        && let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
    {
        let message = panic_message(panic);
        let first = inputs
            .cleanup_error
            .get_or_insert_with(|| {
                crate::error_receipt::ActorReceipt::from_error(ActorError::Panic(message))
            })
            .clone();
        return Err(first.into_error());
    }
    if let Err(error) = control.lifetime.settle_live_decision().await {
        let error = crate::error_receipt::ActorReceipt::from_error(error);
        let first = inputs.cleanup_error.get_or_insert(error).clone();
        return Err(first.into_error());
    }
    // A dispatcher destructor failure belongs to this generation. Retire the
    // old actor, but do not reconstruct over an unsuccessful cleanup receipt.
    match &inputs.cleanup_error {
        Some(error) => Err(error.clone().into_error()),
        None => Ok(()),
    }
}
async fn rebuild_group<A: ParrotTypedDispatch>(
    inputs: &mut TypedInputs<A>,
    group: Arc<crate::children::recovery::Recovery>,
    runtime: &tokio::runtime::Handle,
) -> ActorResult<bool> {
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
            .map_err(ActorError::InternalError)?;
        let factory = inputs.factory.as_ref().expect("repeatable source").clone();
        let construction = group.control.clone();
        let prepared = runtime
            .spawn(parrot_api::trace::scope(
                group.failure_context.clone(),
                async move { factory.prepare::<()>(construction).await },
            ))
            .await
            .map_err(|error| ActorError::InternalError(error.to_string()))?
            .map_err(construction_error)?;
        if let Err(error) = group.arrive(member, Step::Prepared).await {
            prepared
                .discard()
                .await
                .map_err(ActorError::InternalError)?;
            return Err(ActorError::InternalError(error));
        }
        if group.control.is_cancelled() || control.channel.is_sealed() {
            prepared
                .discard()
                .await
                .map_err(ActorError::InternalError)?;
            return Err(ActorError::InternalError(
                "static cohort stopped before publication".into(),
            ));
        }
        control.escalated.lock().unwrap().take();
        control.typed_tree().next_generation()?;
        // Adoption happens on the owning executor; no user init or dispatch is
        // allowed until every sibling has installed its replacement input.
        inputs.actor = Some(prepared.adopt());
        inputs.creation_context = group.failure_context.clone();
        group
            .arrive(member, Step::Published)
            .await
            .map_err(ActorError::InternalError)?;
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

async fn restart<A: ParrotTypedDispatch>(
    inputs: &mut TypedInputs<A>,
    error: Arc<ActorError>,
    deadline: tokio::time::Instant,
    runtime: &tokio::runtime::Handle,
) -> ActorResult<bool> {
    let control = inputs.control.clone();
    if control.channel.is_sealed() || tokio::time::Instant::now() >= deadline {
        control.begin_cleanup(deadline);
        return Ok(false);
    }
    let policy = control
        .lifetime
        .failure_policy(error.clone(), inputs.policy_slots.clone(), deadline)
        .await?;
    let failure_context = if let Some(policy) = policy {
        if let Some(group) = policy.recovery.clone() {
            return rebuild_group(inputs, group, runtime).await;
        }
        if policy.decision != SupervisionDecision::Restart {
            control.typed_tree().defer_failure(error, policy);
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
        control.typed_tree().next_generation()?;
        inputs.creation_context = failure_context;
        construct(inputs, deadline, runtime).await
    })
    .await
}

pub(super) async fn run<A: ParrotTypedDispatch>(
    mut inputs: TypedInputs<A>,
    control: Arc<ActorControl>,
    runtime: tokio::runtime::Handle,
) -> ActorExit {
    let mut hook_started = false;
    let outcome=std::panic::AssertUnwindSafe(async {
        if control.channel.is_sealed(){return Ok(());}
        if !parrot_api::trace::scope(inputs.creation_context.clone(),construct(&mut inputs,tokio::time::Instant::now()+control.timeout,&runtime)).await?{return Ok(());}
        let mut budget=ready_budget::ReadyBudget::default();
        'generation:loop {
            let context=control.typed_tree().context();
            if control.channel.is_sealed(){return Ok(());}
            use crate::typed_supervision::Step;
            let initialization_context=inputs.creation_context.take();
            let initialized=match control.wait_typed_stage(parrot_api::trace::scope(initialization_context.clone(),crate::typed_supervision::lifecycle_stage(|| inputs.actor.as_mut().expect("constructed actor").init_with_context(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed))), &runtime).await {
                Step::Stopped=>return Ok(()),
                Step::Recovery(group)=>{if rebuild_group(&mut inputs,group,&runtime).await? {continue 'generation;} return Ok(());},
                Step::Escalated(error)=>{let deadline=control.cleanup_deadline().unwrap_or_else(||tokio::time::Instant::now()+control.timeout);if restart(&mut inputs,error.clone(),deadline,&runtime).await?{continue 'generation;}return Err(crate::supervision_policy::reply_error(error));},
                Step::Ready(result)=>result.unwrap_or_else(|message|Err(ActorError::Panic(message))),
            };
            if let Err(error)=initialized {
                let error=Arc::new(error);let deadline=tokio::time::Instant::now()+control.timeout;
                let resume=parrot_api::trace::scope(initialization_context,control.lifetime.resume_message(error.clone(),deadline,false,&runtime)).await;
                if !resume && restart(&mut inputs,error.clone(),deadline,&runtime).await?{continue;}
                return Err(crate::supervision_policy::reply_error(error));
            }
            drop(initialization_context);
            loop {
                let message=match budget.wait(control.wait_typed_stage(inputs.receiver.as_ref().expect("typed mailbox").recv_async(), &runtime)).await {
                    Step::Stopped=>break 'generation,
                    Step::Recovery(group)=>{if rebuild_group(&mut inputs,group,&runtime).await? {continue 'generation;} return Ok(());},
                    Step::Escalated(error)=>{let deadline=control.cleanup_deadline().unwrap_or_else(||tokio::time::Instant::now()+control.timeout);if restart(&mut inputs,error.clone(),deadline,&runtime).await?{continue 'generation;}return Err(crate::supervision_policy::reply_error(error));},
                    Step::Ready(Ok(message))=>message,
                    Step::Ready(Err(_))=>break 'generation,
                };
                control.channel.notify_capacity();
                let dispatched=match budget.wait(control.wait_typed_stage(crate::typed_supervision::dispatch(inputs.actor.as_mut().expect("live actor"),message,&control.lifetime,&runtime,control.timeout,Some(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed)), &runtime)).await {
                    Step::Stopped=>break 'generation,
                    Step::Recovery(group)=>{if rebuild_group(&mut inputs,group,&runtime).await? {continue 'generation;} return Ok(());},
                    Step::Escalated(error)=>Err(error),
                    Step::Ready(result)=>result,
                };
                if let Err(error)=dispatched {
                    let deadline=control.lifetime.decision_deadline().unwrap_or_else(||tokio::time::Instant::now()+control.timeout);
                    if restart(&mut inputs,error.clone(),deadline,&runtime).await?{continue 'generation;}
                    return Err(crate::supervision_policy::reply_error(error));
                }
                budget.completed().await;
            }
        }
        if let Some(error)=control.escalated.lock().unwrap().clone(){return Err(crate::supervision_policy::reply_error(error));}
        if control.lifetime.decision_deadline().is_some(){return Ok(());}
        control.typed_tree().join().await?;
        let context=control.typed_tree().context();
        if control.expired_now(){return Err(control.enforce_deadline());}
        if inputs.dispatch_cancel_failed { return Ok(()); }
        hook_started=true;
        tokio::select! {
            biased;
            _=control.expired()=>Err(control.enforce_deadline()),
            result=crate::typed_supervision::lifecycle_stage(|| inputs.actor.as_mut().expect("initialized actor").before_stop_with_context(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed))=>result.unwrap_or_else(|message| Err(ActorError::Panic(message))),
        }
    }).catch_unwind().await.unwrap_or_else(|panic|Err(ActorError::Panic(panic_message(panic))));
    let children = control.typed_tree().join().await;
    let outcome = Outcome::from_result(outcome);
    let cleanup = inputs.retire().and(children).and(if hook_started {
        outcome.clone().result()
    } else {
        Ok(())
    });
    ActorExit {
        outcome: outcome.result(),
        cleanup,
    }
}
