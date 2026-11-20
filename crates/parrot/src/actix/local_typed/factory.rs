//! Repeatable Local actors retain every instance on their original arbiter.
use super::*;
use crate::actix::lifecycle::ActorExit;
use crate::owned_payload::OwnedPayload;
use crate::typed_supervision::Restart;
use parrot_api::supervisor::{
    BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
};
use std::time::Duration;

#[cfg(test)]
mod contracts;

impl ActixActorSystem {
    /// Admit a repeatable Local root, with three restarts per 60 seconds.
    /// The constructor is Send but need not be Sync: all calls and all non-Send
    /// outputs stay on one arbiter. Inputs are owned at call time; admission is
    /// lazy. References and unread messages survive replacement; failed messages
    /// are not replayed. Captures retire only after the final instance.
    pub fn spawn_local_typed_factory<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
    ) -> impl std::future::Future<Output = ActorResult<LocalTypedActorRef<A, M>>> + Send + 'a
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        self.spawn_local_typed_factory_with_strategy(
            factory,
            path,
            OneForOneStrategy {
                max_restarts: 3,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
            },
        )
    }
    /// Repeatable Local root with a bounded independent supervision policy.
    /// Stop and timeout keep ownership until actual instance/capture disposal.
    /// Synchronous construction, polling and destruction cannot be preempted.
    pub fn spawn_local_typed_factory_with_strategy<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
        strategy: OneForOneStrategy,
    ) -> impl std::future::Future<Output = ActorResult<LocalTypedActorRef<A, M>>> + Send + 'a
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        let factory = OwnedPayload::new(factory, &self.runtime, &self.cleanup_slots);
        let policy = OwnedPayload::new(
            Some(SupervisorStrategyType::OneForOne(strategy)),
            &self.runtime,
            &self.cleanup_slots,
        );
        parrot_api::trace::capture(
            self.spawn_local_typed_owned_source(factory, policy, path, None, true),
        )
    }
}

pub(super) struct Prepared<A> {
    pub(super) actor: A,
    pub(super) invocation: Arc<tokio::sync::OwnedSemaphorePermit>,
}

// Acquire invocation ownership before worker capacity without moving the output.
// Once the synchronous call starts, the independent monitor may time out but
// this arbiter still owns the late result. The caller installs it in TypedInputs
// before checking deadlines, so all exit paths isolate its actual destructor.
pub(super) async fn prepare<A, F>(
    factory: &OwnedPayload<F>,
    control: &Arc<ActorControl>,
    slots: Arc<tokio::sync::Semaphore>,
    admission: &crate::constructor::admission::Admission,
    deadline: tokio::time::Instant,
    group: Option<&crate::constructor::ConstructionControl>,
) -> ActorResult<Option<Prepared<A>>>
where
    A: LocalTypedDispatch,
    F: Fn() -> A + Send + 'static,
{
    let single = crate::constructor::ConstructionControl::with_deadline(deadline);
    let construction = group.unwrap_or(&single);
    let invocation = tokio::select! {
        biased;
        _ = control.channel.closed() => return Ok(None),
        _ = tokio::time::sleep_until(deadline) => {
            control.begin_cleanup(deadline);
            return Err(control.enforce_deadline());
        },
        permit = admission.acquire::<()>(construction) => match permit {
            Ok(permit) => permit,
            Err(_) if tokio::time::Instant::now() >= deadline => {
                control.begin_cleanup(deadline);
                return Err(control.enforce_deadline());
            },
            Err(crate::constructor::ConstructionError::Cancelled) => return Err(ActorError::Stopped),
            Err(error) => return Err(ActorError::InternalError(format!("Local construction admission failed: {error:?}"))),
        },
    };
    let permit = tokio::select! {
        biased;
        _ = control.channel.closed() => return Ok(None),
        _ = async {
            match group {
                Some(group) => group.cancelled().await,
                None => std::future::pending().await,
            }
        } => return Err(ActorError::Stopped),
        _ = tokio::time::sleep_until(deadline) => { control.begin_cleanup(deadline); return Err(control.enforce_deadline()); },
        permit = slots.acquire_owned() => permit.map_err(|_| ActorError::Stopped)?,
    };
    if control.channel.is_sealed() {
        return Ok(None);
    }
    if group.is_some_and(|group| group.is_cancelled()) {
        return Err(ActorError::Stopped);
    }
    if tokio::time::Instant::now() >= deadline {
        control.begin_cleanup(deadline);
        return Err(control.enforce_deadline());
    }
    let actor = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| factory.value()()))
        .map(|actor| Some(Prepared { actor, invocation }))
        .map_err(|payload| ActorError::InitializationError(panic_message(payload)));
    drop(permit);
    actor
}
async fn construct<A, F>(
    inputs: &mut TypedInputs<A>,
    factory: &OwnedPayload<F>,
    deadline: tokio::time::Instant,
    runtime: &tokio::runtime::Handle,
) -> ActorResult<bool>
where
    A: LocalTypedDispatch,
    F: Fn() -> A + Send + 'static,
{
    if let Some(group) = inputs.control.lifetime.recovery() {
        return rebuild_group(inputs, factory, group, runtime).await;
    }
    let control = inputs.control.clone();
    let deadline = control.begin_construction(deadline);
    let actor = tokio::select! {
        biased;
        _ = control.channel.closed() => return Ok(false),
        group = control.group_recovery() => return rebuild_group(inputs, factory, group, runtime).await,
        actor = prepare(factory, &control, inputs.constructor_slots.clone(), &inputs.factory_admission, deadline, None) => actor?,
    };
    inputs.install(actor);
    // A cohort may be assigned while the synchronous constructor was blocked.
    // That old output must retire with all other old generations before init.
    if let Some(group) = control.lifetime.recovery() {
        return rebuild_group(inputs, factory, group, runtime).await;
    }
    if inputs.actor.is_none() {
        return Ok(false);
    }
    let published = control.construction_complete(deadline)?;
    if published {
        inputs.invocation.take();
    }
    Ok(published)
}
struct AbortGroup(Arc<crate::children::recovery::Recovery>);
impl Drop for AbortGroup {
    fn drop(&mut self) {
        self.0
            .abort("Local cohort participant exited before activation");
    }
}
async fn rebuild_group<A, F>(
    inputs: &mut TypedInputs<A>,
    factory: &OwnedPayload<F>,
    group: Arc<crate::children::recovery::Recovery>,
    _runtime: &tokio::runtime::Handle,
) -> ActorResult<bool>
where
    A: LocalTypedDispatch,
    F: Fn() -> A + Send + 'static,
{
    use crate::children::recovery::Step;
    let _abort = AbortGroup(group.clone());
    let control = inputs.control.clone();
    control.begin_construction_quiet(group.deadline);
    control.construction_changed();
    let member = control.lifetime.id;
    let work = async {
        retire_generation(inputs).await?;
        if let Err(error) = group.arrive(member, Step::Retired).await {
            return if group.was_cancelled() {
                Ok(false)
            } else {
                Err(ActorError::InternalError(error))
            };
        }
        let deadline = control.begin_reconstruction(group.deadline);
        let construction = match group.creation_control(member, deadline) {
            Ok(control) => control,
            Err(_) if group.was_cancelled() => return Ok(false),
            Err(error) => return Err(ActorError::InternalError(error)),
        };
        let prepared = parrot_api::trace::scope(
            group.failure_context.clone(),
            prepare(
                factory,
                &control,
                inputs.constructor_slots.clone(),
                &inputs.factory_admission,
                deadline,
                Some(&construction),
            ),
        )
        .await;
        let prepared = match prepared {
            Ok(actor) => actor,
            Err(ActorError::Stopped) if group.was_cancelled() => return Ok(false),
            Err(ActorError::Stopped) if group.failure_reason().is_some() => {
                return Err(ActorError::InternalError(group.failure_reason().unwrap()));
            }
            Err(error) => return Err(error),
        };
        inputs.install(prepared);
        if inputs.actor.is_none() {
            return Ok(false);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(control.enforce_deadline());
        }
        if let Err(error) = group.arrive(member, Step::Prepared).await {
            return if group.was_cancelled() {
                Ok(false)
            } else {
                Err(ActorError::InternalError(error))
            };
        }
        if construction.is_cancelled() || control.channel.is_sealed() {
            return if group.was_cancelled() {
                Ok(false)
            } else {
                Err(ActorError::InternalError(
                    group
                        .failure_reason()
                        .unwrap_or_else(|| "Local cohort stopped before publication".into()),
                ))
            };
        }
        let removed = control.escalated.lock().unwrap().take();
        drop(removed);
        control.typed_tree().next_generation()?;
        inputs.creation_context = group.failure_context.clone();
        if let Err(error) = group.arrive(member, Step::Published).await {
            return if group.was_cancelled() {
                Ok(false)
            } else {
                Err(ActorError::InternalError(error))
            };
        }
        let published = control.construction_complete(deadline)?;
        if published {
            inputs.invocation.take();
        }
        Ok(published)
    };
    tokio::pin!(work);
    tokio::select! {
        biased;
        result = &mut work => result,
        _ = control.channel.closed() => { group.cancel("explicit stop cancelled Local cohort"); work.await },
        _ = control.expired() => { group.abort("Local cohort creation/recovery deadline elapsed"); control.begin_cleanup(control.cleanup_deadline().unwrap_or(group.deadline)); work.await },
    }
}
async fn retire_generation<A: LocalTypedDispatch>(
    inputs: &mut TypedInputs<A>,
) -> Result<(), ActorError> {
    let control = inputs.control.clone();
    control.typed_tree().join().await?;
    // Streams belong to the actor generation. Retire them before constructing
    // a replacement so a late item from the old instance can never reach the
    // new actor. Preserve the first cleanup failure for supervision.
    if let Err(error) = inputs.retire_streams() {
        let receipt = crate::error_receipt::ActorReceipt::from_error(error);
        let first = inputs.cleanup_error.get_or_insert(receipt).clone();
        return Err(first.into_error());
    }
    let retired = inputs
        .actor
        .take()
        .map(|actor| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor))));
    inputs.invocation.take();
    if let Some(Err(panic)) = retired {
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
async fn restart<A: LocalTypedDispatch, F: Fn() -> A + Send + 'static>(
    inputs: &mut TypedInputs<A>,
    factory: &crate::owned_payload::OwnedPayload<F>,
    error: crate::error_receipt::ActorReceipt,
    deadline: tokio::time::Instant,
    runtime: &tokio::runtime::Handle,
) -> ActorResult<Restart> {
    let control = inputs.control.clone();
    if control.channel.is_sealed() || tokio::time::Instant::now() >= deadline {
        control.begin_cleanup(deadline);
        return Ok(Restart::Declined);
    }
    let policy = control
        .lifetime
        .failure_policy(error.clone(), inputs.policy_slots.clone(), deadline)
        .await?;
    let failure_context = if let Some(policy) = policy {
        if let Some(group) = policy.recovery.clone() {
            return rebuild_group(inputs, factory, group, runtime)
                .await
                .map(Restart::constructed);
        }
        if policy.decision != SupervisionDecision::Restart {
            control.typed_tree().defer_failure(error, policy);
            return Ok(Restart::Declined);
        }
        policy.failure_context
    } else {
        return Ok(Restart::Declined);
    };
    parrot_api::trace::scope(failure_context.clone(), async move {
        control.begin_construction_quiet(deadline);
        control.construction_changed();
        retire_generation(inputs).await?;
        let deadline = control.begin_reconstruction(deadline);
        if control.channel.is_sealed() {
            control.begin_cleanup(deadline);
            return Ok(Restart::Stopped);
        }
        if tokio::time::Instant::now() >= deadline {
            control.begin_cleanup(deadline);
            return Err(control.enforce_deadline());
        }
        let removed = control.escalated.lock().unwrap().take();
        drop(removed);
        control.typed_tree().next_generation()?;
        inputs.creation_context = failure_context;
        construct(inputs, factory, deadline, runtime)
            .await
            .map(Restart::constructed)
    })
    .await
}

pub(super) async fn run<A, F>(
    mut inputs: TypedInputs<A>,
    factory: &OwnedPayload<F>,
    control: Arc<ActorControl>,
    runtime: tokio::runtime::Handle,
) -> ActorExit
where
    A: LocalTypedDispatch,
    F: Fn() -> A + Send + 'static,
{
    use crate::typed_supervision::{DispatchCleanup, Step, recorded_lifecycle_stage};
    let mut hook_started = false;
    let outcome = std::panic::AssertUnwindSafe(async {
        if control.channel.is_sealed() {
            return Ok(());
        }
        if !parrot_api::trace::scope(
            inputs.creation_context.clone(),
            construct(
                &mut inputs,
                factory,
                tokio::time::Instant::now() + control.creation_timeout,
                &runtime,
            ),
        )
        .await?
        {
            return Ok(());
        }
        let mut reductions = 0;
        'generation: loop {
            let context = control.typed_tree().context();
            if control.channel.is_sealed() {
                return Ok(());
            }
            let mut initialization_context = inputs.creation_context.take();
            let initialized = match control
                .wait_typed_stage(
                    parrot_api::trace::scope(
                        initialization_context.clone(),
                        recorded_lifecycle_stage(
                            parrot_api::trace::SpanKind::ActorInit,
                            context.path(),
                            || {
                                inputs
                                    .actor
                                    .as_mut()
                                    .expect("constructed actor")
                                    .init(&context)
                            },
                            DispatchCleanup::new(
                                &mut inputs.cleanup_error,
                                &mut inputs.dispatch_cancel_failed,
                            )
                            .with_deadline(control.initialization_deadline()),
                        ),
                    ),
                    &runtime,
                )
                .await
            {
                Step::Stopped => return Ok(()),
                Step::Recovery(group) => {
                    if rebuild_group(&mut inputs, factory, group, &runtime).await? {
                        continue 'generation;
                    }
                    return Ok(());
                }
                Step::Escalated(error) => {
                    let deadline = control
                        .cleanup_deadline()
                        .unwrap_or_else(|| tokio::time::Instant::now() + control.timeout);
                    match restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                        Restart::Ready => continue 'generation,
                        Restart::Stopped => return Ok(()),
                        Restart::Declined => {
                            return Err(crate::supervision_policy::reply_error(error));
                        }
                    }
                }
                Step::Ready(outcome) => {
                    if !matches!(&outcome.result, Ok(Ok(()))) {
                        initialization_context = outcome.context;
                    }
                    outcome
                        .result
                        .unwrap_or_else(|message| Err(ActorError::Panic(message)))
                }
            };
            if let Err(error) = initialized {
                control.finish_initialization();
                let error = crate::error_receipt::ActorReceipt::from_error(error);
                let deadline = tokio::time::Instant::now() + control.timeout;
                let resume = parrot_api::trace::scope(
                    initialization_context,
                    control
                        .lifetime
                        .resume_message(error.clone(), deadline, false, &runtime),
                )
                .await;
                if !resume {
                    match restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                        Restart::Ready => continue,
                        Restart::Stopped => return Ok(()),
                        Restart::Declined => {}
                    }
                }
                return Err(crate::supervision_policy::reply_error(error));
            }
            let streams_initialized = {
                let mut stream_context = LocalStreamContext::new(&mut inputs.local_streams);
                match control
                    .wait_typed_stage(
                        parrot_api::trace::scope(
                            initialization_context.clone(),
                            recorded_lifecycle_stage(
                                parrot_api::trace::SpanKind::ActorInitLocalStreams,
                                context.path(),
                                || {
                                    inputs
                                        .actor
                                        .as_mut()
                                        .expect("constructed actor")
                                        .init_local_streams(&context, &mut stream_context)
                                },
                                DispatchCleanup::new(
                                    &mut inputs.cleanup_error,
                                    &mut inputs.dispatch_cancel_failed,
                                )
                                .with_deadline(control.initialization_deadline()),
                            ),
                        ),
                        &runtime,
                    )
                    .await
                {
                    Step::Stopped => return Ok(()),
                    Step::Recovery(group) => {
                        if rebuild_group(&mut inputs, factory, group, &runtime).await? {
                            continue 'generation;
                        }
                        return Ok(());
                    }
                    Step::Escalated(error) => {
                        let deadline = control
                            .cleanup_deadline()
                            .unwrap_or_else(|| tokio::time::Instant::now() + control.timeout);
                        match restart(&mut inputs, factory, error.clone(), deadline, &runtime)
                            .await?
                        {
                            Restart::Ready => continue 'generation,
                            Restart::Stopped => return Ok(()),
                            Restart::Declined => {
                                return Err(crate::supervision_policy::reply_error(error));
                            }
                        }
                    }
                    Step::Ready(outcome) => {
                        if !matches!(&outcome.result, Ok(Ok(()))) {
                            initialization_context = outcome.context;
                        }
                        outcome
                            .result
                            .unwrap_or_else(|message| Err(ActorError::Panic(message)))
                    }
                }
            };
            control.finish_initialization();
            if let Err(error) = streams_initialized {
                let error = crate::error_receipt::ActorReceipt::from_error(error);
                let deadline = tokio::time::Instant::now() + control.timeout;
                let resume = parrot_api::trace::scope(
                    initialization_context,
                    control
                        .lifetime
                        .resume_message(error.clone(), deadline, false, &runtime),
                )
                .await;
                if !resume {
                    match restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                        Restart::Ready => continue,
                        Restart::Stopped => return Ok(()),
                        Restart::Declined => {}
                    }
                }
                return Err(crate::supervision_policy::reply_error(error));
            }
            drop(initialization_context);
            let mut prefer_stream = true;
            loop {
                let work = match control
                    .wait_typed_stage(
                        async {
                            let receiver = inputs.receiver.as_ref().expect("typed mailbox");
                            if inputs.local_streams.is_empty() {
                                super::LocalWork::Message(receiver.recv_async().await)
                            } else if prefer_stream {
                                tokio::select! {
                                    biased;
                                    event = inputs.local_streams.next_event() => {
                                        prefer_stream = false;
                                        super::LocalWork::Stream(event)
                                    },
                                    message = receiver.recv_async() => {
                                        prefer_stream = true;
                                        super::LocalWork::Message(message)
                                    },
                                }
                            } else {
                                tokio::select! {
                                    biased;
                                    message = receiver.recv_async() => {
                                        prefer_stream = true;
                                        super::LocalWork::Message(message)
                                    },
                                    event = inputs.local_streams.next_event() => {
                                        prefer_stream = false;
                                        super::LocalWork::Stream(event)
                                    },
                                }
                            }
                        },
                        &runtime,
                    )
                    .await
                {
                    Step::Stopped => break 'generation,
                    Step::Recovery(group) => {
                        if rebuild_group(&mut inputs, factory, group, &runtime).await? {
                            continue 'generation;
                        }
                        return Ok(());
                    }
                    Step::Escalated(error) => {
                        let deadline = control
                            .cleanup_deadline()
                            .unwrap_or_else(|| tokio::time::Instant::now() + control.timeout);
                        match restart(&mut inputs, factory, error.clone(), deadline, &runtime)
                            .await?
                        {
                            Restart::Ready => continue 'generation,
                            Restart::Stopped => return Ok(()),
                            Restart::Declined => {
                                return Err(crate::supervision_policy::reply_error(error));
                            }
                        }
                    }
                    Step::Ready(work) => work,
                };
                if let super::LocalWork::Stream(Some(event)) = work {
                    let dispatched = match control
                        .wait_typed_stage(
                            super::dispatch_local_event(
                                inputs.actor.as_mut().expect("live actor"),
                                &mut inputs.local_streams,
                                event,
                                &context,
                                &mut inputs.cleanup_error,
                                &mut inputs.dispatch_cancel_failed,
                                control.message_timeout,
                            ),
                            &runtime,
                        )
                        .await
                    {
                        Step::Stopped => break 'generation,
                        Step::Recovery(group) => {
                            if rebuild_group(&mut inputs, factory, group, &runtime).await? {
                                continue 'generation;
                            }
                            return Ok(());
                        }
                        Step::Escalated(error) => Err(error),
                        Step::Ready(result) => {
                            result.map_err(crate::error_receipt::ActorReceipt::from_error)
                        }
                    };
                    if let Err(error) = dispatched {
                        let deadline = control
                            .lifetime
                            .decision_deadline()
                            .unwrap_or_else(|| tokio::time::Instant::now() + control.timeout);
                        match restart(&mut inputs, factory, error.clone(), deadline, &runtime)
                            .await?
                        {
                            Restart::Ready => continue 'generation,
                            Restart::Stopped => return Ok(()),
                            Restart::Declined => {
                                return Err(crate::supervision_policy::reply_error(error));
                            }
                        }
                    }
                    continue;
                }
                let message = match work {
                    super::LocalWork::Message(Ok(message)) => message,
                    super::LocalWork::Message(Err(_)) | super::LocalWork::Stream(None) => {
                        break 'generation;
                    }
                    super::LocalWork::Stream(Some(_)) => {
                        unreachable!("local stream event handled above")
                    }
                };
                control.channel.notify_capacity();
                let dispatched = match control
                    .wait_typed_stage(
                        super::dispatch(
                            inputs.actor.as_mut().expect("live actor"),
                            message,
                            &control.lifetime,
                            &runtime,
                            control.timeout,
                            &context,
                            DispatchCleanup::new(
                                &mut inputs.cleanup_error,
                                &mut inputs.dispatch_cancel_failed,
                            )
                            .with_timeout(control.message_timeout),
                        ),
                        &runtime,
                    )
                    .await
                {
                    Step::Stopped => break 'generation,
                    Step::Recovery(group) => {
                        if rebuild_group(&mut inputs, factory, group, &runtime).await? {
                            continue 'generation;
                        }
                        return Ok(());
                    }
                    Step::Escalated(error) => Err(error),
                    Step::Ready(result) => result,
                };
                if let Err(error) = dispatched {
                    let deadline = control
                        .lifetime
                        .decision_deadline()
                        .unwrap_or_else(|| tokio::time::Instant::now() + control.timeout);
                    match restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                        Restart::Ready => continue 'generation,
                        Restart::Stopped => return Ok(()),
                        Restart::Declined => {
                            return Err(crate::supervision_policy::reply_error(error));
                        }
                    }
                }
                reductions += 1;
                if reductions >= 10 {
                    reductions = 0;
                    tokio::task::yield_now().await;
                }
            }
        }
        if let Some(error) = control.escalated.lock().unwrap().clone() {
            return Err(crate::supervision_policy::reply_error(error));
        }
        if control.lifetime.decision_deadline().is_some() {
            return Ok(());
        }
        control.typed_tree().join().await?;
        let context = control.typed_tree().context();
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
            result = parrot_api::trace::scope(control.take_stop_context(), recorded_lifecycle_stage(
                parrot_api::trace::SpanKind::ActorBeforeStop,
                context.path(),
                || inputs.actor.as_mut().expect("initialized actor").before_stop(&context),
                DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed),
            )) => result.result.unwrap_or_else(|message| Err(ActorError::Panic(message))),
        }
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|panic| Err(ActorError::Panic(panic_message(panic))));
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
