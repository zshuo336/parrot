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
use anyhow::anyhow;
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
    A: ParrotActor + Unpin + 'static,
{
    /// The user-defined actor instance
    inner: A,
    /// The Parrot actor context
    ctx: Option<ActixContext<ActixActor<A>>>,
    /// Actor state
    state: ActorState,
}

impl<A> ActixActor<A>
where
    A: ParrotActor + Unpin + 'static,
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
    A: ParrotActor + Unpin + 'static,
    A::Context: Default,
{
    // Use EmptyConfig since we don't need additional configuration
    type Config = EmptyConfig;
    // Use our ActixContext as the context type
    type Context = ActixContext<Self>;

    // Initialize the actor
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    // Handle incoming messages by delegating to the inner actor
    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        // 创建内部 actor 可以使用的临时上下文
        let mut inner_ctx = A::Context::default();
        
        Box::pin(async { 
            let result = self.inner.receive_message(msg, &mut inner_ctx).await;
            result
        })
    }

    fn receive_message_with_engine<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        // 创建内部 actor 可以使用的临时上下文
        let mut inner_ctx = A::Context::default();
        self.inner.receive_message_with_engine(msg, &mut inner_ctx, engine_ctx)
    }

    // Return the current actor state
    fn state(&self) -> ActorState {
        self.state
    }
}

/// ActorBase is the public name for ActixActor
/// 
/// This alias is what users will directly interact with
pub type ActorBase<A> = ActixActor<A>;

/// IntoActorBase trait for converting user actors to ActorBase
/// 
/// # Overview
/// Simplifies actor conversion for the user
/// 
/// # Implementation
/// This is typically auto-implemented by the ParrotActor derive macro
pub trait IntoActorBase {
    /// Convert self into an ActorBase instance
    fn into_actor_base(self) -> ActorBase<Self> where Self: Sized + ParrotActor + Unpin + 'static;
}

// Default implementation of IntoActorBase for all ParrotActor types
impl<A> IntoActorBase for A 
where 
    A: ParrotActor + Unpin + 'static,
{
    fn into_actor_base(self) -> ActorBase<Self> {
        ActixActor::new(self)
    }
}

impl<A> ActixActorTrait for ActixActor<A>
where
    A: ParrotActor + Unpin + 'static + Debug,
    A::Context: Default,
{
    type Context = ActixContextType<Self>;
    
    fn started(&mut self, ctx: &mut Self::Context) {
        self.state = ActorState::Running;
        
        // Create a path for the actor
        let addr = ctx.address();
        // Create a path string from the address
        let path_str = format!("actix://{:?}", addr);
        
        // Create a wrapped actor ref for the address
        let actor_ref = ActixActorRef::new(addr.clone(), format!("{:?}", self.inner));
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
    A: ParrotActor + Unpin + 'static,
{
    type Result = actix::ResponseFuture<Result<BoxedMessage, ActorError>>;
    
    fn handle(&mut self, msg: ActixMessageWrapper, ctx: &mut Self::Context) -> Self::Result {
        // Extract the message from the envelope
        let payload = msg.envelope.payload;
        
        // Get the context or return error if not initialized
        if self.ctx.is_none() {
            return Box::pin(async {
                Err(ActorError::MessageHandlingError("Actor context not initialized".to_string()))
            });
        }
        
        // get the context pointer from the actix context
        let ctx_ptr = NonNull::new(ctx as *mut Self::Context).unwrap();
        
        // 尝试直接处理消息，不传递上下文
        // 如果 inner actor 可以处理不带上下文的消息，将会返回结果
        // 否则会返回 None 表示需要异步处理
        Box::pin(async move {
            // 通知需要异步处理，无法在 handler 中同步处理
            Err(ActorError::MessageHandlingError("Message requires async processing".to_string()))
        })
    }
}

/// Handler for stop messages
impl<A> Handler<StopMessage> for ActixActor<A>
where
    A: ParrotActor + Unpin + 'static,
    A::Context: Default,
{
    type Result = ();
    
    fn handle(&mut self, _: StopMessage, ctx: &mut Self::Context) -> Self::Result {
        self.state = ActorState::Stopping;
        // Use ActorContext trait method to stop
        ctx.stop();
    }
} 