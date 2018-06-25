use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;

use actix::{Actor as ActixActorTrait, Context as ActixContextInner, Handler, Message as ActixMessage, ResponseFuture, Supervised};
use async_trait::async_trait;
use parrot_api::{
    actor::{Actor, ActorConfig, ActorState},
    errors::ActorError,
    types::{BoxedMessage, BoxedFuture, ActorResult},
    message::{Message, MessageEnvelope},
};
use crate::actix::context::ActixContext;
use crate::actix::message::{MessageConverter, ActixMessageHandler, ActixMessageWrapper};

/// Marker trait for types that can be converted into ActorBase
pub trait IntoActorBase: Actor {
    fn into_actor_base(self) -> ActorBase<Self>;
}

/// Actix-based actor implementation
/// 
/// This is the main actor type that users will interact with.
/// It wraps a user-defined actor type and handles the conversion
/// between Parrot message types and Actix message types.
pub struct ActorBase<A>
where
    A: Actor,
    A::Context: 'static,
{
    inner: A,
    state: ActorState,
}

impl<A> ActorBase<A>
where
    A: Actor,
    A::Context: 'static,
{
    pub fn new(actor: A) -> Self {
        Self {
            inner: actor,
            state: ActorState::Starting,
        }
    }
}

/// Configuration for ActixActor
#[derive(Default)]
pub struct ActixActorConfig {
    // Add any actix-specific configuration here
}

impl ActorConfig for ActixActorConfig {}

/// Implementation of actix Actor trait for ActorBase
impl<A> ActixActorTrait for ActorBase<A>
where
    A: Actor + 'static,
    A::Context: 'static,
{
    type Context = ActixContextInner<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.state = ActorState::Running;
        
        // Create ActixContext to pass to the inner actor
        let mut actor_ctx = ActixContext::new(ctx);
        
        // Initialize the inner actor
        let future = self.inner.init(&mut actor_ctx);
        ctx.wait(future.map(|_| ()));
    }

    fn stopping(&mut self, _ctx: &mut Self::Context) -> actix::Running {
        self.state = ActorState::Stopping;
        actix::Running::Stop
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        self.state = ActorState::Stopped;
    }
}

/// Implementation of supervision strategy for ActorBase
impl<A> Supervised for ActorBase<A>
where
    A: Actor + 'static,
    A::Context: 'static,
{
    fn restarting(&mut self, ctx: &mut ActixContextInner<Self>) {
        self.state = ActorState::Starting;
        // Reset the actor state and prepare for restart
        let mut actor_ctx = ActixContext::new(ctx);
        let _ = self.inner.init(&mut actor_ctx);
    }
}

/// Message handler for ActorBase
/// Processes generic MessageEnvelope and forwards to the inner actor
impl<A> Handler<ActixMessageWrapper> for ActorBase<A>
where
    A: Actor + 'static,
    A::Context: 'static,
{
    type Result = ResponseFuture<Result<BoxedMessage, ActorError>>;

    fn handle(&mut self, msg: ActixMessageWrapper, ctx: &mut ActixContextInner<Self>) -> Self::Result {
        let envelope = msg.envelope;
        let payload = envelope.payload;
        
        // Create ActixContext to pass to the inner actor
        let mut actor_ctx = ActixContext::new(ctx);
        
        // Call the inner actor's receive_message method
        let fut = self.inner.receive_message(payload, &mut actor_ctx);
        
        Box::pin(async move {
            fut.await
        })
    }
}

/// Message for actor stopping
#[derive(Debug)]
pub struct StopMessage;

impl ActixMessage for StopMessage {
    type Result = ();
}

/// Handler for stop message
impl<A> Handler<StopMessage> for ActorBase<A>
where
    A: Actor + 'static,
    A::Context: 'static,
{
    type Result = ();

    fn handle(&mut self, _: StopMessage, ctx: &mut ActixContextInner<Self>) {
        self.state = ActorState::Stopping;
        ctx.stop();
    }
}

/// Actor reference for ActixActor
pub struct ActixActorRef<A>
where
    A: Actor + 'static,
{
    addr: actix::Addr<ActorBase<A>>,
    _marker: PhantomData<A>,
}

impl<A> ActixActorRef<A>
where
    A: Actor + 'static,
{
    pub fn new(addr: actix::Addr<ActorBase<A>>) -> Self {
        Self {
            addr,
            _marker: PhantomData,
        }
    }
    
    /// Send a message to the actor
    pub async fn send<T: Message + 'static>(&self, msg: T) -> Result<T::Result, ActorError> {
        // Create message envelope
        let envelope = msg.into_envelope();
        
        // Convert to ActixMessageWrapper
        let wrapper = ActixMessageWrapper { envelope };
        
        // Send message to actor
        let result = self.addr.send(wrapper).await
            .map_err(|e| ActorError::MessageDeliveryFailure(e.to_string()))?;
            
        // Extract and convert result
        match result {
            Ok(boxed) => {
                T::extract_result(boxed)
            },
            Err(e) => Err(e),
        }
    }
    
    /// Send a message without waiting for response
    pub fn do_send<T: Message + 'static>(&self, msg: T) -> Result<(), ActorError> {
        // Create message envelope
        let envelope = MessageEnvelope::new(msg, None, None);
        
        // Convert to ActixMessageWrapper
        let wrapper = ActixMessageWrapper { envelope };
        
        // Send message to actor
        self.addr.do_send(wrapper);
        
        Ok(())
    }
} 