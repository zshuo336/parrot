use std::marker::PhantomData;
use std::any::Any;
use std::pin::Pin;
use std::fmt::Debug;
use actix::{Actor as ActixActorTrait, Handler, Context as ActixContextType, Running, AsyncContext, ActorContext as ActixActorContextTrait, Addr};
use parrot_api::actor::{Actor as ParrotActor, ActorState, EmptyConfig, EngineContextHandle};
use parrot_api::message::MessageEnvelope;
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture, WeakActorTarget, BoxedActorRef};
use parrot_api::errors::ActorError;
use parrot_api::address::ActorPath;
use parrot_api::context::ActorContext;
use parrot_api::supervisor::SupervisorStrategyType;
use crate::actix::context::ActixContext;
use crate::actix::message::ActixMessageWrapper;
use crate::actix::reference::{StopMessage, ActixActorRef};
use async_trait::async_trait;
use std::sync::Arc;
use std::cell::RefCell;
use std::rc::Rc;
use anyhow::{anyhow, Context};
use std::time::Duration;
use std::sync::Mutex;
use std::ptr::NonNull;
use actix::fut::{ActorFuture, WrapFuture};
use actix::prelude::AtomicResponse;
use futures::FutureExt;



/// ActixActor wraps a user-defined actor for the Actix engine
/// 
/// # Overview
/// This is the main adapter between user-defined actors and
/// the Actix engine implementation
/// 
/// # Key Responsibilities
/// - Implement actix::Actor for any Parrot Actor
/// - Delegate message handling to user code
/// - Manage actor lifecycle with context
/// 
/// # Implementation Details
/// - Uses type erasure for message routing
/// - Preserves context between calls
/// - Passes messages to user-defined handle_message method
/// 
/// # Type Parameters
/// - `A`: The user-defined actor type

pub struct ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>> + Unpin + 'static,
{
    /// The user-defined actor instance
    inner: A,
    /// The Parrot actor context
    ctx: Option<ActixContext<Self>>,
    /// Actor state
    state: ActorState,
}

impl<A> ActixActor<A>
where
    A:  ParrotActor<Context = ActixContext<Self>> + Unpin + 'static,
{
    /// Create a new ActixActor wrapping a user-defined actor
    pub fn new(inner: A) -> Self {
        Self {
            inner,
            ctx: None,
            state: ActorState::Starting,
        }
    }
    
    /// Get the actor's state
    pub fn state(&self) -> ActorState {
        self.state
    }
}


// Implement ParrotActor for ActixActor to allow nesting
#[async_trait]
impl<A> ParrotActor for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>> + Unpin + 'static,
{
    // Use EmptyConfig since we don't need additional configuration
    type Config = EmptyConfig;
    // Use our ActixContext as the context type
    type Context = ActixContext<Self>;

    // Initialize the actor
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    // not use on actix engine
    fn receive_message<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { 
            Err(ActorError::MessageHandlingError("Not use on actix engine".to_string()))
        })
    }

    fn receive_message_with_engine<'a>(&'a mut self, msg: BoxedMessage, ctx: &'a mut Self::Context, engine_ctx: EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        self.inner.receive_message_with_engine(msg, ctx, engine_ctx)
    }

    // Return the current actor state
    fn state(&self) -> ActorState {
        self.state
    }
}

impl<A> ActixActorTrait for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>> + Unpin + 'static,
{
    type Context = ActixContextType<Self>;
    
    fn started(&mut self, ctx: &mut Self::Context) {
        self.state = ActorState::Running;
        
        // Create a path for the actor
        let addr = ctx.address();
        // Create a path string from the address
        let path_str = format!("actix://{:?}", addr);
        
        // Create a wrapped actor ref for the address
        let actor_ref = ActixActorRef::new(addr.clone(), path_str.clone());
        let path = ActorPath::new(Arc::new(actor_ref) as WeakActorTarget, path_str);
        // Create and store the context wrapper
        self.ctx = Some(ActixContext::new(addr, path));
    }
    
    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        // Handle actor stopping event
        self.state = ActorState::Stopping;
        Running::Stop
    }
    
    fn stopped(&mut self, _: &mut Self::Context) {
        // Handle actor stopped event
        self.state = ActorState::Stopped;
    }
}

/// Handler implementation for ActixMessageWrapper
///
/// # Overview
/// Two dispatch paths inside a single handler, preserving actor-serial
/// semantics while enabling async handlers:
///
/// 1. **Sync fast path** (default): `receive_message_with_engine` is probed
///    first. A `Some(..)` return handles the message fully inline with zero
///    allocation; `None` keeps the legacy drop behavior.
/// 2. **Async path** (opt-in via `use_async_handler() == true`): every
///    message is delegated to `receive_message` (an async fn). The handler
///    future is parked on `ctx.wait` through an
///    [`actix::prelude::AtomicResponse`], so the arbiter thread keeps
///    polling other actors / timers during the await while this actor
///    stays logically serial — subsequent messages queue behind the
///    in-flight one, matching the thread engine's per-actor serialization.
///
/// The `Option<ActorResult<BoxedMessage>>` reply resolves only after the
/// (possibly async) handler completes, so `Addr::send`, `do_send`, and ask
/// timeouts keep working unchanged.
impl<A> Handler<ActixMessageWrapper> for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>> + Unpin + 'static,
{
    type Result = AtomicResponse<Self, Option<ActorResult<BoxedMessage>>>;

    fn handle(&mut self, msg: ActixMessageWrapper, ctx: &mut Self::Context) -> Self::Result {
        // Extract the message from the envelope
        let payload = msg.envelope.payload;

        // Intercept engine-internal stop requests. `ActixActorRef::stop`
        // delivers StopMessage wrapped in an ActixMessageWrapper; without
        // this interception the payload would be handed to the user dispatch
        // (which treats it as unknown) and the actor would never stop.
        if payload.is::<crate::actix::reference::StopMessage>() {
            self.state = ActorState::Stopping;
            ctx.stop();
            let ready = Some(Ok(Box::new(()) as BoxedMessage));
            return AtomicResponse::new(Box::pin(
                futures::future::ready(ready).into_actor(self),
            ));
        }

        // Get the context or return error if not initialized
        let Some(actor_ctx) = self.ctx.as_mut() else {
            let err = Some(Err(ActorError::MessageHandlingError(
                "ActixActor context not initialized".to_string(),
            )));
            return AtomicResponse::new(Box::pin(
                futures::future::ready(err).into_actor(self),
            ));
        };

        // Mint the safe engine-context handle (ADR-2). This is the single
        // `unsafe` boundary on the dispatch path: the actix `ctx` outlives
        // the synchronous `receive_message_with_engine` call (we hold
        // `&mut` right here) and actor dispatch is serial, so the
        // `from_raw` contract is upheld.
        let engine_handle = unsafe {
            EngineContextHandle::from_raw(NonNull::new(ctx as *mut Self::Context).unwrap())
        };

        // ---- Sync fast path ----
        // Only when the actor did not opt into the async dispatch path.
        if !self.inner.use_async_handler() {
            if let Some(result) = self.inner.receive_message_with_engine(payload, actor_ctx, engine_handle) {
                return AtomicResponse::new(Box::pin(
                    futures::future::ready(Some(result)).into_actor(self),
                ));
            }
            // `None` from the sync probe means the actor's
            // `handle_message_engine` did not handle this message. The
            // legacy adapter silently dropped it, which surfaced to ask
            // callers as an opaque "No response from actor" — hard to
            // diagnose. Reply with an explicit, actionable error instead
            // (message routing is unchanged for `Some(..)` results).
            let err = Some(Err(ActorError::MessageHandlingError(
                "Message not handled: the sync engine handler (handle_message_engine) \
                 returned None for this message type. Handle it there, or opt the \
                 actor into the async dispatch path with #[ParrotActor(async_handler = true)] \
                 / use_async_handler() == true."
                    .to_string(),
            )));
            return AtomicResponse::new(Box::pin(
                futures::future::ready(err).into_actor(self),
            ));
        }

        // ---- Async dispatch path ----
        // Create the self-referential handler future here, borrowing two
        // disjoint fields of `self` (`inner: A` and `ctx: ActixContext`).
        // See `AsyncDispatchFuture` for the soundness argument of the
        // lifetime extension performed below.
        let handler_fut = {
            let actor = &mut self.inner;
            let parrot_ctx = self.ctx.as_mut().expect("ctx presence checked above");
            actor.receive_message(payload, parrot_ctx)
        };
        // SAFETY: lifetime extension of a future that borrows fields of the
        // surrounding `ActixActor`. Sound because:
        // - the `ActixActor` lives inside the heap-boxed `ContextFut`, so
        //   the borrowed addresses are stable for the whole actor lifetime;
        // - while this future is parked in `ctx.wait`, actix does not poll
        //   the mailbox (`waiting()` gate), so no other handler can access
        //   the borrowed fields concurrently;
        // - the future is dropped when the wait item completes or the
        //   ContextFut itself is dropped, never outliving the borrow.
        let handler_fut: BoxedFuture<'static, ActorResult<BoxedMessage>> =
            unsafe { std::mem::transmute(handler_fut) };

        AtomicResponse::new(Box::pin(AsyncDispatchFuture {
            fut: Some(handler_fut),
            _marker: PhantomData,
        }))
    }
}

/// Bridge future that drives a `ParrotActor::receive_message` handler future
/// inside actix's wait queue.
///
/// # Overview
/// `ParrotActor::receive_message` returns a self-referential future
/// (`&'a mut A` / `&'a mut A::Context` borrowed by the returned
/// `BoxedFuture<'a, _>`), which cannot be expressed with safe Rust inside
/// actix's owned-future model. This bridge stores that future with its
/// lifetime extended to `'static` (see the SAFETY comment at the
/// transmute site) and simply forwards every poll.
///
/// # Semantics
/// - The actor stays put inside `ActixActor` — nothing is moved out, so
///   lifecycle (`state()`, stop interception, later messages) keeps working.
/// - Actor serialization is guaranteed by actix's `ctx.wait` gate: while
///   this future is pending, the mailbox is not polled.
/// - On completion the reply is delivered through the standard actix
///   oneshot by `AtomicResponse`, preserving ask/tell/timeout semantics.
struct AsyncDispatchFuture<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>> + Unpin + 'static,
{
    /// The handler future (created in `Handler::handle`, lifetime extended).
    fut: Option<BoxedFuture<'static, ActorResult<BoxedMessage>>>,
    _marker: PhantomData<fn(&mut A)>,
}

impl<A> ActorFuture<ActixActor<A>> for AsyncDispatchFuture<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>> + Unpin + 'static,
{
    type Output = Option<ActorResult<BoxedMessage>>;

    fn poll(
        self: Pin<&mut Self>,
        _act: &mut ActixActor<A>,
        _ctx: &mut actix::Context<ActixActor<A>>,
        task: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: this struct has no pinned fields (`Pin<Box<dyn Future>>`
        // is unpin-movable as a whole), so `get_unchecked_mut` is sound.
        let this = unsafe { self.get_unchecked_mut() };
        let fut = this
            .fut
            .as_mut()
            .expect("handler future present until completion");
        match fut.as_mut().poll(task) {
            std::task::Poll::Ready(result) => {
                // Drop the completed (borrowing) future before returning,
                // releasing its borrows on the actor storage.
                this.fut = None;
                std::task::Poll::Ready(Some(result))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Handler for stop messages
impl<A> Handler<StopMessage> for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>> + Unpin + 'static,
{
    type Result = ();
    
    fn handle(&mut self, _: StopMessage, ctx: &mut Self::Context) -> Self::Result {
        self.state = ActorState::Stopping;
        // Use ActorContext trait method to stop
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
    fn into_actor_base(self) -> ActorBase<Self> where Self: Sized;
} 