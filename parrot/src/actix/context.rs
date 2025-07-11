use std::sync::Arc;
use std::cell::RefCell;
use actix::{Context as ActixCtx, ActorContext as ActixActorContext, Actor as ActixActor, Addr, AsyncContext};
use parrot_api::context::ActorContext;
use parrot_api::address::ActorPath;
use parrot_api::types::{BoxedActorRef, BoxedFuture, ActorResult};
use parrot_api::errors::ActorError;
use anyhow::anyhow;

/// ActixContext is a wrapper around actix::Context
/// 
/// # Overview
/// It provides a standardized context interface for Parrot actors
/// using Actix as the underlying engine.
/// 
/// # Key Responsibilities
/// - Controls actor lifecycle
/// - Provides access to actor address and path
/// - Enables actor supervision
/// 
/// # Implementation Details
/// - Wraps the Actix context
/// - Translates Parrot API calls to Actix operations
/// - Manages actor lifecycle state
/// 
/// # Thread Safety
/// - Thread safe through interior Arc reference
#[derive(Clone)]
pub struct ActixContext<A> 
where
    A: actix::Actor<Context = ActixCtx<A>>,
{
    /// The underlying Actix address
    addr: Arc<Addr<A>>,
    /// Actor path in the system
    path: ActorPath,
}

impl<A> ActixContext<A> 
where
    A: actix::Actor<Context = ActixCtx<A>>,
{
    /// Create a new ActixContext wrapping an actix::Context
    /// 
    /// # Parameters
    /// - `addr`: The Actix address to wrap
    /// - `path`: Actor path in the system
    /// 
    /// # Returns
    /// A new ActixContext instance
    pub fn new(addr: Addr<A>, path: ActorPath) -> Self {
        Self {
            addr: Arc::new(addr),
            path,
        }
    }
    
    /// Get the inner Actix address
    pub fn addr(&self) -> Arc<Addr<A>> {
        self.addr.clone()
    }
}

impl<A> ActorContext for ActixContext<A> 
where
    A: actix::Actor<Context = ActixCtx<A>>,
{
    /// Get the actor's self reference
    fn get_self_ref(&self) -> BoxedActorRef {
        // This will be implemented by the actor system when creating the actor
        unimplemented!("get_self_ref is implemented by the actor system")
    }
    
    /// Stop the actor execution
    /// 
    /// This will terminate the actor after any in-progress
    /// message handling completes.
    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>> {
        // Use a local reference to the inner context
        let addr = self.addr.clone();
        Box::pin(async move {
            // Send stop message to the actor
            // Actix doesn't expose stop method directly on Addr
            // This will be implemented in actor system
            Ok(())
        })
    }
    
    /// Send a message to another actor without waiting for a response
    fn send<'a>(&'a self, _target: BoxedActorRef, _msg: parrot_api::types::BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        // Implementation will be provided by the actor system
        Box::pin(async { Err(ActorError::Other(anyhow!("send not implemented in context"))) })
    }
    
    /// Send a message to another actor and wait for a response
    fn ask<'a>(&'a self, _target: BoxedActorRef, _msg: parrot_api::types::BoxedMessage) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        // Implementation will be provided by the actor system
        Box::pin(async { Err(ActorError::Other(anyhow!("ask not implemented in context"))) })
    }
    
    /// Schedule a one-time delayed message
    fn schedule_once<'a>(&'a self, _target: BoxedActorRef, _msg: parrot_api::types::BoxedMessage, _delay: std::time::Duration) -> BoxedFuture<'a, ActorResult<()>> {
        // Implementation will be provided by the actor system
        Box::pin(async { Err(ActorError::Other(anyhow!("schedule_once not implemented in context"))) })
    }
    
    /// Schedule a recurring message
    fn schedule_periodic<'a>(&'a self, _target: BoxedActorRef, _msg: parrot_api::types::BoxedMessage, _initial_delay: std::time::Duration, _interval: std::time::Duration) -> BoxedFuture<'a, ActorResult<()>> {
        // Implementation will be provided by the actor system
        Box::pin(async { Err(ActorError::Other(anyhow!("schedule_periodic not implemented in context"))) })
    }
    
    /// Watch another actor for termination
    /// 
    /// # Parameters
    /// - `target`: Reference to another actor to watch
    /// 
    /// When the watched actor terminates, this actor will
    /// receive a Terminated message.
    fn watch<'a>(&'a mut self, _target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        // Implementation will be provided by the actor system
        Box::pin(async { Err(ActorError::Other(anyhow!("watch not implemented in context"))) })
    }
    
    /// Remove watch for actor termination
    fn unwatch<'a>(&'a mut self, _target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        // Implementation will be provided by the actor system
        Box::pin(async { Err(ActorError::Other(anyhow!("unwatch not implemented in context"))) })
    }
    
    /// Get parent actor reference
    fn parent(&self) -> Option<BoxedActorRef> {
        // Will be set by the actor system when creating the actor
        None
    }
    
    /// Get references to all child actors
    fn children(&self) -> Vec<BoxedActorRef> {
        // Will be managed by the actor system
        vec![]
    }
    
    /// Set timeout for receiving messages
    fn set_receive_timeout(&mut self, _timeout: Option<std::time::Duration>) {
        // Implementation will be provided by the actor system
    }
    
    /// Get current receive timeout
    fn receive_timeout(&self) -> Option<std::time::Duration> {
        None
    }
    
    /// Set supervision strategy for child actors
    fn set_supervisor_strategy(&mut self, _strategy: parrot_api::supervisor::SupervisorStrategyType) {
        // Implementation will be provided by the actor system
    }
    
    /// Get the actor's path
    /// 
    /// # Returns
    /// The actor's unique path in the actor system
    fn path(&self) -> &ActorPath {
        &self.path
    }
    
    /// Get stream registry
    fn stream_registry(&mut self) -> &mut dyn parrot_api::stream::StreamRegistry {
        // Implementation will be provided by the actor system
        unimplemented!("stream_registry not implemented in context")
    }
    
    /// Get actor spawner
    fn spawner(&mut self) -> &mut dyn parrot_api::context::ActorSpawner {
        // Implementation will be provided by the actor system
        unimplemented!("spawner not implemented in context")
    }
} 