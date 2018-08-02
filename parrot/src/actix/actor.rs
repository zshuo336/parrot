use std::marker::PhantomData;
use std::any::Any;
use std::pin::Pin;
use std::fmt::Debug;
use actix::{Actor as ActixActorTrait, Handler, Context as ActixContextType, Running, AsyncContext, ActorContext as ActixActorContextTrait, Addr};
use parrot_api::actor::{Actor as ParrotActor, ActorState, EmptyConfig};
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

    fn receive_message_with_engine<'a>(&'a mut self, msg: BoxedMessage, ctx: &'a mut Self::Context, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
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
        let path = ActorPath {
            target: Arc::new(actor_ref) as WeakActorTarget,
            path: path_str,
        };
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
impl<A> Handler<ActixMessageWrapper> for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>> + Unpin + 'static,
{
    type Result = Option<ActorResult<BoxedMessage>>;
    
    fn handle(&mut self, msg: ActixMessageWrapper, ctx: &mut Self::Context) -> Self::Result {
        // Extract the message from the envelope
        let payload = msg.envelope.payload;
        
        // Get the context or return error if not initialized
        let ctx_option = self.ctx.as_mut();
        if let Some(actor_ctx) = ctx_option {
            // get the context pointer from the actix context
            let ctx_ptr = NonNull::new(ctx as *mut Self::Context).unwrap();
            
            self.inner.receive_message_with_engine(payload, actor_ctx, ctx_ptr)
        } else {
            None
        }
    }
}

/// Handler for stop messages
impl<A> Handler<StopMessage> for ActixActor<A>
where
    A: ParrotActor<Context = ActixContext<Self>> + Unpin + 'static,
    A::Context: Default,
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