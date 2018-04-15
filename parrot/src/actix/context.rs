use std::time::Duration;
use actix::{Actor as ActixActor, Context as ActixContext, SpawnHandle, Handler, AsyncContext, dev::ActorFuture};
use async_trait::async_trait;
use std::any::Any;
use parrot_api::{
    context::{ActorContext, ActorSpawner},
    address::{ActorPath, ActorRef},
    types::{BoxedMessage, BoxedFuture, ActorResult, BoxedActorRef},
    supervisor::{SupervisorStrategyType, DefaultStrategy},
    stream::StreamRegistry,
    errors::ActorError,
    actor::Actor,
    message::MessageEnvelope,
};
use super::actor::ContextConverter;
use super::message::{ActixMessageWrapper, wrap_message};

// Wrapper around actix Context to implement ActorContext trait
pub struct ActixActorContext<A>
where
    A: ActixActor<Context = ActixContext<A>> + Actor + Send + Sync + 'static,
{
    inner: ActixContext<A>,
    path: ActorPath,
    parent: Option<BoxedActorRef>,
    children: Vec<BoxedActorRef>,
    receive_timeout: Option<Duration>,
    supervisor_strategy: SupervisorStrategyType,
    stream_registry: Box<dyn StreamRegistry + Send + Sync>,
}

impl<A> ActixActorContext<A>
where
    A: ActixActor<Context = ActixContext<A>> + Actor + Send + Sync + 'static,
{
    pub fn new(
        inner: ActixContext<A>,
        path: ActorPath,
        parent: Option<BoxedActorRef>,
        stream_registry: Box<dyn StreamRegistry + Send + Sync>,
    ) -> Self {
        Self {
            inner,
            path,
            parent,
            children: Vec::new(),
            receive_timeout: None,
            supervisor_strategy: SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure),
            stream_registry,
        }
    }

    // 内部方法，创建消息封装
    fn create_envelope(&self, msg: BoxedMessage) -> MessageEnvelope {
        MessageEnvelope::new(msg, Some(self.get_self_ref()), None)
    }
}

impl<A> ContextConverter<A> for ActixActorContext<A>
where
    A: ActixActor<Context = ActixContext<A>> + Actor + Send + Sync + 'static,
{
    type ActixActor = A;
    
    fn to_actix_context(ctx: &mut dyn ActorContext) -> &mut ActixContext<Self::ActixActor> {
        let ctx = unsafe {
            &mut *(ctx as *mut dyn ActorContext as *mut ActixActorContext<A>)
        };
        &mut ctx.inner
    }

    fn from_actix_context(ctx: &mut ActixContext<Self::ActixActor>) -> &mut dyn ActorContext {
        unimplemented!()
    }
}

#[async_trait]
impl<A> ActorContext for ActixActorContext<A>
where
    A: ActixActor<Context = ActixContext<A>> + Actor + Send + Sync + 'static,
    Box<dyn ActorFuture<A, Output = ()>>: Send + Sync,
{
    fn get_self_ref(&self) -> BoxedActorRef {
        // Create and return a BoxedActorRef for this actor
        unimplemented!()
    }

    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.inner.stop();
            Ok(())
        })
    }

    fn send<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // 在内部创建消息封装
            let envelope = self.create_envelope(msg);
            
            // 发送消息并等待响应
            target.send(Box::new(envelope) as BoxedMessage).await
        })
    }

    fn ask<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            target.send(Box::new(MessageEnvelope::new(msg, None, None)) as BoxedMessage).await
        })
    }

    fn schedule_once<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: BoxedMessage,
        delay: Duration,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            // Schedule message using actix Context::notify_later
            Ok(())
        })
    }

    fn schedule_periodic<'a>(
        &'a self,
        target: BoxedActorRef,
        msg: BoxedMessage,
        initial_delay: Duration,
        interval: Duration,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            // Schedule periodic message using actix Context::notify_interval
            Ok(())
        })
    }

    fn watch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            // Implement actor watching using actix Context::watch
            Ok(())
        })
    }

    fn unwatch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            // Implement actor unwatching
            Ok(())
        })
    }

    fn parent(&self) -> Option<BoxedActorRef> {
        self.parent.clone()
    }

    fn children(&self) -> Vec<BoxedActorRef> {
        self.children.clone()
    }

    fn set_receive_timeout(&mut self, timeout: Option<Duration>) {
        self.receive_timeout = timeout;
    }

    fn receive_timeout(&self) -> Option<Duration> {
        self.receive_timeout
    }

    fn set_supervisor_strategy(&mut self, strategy: SupervisorStrategyType) {
        self.supervisor_strategy = strategy;
    }

    fn path(&self) -> &ActorPath {
        &self.path
    }

    fn stream_registry(&mut self) -> &mut dyn StreamRegistry {
        self.stream_registry.as_mut()
    }

    fn spawner(&mut self) -> &mut dyn ActorSpawner {
        // Return the actor spawner implementation
        unimplemented!()
    }

} 