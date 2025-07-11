use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use actix::System as ActixSystem;
use async_trait::async_trait;
use anyhow::anyhow;
use parrot_api::system::{ActorSystemConfig, SystemError, SystemStatus, SystemState, SystemResources};
use parrot_api::actor::Actor as ParrotActor;
use parrot_api::address::ActorRef;
use parrot_api::message::Message;
use parrot_api::types::{BoxedActorRef, ActorResult};
use crate::actix::actor::ActixActor;
use crate::actix::reference::ActixActorRef;
use std::time::Duration;

/// ActixActorSystem implements the actor system for Actix
/// 
/// # Overview
/// Core system for managing actors in the Actix backend
/// 
/// # Key Responsibilities
/// - Create and manage actors
/// - Track actor references
/// - Handle system-wide operations
/// - Mediate broadcast messages
/// 
/// # Implementation Details
/// - Wraps an actix::System instance
/// - Manages actor address mappings
/// - Handles actor lifecycle
/// - Provides supervision capabilities
pub struct ActixActorSystem {
    /// Underlying Actix system
    system: Arc<ActixSystem>,
    /// Registry of active actors by path
    actors: RwLock<HashMap<String, BoxedActorRef>>,
    /// System configuration
    config: ActorSystemConfig,
}

impl ActixActorSystem {
    /// Create a new ActixActorSystem
    /// 
    /// # Parameters
    /// - `config`: System configuration parameters
    /// 
    /// # Returns
    /// A new ActixActorSystem instance or error
    pub async fn new() -> Result<Self, SystemError> {
        // Create a new Actix system with default settings
        let _ = actix::System::new();
        
        Ok(Self {
            system: Arc::new(ActixSystem::current()),
            actors: RwLock::new(HashMap::new()),
            config: ActorSystemConfig::default(),
        })
    }
    
    /// Spawn a root-level actor
    /// 
    /// # Type Parameters
    /// - `A`: Actor type implementing ParrotActor
    /// 
    /// # Parameters
    /// - `actor`: Actor instance to spawn
    /// - `config`: Actor configuration
    /// 
    /// # Returns
    /// Reference to the created actor or error
    pub async fn spawn_root_typed<A: ParrotActor + 'static>(
        &self,
        actor: A,
        _config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        // Create ActixActor wrapper
        let actor_base = ActixActor::new(actor);
        
        // Start the actor in Actix
        let addr = actor_base.start();
        
        // Generate actor path
        let path = format!("actix://{}", addr.clone());
        
        // Create actor reference
        let actor_ref = Box::new(ActixActorRef::new(addr, path.clone())) as Box<dyn ActorRef>;
        
        // Register actor
        let mut actors = self.actors.write().map_err(|_| {
            SystemError::Other(anyhow!("Failed to acquire write lock"))
        })?;
        
        actors.insert(path, actor_ref.clone_boxed());
        
        Ok(actor_ref)
    }
    
    /// Get an actor by its path
    /// 
    /// # Parameters
    /// - `path`: The actor's path
    /// 
    /// # Returns
    /// Reference to the actor if found
    pub async fn get_actor(&self, path: &String) -> Option<BoxedActorRef> {
        let actors = match self.actors.read() {
            Ok(actors) => actors,
            Err(_) => return None,
        };
        
        actors.get(path).map(|actor_ref| actor_ref.clone_boxed())
    }
    
    /// Broadcast a message to all actors
    /// 
    /// # Type Parameters
    /// - `M`: Message type implementing Message
    /// 
    /// # Parameters
    /// - `msg`: Message to broadcast
    /// 
    /// # Returns
    /// Success or error
    pub async fn broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        let actors = match self.actors.read() {
            Ok(actors) => actors,
            Err(_) => return Err(SystemError::Other(anyhow!("Failed to acquire read lock"))),
        };
        
        for actor_ref in actors.values() {
            // Use the ActorRefExt::tell method to send the message without waiting for a response
            // Clone the message for each actor
            let actor_ref_clone = actor_ref.clone_boxed();
            let msg_clone = msg.clone();
            
            // Spawn a task to send the message
            tokio::spawn(async move {
                let boxed_msg = Box::new(msg_clone) as Box<dyn std::any::Any + Send>;
                let _ = actor_ref_clone.send(boxed_msg).await;
            });
        }
        
        Ok(())
    }
    
    /// Shutdown the actor system
    /// 
    /// # Returns
    /// Success or error
    pub async fn shutdown(self) -> Result<(), SystemError> {
        // Stop all actors
        {
            let actors = match self.actors.read() {
                Ok(actors) => actors,
                Err(_) => return Err(SystemError::Other(anyhow!("Failed to acquire read lock"))),
            };
            
            for actor_ref in actors.values() {
                // Spawn a task to stop the actor
                let actor_ref_clone = actor_ref.clone_boxed();
                tokio::spawn(async move {
                    let _ = actor_ref_clone.stop().await;
                });
            }
        }
        
        // Wait for actors to stop (could add a timeout here)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        
        // Stop the Actix system
        actix::System::current().stop();
        
        Ok(())
    }
    
    /// Get status of the actor system
    /// 
    /// # Returns
    /// Current system status
    pub fn status(&self) -> SystemStatus {
        SystemStatus {
            state: SystemState::Running,
            active_actors: self.actors.read().map(|a| a.len()).unwrap_or(0),
            uptime: Duration::from_secs(0), // 实际应用中应该计算真实运行时间
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 1,
            },
        }
    }
} 