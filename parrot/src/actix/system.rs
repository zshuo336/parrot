use std::sync::{Arc, RwLock};
use std::collections::HashMap;

use actix::System as ActixSystemInner;
use async_trait::async_trait;
use parrot_api::{
    system::{ActorSystem, ActorSystemConfig, SystemError, SystemStatus},
    actor::Actor,
    address::{ActorPath, ActorRef},
    types::{BoxedActorRef, ActorResult},
    message::Message,
};
use crate::actix::{
    actor::{ActorBase, ActixActorRef},
    context::ActixContext,
};
use uuid::Uuid;

/// Actix implementation of the Actor System
pub struct ActixActorSystem {
    system: Arc<ActixSystemInner>,
    config: ActorSystemConfig,
    actors: RwLock<HashMap<String, BoxedActorRef>>,
}

impl ActixActorSystem {
    /// Create a new ActixActorSystem
    pub async fn new() -> Result<Self, SystemError> {
        // Create a new actix system
        let system = ActixSystemInner::new();
        
        Ok(Self {
            system: Arc::new(system),
            config: ActorSystemConfig::default(),
            actors: RwLock::new(HashMap::new()),
        })
    }
    
    /// Create actor and register it with the system
    pub async fn spawn_root_typed<A: Actor<Context = ActixContext> + 'static>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        // Create a unique path for the actor
        let path = format!("/user/{}", Uuid::new_v4());
        let actor_path = ActorPath::new(&path);
        
        // Convert to ActorBase
        let actor_base = ActorBase::new(actor);
        
        // Start the actor
        let addr = actor_base.start();
        
        // Create actor reference
        let actor_ref = Box::new(ActixActorRef::new(addr)) as BoxedActorRef;
        
        // Register the actor
        let mut actors = self.actors.write().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire write lock"))
        })?;
        
        actors.insert(path, actor_ref.clone());
        
        Ok(actor_ref)
    }
    
    /// Look up actor by path
    pub async fn get_actor(&self, path: &ActorPath) -> Option<BoxedActorRef> {
        let actors = match self.actors.read() {
            Ok(guard) => guard,
            Err(_) => return None,
        };
        
        actors.get(path.path()).cloned()
    }
    
    /// Broadcast a message to all actors in the system
    pub async fn broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        let actors = self.actors.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        for actor_ref in actors.values() {
            let _ = actor_ref.send(Box::new(msg.clone()));
        }
        
        Ok(())
    }
    
    /// Shutdown the actor system
    pub async fn shutdown(self) -> Result<(), SystemError> {
        // Clear actors
        let mut actors = self.actors.write().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire write lock"))
        })?;
        
        actors.clear();
        
        // Stop the actix system
        match Arc::try_unwrap(self.system) {
            Ok(system) => {
                system.stop();
                Ok(())
            },
            Err(_) => Err(SystemError::ShutdownError("Failed to stop actor system: still has references".to_string())),
        }
    }
}

#[async_trait]
impl ActorSystem for ActixActorSystem {
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError> {
        Self::new().await
    }

    async fn spawn_root_typed<A: Actor + 'static>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        if let Ok(actor) = actor.downcast::<dyn Actor<Context = ActixContext>>() {
            self.spawn_root_typed(actor, config).await
        } else {
            Err(SystemError::Other(anyhow::anyhow!(
                "Actor cannot be downcasted to ActixContext-compatible actor"
            )))
        }
    }

    async fn spawn_root_boxed(
        &self,
        actor: Box<dyn Actor<Config = Box<dyn std::any::Any + Send>, Context = dyn parrot_api::context::ActorContext>>,
        config: Box<dyn std::any::Any + Send>,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        Err(SystemError::NotImplemented(
            "Type-erased actor creation not implemented".to_string()
        ))
    }

    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        self.get_actor(path).await
    }

    async fn broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        self.broadcast(msg).await
    }

    fn status(&self) -> SystemStatus {
        SystemStatus::default()
    }

    async fn shutdown(self) -> Result<(), SystemError> {
        self.shutdown().await
    }
} 