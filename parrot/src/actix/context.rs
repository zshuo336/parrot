use std::sync::Arc;
use std::cell::RefCell;
use actix::{Actor as ActixActor, ActorContext as ActixActorContext, Addr, AsyncContext, Context as ActixCtx, Message};
use parrot_api::context::{ActorContext, ReadOnlyChildrenVec};
use parrot_api::address::ActorPath;
use parrot_api::message::{BoxedMessageClone, CloneableMessage};
use parrot_api::types::{BoxedActorRef, BoxedFuture, ActorResult};
use parrot_api::errors::ActorError;
use anyhow::anyhow;
use std::sync::RwLock;

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
    /// The actor's parent reference
    parent: Option<Arc<BoxedActorRef>>,
    /// The actor's child references
    children: Option<Arc<RwLock<Vec<BoxedActorRef>>>>,
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
        let ctx = Self {
            addr: Arc::new(addr),
            path,
            parent: None,
            children: None,
        };
        ctx
    }

    /// Create a new ActixContext with a parent reference
    /// 
    /// # Parameters
    /// - `addr`: The Actix address to wrap
    /// - `path`: Actor path in the system
    /// - `parent`: The parent actor reference
    /// 
    /// # Returns
    /// A new ActixContext instance
    pub fn new_with_parent(addr: Addr<A>, path: ActorPath, parent: BoxedActorRef) -> Self {
        let mut ctx = Self::new(addr, path);
        ctx.parent = Some(Arc::new(parent));
        ctx
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
        self.path.target.clone_boxed()
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
    fn send<'a>(&'a self, target: BoxedActorRef, msg: parrot_api::types::BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            target.send(msg).await.map_err(|e| ActorError::Other(anyhow!("Failed to send message: {}", e)))?;
            Ok(())
        })
    }
    
    /// Send a message to another actor and wait for a response
    fn ask<'a>(&'a self, target: BoxedActorRef, msg: parrot_api::types::BoxedMessage) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        Box::pin(async move {
            let result = target.send(msg).await.map_err(|e| ActorError::Other(anyhow!("Failed to send message: {}", e)))?;
            Ok(result)
        })
    }
    
    /// Schedule a one-time delayed message
    fn schedule_once<'a>(&'a self, target: BoxedActorRef, msg: parrot_api::types::BoxedMessage, delay: std::time::Duration) -> BoxedFuture<'a, ActorResult<()>> {
        // Use a local reference to the inner context
        let addr = self.addr.clone();
        
        Box::pin(async move {
            // Create a delayed task using actix runtime
            actix::spawn(async move {
                // Wait for the specified delay
                actix::clock::sleep(delay).await;
                
                // Send the message after delay
                target.send(msg).await.ok(); // Ignore send errors for scheduled messages
            });
            
            Ok(())
        })
    }
    
    /// Schedule a recurring message
    fn schedule_periodic<'a>(&'a self, target: BoxedActorRef, msg: CloneableMessage, initial_delay: std::time::Duration, interval: std::time::Duration) -> BoxedFuture<'a, ActorResult<()>> {
        // Use a local reference to the inner context
        let addr = self.addr.clone();
        
        Box::pin(async move {
            // Wait for the initial delay
            actix::clock::sleep(initial_delay).await;
            // Send the message after initial delay
            target.send(msg.clone().into_boxed()).await.ok();

            // Create a periodic task
            loop {
                // Wait for the interval
                actix::clock::sleep(interval).await;
        
                // Send the message after interval
                target.send(msg.clone().into_boxed()).await.ok();
            }            
        })
    }
    
    /// Watch another actor for termination
    /// 
    /// # Parameters
    /// - `target`: Reference to another actor to watch
    /// 
    /// When the watched actor terminates, this actor will
    /// receive a Terminated message.
    fn watch<'a>(&'a mut self, _target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Err(ActorError::Other(anyhow!("unwatch not implemented in context"))) })
    }
    
    /// Remove watch for actor termination
    fn unwatch<'a>(&'a mut self, _target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        // Implementation will be provided by the actor system
        Box::pin(async { Err(ActorError::Other(anyhow!("unwatch not implemented in context"))) })
    }
    
    /// Set the parent actor reference
    fn set_parent(&mut self, parent: BoxedActorRef) {
        self.parent = Some(Arc::new(parent));
    }

    /// Get parent actor reference
    fn parent(&self) -> Option<BoxedActorRef> {
        self.parent.as_ref().map(|p| p.as_ref().clone_boxed())
    }
    
    /// Add a child actor to the context
    fn add_child(&mut self, child: BoxedActorRef) {
        if let Some(children) = &mut self.children {
            children.write().unwrap().push(child);
        } else {
            self.children = Some(Arc::new(RwLock::new(vec![child])));
        }
    }

    /// Remove a child actor from the context
    fn remove_child(&mut self, child: BoxedActorRef) {
        if let Some(children) = &mut self.children {
            let mut children = children.write().unwrap();
            children
                .iter()
                .position(|c| c.as_ref().eq(&*child))
                .map(|index| children.remove(index));
        }
    }

    /// Get references to all child actors
    fn children(&self) -> Option<ReadOnlyChildrenVec> {
        self.children.as_ref().map(|children| ReadOnlyChildrenVec::new(children.clone()))
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