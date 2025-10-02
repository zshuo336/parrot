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
use crate::thread::system::ThreadActorSystem;
use uuid::Uuid;
use anyhow;

/// Supported ActorSystem implementation types
#[derive(Clone)]
pub enum ActorSystemImpl {
    /// Actix implementation
    Actix(ActixActorSystem),
    /// Thread engine implementation (native shared-pool / dedicated-thread)
    Thread(Arc<ThreadActorSystem>),
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
            },
            ActorSystemImpl::Thread(sys) => {
                // The engine-specific typed entry point requires the
                // `Context = ThreadContext<A>` bound, which cannot be proven
                // from the fully generic context; return a helpful error.
                let _ = sys;
                Err(SystemError::ActorCreationError(
                    "Generic actor creation not supported directly. Use spawn_root_thread for the thread system.".to_string()
                ))
            },
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
                },
                ActorSystemImpl::Thread(sys) => {
                    use parrot_api::system::ActorSystem as _;
                    sys.get_actor(&path_copy).await
                },
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
                ActorSystemImpl::Thread(sys) => {
                    use parrot_api::system::ActorSystem as _;
                    sys.broadcast(msg_copy).await
                },
                // Add branches for other system types
            }
        }
        .await
    }
    
    /// Forward shutdown method
    pub async fn shutdown(self) -> Result<(), SystemError> {
        match self {
            ActorSystemImpl::Actix(sys) => sys.shutdown().await,
            ActorSystemImpl::Thread(sys) => sys
                .shutdown_internal()
                .await
                .map_err(|e| SystemError::Other(anyhow::anyhow!(e.to_string()))),
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

    /// Register a thread engine system
    pub async fn register_thread_system(
        &self,
        name: String,
        system: Arc<ThreadActorSystem>,
        set_as_default: bool,
    ) -> Result<(), SystemError> {
        let mut systems = self.systems.write().map_err(|_| {
            SystemError::Other(anyhow::anyhow!("Failed to acquire write lock"))
        })?;

        // Store the system
        systems.insert(name.clone(), ActorSystemImpl::Thread(system));

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

    /// Spawn an actor specific to the thread engine system
    ///
    /// This method is a thread-engine-specific version of spawn_root_typed,
    /// designed for actors whose `Context` is `ThreadContext<A>`.
    pub async fn spawn_root_thread<A>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError>
    where
        A: Actor<Context = crate::thread::context::ThreadContext<A>>
            + Send
            + Sync
            + 'static,
    {
        // Get the default system name
        let default_name = self.get_default_system_name()?;

        // Get the system instance
        let system = self.get_system_impl(&default_name)?;

        // Check if the default system is a thread engine system
        match system {
            ActorSystemImpl::Thread(sys) => {
                let typed_ref = sys
                    .spawn_root_typed_thread(actor, config)
                    .await
                    .map_err(|e| SystemError::ActorCreationError(e.to_string()))?;
                Ok(Box::new(typed_ref) as Box<dyn ActorRef>)
            },
            _ => Err(SystemError::ActorCreationError(
                "Default system is not a thread engine system".to_string()
            )),
        }
    }

    /// Spawn an actor specific to the thread engine system in a named system
    pub async fn spawn_root_thread_in_system<A>(
        &self,
        system_name: &str,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError>
    where
        A: Actor<Context = crate::thread::context::ThreadContext<A>>
            + Send
            + Sync
            + 'static,
    {
        let system = self.get_system_impl(system_name)?;

        match system {
            ActorSystemImpl::Thread(sys) => {
                let typed_ref = sys
                    .spawn_root_typed_thread(actor, config)
                    .await
                    .map_err(|e| SystemError::ActorCreationError(e.to_string()))?;
                Ok(Box::new(typed_ref) as Box<dyn ActorRef>)
            },
            _ => Err(SystemError::ActorCreationError(
                format!("System '{}' is not a thread engine system", system_name)
            )),
        }
    }

    /// Get a thread engine system by name
    pub fn get_thread_system(&self, name: &str) -> Result<Arc<ThreadActorSystem>, SystemError> {
        match self.get_system_impl(name)? {
            ActorSystemImpl::Thread(sys) => Ok(sys),
            _ => Err(SystemError::Other(anyhow::anyhow!(
                format!("System '{}' is not a thread engine system", name)
            ))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::context::ThreadContext;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

    /// Simple echo actor for the thread engine.
    #[derive(Debug, Default)]
    struct EchoActor {
        received: std::sync::Mutex<Vec<u64>>,
    }

    impl Actor for EchoActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(
            &'a mut self,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(v) = msg.downcast_ref::<u64>() {
                    self.received.lock().unwrap().push(*v);
                }
                Ok(msg)
            })
        }

        fn receive_message_with_engine<'a>(
            &'a mut self,
            _msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
            _engine_ctx: parrot_api::actor::EngineContextHandle,
        ) -> Option<ActorResult<BoxedMessage>> {
            None
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    #[tokio::test]
    async fn test_parrot_system_with_no_registrations() {
        let system = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();

        assert!(system.list_registered_systems().unwrap().is_empty());

        // No default system: spawn and broadcast must fail cleanly.
        let spawn = system
            .spawn_root_thread(EchoActor::default(), EmptyConfig)
            .await;
        assert!(spawn.is_err());

        system.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_register_thread_system_and_spawn() {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();

        let thread_system = crate::thread::system::ThreadActorSystem::shared(
            crate::thread::config::ThreadActorSystemConfig::default(),
        );

        parrot
            .register_thread_system("thread-main".into(), thread_system, true)
            .await
            .unwrap();

        // Registered and set as default (first registration wins by default).
        let names = parrot.list_registered_systems().unwrap();
        assert_eq!(names, vec!["thread-main".to_string()]);

        // Spawn a thread-engine actor through the Parrot facade.
        let actor_ref = parrot
            .spawn_root_thread(EchoActor::default(), EmptyConfig)
            .await
            .expect("spawn through ParrotActorSystem");
        assert!(actor_ref.path().starts_with("/user/"));

        // The actor lives in the thread system registry.
        let ts = parrot.get_thread_system("thread-main").unwrap();
        assert_eq!(ts.actor_count(), 1);

        // get_thread_system rejects non-thread systems.
        assert!(parrot.get_thread_system("missing").is_err());

        parrot.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_named_system_spawn_and_default_selection() {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();

        let ts = crate::thread::system::ThreadActorSystem::shared(
            crate::thread::config::ThreadActorSystemConfig::default(),
        );
        parrot
            .register_thread_system("engine-a".into(), ts, false)
            .await
            .unwrap();

        // First registration becomes the default even when set_as_default=false.
        let spawned = parrot
            .spawn_root_thread(EchoActor::default(), EmptyConfig)
            .await;
        assert!(spawned.is_ok());

        // Explicit named spawn works too.
        let named = parrot
            .spawn_root_thread_in_system("engine-a", EchoActor::default(), EmptyConfig)
            .await;
        assert!(named.is_ok());

        // Unknown system name fails.
        let missing = parrot
            .spawn_root_thread_in_system("nope", EchoActor::default(), EmptyConfig)
            .await;
        assert!(missing.is_err());

        // set_default_system validates the name.
        assert!(parrot.set_default_system("engine-a").is_ok());
        assert!(parrot.set_default_system("missing").is_err());

        parrot.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_get_actor_through_facade() {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();

        let ts = crate::thread::system::ThreadActorSystem::shared(
            crate::thread::config::ThreadActorSystemConfig::default(),
        );
        parrot
            .register_thread_system("lookup".into(), ts, true)
            .await
            .unwrap();

        let actor_ref = parrot
            .spawn_root_thread(EchoActor::default(), EmptyConfig)
            .await
            .unwrap();
        let path_str = actor_ref.path();

        // Look up by path via the ActorSystem trait method.
        let found = parrot.get_actor(&ActorPath::placeholder(&path_str)).await;
        assert!(found.is_some(), "spawned actor must be discoverable");

        let missing = parrot
            .get_actor(&ActorPath::placeholder("/user/nonexistent"))
            .await;
        assert!(missing.is_none());

        parrot.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_status_reports_running() {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();
        let status = parrot.status();
        assert_eq!(status.state, SystemState::Running);
        parrot.internal_shutdown().await.unwrap();
    }
}
