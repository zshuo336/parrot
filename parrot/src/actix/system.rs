use std::sync::Arc;
use actix::{System as ActixSystem, Actor as ActixActor, Context as ActixContext};
use async_trait::async_trait;
use parrot_api::{
    system::{ActorSystem, ActorSystemConfig, SystemError, SystemStatus},
    actor::Actor,
    address::{ActorPath, ActorRef},
    types::{BoxedActorRef, ActorResult},
    message::Message,
    context::ActorContext,
};
use super::{
    actor::ActixActorWrapper,
    address::ActixActorRef,
    context::ActixActorContext,
};
use uuid::Uuid;

pub struct ActixActorSystem {
    system: Arc<ActixSystem>,
    config: ActorSystemConfig,
}

#[async_trait]
impl ActorSystem for ActixActorSystem {
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError> {
        let system = ActixSystem::new();
        Ok(Self { 
            system: Arc::new(system),
            config,
        })
    }

    async fn spawn_root_typed<A: Actor>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        // Create a unique path for the actor
        let path = format!("/user/{}", Uuid::new_v4());
        let actor_path = ActorPath {
            path: path.clone(),
            target: Arc::new(()),  // TODO: Implement proper WeakActorTarget
        };

        // Wrap the actor
        let wrapped_actor = ActixActorWrapper::new(actor);

        // Start the actor
        let addr = wrapped_actor.start();
        
        // Create and return the actor reference
        Ok(Box::new(ActixActorRef::new(addr, path)))
    }

    async fn spawn_root_boxed(
        &self,
        actor: Box<dyn Actor<Config = Box<dyn std::any::Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn std::any::Any + Send>,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        // Create a unique path for the actor
        let path = format!("/user/{}", Uuid::new_v4());
        
        // Start the actor (implementation depends on how we handle type-erased actors)
        Err(SystemError::NotImplemented(
            "Type-erased actor creation not implemented".to_string()
        ))
    }

    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        // Lookup actor by path in the actix system
        None
    }

    async fn broadcast<M: Message>(&self, msg: M) -> Result<(), SystemError> {
        // Broadcast message to all actors in the system
        Ok(())
    }

    fn status(&self) -> SystemStatus {
        // Return current system status
        SystemStatus::default()
    }

    async fn shutdown(self) -> Result<(), SystemError> {
        // Shutdown the actix system
        Arc::try_unwrap(self.system)
            .map_err(|_| SystemError::ShutdownError("System still has references".to_string()))?
            .stop();
        Ok(())
    }
}

// Helper functions for system management
impl ActixActorSystem {
    pub fn current() -> &'static ActixSystem {
        ActixSystem::current()
    }

    pub fn register_actor(&self, path: ActorPath, actor_ref: BoxedActorRef) {
        // Register actor in the system registry
    }

    pub fn unregister_actor(&self, path: &ActorPath) {
        // Remove actor from the system registry
    }
} 