//! Local actor construction and execution on a registered Actix owner thread.
// Root and parent-owned child construction share one owner-thread boundary.
// Repeatable constructors never move their non-Send outputs off that owner.
mod factory;

pub(crate) struct LocalParent<'a> {
    pub scope: &'a Arc<crate::children::ChildScope>,
    pub identity: Option<std::sync::Weak<dyn parrot_api::address::ActorRef>>,
}

pub(super) struct LocalSource<'a, A, M>
where
    A: LocalTypedDispatch,
    M: ParrotMsgVariant<LocalProtocol<A>>,
{
    pub path: &'a str,
    pub parent: Option<LocalParent<'a>>,
    pub repeatable: bool,
    pub erase: fn(LocalTypedActorRef<A, M>) -> parrot_api::types::BoxedActorRef,
}
use crate::actix::{
    lifecycle::{ActorControl, Outcome, panic_message},
    system::{ActixActorSystem, Registration},
};
use futures::FutureExt;
use parrot_api::local_typed::{
    LocalProtocol, LocalStreamContext, LocalStreamEvent, LocalStreamEventKind, LocalStreamRegistry,
    LocalTypedActorRef, LocalTypedDispatch,
};
use parrot_api::typed_channel::{TypedActorRef, TypedEnvelope};
use parrot_api::{errors::ActorError, typed::ParrotMsgVariant, types::ActorResult};
use std::sync::Arc;

// Own the live instance and mailbox outside every user future. Tokio may drop
// the whole execution future when an arbiter stops; poll-level catch_unwind
// does not run the cleanup code after an interrupted await.
struct TypedInputs<A: LocalTypedDispatch> {
    actor: Option<A>,
    creation_context: Option<parrot_api::trace::TraceContext>,
    receiver: Option<flume::Receiver<TypedEnvelope<LocalProtocol<A>>>>,
    local_streams: LocalStreamRegistry,
    cleanup_error: Option<crate::error_receipt::ActorReceipt>,
    dispatch_cancel_failed: bool,
    control: Arc<ActorControl>,
    constructor_slots: Arc<tokio::sync::Semaphore>,
    policy_slots: Arc<tokio::sync::Semaphore>,
    finished: Option<tokio::sync::oneshot::Sender<Outcome>>,
}
impl<A: LocalTypedDispatch> TypedInputs<A> {
    fn retire_streams(&mut self) -> ActorResult<()> {
        let streams = std::mem::take(&mut self.local_streams);
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(streams))) {
            Ok(()) => Ok(()),
            Err(payload) => Err(ActorError::Panic(panic_message(payload))),
        }
    }

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
        if let Err(error) = self.retire_streams()
            && cleanup.is_ok()
        {
            cleanup = Err(error);
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
impl<A: LocalTypedDispatch> Drop for TypedInputs<A> {
    fn drop(&mut self) {
        if self.finished.is_some() {
            // Separate unwind boundaries keep one destructor from skipping
            // another, including during cancellation of a user future.
            let _ = self.retire();
        }
    }
}

async fn run_actor<A: LocalTypedDispatch>(
    mut inputs: TypedInputs<A>,
    control: Arc<ActorControl>,
    runtime: tokio::runtime::Handle,
) -> crate::actix::lifecycle::ActorExit {
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
        let initialization_context = inputs.creation_context.take();
        match control
            .wait_typed_stage(parrot_api::trace::scope(initialization_context.clone(), crate::typed_supervision::guarded_stage(|| actor.init(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed))), &runtime)
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
        {
            let mut stream_context = LocalStreamContext::new(&mut inputs.local_streams);
            match control
                .wait_typed_stage(
                    parrot_api::trace::scope(
                        initialization_context.clone(),
                        crate::typed_supervision::guarded_stage(
                            || actor.init_local_streams(&context, &mut stream_context),
                            crate::typed_supervision::DispatchCleanup::new(
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
                Step::Ready(result) => result
                    .unwrap_or_else(|message| Err(ActorError::Panic(message)))
                    .inspect_err(|_| {
                        control
                            .supervision_failed
                            .store(true, std::sync::atomic::Ordering::Release);
                    })?,
            }
        }
        drop(initialization_context);
        let mut reductions = 0;
        let mut prefer_stream = true;
        loop {
            let work = match control
                .wait_typed_stage(
                    async {
                        if inputs.local_streams.is_empty() {
                            LocalWork::Message(receiver.recv_async().await)
                        } else if prefer_stream {
                            // Alternate the priority when both sources are
                            // ready. This gives a continuously-ready stream
                            // progress without starving the typed mailbox.
                            tokio::select! {
                                biased;
                                event = inputs.local_streams.next_event() => {
                                    prefer_stream = false;
                                    LocalWork::Stream(event)
                                },
                                message = receiver.recv_async() => {
                                    prefer_stream = true;
                                    LocalWork::Message(message)
                                },
                            }
                        } else {
                            tokio::select! {
                                biased;
                                message = receiver.recv_async() => {
                                    prefer_stream = true;
                                    LocalWork::Message(message)
                                },
                                event = inputs.local_streams.next_event() => {
                                    prefer_stream = false;
                                    LocalWork::Stream(event)
                                },
                            }
                        }
                    },
                    &runtime,
                )
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
                Step::Ready(work) => work,
            };
            if let LocalWork::Stream(Some(event)) = work {
                match control.wait_typed_stage(
                    dispatch_local_event(
                        actor,
                        &mut inputs.local_streams,
                        event,
                        &context,
                        &mut inputs.cleanup_error,
                        &mut inputs.dispatch_cancel_failed,
                    ),
                    &runtime,
                ).await {
                    Step::Stopped => break,
                    Step::Escalated(error) => {
                        was_escalated = true;
                        return Err(crate::supervision_policy::reply_error(error));
                    }
                    Step::Recovery(group) => {
                        group.abort("non-repeatable parent cannot recover");
                        return Err(ActorError::InternalError("non-repeatable parent cannot recover".into()));
                    }
                    Step::Ready(Err(error)) => {
                        control.supervision_failed.store(true, std::sync::atomic::Ordering::Release);
                        return Err(crate::supervision_policy::reply_error(Arc::new(error)));
                    }
                    Step::Ready(Ok(())) => {}
                }
                continue;
            }
            let envelope = match work {
                LocalWork::Message(Ok(envelope)) => envelope,
                LocalWork::Message(Err(_)) | LocalWork::Stream(None) => break,
                LocalWork::Stream(Some(_)) => unreachable!("local stream event handled above"),
            };
            control.channel.notify_capacity();
            match control
                .wait_typed_stage(
                    dispatch(
                        actor,
                        envelope,
                        &control.lifetime,
                        &runtime,
                        control.timeout,
                        &context,
                        crate::typed_supervision::DispatchCleanup::new(
                            &mut inputs.cleanup_error,
                            &mut inputs.dispatch_cancel_failed,
                        ),
                    ),
                    &runtime,
                )
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
            reductions += 1;
            if reductions >= 10 {
                reductions = 0;
                tokio::task::yield_now().await;
            }
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
            result = crate::typed_supervision::guarded_stage(|| actor.before_stop(&context), crate::typed_supervision::DispatchCleanup::new(&mut inputs.cleanup_error, &mut inputs.dispatch_cancel_failed)) => result.unwrap_or_else(|message| Err(ActorError::Panic(message))),
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

enum LocalWork<A: LocalTypedDispatch> {
    Message(Result<TypedEnvelope<LocalProtocol<A>>, flume::RecvError>),
    Stream(Option<LocalStreamEvent>),
}

fn dispatch_local_event<A: LocalTypedDispatch>(
    actor: &mut A,
    streams: &mut LocalStreamRegistry,
    event: LocalStreamEvent,
    context: &parrot_api::typed_context::TypedContext,
    cleanup_error: &mut Option<crate::error_receipt::ActorReceipt>,
    dispatch_cancel_failed: &mut bool,
) -> impl std::future::Future<Output = ActorResult<()>> {
    let parent = event.trace_context();
    parrot_api::trace::scope(parent.as_ref().map(|parent| parent.child()), async move {
        struct EventGuard<'a> {
            event: Option<LocalStreamEvent>,
            cleanup_error: &'a mut Option<crate::error_receipt::ActorReceipt>,
            local_cleanup: Option<crate::error_receipt::ActorReceipt>,
        }
        impl EventGuard<'_> {
            fn record_cleanup(&mut self, payload: Box<dyn std::any::Any + Send>) -> ActorError {
                let message = panic_message(payload);
                self.local_cleanup.get_or_insert_with(|| {
                    crate::error_receipt::ActorReceipt::from_error(ActorError::Panic(
                        message.clone(),
                    ))
                });
                ActorError::Panic(message)
            }
            fn record_local_cleanup(&mut self) {
                if self.local_cleanup.is_some()
                    && let Some(event) = self.event.as_mut()
                {
                    event.record_callback_result(&Err(ActorError::Panic(String::new())));
                }
            }
            fn complete(
                &mut self,
                streams: &mut LocalStreamRegistry,
                requeue: bool,
            ) -> ActorResult<()> {
                self.record_local_cleanup();
                let event = self.event.take().expect("local event guard");
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    event.complete(streams, requeue)
                }))
                .map_err(|payload| self.record_cleanup(payload))
            }
        }
        impl Drop for EventGuard<'_> {
            fn drop(&mut self) {
                // Cancellation drops the callback future before this guard.
                // Its private cleanup receipt belongs to this event only.
                self.record_local_cleanup();
                if let Some(event) = self.event.take()
                    && let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(event)))
                {
                    self.record_cleanup(payload);
                }
                if let Some(error) = self.local_cleanup.take() {
                    self.cleanup_error.get_or_insert(error);
                }
            }
        }
        let mut guard = EventGuard {
            event: Some(event),
            cleanup_error,
            local_cleanup: None,
        };
        let event = guard.event.as_mut().expect("local event guard");
        let id = event.id();
        let kind = event.kind();
        let source_error = kind == LocalStreamEventKind::Error;
        let result = match kind {
            LocalStreamEventKind::Started => crate::typed_supervision::recorded_stream_stage(
                parrot_api::trace::SpanKind::StreamStarted,
                context.path(),
                || actor.local_stream_started(id, context),
                crate::typed_supervision::DispatchCleanup::new(
                    &mut guard.local_cleanup,
                    dispatch_cancel_failed,
                ),
            )
            .await
            .unwrap_or_else(|message| Err(ActorError::Panic(message))),
            LocalStreamEventKind::Item => crate::typed_supervision::recorded_stream_stage(
                parrot_api::trace::SpanKind::StreamItem,
                context.path(),
                || {
                    actor.local_stream_item(
                        id,
                        guard
                            .event
                            .as_mut()
                            .expect("local event guard")
                            .take_item()
                            .expect("local item event"),
                        context,
                    )
                },
                crate::typed_supervision::DispatchCleanup::new(
                    &mut guard.local_cleanup,
                    dispatch_cancel_failed,
                ),
            )
            .await
            .unwrap_or_else(|message| Err(ActorError::Panic(message))),
            LocalStreamEventKind::Finished => crate::typed_supervision::recorded_stream_stage(
                parrot_api::trace::SpanKind::StreamFinished,
                context.path(),
                || actor.local_stream_finished(id, context),
                crate::typed_supervision::DispatchCleanup::new(
                    &mut guard.local_cleanup,
                    dispatch_cancel_failed,
                ),
            )
            .await
            .unwrap_or_else(|message| Err(ActorError::Panic(message))),
            LocalStreamEventKind::Error => {
                let error = guard
                    .event
                    .as_mut()
                    .expect("local event guard")
                    .take_error()
                    .expect("local stream error event");
                let hook_error = ActorError::InternalError(error.to_string());
                let hook = crate::typed_supervision::recorded_stream_stage(
                    parrot_api::trace::SpanKind::StreamError,
                    context.path(),
                    || actor.local_stream_error(id, hook_error, context),
                    crate::typed_supervision::DispatchCleanup::new(
                        &mut guard.local_cleanup,
                        dispatch_cancel_failed,
                    ),
                )
                .await
                .unwrap_or_else(|message| Err(ActorError::Panic(message)));
                if let Err(hook_error) = hook {
                    Err(hook_error)
                } else {
                    Err(error)
                }
            }
        };
        guard
            .event
            .as_mut()
            .expect("local event guard")
            .record_callback_result(&result);
        match result {
            Ok(()) => guard.complete(streams, kind != LocalStreamEventKind::Finished),
            Err(error) => {
                if !source_error {
                    let hook_error = ActorError::InternalError(error.to_string());
                    let hook = parrot_api::trace::scope(
                        parent.map(|parent| parent.child()),
                        crate::typed_supervision::recorded_stream_stage(
                            parrot_api::trace::SpanKind::StreamError,
                            context.path(),
                            || actor.local_stream_error(id, hook_error, context),
                            crate::typed_supervision::DispatchCleanup::new(
                                &mut guard.local_cleanup,
                                dispatch_cancel_failed,
                            ),
                        ),
                    )
                    .await
                    .unwrap_or_else(|message| Err(ActorError::Panic(message)));
                    guard
                        .event
                        .as_mut()
                        .expect("local event guard")
                        .record_callback_result(&hook);
                }
                let _ = guard.complete(streams, false);
                Err(error)
            }
        }
    })
}

async fn dispatch<A: LocalTypedDispatch>(
    actor: &mut A,
    envelope: TypedEnvelope<LocalProtocol<A>>,
    lifetime: &Arc<crate::children::ChildLifetime>,
    runtime: &tokio::runtime::Handle,
    timeout: std::time::Duration,
    context: &parrot_api::typed_context::TypedContext,
    cleanup: crate::typed_supervision::DispatchCleanup<'_>,
) -> Result<(), Arc<ActorError>> {
    let TypedEnvelope {
        msg,
        reply,
        trace_context,
    } = envelope;
    parrot_api::trace::scope(
        trace_context.map(|parent| parent.child()),
        crate::typed_supervision::dispatch_stage(
            context.path(),
            || actor.dispatch(msg, context),
            reply,
            lifetime,
            runtime,
            timeout,
            cleanup,
        ),
    )
    .await
}

// This owner crosses to the arbiter before an A exists. Only Send protocol
// values are captured; no actor or local future is smuggled across a thread.
struct Queued<A: LocalTypedDispatch> {
    receiver: Option<flume::Receiver<TypedEnvelope<LocalProtocol<A>>>>,
    creation_context: Option<parrot_api::trace::TraceContext>,
    control: Arc<ActorControl>,
    constructor_slots: Arc<tokio::sync::Semaphore>,
    policy_slots: Arc<tokio::sync::Semaphore>,
    finished: Option<tokio::sync::oneshot::Sender<Outcome>>,
}
impl<A: LocalTypedDispatch> Queued<A> {
    fn activate(mut self) -> TypedInputs<A> {
        TypedInputs {
            actor: None,
            creation_context: self.creation_context.take(),
            receiver: self.receiver.take(),
            local_streams: LocalStreamRegistry::new(),
            cleanup_error: None,
            dispatch_cancel_failed: false,
            control: self.control.clone(),
            constructor_slots: self.constructor_slots.clone(),
            policy_slots: self.policy_slots.clone(),
            finished: self.finished.take(),
        }
    }
}
impl<A: LocalTypedDispatch> Drop for Queued<A> {
    fn drop(&mut self) {
        if self.finished.is_none() {
            return;
        }
        self.control
            .begin_cleanup(tokio::time::Instant::now() + self.control.timeout);
        let mut result = Ok(());
        if let Some(receiver) = self.receiver.take() {
            while let Ok(envelope) = receiver.try_recv() {
                if let Err(payload) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(envelope)))
                {
                    let error = ActorError::Panic(panic_message(payload));
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
        }
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(Outcome::from_result(result));
        }
    }
}

async fn run_source<A, F>(
    queued: Queued<A>,
    factory: crate::owned_payload::OwnedPayload<F>,
    policy: crate::owned_payload::OwnedPayload<
        Option<parrot_api::supervisor::SupervisorStrategyType>,
    >,
    control: Arc<ActorControl>,
    runtime: tokio::runtime::Handle,
    repeatable: bool,
) -> crate::actix::lifecycle::ActorExit
where
    A: LocalTypedDispatch,
    F: Fn() -> A + Send + 'static,
{
    let mut inputs = queued.activate();
    control.lifetime.set_strategy(policy.take());
    if repeatable {
        let mut exit = factory::run(inputs, &factory, control, runtime).await;
        exit.cleanup = exit.cleanup.and(factory.retire().await);
        return exit;
    }
    let constructed = if control.channel.is_sealed() {
        Ok(None)
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parrot_api::trace::in_scope(inputs.creation_context.clone(), || factory.value()())
        }))
        .map(Some)
        .map_err(|payload| ActorError::Panic(panic_message(payload)))
    };
    let construction = match constructed {
        Ok(actor) => {
            inputs.actor = actor;
            Ok(())
        }
        Err(error) => Err(error),
    };
    // The Send constructor's captures have their own cleanup owner, independent
    // of the local result. A capture Drop failure cannot publish a live actor.
    let capture_cleanup = factory.retire().await;
    if construction.is_ok()
        && capture_cleanup.is_ok()
        && inputs.actor.is_some()
        && !control.channel.is_sealed()
    {
        return run_actor(inputs, control, runtime).await;
    }
    let cleanup = inputs.retire();
    let outcome = construction.and(capture_cleanup);
    if outcome.is_err() {
        control
            .supervision_failed
            .store(true, std::sync::atomic::Ordering::Release);
    }
    crate::actix::lifecycle::ActorExit { outcome, cleanup }
}

impl ActixActorSystem {
    /// Construct a local actor on its owning arbiter and admit its typed mailbox.
    ///
    /// Only the constructor and Send messages cross threads. Actor state and
    /// handler futures may be !Send; construction, polling and actor destruction
    /// stay on one arbiter. Success acknowledges admission, not initialization.
    /// Queued messages wait for construction and initialization. The constructor
    /// is called at most once; this entry does not promise factory restart.
    ///
    /// Unpolled cancellation retires constructor captures without constructing an
    /// actor. Stops and shutdown join real cleanup; a synchronous constructor or
    /// destructor cannot be forcibly interrupted, and a stop timeout does not
    /// release the owner's responsibility for its later retirement.
    pub fn spawn_local_typed<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
    ) -> impl std::future::Future<Output = ActorResult<LocalTypedActorRef<A, M>>> + Send + 'a
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        let factory =
            crate::owned_payload::OwnedPayload::new(factory, &self.runtime, &self.cleanup_slots);
        let policy =
            crate::owned_payload::OwnedPayload::new(None, &self.runtime, &self.cleanup_slots);
        parrot_api::trace::capture(
            self.spawn_local_typed_owned_source(factory, policy, path, None, false),
        )
    }

    /// One-shot Local root with an explicit supervision policy. Resume retains
    /// its state; Restart reports that no repeatable factory is available.
    pub fn spawn_local_typed_with_strategy<'a, A, M, F>(
        self: &'a Arc<Self>,
        factory: F,
        path: &'a str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> impl std::future::Future<Output = ActorResult<LocalTypedActorRef<A, M>>> + Send + 'a
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        let factory =
            crate::owned_payload::OwnedPayload::new(factory, &self.runtime, &self.cleanup_slots);
        let policy = crate::owned_payload::OwnedPayload::new(
            Some(strategy),
            &self.runtime,
            &self.cleanup_slots,
        );
        parrot_api::trace::capture(
            self.spawn_local_typed_owned_source(factory, policy, path, None, false),
        )
    }

    pub(crate) async fn spawn_local_typed_owned_source<A, M, F>(
        self: &Arc<Self>,
        factory: crate::owned_payload::OwnedPayload<F>,
        policy: crate::owned_payload::OwnedPayload<
            Option<parrot_api::supervisor::SupervisorStrategyType>,
        >,
        path: &str,
        parent: Option<&crate::typed_context::ManagedContext>,
        repeatable: bool,
    ) -> ActorResult<LocalTypedActorRef<A, M>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        self.spawn_local_source(
            factory,
            policy,
            LocalSource {
                path,
                parent: parent.map(|p| LocalParent {
                    scope: &p.scope,
                    identity: p.own(),
                }),
                repeatable,
                erase: |reference| reference.into_dyn(),
            },
        )
        .await
    }

    pub(super) async fn spawn_local_source<A, M, F>(
        self: &Arc<Self>,
        factory: crate::owned_payload::OwnedPayload<F>,
        policy: crate::owned_payload::OwnedPayload<
            Option<parrot_api::supervisor::SupervisorStrategyType>,
        >,
        source: LocalSource<'_, A, M>,
    ) -> ActorResult<LocalTypedActorRef<A, M>>
    where
        A: LocalTypedDispatch,
        M: ParrotMsgVariant<LocalProtocol<A>>,
        F: Fn() -> A + Send + 'static,
    {
        let creation_context = parrot_api::trace::TraceContext::current();
        let LocalSource {
            path,
            parent,
            repeatable,
            erase,
        } = source;
        let receipt = factory.receipt();
        let policy_receipt = policy.receipt();
        let mut published_receipt = Some(factory.receipt());
        let mut published_policy = Some(policy.receipt());
        let mut factory = Some(factory);
        let mut policy = Some(policy);
        let mut admit = |lifetime: Option<Arc<crate::children::ChildLifetime>>| {
            parrot_api::engine::SpawnOptions::at(path)
                .validate()
                .map_err(|error| ActorError::InternalError(error.to_string()))?;
            let (sender, receiver) = parrot_api::typed_channel::typed_channel::<LocalProtocol<A>>(
                self.settings.mailbox_capacity,
            );
            let (control, finished) = ActorControl::with_lifetime(
                self.settings.shutdown_timeout,
                self.settings.max_context_tasks,
                lifetime,
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
                    parent: parent.as_ref().and_then(|parent| parent.identity.clone()),
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
                .unwrap_or_else(|_| unreachable!("one local tree"));
            if repeatable {
                let weak = Arc::downgrade(&control);
                control.lifetime.bind_recovery(Arc::new(move |group| {
                    if let Some(control) = weak.upgrade() {
                        if control.channel.is_sealed() {
                            group.abort("Local factory member is stopping");
                        }
                        control.begin_construction(group.deadline);
                        control.typed_escalation_changed.notify_waiters();
                    } else {
                        group.abort("Local factory member owner disappeared");
                    }
                }));
            }
            let actor_ref = TypedActorRef::new(sender, path.to_owned())
                .with_lifecycle(control.clone(), self.settings.ask_timeout);
            actor_ref.install_termination_recipient()?;
            let erased: Arc<dyn parrot_api::address::ActorRef> =
                Arc::from(erase(actor_ref.clone()));
            own.set(Arc::downgrade(&erased))
                .unwrap_or_else(|_| unreachable!("one local identity"));
            let mut registry = self.registry.write().unwrap();
            if registry.shutting_down {
                return Err(ActorError::Stopped);
            }
            if registry.actors.contains_key(path) {
                return Err(ActorError::InternalError(format!(
                    "actor path already exists: {path}"
                )));
            }
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            let (retired_tx, retired_rx) = tokio::sync::oneshot::channel();
            // Repeatable sources retire captures after the final instance. The
            // completed execution receipt preserves an earlier hook/instance
            // failure when independent capture disposal also fails later.
            let (execution_cleanup, execution_receipt) = if repeatable {
                let (send, receive) = tokio::sync::oneshot::channel();
                (Some(send), Some(receive))
            } else {
                (None, None)
            };
            let queued = Queued::<A> {
                receiver: Some(receiver),
                creation_context: creation_context.clone(),
                control: control.clone(),
                constructor_slots: self.constructor_slots.clone(),
                policy_slots: self.policy_slots.clone(),
                finished: Some(retired_tx),
            };
            let source = factory.take().expect("one admitted constructor");
            let source_policy = policy.take().expect("one admitted local policy");
            let task_control = control.clone();
            let runtime = self.runtime.clone();
            if !self.arbiter_pool().next_arbiter().spawn_fn(move || {
                // spawn_fn transfers a Send closure; the !Send future is
                // created only after that closure arrives on the owner.
                actix::spawn(async move {
                    let exit = std::panic::AssertUnwindSafe(run_source(
                        queued,
                        source,
                        source_policy,
                        task_control,
                        runtime,
                        repeatable,
                    ))
                    .catch_unwind()
                    .await;
                    let exit = match exit {
                        Ok(mut exit) => {
                            if let Some(send) = execution_cleanup {
                                let cleanup = Outcome::from_result(exit.cleanup);
                                exit.cleanup = cleanup.clone().result();
                                let _ = send.send(cleanup);
                            }
                            exit
                        }
                        Err(payload) => crate::actix::lifecycle::ActorExit {
                            outcome: Err(ActorError::Panic(panic_message(payload))),
                            cleanup: Err(ActorError::InternalError(
                                "local execution ended without cleanup outcome".into(),
                            )),
                        },
                    };
                    let _ = result_tx.send(exit);
                });
            }) {
                return Err(ActorError::InternalError(
                    "Actix arbiter rejected local actor execution".into(),
                ));
            }
            registry.actors.insert(
                path.to_owned(),
                Registration {
                    actor_ref: erased,
                    control: control.clone(),
                },
            );
            let unpublished = factory_receipt(
                published_receipt.take().expect("one constructor receipt"),
                published_policy.take().expect("one policy receipt"),
                retired_rx,
                tree,
                execution_receipt,
            );
            self.monitor_actor_with_disposal(
                path.to_owned(),
                control,
                finished,
                result_rx,
                Some(Box::pin(unpublished)),
                None,
            );
            Ok(actor_ref)
        };
        let admission = if let Some(parent) = &parent {
            let mut reference = None;
            let result = parent.scope.spawn(|lifetime| {
                let actor = admit(Some(lifetime.clone()))?;
                let erased = erase(actor.clone());
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
            result.map(|_| reference.expect("admitted Local child"))
        } else {
            admit(None)
        };
        match admission {
            Ok(reference) => Ok(reference),
            Err(error) => {
                drop(factory);
                drop(policy);
                let (factory, policy) = tokio::join!(receipt, policy_receipt);
                factory.and(policy)?;
                Err(error)
            }
        }
    }
}

async fn factory_receipt(
    receipt: parrot_api::types::BoxedFuture<'static, ActorResult<()>>,
    policy: parrot_api::types::BoxedFuture<'static, ActorResult<()>>,
    retired: tokio::sync::oneshot::Receiver<Outcome>,
    tree: Arc<crate::typed_context::TypedTree>,
    execution: Option<tokio::sync::oneshot::Receiver<Outcome>>,
) -> ActorResult<()> {
    let execution = async {
        match execution {
            Some(receipt) => receipt.await.ok(),
            None => None,
        }
    };
    let (factory, policy, retired, children, execution) =
        tokio::join!(receipt, policy, retired, tree.join(), execution);
    let retired = retired
        .map_err(|_| ActorError::InternalError("local input retirement receipt lost".into()))?
        .result();
    match execution {
        Some(execution) => execution
            .result()
            .and(retired)
            .and(children)
            .and(factory)
            .and(policy),
        // Cancellation may skip the final execution receipt. The independent
        // input receipts still prove actual disposal and preserve its errors.
        None => factory.and(policy).and(retired).and(children),
    }
}
