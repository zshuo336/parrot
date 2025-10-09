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
    types::{ActorResult, BoxedFuture, BoxedMessage, WeakActorTarget},
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
    state: ActorState,
    control: Arc<ActorControl>,
    outcome: ActorOutcome,
    path: String,
    initialized: bool,
    hook_started: bool,
    managed: bool,
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
            state: ActorState::Starting,
            control,
            outcome: Default::default(),
            path: format!("actix://{}", uuid::Uuid::new_v4()),
            initialized: false,
            hook_started: false,
            managed: false,
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
        self.control.channel.is_sealed()
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
        let mut actor_ref = ActixActorRef::new(addr.clone(), self.path.clone());
        if self.managed {
            actor_ref = actor_ref.with_control(self.control.clone(), self.ask_timeout);
        }
        let path = ActorPath::new(Arc::new(actor_ref) as WeakActorTarget, self.path.clone());
        let mut actor_ctx = ActixContext::new(addr, path).with_control(self.control.clone());
        let mut inner = self.inner.take().expect("actor not yet initialized");
        let control = self.control.clone();
        let initialize = async move {
            let result = std::panic::AssertUnwindSafe(async {
                if control.channel.is_sealed() {
                    return Ok(false);
                }
                tokio::select! {
                    biased;
                    _ = control.channel.closed() => Ok(false),
                    result = inner.init(&mut actor_ctx) => result.map(|_| true),
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|payload| Err(ActorError::Panic(panic_message(payload))));
            (inner, actor_ctx, result)
        };
        ctx.wait(
            initialize
                .into_actor(self)
                .map(|(inner, actor_ctx, result), actor, ctx| {
                    actor.inner = Some(inner);
                    actor.ctx = Some(actor_ctx);
                    match result {
                        Ok(true) => {
                            actor.initialized = true;
                            actor.state = ActorState::Running;
                        }
                        Ok(false) => {
                            *actor.outcome.lock().unwrap() = Some(Ok(()));
                        }
                        Err(error) => {
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
            async move { control.channel.closed().await }
                .into_actor(self)
                .map(|_, _, ctx| ctx.stop()),
        );
    }
    fn stopping(&mut self, ctx: &mut Self::Context) -> Running {
        self.state = ActorState::Stopping;
        self.control
            .request_stop(tokio::time::Instant::now() + self.control.timeout);
        if !self.initialized || self.outcome.lock().unwrap().is_some() {
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
                    return Err(ActorError::Timeout);
                }
                tokio::select! {
                    biased;
                    _ = control.expired() => Err(ActorError::Timeout),
                    result = async {
                        let tasks_result = control.tasks.join().await;
                        inner.before_stop(&mut actor_ctx).await.and(tasks_result)
                    } => result,
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|payload| Err(ActorError::Panic(panic_message(payload))));
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

impl<A> Handler<ActixMessageWrapper> for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>>
        + parrot_api::actor::ActixEngineExt<Context = ActixContext<Self>>
        + Unpin
        + 'static,
{
    type Result = AtomicResponse<Self, Option<ActorResult<BoxedMessage>>>;
    fn handle(&mut self, message: ActixMessageWrapper, ctx: &mut Self::Context) -> Self::Result {
        let payload = message.envelope.payload;
        if payload.is::<StopMessage>() || self.stop_requested() {
            self.control
                .request_stop(tokio::time::Instant::now() + self.control.timeout);
            ctx.stop();
            return AtomicResponse::new(Box::pin(
                futures::future::ready(Some(Err(ActorError::Stopped))).into_actor(self),
            ));
        }
        let Some(inner) = self.inner.as_mut() else {
            return AtomicResponse::new(Box::pin(
                futures::future::ready(Some(Err(ActorError::InternalError(
                    "actor already owned by dispatch".into(),
                ))))
                .into_actor(self),
            ));
        };
        let Some(actor_ctx) = self.ctx.as_mut() else {
            return AtomicResponse::new(Box::pin(
                futures::future::ready(Some(Err(ActorError::MessageHandlingError(
                    "ActixActor context not initialized".into(),
                ))))
                .into_actor(self),
            ));
        };
        if !inner.use_async_handler() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                inner.receive_message_with_engine(payload, actor_ctx, EngineContextHandle::new(ctx))
            }));
            let result = match result {
                Ok(Some(result)) => result,
                Ok(None) => Err(ActorError::MessageHandlingError("Message not handled: the sync engine handler (handle_message_engine) returned None for this message type. Handle it there, or opt the actor into the async dispatch path with #[ParrotActor(async_handler = true)] / use_async_handler() == true.".into())),
                Err(payload) => {
                    let error = panic_message(payload);
                    *self.outcome.lock().unwrap() = Some(Err(ActorError::Panic(error.clone())));
                    ctx.stop();
                    Err(ActorError::Panic(error))
                }
            };
            return AtomicResponse::new(Box::pin(
                futures::future::ready(Some(result)).into_actor(self),
            ));
        }
        // This future owns the actual actor and context. No borrow of the
        // surrounding ActixActor survives a poll or cancellation boundary.
        let mut inner = self.inner.take().unwrap();
        let mut actor_ctx = self.ctx.take().unwrap();
        let control = self.control.clone();
        let dispatch = async move {
            let result = std::panic::AssertUnwindSafe(async {
                tokio::select! {
                    biased;
                    _ = control.channel.closed() => Err(ActorError::Stopped),
                    result = inner.receive_message(payload, &mut actor_ctx) => result,
                }
            })
            .catch_unwind()
            .await;
            (inner, actor_ctx, result)
        };
        AtomicResponse::new(Box::pin(dispatch.into_actor(self).map(
            |(inner, actor_ctx, result), actor, ctx| {
                actor.inner = Some(inner);
                actor.ctx = Some(actor_ctx);
                let result = match result {
                    Ok(result) => result,
                    Err(payload) => {
                        let error = panic_message(payload);
                        *actor.outcome.lock().unwrap() =
                            Some(Err(ActorError::Panic(error.clone())));
                        ctx.stop();
                        Err(ActorError::Panic(error))
                    }
                };
                if actor.stop_requested() {
                    ctx.stop();
                }
                Some(result)
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
