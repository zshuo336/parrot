use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use actix::{System as ActixSystem, Actor as ActixActor, Context as ActixContext};
use async_trait::async_trait;
use parrot_api::{
    system::{ActorSystem, ActorSystemConfig, SystemError, SystemStatus, SystemState, SystemResources},
    actor::Actor,
    address::{ActorPath, ActorRef},
    types::{BoxedActorRef, ActorResult},
    message::Message,
    context::ActorContext,
};
use crate::actix::ActixActorSystem;
use uuid::Uuid;
use anyhow;

/// Supported ActorSystem implementation types
#[derive(Clone)]
pub enum ActorSystemImpl {
    /// Actix implementation
    Actix(ActixActorSystem),
}

impl ActorSystemImpl {
    /// Forward spawn_root_typed method; generic version returns an error
    pub async fn spawn_root_typed<A: Actor + 'static>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        match self {
            ActorSystemImpl::Actix(_) => {
                Err(SystemError::ActorCreationError(
                    "Generic actor creation not supported directly. Use spawn_root_typed_actix for Actix system.".to_string()
                ))
            }
            // Add branches for other system types
        }
    }
    
    /// Actix-specific spawn method with necessary type constraints
    pub async fn spawn_root_typed_actix<A>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError>
    where
        A: Actor<Context = crate::actix::context::ActixContext<
                crate::actix::actor::ActixActor<A>,
            >> + std::marker::Unpin + 'static
    {
        // Move actor and config into a local tuple to avoid capturing external variables in the async block
        let actor_data = (actor, config);
        let self_clone = self.clone();
        
        async move {
            match self_clone {
                ActorSystemImpl::Actix(sys) => {
                    sys.spawn_root_typed(actor_data.0, actor_data.1).await
                },
                _ => Err(SystemError::ActorCreationError("Not an Actix system".to_string())),
            }
        }
        .await
    }
    
    /// Forward get_actor method
    pub async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        // Clone path to avoid borrowing the original in the async block
        let path_copy = ActorPath {
            path: path.path.clone(),
            target: path.target.clone(),
        };
        let self_clone = self.clone();
        
        async move {
            match self_clone {
                ActorSystemImpl::Actix(sys) => {
                    // ActixActorSystem's get_actor method expects a &String
                    sys.get_actor(&path_copy.path).await
                }
            }
        }
        .await
    }
    
    /// Forward broadcast method
    pub async fn broadcast<M: Message + Clone + 'static>(
        &self,
        msg: M,
    ) -> Result<(), SystemError> {
        // Clone the message to avoid borrowing the original in the async block
        let msg_copy = msg.clone();
        let self_clone = self.clone();
        
        async move {
            match self_clone {
                ActorSystemImpl::Actix(sys) => sys.broadcast(msg_copy).await,
                // Add branches for other system types
            }
        }
        .await
    }
    
    /// Forward shutdown method
    pub async fn shutdown(self) -> Result<(), SystemError> {
        match self {
            ActorSystemImpl::Actix(sys) => sys.shutdown().await,
            // Add branches for other system types
        }
    }
}

/// ParrotActorSystem manages multiple ActorSystem implementations
pub struct ParrotActorSystem {
    // Main system configuration
    config: ActorSystemConfig,
    // Store registered ActorSystem implementations, using name as key
    systems: RwLock<HashMap<String, ActorSystemImpl>>,
    // Default system name
    default_system: RwLock<Option<String>>,
}

impl ParrotActorSystem {
    /// Create new ParrotActorSystem
    pub async fn new(config: ActorSystemConfig) -> Result<Self, SystemError> {
        Ok(Self {
            config,
            systems: RwLock::new(HashMap::new()),
            default_system: RwLock::new(None),
        })
    }

    /// Register an Actix system
    pub async fn register_actix_system(
        &self,
        name: String,
        system: ActixActorSystem,
        set_as_default: bool,
    ) -> Result<(), SystemError> {
        let mut systems = self.systems.write().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire write lock"))
        })?;
        
        // Store the system
        systems.insert(name.clone(), ActorSystemImpl::Actix(system));
        
        // Set as default if needed
        if set_as_default || self.default_system.read().unwrap().is_none() {
            *self.default_system.write().unwrap() = Some(name);
        }
        
        Ok(())
    }

    /// Set default system
    pub fn set_default_system(&self, name: &str) -> Result<(), SystemError> {
        // Verify system exists
        let systems = self.systems.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        if !systems.contains_key(name) {
            return Err(SystemError::Other(anyhow::anyhow!(
                format!("System not found: {}", name)
            )));
        }
        
        // Set as default
        *self.default_system.write().unwrap() = Some(name.to_string());
        Ok(())
    }

    /// Get default system
    fn get_default_system_impl(&self) -> Result<ActorSystemImpl, SystemError> {
        let guard = self.default_system.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        let default_name = guard.as_ref().ok_or_else(|| {
            SystemError::Other(anyhow::anyhow!("No default system registered"))
        })?;
            
        self.get_system_impl(default_name)
    }
    
    /// Get system by name
    fn get_system_impl(&self, name: &str) -> Result<ActorSystemImpl, SystemError> {
        let systems = self.systems.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        // Clone the system instance to release the read lock
        match systems.get(name) {
            Some(system) => Ok(system.clone()),
            None => Err(SystemError::Other(anyhow::anyhow!(
                format!("System not found: {}", name)
            ))),
        }
    }
    
    /// List all registered system names
    pub fn list_registered_systems(&self) -> Result<Vec<String>, SystemError> {
        let systems = self.systems.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        Ok(systems.keys().cloned().collect())
    }
    
    /// Create actor in default system
    pub async fn internal_spawn_actor<A: Actor + 'static>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        // First, retrieve the default system name and release the lock immediately
        let default_name = self.get_default_system_name()?;
        
        // Then clone the system to avoid holding the lock across an await
        let system = self.get_system_impl(&default_name)?;
        
        // Perform the actual spawn operation in a new async block
        // No locks are held at this point, so the future can safely be sent across threads
        system.spawn_root_typed(actor, config).await
    }
    
    /// Create actor in specified system
    pub async fn spawn_actor_in_system<A: Actor + 'static>(
        &self,
        system_name: &str,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        // Use get_system_impl to get a cloned system instance
        let system = self.get_system_impl(system_name)?;
            
        system.spawn_root_typed(actor, config).await
    }
    
    /// Query actor by path
    pub async fn internal_get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        // Try to get the actor from the default system
        let default_system = match self.get_default_system_name() {
            Ok(name) => name,
            Err(_) => "".to_string(), // Use an empty string to indicate no default system
        };
        
        if !default_system.is_empty() {
            // Clone the default system instance
            if let Ok(system) = self.get_system_impl(&default_system) {
                if let Some(actor) = system.get_actor(path).await {
                    return Some(actor);
                }
            }
        }
        
        // If not found in the default system, try all registered systems
        if let Ok(system_names) = self.list_registered_systems() {
            for name in system_names {
                // Skip the default system which has already been checked
                if name == default_system {
                    continue;
                }
                
                if let Ok(system) = self.get_system_impl(&name) {
                    if let Some(actor) = system.get_actor(path).await {
                        return Some(actor);
                    }
                }
            }
        }
        
        None
    }
    
    /// Get the default system name
    fn get_default_system_name(&self) -> Result<String, SystemError> {
        let guard = self.default_system.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        guard.clone().ok_or_else(|| {
            SystemError::Other(anyhow::anyhow!("No default system registered"))
        })
    }
    
    /// Broadcast message to all systems
    pub async fn internal_broadcast<M: Message + Clone + 'static>(
        &self,
        msg: M,
    ) -> Result<(), SystemError> {
        // Get all registered system names
        let system_names = self.list_registered_systems()?;
        
        for name in system_names {
            // Clone each system instance
            if let Ok(system) = self.get_system_impl(&name) {
                system.broadcast(msg.clone()).await?;
            }
        }
        
        Ok(())
    }
    
    /// Shutdown all systems
    pub async fn internal_shutdown(self) -> Result<(), SystemError> {
        println!("ParrotActorSystem: Starting shutdown sequence");
        
        let systems = match self.systems.into_inner() {
            Ok(systems) => systems,
            Err(_) => {
                println!("ParrotActorSystem: Failed to unwrap systems");
                return Err(SystemError::Other(anyhow::anyhow!(
                    "Failed to unwrap systems"
                )))
            }
        };
        
        println!("ParrotActorSystem: Shutting down {} registered systems", systems.len());
        
        let mut errors = Vec::new();
        
        for (name, system) in systems {
            println!("ParrotActorSystem: Shutting down system '{}'", name);
            if let Err(e) = system.shutdown().await {
                let error_msg = format!("Failed to shut down system {}: {}", name, e);
                println!("ParrotActorSystem: {}", error_msg);
                errors.push(error_msg);
            } else {
                println!("ParrotActorSystem: System '{}' shutdown completed", name);
            }
        }
        
        if errors.is_empty() {
            println!("ParrotActorSystem: All systems shutdown successfully");
            Ok(())
        } else {
            let error_msg = errors.join("; ");
            println!("ParrotActorSystem: Shutdown completed with errors: {}", error_msg);
            Err(SystemError::Other(anyhow::anyhow!(error_msg)))
        }
    }

    /// Spawn an actor specific to the Actix system
    ///
    /// This method is an Actix-specific version of spawn_root_typed,
    /// designed for actors compatible with the Actix backend
    pub async fn spawn_root_actix<A>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError>
    where
        A: Actor<Context = crate::actix::context::ActixContext<
                crate::actix::actor::ActixActor<A>,
            >> + std::marker::Unpin + 'static
    {
        // Get the default system name
        let default_name = self.get_default_system_name()?;
        
        // Get the system instance
        let system = self.get_system_impl(&default_name)?;
        
        // Check if the default system is an Actix system
        match system {
            ActorSystemImpl::Actix(_) => {
                // Use the Actix-specific spawn method
                system.spawn_root_typed_actix(actor, config).await
            },
            _ => Err(SystemError::ActorCreationError(
                "Default system is not an Actix system".to_string()
            )),
        }
    }
}

#[async_trait]
impl ActorSystem for ParrotActorSystem {
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError> {
        Ok(Self {
            config,
            systems: RwLock::new(HashMap::new()),
            default_system: RwLock::new(None),
        })
    }

    async fn spawn_root_typed<A: Actor + 'static>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        // Move actor and config into a local variable to avoid holding locks across await points
        let actor_data = (actor, config);
        
        // Use the generic implementation; for Actix-compatible actors, use spawn_root_actix
        async move {
            self.internal_spawn_actor(actor_data.0, actor_data.1).await
        }
        .await
    }

    async fn spawn_root_boxed(
        &self,
        actor: Box<dyn Actor<Config = Box<dyn std::any::Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn std::any::Any + Send>,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        Err(SystemError::ActorCreationError(
            "Type-erased actor creation not implemented".to_string()
        ))
    }

    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        // Clone the path to avoid referencing the original variable in the new async block
        let path_copy = ActorPath {
            path: path.path.clone(),
            target: path.target.clone(),
        };
        
        async move {
            self.internal_get_actor(&path_copy).await
        }
        .await
    }

    /// Sends a message to all actors in the system.
    ///
    /// This method provides a way to notify all actors of
    /// system-wide events or state changes.
    ///
    /// # Type Parameters
    /// * `M` - Message type implementing Message trait
    ///
    /// # Parameters
    /// * `msg` - Message to broadcast
    ///
    /// # Returns
    /// Success or failure of the broadcast operation
    async fn broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        // Clone the message to avoid capturing the original in the new async block
        let msg_copy = msg.clone();
        
        async move {
            self.internal_broadcast(msg_copy).await
        }
        .await
    }

    fn status(&self) -> SystemStatus {
        // Return combined status
        SystemStatus {
            state: SystemState::Running,
            active_actors: 0,
            uptime: std::time::Duration::from_secs(0),
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        }
    }

    async fn shutdown(self) -> Result<(), SystemError> {
        self.internal_shutdown().await
    }
}
