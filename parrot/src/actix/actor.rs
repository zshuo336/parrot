use std::any::Any;
use actix::{Actor as ActixActor, Context as ActixContext, Handler, Message as ActixMessage};
use async_trait::async_trait;
use parrot_api::{
    actor::{Actor as ParrotActor, ActorConfig, ActorState},
    context::ActorContext as ParrotActorContext,
    errors::ActorError,
    types::{BoxedMessage, BoxedFuture, ActorResult},
    message::{Message, MessageEnvelope},
};
use crate::actix::message::{ActixMessageWrapper, ActixMessageHandler};

// Wrapper around actix Actor to implement ParrotActor trait
pub struct ActixActorWrapper<A>
where
    A: ActixActor,
{
    inner: A,
    state: ActorState,
}

impl<A> ActixActorWrapper<A>
where
    A: ActixActor,
{
    pub fn new(actor: A) -> Self {
        Self {
            inner: actor,
            state: ActorState::Starting,
        }
    }
}

// Implement actix Actor trait for ActixActorWrapper
impl<A> ActixActor for ActixActorWrapper<A>
where
    A: ActixActor,
{
    type Context = ActixContext<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        self.state = ActorState::Running;
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        self.state = ActorState::Stopped;
    }
}

// Configuration for ActixActorWrapper
#[derive(Default)]
pub struct ActixActorConfig {
    // Add any actix-specific configuration here
}

impl ActorConfig for ActixActorConfig {}

// Implement ParrotActor for ActixActorWrapper
#[async_trait]
impl<A> ParrotActor for ActixActorWrapper<A>
where
    A: ActixActor + Send + 'static,
{
    type Config = ActixActorConfig;
    type Context = dyn ParrotActorContext;

    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.state = ActorState::Running;
            Ok(())
        })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // Convert BoxedMessage to concrete message type
            // Handle the message using actix's Handler trait
            // Return the result as BoxedMessage
            Err(ActorError::MessageHandlingError(
                "Message handling not implemented".to_string(),
            ))
        })
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

pub trait MessageConverter<M: Message> {
    type ActixMessage: ActixMessage<Result = M::Result>;
    fn to_actix(msg: M) -> Self::ActixMessage;
    fn from_actix(msg: Self::ActixMessage) -> M;
}

pub trait ContextConverter<A: ParrotActor> {
    type ActixActor: ActixActor<Context = ActixContext<Self::ActixActor>>;
    fn to_actix_context(ctx: &mut dyn ParrotActorContext) -> &mut ActixContext<Self::ActixActor>;
    fn from_actix_context(ctx: &mut ActixContext<Self::ActixActor>) -> &mut dyn ParrotActorContext;
} 

// Implement default supervision strategy
impl<A> actix::Supervised for ActixActorWrapper<A>
where
    A: ActixActor,
{
    fn restarting(&mut self, _ctx: &mut ActixContext<Self>) {
        self.state = ActorState::Starting;
    }
}

// Implement ActixMessageHandler for ActixActorWrapper
impl<A> ActixMessageHandler for ActixActorWrapper<A>
where
    A: ActixActor + Send + 'static,
{
    fn handle_parrot_message(&mut self, msg: MessageEnvelope) -> ActorResult<BoxedMessage> {
        // Default implementation - can be overridden
        Err(ActorError::MessageHandlingError(
            "Message handling not implemented".to_string(),
        ))
    }
}

// Implement Handler for ActixMessageWrapper
impl<A> Handler<ActixMessageWrapper> for ActixActorWrapper<A>
where
    A: ActixActor + Send + 'static,
{
    type Result = ActorResult<BoxedMessage>;

    fn handle(&mut self, msg: ActixMessageWrapper, _ctx: &mut actix::Context<Self>) -> Self::Result {
        self.handle_parrot_message(msg.envelope)
    }
}

// 定义停止消息类型
#[derive(Debug, Clone)]
pub struct StopMessage;

// 实现消息特性
impl actix::Message for StopMessage {
    type Result = ();
}

// 为ActixActorWrapper实现StopMessage的处理
impl<A> Handler<StopMessage> for ActixActorWrapper<A>
where
    A: ActixActor + Send + 'static,
{
    type Result = ();

    fn handle(&mut self, _: StopMessage, ctx: &mut actix::Context<Self>) -> Self::Result {
        actix::ActorContext::stop(ctx);
    }
} 