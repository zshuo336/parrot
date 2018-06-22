use std::time::Duration;
use std::sync::Arc;

use actix::{Context as ActixContextInner, Actor as ActixActor, Addr};
use parrot_api::{
    context::{ActorContext, ActorSpawner, StreamRegistry},
    address::{ActorPath, ActorRef},
    types::{BoxedActorRef, BoxedMessage, ActorResult, BoxedFuture},
    errors::ActorError,
    actor::Actor,
    supervisor::SupervisorStrategyType,
};
use crate::actix::{ActorBase, ActixActorRef, message::ActixMessageWrapper};

/// ActixContext is the implementation of ActorContext for the Actix backend.
pub struct ActixContext<'a, A: ActixActor> {
    ctx: &'a mut ActixContextInner<ActorBase<A>>,
    path: ActorPath,
    children: Vec<BoxedActorRef>,
    receive_timeout: Option<Duration>,
    supervisor_strategy: SupervisorStrategyType,
}

impl<'a, A: ActixActor> ActixContext<'a, A> {
    pub fn new(ctx: &'a mut ActixContextInner<ActorBase<A>>) -> Self {
        let addr = ctx.address();
        let path = ActorPath {
            path: format!("/user/{}", addr.id()),
            target: Arc::new(()),
        };
        
        Self {
            ctx,
            path,
            children: Vec::new(),
            receive_timeout: None,
            supervisor_strategy: SupervisorStrategyType::OneForOne,
        }
    }
}

impl<'a, A: ActixActor> ActorContext for ActixContext<'a, A> {
    fn get_self_ref(&self) -> BoxedActorRef {
        let addr = self.ctx.address();
        Box::new(ActixActorRef::new(addr))
    }

    fn stop<'b>(&'b mut self) -> BoxedFuture<'b, ActorResult<()>> {
        Box::pin(async move {
            self.ctx.stop();
            Ok(())
        })
    }

    fn send<'b>(&'b self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'b, ActorResult<()>> {
        Box::pin(async move {
            if let Some(target_ref) = target.as_any().downcast_ref::<ActixActorRef<A>>() {
                // Create message envelope
                let envelope = parrot_api::message::MessageEnvelope::new_generic(msg, None, None);
                
                // Convert to ActixMessageWrapper
                let wrapper = ActixMessageWrapper { envelope };
                
                // Send message to actor
                let _ = target_ref.addr.do_send(wrapper);
                Ok(())
            } else {
                Err(ActorError::MessageDeliveryFailure("Invalid actor reference type".to_string()))
            }
        })
    }

    fn ask<'b>(&'b self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'b, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(target_ref) = target.as_any().downcast_ref::<ActixActorRef<A>>() {
                // Create message envelope
                let envelope = parrot_api::message::MessageEnvelope::new_generic(msg, None, None);
                
                // Convert to ActixMessageWrapper
                let wrapper = ActixMessageWrapper { envelope };
                
                // Send message to actor and await response
                let result = target_ref.addr.send(wrapper).await
                    .map_err(|e| ActorError::MessageDeliveryFailure(e.to_string()))?;
                    
                result
            } else {
                Err(ActorError::MessageDeliveryFailure("Invalid actor reference type".to_string()))
            }
        })
    }

    fn schedule_once<'b>(&'b self, target: BoxedActorRef, msg: BoxedMessage, delay: Duration) -> BoxedFuture<'b, ActorResult<()>> {
        Box::pin(async move {
            if let Some(target_ref) = target.as_any().downcast_ref::<ActixActorRef<A>>() {
                // Create message envelope
                let envelope = parrot_api::message::MessageEnvelope::new_generic(msg, None, None);
                
                // Convert to ActixMessageWrapper
                let wrapper = ActixMessageWrapper { envelope };
                
                // Schedule message
                let _ = target_ref.addr.send_later(wrapper, delay);
                Ok(())
            } else {
                Err(ActorError::MessageDeliveryFailure("Invalid actor reference type".to_string()))
            }
        })
    }

    fn schedule_periodic<'b>(&'b self, target: BoxedActorRef, msg: BoxedMessage, initial_delay: Duration, interval: Duration) -> BoxedFuture<'b, ActorResult<()>> {
        Box::pin(async move {
            if let Some(target_ref) = target.as_any().downcast_ref::<ActixActorRef<A>>() {
                // Create message envelope
                let envelope = parrot_api::message::MessageEnvelope::new_generic(msg.clone(), None, None);
                
                // Convert to ActixMessageWrapper
                let wrapper = ActixMessageWrapper { envelope };
                
                // Schedule initial message
                let _ = target_ref.addr.send_later(wrapper, initial_delay);
                
                // Set up periodic messages
                let addr = target_ref.addr.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(initial_delay).await;
                    
                    loop {
                        tokio::time::sleep(interval).await;
                        
                        // Create new envelope for each message
                        let envelope = parrot_api::message::MessageEnvelope::new_generic(msg.clone(), None, None);
                        let wrapper = ActixMessageWrapper { envelope };
                        
                        // Send message
                        if addr.send(wrapper).await.is_err() {
                            break;
                        }
                    }
                });
                
                Ok(())
            } else {
                Err(ActorError::MessageDeliveryFailure("Invalid actor reference type".to_string()))
            }
        })
    }

    fn watch<'b>(&'b mut self, target: BoxedActorRef) -> BoxedFuture<'b, ActorResult<()>> {
        Box::pin(async move {
            if let Some(target_ref) = target.as_any().downcast_ref::<ActixActorRef<A>>() {
                // Watch the actor address
                self.ctx.watch(target_ref.addr.clone().recipient());
                Ok(())
            } else {
                Err(ActorError::MessageDeliveryFailure("Invalid actor reference type".to_string()))
            }
        })
    }

    fn unwatch<'b>(&'b mut self, target: BoxedActorRef) -> BoxedFuture<'b, ActorResult<()>> {
        Box::pin(async move {
            if let Some(target_ref) = target.as_any().downcast_ref::<ActixActorRef<A>>() {
                // Unwatch the actor address
                self.ctx.unwatch(target_ref.addr.clone().recipient());
                Ok(())
            } else {
                Err(ActorError::MessageDeliveryFailure("Invalid actor reference type".to_string()))
            }
        })
    }

    fn parent(&self) -> Option<BoxedActorRef> {
        None // Actix doesn't have a built-in parent-child relationship
    }
    
    fn children(&self) -> Vec<BoxedActorRef> {
        self.children.clone()
    }
    
    fn set_receive_timeout(&mut self, timeout: Option<Duration>) {
        self.receive_timeout = timeout;
        if let Some(duration) = timeout {
            self.ctx.run_later(duration, |_, _| {
                // Handle timeout
            });
        }
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
        // Not implemented yet
        unimplemented!("Stream registry not implemented for ActixContext")
    }

    fn spawner(&mut self) -> &mut dyn ActorSpawner {
        // Not implemented yet
        unimplemented!("Actor spawner not implemented for ActixContext")
    }
}

// Implementation of ActorSpawner for ActixContext
impl<'a, A: ActixActor> ActorSpawner for ActixContext<'a, A> {
    fn spawn<'b>(&'b self, actor: BoxedMessage, config: BoxedMessage) -> BoxedFuture<'b, ActorResult<BoxedActorRef>> {
        Box::pin(async move {
            // Not implemented yet
            Err(ActorError::NotImplemented("Actor spawning not implemented for ActixContext".to_string()))
        })
    }
    
    fn spawn_with_strategy<'b>(&'b self, actor: BoxedMessage, config: BoxedMessage, strategy: SupervisorStrategyType) -> BoxedFuture<'b, ActorResult<BoxedActorRef>> {
        Box::pin(async move {
            // Not implemented yet
            Err(ActorError::NotImplemented("Actor spawning with strategy not implemented for ActixContext".to_string()))
        })
    }
} 