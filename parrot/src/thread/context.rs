use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use tokio::runtime::Handle;

use parrot_api::actor::Actor;
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::context::{ActorContext, ActorSpawner, ScheduledTask, ReadOnlyChildrenVec};
use parrot_api::errors::ActorError;
use parrot_api::stream::StreamRegistry;
use parrot_api::supervisor::SupervisorStrategyType;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use parrot_api::message::CloneableMessage;
use crate::thread::address::ThreadActorRef;
use crate::thread::config::{BackpressureStrategy, SupervisorStrategy};
use crate::logging;
use std::sync::RwLock;

/// Weak reference to the actor system to avoid circular references
pub type WeakSystemRef = Weak<dyn SystemRef + Send + Sync>;

/// Trait representing required system methods for the ThreadContext
#[async_trait]
pub trait SystemRef {
    fn runtime_handle(&self) -> &Handle;
    fn default_ask_timeout(&self) -> Duration;
    fn default_backpressure_strategy(&self) -> BackpressureStrategy;
    async fn spawn_actor(&self, actor: BoxedMessage, config: BoxedMessage, strategy: Option<SupervisorStrategy>) -> ActorResult<BoxedActorRef>;
}

/// ThreadContext provides the execution context for an actor instance.
/// 
/// It implements ActorContext from parrot-api and provides access to:
/// - The actor's own reference (self)
/// - Parent and children references
/// - The actor system
/// - Actor configuration and lifecycle management
/// - Methods for spawning child actors
#[derive(Debug)]
pub struct ThreadContext<A: Actor + Send + Sync + 'static> {
    /// Weak reference to the actor system
    system: WeakSystemRef,
    
    /// Handle to the Tokio runtime
    runtime_handle: Handle,
    
    /// Actor's own reference
    self_ref: Option<BoxedActorRef>,
    
    /// Reference to parent actor
    parent_ref: Option<BoxedActorRef>,

    /// References to child actors
    children_refs: Option<Arc<RwLock<HashMap<String, BoxedActorRef>>>>,
    
    /// Actor's path
    path: ActorPath,
    
    /// Supervision strategy for child actors
    supervisor_strategy: SupervisorStrategy,
    
    /// Receive timeout for ask operations
    receive_timeout: Option<Duration>,
    
    /// Default backpressure strategy
    backpressure_strategy: BackpressureStrategy,
    
    /// Stream registry (placeholder for now)
    stream_registry: (),
    
    /// Phantom data to associate with actor type
    _phantom: PhantomData<A>,
}

impl<A: Actor + Send + Sync + 'static> ThreadContext<A> {
    /// Creates a new ThreadContext.
    ///
    /// # Parameters
    /// * `system` - Weak reference to the actor system
    /// * `runtime_handle` - Handle to the Tokio runtime
    /// * `path` - The actor's path
    /// * `parent_ref` - Optional reference to parent actor
    /// * `supervisor_strategy` - Supervision strategy for child actors
    pub fn new(
        system: WeakSystemRef,
        runtime_handle: Handle,
        path: ActorPath,
        parent_ref: Option<BoxedActorRef>,
        supervisor_strategy: SupervisorStrategy,
    ) -> Self {
        Self {
            system,
            runtime_handle,
            self_ref: None,
            parent_ref,
            children_refs: None,
            path,
            supervisor_strategy,
            receive_timeout: None,
            backpressure_strategy: BackpressureStrategy::Block,
            stream_registry: (),
            _phantom: PhantomData,
        }
    }
    
    /// Sets the self reference.
    pub fn set_self_ref(&mut self, actor_ref: BoxedActorRef) {
        self.self_ref = Some(actor_ref);
    }
    
    /// get self reference. 
    /// 
    /// # Returns
    /// The actor's own reference, if it is set. otherwise, it will panic.
    pub fn get_self_ref(&self) -> BoxedActorRef {
        self.self_ref.as_ref().expect("Self reference not set").clone_boxed()
    }


    fn set_parent(&mut self, parent: BoxedActorRef) {
        self.parent_ref = Some(parent);
    }
    
    /// Upgrades the weak system reference to a strong reference if available.
    fn system(&self) -> Option<Arc<dyn SystemRef + Send + Sync>> {
        self.system.upgrade()
    }
    
    /// Adds a child actor reference.
    pub fn add_child(&mut self, child_ref: BoxedActorRef) {
        let path_str = child_ref.path();
        if self.children_refs.is_none() {
            self.children_refs = Some(Arc::new(RwLock::new(HashMap::new())));
        }
        self.children_refs.as_mut().unwrap().write().unwrap().insert(path_str, child_ref);
    }
    
    /// Removes a child actor reference.
    pub fn remove_child_by_path(&mut self, path: &ActorPath) -> Option<BoxedActorRef> {
        self.children_refs.as_mut().unwrap().write().unwrap().remove(path.path())
    }

    /// Removes a child actor reference.
    pub fn remove_child_by_ref(&mut self, child: &BoxedActorRef) -> Option<BoxedActorRef> {
        self.children_refs.as_mut().unwrap().write().unwrap().remove(&child.path())
    }

    /// Returns the children references.
    pub fn children(&self) -> Option<ReadOnlyChildrenVec> {
        self.children_refs.as_ref().map(|children| {
            let boxed_children = children.read().unwrap().values()
                .map(|r| r.clone_boxed())
                .collect::<Vec<_>>();
            ReadOnlyChildrenVec::new(Arc::new(RwLock::new(boxed_children)))
        })
    }

    /// Sets the backpressure strategy.
    pub fn set_backpressure_strategy(&mut self, strategy: BackpressureStrategy) {
        self.backpressure_strategy = strategy;
    }
    
    /// Gets the current backpressure strategy.
    pub fn backpressure_strategy(&self) -> &BackpressureStrategy {
        &self.backpressure_strategy
    }
}

impl<A: Actor + Send + Sync + 'static> ActorContext for ThreadContext<A> {
    fn get_self_ref(&self) -> BoxedActorRef {
        self.get_self_ref()
    }
    
    fn set_parent(&mut self, parent: BoxedActorRef) {
        self.set_parent(parent);
    }

    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if let Some(self_ref) = &self.self_ref {
                self_ref.stop().await?;
                
                // Stop all children
                if let Some(children) = &self.children_refs {
                    let children_to_stop: Vec<_> = children.read().unwrap()
                        .values()
                        .map(|r| r.clone_boxed())
                        .collect();
                    
                    for child in children_to_stop {
                        let result = child.stop().await;
                        if let Err(e) = result {
                            logging::error!("Failed to stop child actor-{}: {}", child.path(), e);
                        }
                    }
                }
                
                Ok(())
            } else {
                Err(ActorError::Other(anyhow::anyhow!("Self reference not set")))
            }
        })
    }
    
    fn send<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            target.send(msg).await?;
            Ok(())
        })
    }
    
    /// todo: implement complete ask
    fn ask<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let timeout_duration = self.receive_timeout.unwrap_or_else(|| {
            self.system()
                .map(|s| s.default_ask_timeout())
                .unwrap_or_else(|| Duration::from_secs(5))
        });
        
        Box::pin(async move {
            // We assume target is a ThreadActorRef with ask capability
            // In a full implementation, we would need to handle different actor ref types
            // and potentially use an adapter pattern
            
            // For now, we'll just call target.send and create a dummy response
            target.send(msg).await?;
            
            // This is a placeholder - in a real implementation we'd do proper ask
            Ok(Box::new(()) as BoxedMessage)
        })
    }
    
    fn schedule_once<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage, delay: Duration) -> BoxedFuture<'a, ActorResult<()>> {
        let runtime = self.runtime_handle.clone();
        
        Box::pin(async move {
            let target_clone = target.clone_boxed();
            
            // Create a task to send the message after the delay
            runtime.spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = target_clone.send(msg).await;
            });
            
            Ok(())
        })
    }
    
    fn schedule_periodic<'a>(&'a self, target: BoxedActorRef, msg: CloneableMessage, initial_delay: Duration, interval: Duration) -> BoxedFuture<'a, ActorResult<()>> {
        let runtime = self.runtime_handle.clone();
        
        Box::pin(async move {
            let target_clone = target.clone_boxed();
            
            runtime.spawn(async move {
                // Initial delay
                tokio::time::sleep(initial_delay).await;
                
                // Send first message
                let _ = target_clone.send(msg.clone().into_boxed()).await;

                // Set up periodic interval
                let mut interval_timer = tokio::time::interval(interval);
                
                loop {
                    interval_timer.tick().await;
                    
                    let target_copy = target_clone.clone_boxed();
                    if let Err(_) = target_copy.send(msg.clone().into_boxed()).await {
                        // Target is likely dead, stop sending
                        break;
                    }
                }
            });
            
            Ok(())
        })
    }
    
    fn watch<'a>(&'a mut self, _target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        // TODO: Implement watching mechanism
        Box::pin(async { Ok(()) })
    }
    
    fn unwatch<'a>(&'a mut self, _target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        // TODO: Implement unwatching mechanism
        Box::pin(async { Ok(()) })
    }
    
    fn parent(&self) -> Option<BoxedActorRef> {
        self.parent_ref.as_ref().map(|r| r.clone_boxed())
    }

    fn add_child(&mut self, child: BoxedActorRef) {
        self.add_child(child);
    }

    fn remove_child(&mut self, child: BoxedActorRef) {
        self.remove_child_by_ref(&child);
    }
    
    fn children(&self) -> Option<ReadOnlyChildrenVec> {
        self.children()
    }
    
    fn set_receive_timeout(&mut self, timeout: Option<Duration>) {
        self.receive_timeout = timeout;
    }
    
    fn receive_timeout(&self) -> Option<Duration> {
        self.receive_timeout
    }
    
    fn set_supervisor_strategy(&mut self, strategy: SupervisorStrategyType) {
        // Convert from API strategy type to our internal strategy type
        // For now, just set a default
        self.supervisor_strategy = SupervisorStrategy::Restart {
            max_retries: 3,
            within: Duration::from_secs(10),
        };
    }
    
    fn path(&self) -> &ActorPath {
        &self.path
    }
    
    fn stream_registry(&mut self) -> &mut dyn StreamRegistry {
        // TODO: Implement proper stream registry
        panic!("Stream registry not yet implemented for ThreadContext");
    }
    
    fn spawner(&mut self) -> &mut dyn ActorSpawner {
        // Self-reference as the spawner
        self
    }
}

#[async_trait]
impl<A: Actor + Send + Sync + 'static> ActorSpawner for ThreadContext<A> {
    fn spawn<'a>(&'a self, actor: BoxedMessage, config: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        Box::pin(async move {
            if let Some(system) = self.system() {
                // Use the current context's supervisor strategy
                system.spawn_actor(actor, config, Some(self.supervisor_strategy.clone())).await
            } else {
                Err(ActorError::Other(anyhow::anyhow!("Actor system not available")))
            }
        })
    }
    
    fn spawn_with_strategy<'a>(&'a self, actor: BoxedMessage, config: BoxedMessage, _strategy: SupervisorStrategyType) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        // For now, we'll just use our internal strategy type
        // In a full implementation, we would convert from API strategy type
        self.spawn(actor, config)
    }
}

#[cfg(test)]
mod tests {
    // TODO: Add tests for ThreadContext
} 