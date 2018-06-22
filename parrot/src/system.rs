use std::sync::{Arc, RwLock};
use std::collections::HashMap;
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
use crate::actix::ActixActorSystem;
use uuid::Uuid;
use anyhow;

/// Supported ActorSystem implementation types
pub enum ActorSystemImpl {
    /// Actix implementation
    Actix(ActixActorSystem),
}

impl ActorSystemImpl {
    /// Forward spawn_root_typed method
    pub async fn spawn_root_typed<A: Actor + 'static>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        match self {
            ActorSystemImpl::Actix(sys) => sys.spawn_root_typed(actor, config).await,
            // Add branches for other system types
        }
    }
    
    /// Forward get_actor method
    pub async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        match self {
            ActorSystemImpl::Actix(sys) => sys.get_actor(path).await,
        }
    }
    
    /// Forward broadcast method
    pub async fn broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        match self {
            ActorSystemImpl::Actix(sys) => sys.broadcast(msg).await,
            // Add branches for other system types
        }
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
            return Err(SystemError::Other(anyhow::anyhow!(format!("System not found: {}", name))));
        }
        
        // Set as default
        *self.default_system.write().unwrap() = Some(name.to_string());
        Ok(())
    }

    /// Get default system
    fn get_default_system_impl(&self) -> Result<&ActorSystemImpl, SystemError> {
        let guard = self.default_system.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        let default_name = guard.as_ref()
            .ok_or_else(|| SystemError::Other(anyhow::anyhow!("No default system registered")))?;
            
        self.get_system_impl(default_name)
    }
    
    /// Get system by name
    fn get_system_impl(&self, name: &str) -> Result<&ActorSystemImpl, SystemError> {
        let systems = self.systems.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        systems.get(name)
            .ok_or_else(|| SystemError::Other(anyhow::anyhow!(format!("System not found: {}", name))))
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
        let systems = self.systems.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        let default_system_guard = self.default_system.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock for default system"))
        })?;
        let default_name = default_system_guard.as_ref()
            .ok_or_else(|| SystemError::Other(anyhow::anyhow!("No default system registered")))?;
            
        let system = systems.get(default_name)
            .ok_or_else(|| SystemError::Other(anyhow::anyhow!(format!("Default system not found: {}", default_name))))?;
            
        system.spawn_root_typed(actor, config).await
    }
    
    /// Create actor in specified system
    pub async fn spawn_actor_in_system<A: Actor + 'static>(
        &self,
        system_name: &str,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        let systems = self.systems.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        let system = systems.get(system_name)
            .ok_or_else(|| SystemError::Other(anyhow::anyhow!(format!("System not found: {}", system_name))))?;
            
        system.spawn_root_typed(actor, config).await
    }
    
    /// Query actor by path
    pub async fn internal_get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        // Try default system first
        if let Ok(default_name) = self.default_system.read().unwrap().as_ref().ok_or(()) {
            let systems = match self.systems.read() {
                Ok(systems) => systems,
                Err(_) => return None,
            };
            
            if let Some(system) = systems.get(default_name) {
                if let Some(actor) = system.get_actor(path).await {
                    return Some(actor);
                }
            }
        }
        
        // If not found in default system, try all systems
        let systems = match self.systems.read() {
            Ok(systems) => systems,
            Err(_) => return None,
        };
        
        for system in systems.values() {
            if let Some(actor) = system.get_actor(path).await {
                return Some(actor);
            }
        }
        
        None
    }
    
    /// Broadcast message to all systems
    pub async fn internal_broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        let systems = self.systems.read().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire read lock"))
        })?;
        
        for system in systems.values() {
            system.broadcast(msg.clone()).await?;
        }
        
        Ok(())
    }
    
    /// Shutdown all systems
    pub async fn internal_shutdown(self) -> Result<(), SystemError> {
        let systems = match self.systems.into_inner() {
            Ok(systems) => systems,
            Err(_) => return Err(SystemError::Other(anyhow::anyhow!("Failed to unwrap systems"))),
        };
        
        let mut errors = Vec::new();
        
        for (name, system) in systems {
            if let Err(e) = system.shutdown().await {
                errors.push(format!("Failed to shut down system {}: {}", name, e));
            }
        }
        
        if errors.is_empty() {
            Ok(())
        } else {
            Err(SystemError::Other(anyhow::anyhow!(errors.join("; "))))
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
        self.internal_spawn_actor(actor, config).await
    }

    async fn spawn_root_boxed(
        &self,
        actor: Box<dyn Actor<Config = Box<dyn std::any::Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn std::any::Any + Send>,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        Err(SystemError::NotImplemented(
            "Type-erased actor creation not implemented".to_string()
        ))
    }

    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        self.internal_get_actor(path).await
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
        self.internal_broadcast(msg).await
    }

    fn status(&self) -> SystemStatus {
        // Return combined status
        SystemStatus::default()
    }

    async fn shutdown(self) -> Result<(), SystemError> {
        self.internal_shutdown().await
    }
}
