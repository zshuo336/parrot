use crate::actix::{
    context::ActixContext,
    lifecycle::{ActorControl, panic_message},
    message::ActixMessageWrapper,
    reference::{ActixActorRef, StopMessage},
};
use actix::fut::{ActorFutureExt, WrapFuture};
use actix::prelude::AtomicResponse;
use actix::{Actor as ActixActorTrait, ActorContext as _, AsyncContext, Context, Handler, Running};
use futures::FutureExt;
use parrot_api::{
    actor::{Actor as ParrotActor, ActorState, EmptyConfig, EngineContextHandle},
    address::ActorPath,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage, WeakActorTarget},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) type ActorOutcome = Arc<Mutex<Option<ActorResult<()>>>>;

/// Actor adapter with serial dispatch and owned, cancellable lifecycle futures.
pub struct ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<Self>>
        + Unpin
        + 'static,
{
    inner: Option<A>,
    ctx: Option<ActixContext<Self>>,
    initialization_trace: Option<parrot_api::trace::TraceContext>,
    state: ActorState,
    control: Arc<ActorControl>,
    outcome: ActorOutcome,
    path: String,
    initialized: bool,
    hook_started: bool,
    managed: bool,
    spawn_service: Option<crate::actix::system::WeakSystem>,
    parent: Option<BoxedActorRef>,
    mailbox_capacity: usize,
    ask_timeout: Duration,
}
impl<A> ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<Self>>
        + Unpin
        + 'static,
{
    /// Low-level adapter for direct Actix use. System spawns additionally own
    /// registry membership, stop receipts and arbiter retirement.
    pub fn new(inner: A) -> Self {
        let (control, _) = ActorControl::new(Duration::from_secs(5));
        Self {
            inner: Some(inner),
            ctx: None,
            initialization_trace: parrot_api::trace::TraceContext::current(),
            state: ActorState::Starting,
            control,
            outcome: Default::default(),
            path: format!("actix://{}", uuid::Uuid::new_v4()),
            initialized: false,
            hook_started: false,
            managed: false,
            spawn_service: None,
            parent: None,
            mailbox_capacity: 1024,
            ask_timeout: Duration::from_secs(5),
        }
    }
    pub(crate) fn managed(
        inner: A,
        path: String,
        control: Arc<ActorControl>,
        outcome: ActorOutcome,
        capacity: usize,
        ask_timeout: Duration,
    ) -> Self {
        let mut actor = Self::new(inner);
        actor.path = path;
        actor.control = control;
        actor.outcome = outcome;
        actor.managed = true;
        actor.mailbox_capacity = capacity;
        actor.ask_timeout = ask_timeout;
        actor
    }
    pub(crate) fn with_spawn_service(
        mut self,
        service: crate::actix::system::WeakSystem,
        parent: Option<BoxedActorRef>,
    ) -> Self {
        self.spawn_service = Some(service);
        self.parent = parent;
        self
    }
    pub fn state(&self) -> ActorState {
        self.state
    }
    pub fn try_inner_mut(&mut self) -> Option<&mut A> {
        self.inner.as_mut()
    }
    pub fn inner_mut(&mut self) -> &mut A {
        self.try_inner_mut()
            .expect("actor is owned by an in-flight async handler")
    }
    fn stop_requested(&self) -> bool {
        self.control.is_stopping()
    }
}
// Admission and initialization share the generation's pause and cancellation.
// Keep the init invocation inside a lazy future: creating it is not permission
// to call user code before the admission gates have completed.
async fn initialize_after_admission(
    control: &ActorControl,
    admission: impl std::future::Future<Output = ActorResult<()>>,
    initialization: impl std::future::Future<Output = ActorResult<()>>,
) -> ActorResult<bool> {
    if control.is_stopping() {
        return Ok(false);
    }
    tokio::select! {
        biased;
        _ = control.cancellation() => Ok(false),
        result = control.dynamic_escalation.guard(async {
            admission.await?;
            // Cancellation can arrive after its branch was polled Pending,
            // while admission becomes Ready in this same select poll.
            if control.is_stopping() {
                return Ok(false);
            }
            initialization.await.map(|_| true)
        }) => result,
    }
}

impl<A> ParrotActor for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<Self>>
        + Unpin
        + 'static,
{
    type Config = EmptyConfig;
    type Context = ActixContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async {
            Err(ActorError::MessageHandlingError(
                "Not use on actix engine".into(),
            ))
        })
    }
    fn state(&self) -> ActorState {
        self.state
    }
}
impl<A> ActixActorTrait for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<Self>>
        + Unpin
        + 'static,
{
    type Context = Context<Self>;
    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.set_mailbox_capacity(self.mailbox_capacity);
        let addr = ctx.address();
        let idle_recipient = Arc::new(crate::actix::reference::IdleRecipient {
            addr: addr.downgrade(),
        });
        let mut actor_ref = ActixActorRef::new(addr.clone(), self.path.clone());
        if self.managed {
            actor_ref = actor_ref.with_control(self.control.clone());
        }
        let path = ActorPath::new(Arc::new(actor_ref) as WeakActorTarget, self.path.clone());
        let mut actor_ctx = ActixContext::new(addr, path)
            .with_ask_timeout(self.ask_timeout)
            .with_control(self.control.clone());
        actor_ctx.set_spawn_service(self.spawn_service.clone(), self.parent.take());
        self.control
            .children
            .bind(Arc::new(crate::actix::reference::ChildRecipient {
                addr: ctx.address().downgrade(),
            }));
        let weak_control = Arc::downgrade(&self.control);
        let decision_runtime = self
            .spawn_service
            .as_ref()
            .map(|service| service.runtime.clone())
            .unwrap_or_else(tokio::runtime::Handle::current);
        self.control.children.bind_escalator(Arc::new(move |error| {
            if let Some(control) = weak_control.upgrade()
                && !control.is_stopping()
                && control.dynamic_escalation.begin(error.clone())
            {
                let deadline = tokio::time::Instant::now() + control.timeout;
                let decision = control.lifetime.start_escalation(
                    error.clone(),
                    deadline,
                    false,
                    &decision_runtime,
                );
                decision_runtime.spawn(parrot_api::trace::capture(async move {
                    if decision.await {
                        if !control.is_stopping()
                            && !control.lifetime.is_stopping()
                            && control.lifetime.recovery().is_none()
                            && !control
                                .supervision_failed
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            control.dynamic_escalation.resume(&error);
                        }
                        return;
                    }
                    if control.is_stopping() {
                        return;
                    }
                    control.record_failure_context();
                    *control.escalated.lock().unwrap() = Some(error);
                    control
                        .supervision_failed
                        .store(true, std::sync::atomic::Ordering::Release);
                    control.begin_cleanup(deadline);
                }));
            }
        }));
        actor_ctx.idle.bind(idle_recipient);
        actor_ctx
            .stream_registry
            .bind(Arc::new(crate::actix::reference::StreamRecipient {
                addr: ctx.address().downgrade(),
            }));
        let mut inner = self.inner.take().expect("actor not yet initialized");
        let control = self.control.clone();
        let initialize = async move {
            let result = std::panic::AssertUnwindSafe(initialize_after_admission(
                &control,
                async {
                    if let Some(gate) = &control.initialization_gate {
                        super::lifecycle::await_completion(gate.clone()).await?;
                    }
                    if let Some(gate) = &control.group_activation {
                        gate.activation().await.map_err(ActorError::InternalError)?;
                    }
                    Ok(())
                },
                async { inner.init(&mut actor_ctx).await },
            ))
            .catch_unwind()
            .await
            .unwrap_or_else(|payload| Err(ActorError::Panic(panic_message(payload))));
            if result.is_err() {
                control.record_failure_context();
            }
            (inner, actor_ctx, result)
        };
        ctx.wait(
            parrot_api::trace::scope(self.initialization_trace.take(), initialize)
                .into_actor(self)
                .map(|(inner, actor_ctx, result), actor, ctx| {
                    actor.inner = Some(inner);
                    actor.ctx = Some(actor_ctx);
                    match result {
                        Ok(true) => {
                            actor.initialized = true;
                            actor.state = ActorState::Running;
                            actor.ctx.as_ref().unwrap().idle.activate();
                        }
                        Ok(false) => {
                            *actor.outcome.lock().unwrap() = Some(Ok(()));
                        }
                        Err(error) => {
                            actor
                                .control
                                .supervision_failed
                                .store(true, std::sync::atomic::Ordering::Release);
                            *actor.outcome.lock().unwrap() = Some(Err(error));
                        }
                    }
                    if !actor.initialized || actor.stop_requested() {
                        ctx.stop();
                    }
                }),
        );
        let control = self.control.clone();
        ctx.spawn(
            async move { control.cancellation().await }
                .into_actor(self)
                .map(|_, _, ctx| ctx.stop()),
        );
    }
    fn stopping(&mut self, ctx: &mut Self::Context) -> Running {
        self.state = ActorState::Stopping;
        if let Some(actor_ctx) = &self.ctx {
            actor_ctx.idle.close();
        }
        self.control
            .begin_cleanup(tokio::time::Instant::now() + self.control.timeout);
        if self.control.escalated.lock().unwrap().is_some()
            || self.control.dynamic_escalation.pending().is_some()
            || self.control.lifetime.decision_deadline().is_some()
        {
            // The monitor retains the originating failure separately. An actual
            // execution/disposal failure must remain observable and take priority.
            self.outcome.lock().unwrap().get_or_insert(Ok(()));
        }
        if self.outcome.lock().unwrap().is_some() {
            return Running::Stop;
        }
        // ContextFut::drop may poll stopping after the host runtime has left
        // Tokio. Graceful hooks require a live executor; forced teardown must
        // dispose owned state without constructing timers or user futures.
        if tokio::runtime::Handle::try_current().is_err() {
            *self.outcome.lock().unwrap() = Some(Err(ActorError::InternalError(
                "Actix host runtime ended before graceful actor cleanup".into(),
            )));
            return Running::Stop;
        }
        // A mailbox dispatch may have transferred ownership into an
        // AtomicResponse that Actix has not polled/merged yet. Returning Stop
        // would discard it without restoring actor/context or running the hook.
        // Returning Continue lets the sealed-channel branch restore ownership.
        // The same rule covers an initialization future interrupted before poll.
        if self.inner.is_none() || self.ctx.is_none() {
            return Running::Continue;
        }
        if !self.initialized {
            self.outcome.lock().unwrap().get_or_insert(Ok(()));
            return Running::Stop;
        }
        // Actix can revisit stopping before merging the hook's wait future,
        // for example when the cancellation listener also calls ctx.stop().
        if self.hook_started {
            return Running::Continue;
        }
        let (Some(mut inner), Some(mut actor_ctx)) = (self.inner.take(), self.ctx.take()) else {
            return Running::Stop;
        };
        self.hook_started = true;
        let control = self.control.clone();
        let stop = async move {
            let result = std::panic::AssertUnwindSafe(async {
                if control.expired_now() {
                    return Err(control.enforce_deadline());
                }
                tokio::select! {
                    biased;
                    _ = control.expired() => Err(control.enforce_deadline()),
                    result = async {
                        let children_result = control.children.join().await;
                        let tasks_result = control.tasks.join().await.and(children_result);
                        inner.before_stop(&mut actor_ctx).await.map_err(crate::error_receipt::protect_actor).and(tasks_result)
                    } => result,
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|payload| Err(ActorError::Panic(panic_message(payload))));
            let result = result.map_err(|error| {
                let error = crate::error_receipt::ActorReceipt::from_error(error);
                *control.stop_failure.lock().unwrap() = Some(error.clone());
                error.into_error()
            });
            (inner, actor_ctx, result)
        };
        ctx.wait(
            stop.into_actor(self)
                .map(|(inner, actor_ctx, result), actor, ctx| {
                    actor.inner = Some(inner);
                    actor.ctx = Some(actor_ctx);
                    *actor.outcome.lock().unwrap() = Some(result);
                    ctx.stop();
                }),
        );
        Running::Continue
    }
    fn stopped(&mut self, _: &mut Self::Context) {
        self.state = ActorState::Stopped;
    }
}

impl<A> ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<Self>>
        + Unpin
        + 'static,
{
    fn handle_ready(
        &mut self,
        message: ActixMessageWrapper,
        ctx: &mut Context<Self>,
    ) -> actix::ResponseActFuture<Self, Option<ActorResult<BoxedMessage>>> {
        let decision_runtime = self
            .spawn_service
            .as_ref()
            .map(|service| service.runtime.clone())
            .unwrap_or_else(tokio::runtime::Handle::current);
        let trace_context = message.envelope.trace_context.map(|parent| parent.child());
        let mut payload = message.envelope.payload;
        if payload.is::<StopMessage>() || self.stop_requested() {
            let deadline = tokio::time::Instant::now() + self.control.timeout;
            if payload.is::<StopMessage>() {
                self.control.request_stop(deadline);
            } else {
                self.control.begin_cleanup(deadline);
            }
            ctx.stop();
            return Box::pin(
                futures::future::ready(Some(Err(ActorError::Stopped))).into_actor(self),
            );
        }
        let Some(inner) = self.inner.as_mut() else {
            return Box::pin(
                futures::future::ready(Some(Err(ActorError::InternalError(
                    "actor already owned by dispatch".into(),
                ))))
                .into_actor(self),
            );
        };
        let Some(actor_ctx) = self.ctx.as_mut() else {
            return Box::pin(
                futures::future::ready(Some(Err(ActorError::MessageHandlingError(
                    "ActixActor context not initialized".into(),
                ))))
                .into_actor(self),
            );
        };
        let activity = if payload.is::<crate::receive_timeout::IdleSignal>() {
            let signal = *payload
                .downcast::<crate::receive_timeout::IdleSignal>()
                .unwrap();
            let Some(activity) = actor_ctx.idle.claim(signal) else {
                return Box::pin(
                    futures::future::ready(Some(Ok(Box::new(()) as BoxedMessage))).into_actor(self),
                );
            };
            payload = Box::new(parrot_api::context::LifecycleEvent::ReceiveTimeout);
            activity
        } else {
            actor_ctx.idle.enter()
        };
        if !payload.is::<crate::streams::StreamEvent>()
            && !payload.is::<crate::children::ChildTerminated>()
            && !inner.use_async_handler()
        {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                parrot_api::trace::in_scope(trace_context.clone(), || {
                    let recording = parrot_api::trace::SpanGuard::current(
                        parrot_api::trace::SpanKind::DynamicHandler,
                        &self.path,
                    );
                    let result = inner.receive_message_with_engine(
                        payload,
                        actor_ctx,
                        EngineContextHandle::new(ctx),
                    );
                    recording.finish(result.as_ref().map_or(
                        parrot_api::trace::SpanStatus::Error,
                        parrot_api::trace::SpanStatus::from_result,
                    ));
                    result
                })
            }));
            let result = match result {
                Ok(Some(result)) => result,
                Ok(None) => return Box::pin(futures::future::ready(Some(Err(ActorError::MessageHandlingError("Message not handled: the sync engine handler (handle_message_engine) returned None for this message type. Handle it there, or opt the actor into the async dispatch path with #[ParrotActor(async_handler = true)] / use_async_handler() == true.".into())))).into_actor(self)),
                Err(payload) => Err(ActorError::Panic(panic_message(payload))),
            };
            let control = self.control.clone();
            let unmanaged = !self.managed;
            return Box::pin(
                parrot_api::trace::scope(trace_context, async move {
                    let _activity = activity;
                    tokio::select! {
                        biased;
                        _ = control.cancellation() => (Err(ActorError::Stopped), None),
                        result = control.dynamic_escalation.guard(control.supervise_message(result, unmanaged, &decision_runtime)) => result,
                    }
                })
                .into_actor(self)
                .map(|(result, failure), actor, ctx| {
                    if let Some(error) = failure {
                        actor
                            .control
                            .supervision_failed
                            .store(true, std::sync::atomic::Ordering::Release);
                        *actor.outcome.lock().unwrap() =
                            Some(Err(crate::supervision_policy::reply_error(error)));
                        ctx.stop();
                    }
                    if actor.stop_requested() {
                        ctx.stop();
                    }
                    Some(result)
                }),
            );
        }
        // This future owns the actual actor and context. No borrow of the
        // surrounding ActixActor survives a poll or cancellation boundary.
        let mut inner = self.inner.take().unwrap();
        let mut actor_ctx = self.ctx.take().unwrap();
        let control = self.control.clone();
        let unmanaged = !self.managed;
        let dispatch = async move {
            let _activity = activity;
            let work = async {
                let recording = parrot_api::trace::SpanGuard::current(
                    parrot_api::trace::SpanKind::DynamicHandler,
                    parrot_api::context::ActorContext::path(&actor_ctx).path(),
                );
                let result = std::panic::AssertUnwindSafe(async {
                    if payload.is::<crate::children::ChildTerminated>() {
                        inner
                            .handle_child_terminated(
                                payload
                                    .downcast::<crate::children::ChildTerminated>()
                                    .unwrap()
                                    .0,
                                &mut actor_ctx,
                            )
                            .await?;
                        Ok(Box::new(()) as BoxedMessage)
                    } else if payload.is::<crate::streams::StreamEvent>() {
                        crate::streams::dispatch(
                            *payload.downcast::<crate::streams::StreamEvent>().unwrap(),
                            &mut inner,
                            &mut actor_ctx,
                        )
                        .await
                    } else {
                        inner.receive_message(payload, &mut actor_ctx).await
                    }
                })
                .catch_unwind()
                .await;
                let result =
                    result.unwrap_or_else(|payload| Err(ActorError::Panic(panic_message(payload))));
                recording.finish_result(&result);
                control
                    .supervise_message(result, unmanaged, &decision_runtime)
                    .await
            };
            let (result, failure) = tokio::select! {
                biased;
                _ = control.cancellation() => (Err(ActorError::Stopped), None),
                result = control.dynamic_escalation.guard(work) => result,
            };
            (inner, actor_ctx, result, failure)
        };
        Box::pin(
            parrot_api::trace::scope(trace_context, dispatch)
                .into_actor(self)
                .map(|(inner, actor_ctx, result, failure), actor, ctx| {
                    actor.inner = Some(inner);
                    actor.ctx = Some(actor_ctx);
                    if let Some(error) = failure {
                        actor
                            .control
                            .supervision_failed
                            .store(true, std::sync::atomic::Ordering::Release);
                        *actor.outcome.lock().unwrap() =
                            Some(Err(crate::supervision_policy::reply_error(error)));
                        ctx.stop();
                    }
                    if actor.stop_requested() {
                        ctx.stop();
                    }
                    Some(result)
                }),
        )
    }
}
impl<A> Handler<ActixMessageWrapper> for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<Self>>
        + Unpin
        + 'static,
{
    type Result = AtomicResponse<Self, Option<ActorResult<BoxedMessage>>>;
    fn handle(&mut self, message: ActixMessageWrapper, ctx: &mut Context<Self>) -> Self::Result {
        // Validate ownership and initialization at admission. Deferring these
        // errors until the pause future is polled could turn a rejected direct
        // pre-start call into a user message after started() creates the context.
        if self.inner.is_none() || self.ctx.is_none() {
            return AtomicResponse::new(self.handle_ready(message, ctx));
        }
        let control = self.control.clone();
        if message.envelope.payload.is::<StopMessage>() {
            control.request_stop(tokio::time::Instant::now() + control.timeout);
        }
        let ready = async move {
            tokio::select! {
                biased;
                _ = control.cancellation() => false,
                _ = control.dynamic_escalation.ready() => true,
            }
        };
        AtomicResponse::new(Box::pin(ready.into_actor(self).then(
            move |ready, actor, ctx| {
                if ready {
                    actor.handle_ready(message, ctx)
                } else {
                    ctx.stop();
                    Box::pin(
                        futures::future::ready(Some(Err(ActorError::Stopped))).into_actor(actor),
                    )
                }
            },
        )))
    }
}
impl<A> Handler<StopMessage> for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<Self>>
        + Unpin
        + 'static,
{
    type Result = ();
    fn handle(&mut self, _: StopMessage, ctx: &mut Self::Context) {
        self.control
            .request_stop(tokio::time::Instant::now() + self.control.timeout);
        ctx.stop();
    }
}
/// ActorBase is a simple wrapper for user actors
///
/// # Overview
/// This wrapper exists to support the ParrotActor derive macro
///
/// # Key Responsibilities
/// - Wrap a user-defined actor
///
/// # Implementation Details
/// - Used by the parrot-api-derive macro
pub struct ActorBase<A> {
    /// The wrapped actor
    pub actor: A,
}

impl<A> ActorBase<A> {
    /// Create a new ActorBase
    pub fn new(actor: A) -> Self {
        ActorBase { actor }
    }
}

/// IntoActorBase trait for conversion to ActorBase
///
/// # Overview
/// This trait is used by the ParrotActor derive macro
///
/// # Key Responsibilities
/// - Convert a user actor to an ActorBase
pub trait IntoActorBase {
    /// Convert self to ActorBase
    fn into_actor_base(self) -> ActorBase<Self>
    where
        Self: Sized;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_admission_never_polls_initialization() {
        let (control, _finished) = ActorControl::new(Duration::from_secs(5));
        let initialized = std::cell::Cell::new(0);
        let result = initialize_after_admission(
            &control,
            async {
                // The biased cancellation branch was already polled Pending.
                // Admission now observes a completed stop before returning Ready
                // in that SAME poll, reproducing the cross-arbiter interleaving.
                control.request_stop(tokio::time::Instant::now() + control.timeout);
                Ok(())
            },
            async {
                initialized.set(initialized.get() + 1);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(!result, "cancelled admission was reported initialized");
        assert_eq!(
            initialized.get(),
            0,
            "init ran after admission was cancelled"
        );
    }
}
