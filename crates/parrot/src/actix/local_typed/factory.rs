//! Repeatable Local actors retain every instance on their original arbiter.
use super::*;
use crate::actix::lifecycle::ActorExit;
use crate::owned_payload::OwnedPayload;
use parrot_api::supervisor::{
    BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
};
use std::time::Duration;

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

// Acquire bounded constructor capacity without moving its output to a worker.
// Once the synchronous call starts, the independent monitor may time out but
// this arbiter still owns the late result. The caller installs it in TypedInputs
// before checking deadlines, so all exit paths isolate its actual destructor.
async fn prepare<A, F>(
    factory: &OwnedPayload<F>,
    control: &Arc<ActorControl>,
    slots: Arc<tokio::sync::Semaphore>,
    deadline: tokio::time::Instant,
    group: Option<&crate::constructor::ConstructionControl>,
) -> ActorResult<Option<A>>
where
    A: LocalTypedDispatch,
    F: Fn() -> A + Send + 'static,
{
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
        .map(Some)
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
        actor = prepare(factory, &control, inputs.constructor_slots.clone(), deadline, None) => actor?,
    };
    inputs.actor = actor;
    // A cohort may be assigned while the synchronous constructor was blocked.
    // That old output must retire with all other old generations before init.
    if let Some(group) = control.lifetime.recovery() {
        return rebuild_group(inputs, factory, group, runtime).await;
    }
    if inputs.actor.is_none() {
        return Ok(false);
    }
    control.construction_complete(deadline)
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
    control.begin_construction(group.deadline);
    let member = control.lifetime.id;
    let work = async {
        retire_generation(inputs).await?;
        group
            .arrive(member, Step::Retired)
            .await
            .map_err(ActorError::InternalError)?;
        inputs.actor = parrot_api::trace::scope(
            group.failure_context.clone(),
            prepare(
                factory,
                &control,
                inputs.constructor_slots.clone(),
                group.deadline,
                Some(&group.control),
            ),
        )
        .await?;
        if inputs.actor.is_none() {
            return Err(ActorError::Stopped);
        }
        group
            .arrive(member, Step::Prepared)
            .await
            .map_err(ActorError::InternalError)?;
        if group.control.is_cancelled() || control.channel.is_sealed() {
            return Err(ActorError::Stopped);
        }
        control.escalated.lock().unwrap().take();
        control.typed_tree().next_generation()?;
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
        result = &mut work => result,
        _ = control.channel.closed() => { group.abort("explicit stop cancelled Local cohort"); work.await },
        _ = tokio::time::sleep_until(group.deadline) => { group.abort("Local cohort recovery deadline elapsed"); control.begin_cleanup(group.deadline); work.await },
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
async fn restart<A: LocalTypedDispatch, F: Fn() -> A + Send + 'static>(
    inputs: &mut TypedInputs<A>,
    factory: &crate::owned_payload::OwnedPayload<F>,
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
            return rebuild_group(inputs, factory, group, runtime).await;
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
        construct(inputs, factory, deadline, runtime).await
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
    use crate::typed_supervision::{DispatchCleanup, Step, guarded_stage};
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
                tokio::time::Instant::now() + control.timeout,
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
            let initialization_context = inputs.creation_context.take();
            let initialized = match control
                .wait_typed_stage(
                    parrot_api::trace::scope(
                        initialization_context.clone(),
                        guarded_stage(
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
                            ),
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
                    if restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                        continue 'generation;
                    }
                    return Err(crate::supervision_policy::reply_error(error));
                }
                Step::Ready(result) => {
                    result.unwrap_or_else(|message| Err(ActorError::Panic(message)))
                }
            };
            if let Err(error) = initialized {
                let error = Arc::new(error);
                let deadline = tokio::time::Instant::now() + control.timeout;
                let resume = parrot_api::trace::scope(
                    initialization_context,
                    control
                        .lifetime
                        .resume_message(error.clone(), deadline, false, &runtime),
                )
                .await;
                if !resume
                    && restart(&mut inputs, factory, error.clone(), deadline, &runtime).await?
                {
                    continue;
                }
                return Err(crate::supervision_policy::reply_error(error));
            }
            let streams_initialized = {
                let mut stream_context = LocalStreamContext::new(&mut inputs.local_streams);
                match control
                    .wait_typed_stage(
                        parrot_api::trace::scope(
                            initialization_context.clone(),
                            guarded_stage(
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
                                ),
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
                        if restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                            continue 'generation;
                        }
                        return Err(crate::supervision_policy::reply_error(error));
                    }
                    Step::Ready(result) => {
                        result.unwrap_or_else(|message| Err(ActorError::Panic(message)))
                    }
                }
            };
            if let Err(error) = streams_initialized {
                let error = Arc::new(error);
                let deadline = tokio::time::Instant::now() + control.timeout;
                let resume = parrot_api::trace::scope(
                    initialization_context,
                    control
                        .lifetime
                        .resume_message(error.clone(), deadline, false, &runtime),
                )
                .await;
                if !resume
                    && restart(&mut inputs, factory, error.clone(), deadline, &runtime).await?
                {
                    continue;
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
                        if restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                            continue 'generation;
                        }
                        return Err(crate::supervision_policy::reply_error(error));
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
                        Step::Ready(result) => result.map_err(Arc::new),
                    };
                    if let Err(error) = dispatched {
                        let deadline = control
                            .lifetime
                            .decision_deadline()
                            .unwrap_or_else(|| tokio::time::Instant::now() + control.timeout);
                        if restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                            continue 'generation;
                        }
                        return Err(crate::supervision_policy::reply_error(error));
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
                            ),
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
                    if restart(&mut inputs, factory, error.clone(), deadline, &runtime).await? {
                        continue 'generation;
                    }
                    return Err(crate::supervision_policy::reply_error(error));
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
            result = guarded_stage(
                || inputs.actor.as_mut().expect("initialized actor").before_stop(&context),
                DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed),
            ) => result.unwrap_or_else(|message| Err(ActorError::Panic(message))),
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
