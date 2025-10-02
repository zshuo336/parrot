use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use actix::System as ActixSystem;
use actix::prelude::ArbiterHandle;
use async_trait::async_trait;
use anyhow::anyhow;
use parrot_api::system::{ActorSystemConfig, SystemError, SystemStatus, SystemState, SystemResources};
use parrot_api::actor::Actor as ParrotActor;
use parrot_api::address::ActorRef;
use parrot_api::message::Message;
use parrot_api::types::{BoxedActorRef, ActorResult};
use crate::actix::actor::ActixActor;
use crate::actix::reference::ActixActorRef;
use crate::actix::context::ActixContext;
use std::time::Duration;
use uuid::Uuid;

/// Arbiter pool for spreading actors across multiple actix arbiters.
///
/// # Overview
/// A single actix `System` runs one main arbiter by default; all actors
/// spawned via `Actor::start` share it and execute strictly serially, so
/// CPU-bound handlers block every other actor (head-of-line blocking) and
/// multi-core CPUs stay idle. This pool holds one `ArbiterHandle` per worker
/// thread and hands them out round-robin at spawn time.
///
/// # Implementation Details
/// - Each arbiter is an OS thread running its own single-threaded tokio
///   runtime (`enable_all`, timers available), hosting any number of actors.
/// - Actors on different arbiters run fully in parallel; actors sharing an
///   arbiter serialize, matching actix semantics.
/// - Handles are kept for the lifetime of the pool: threads stay parked and
///   are only torn down at system shutdown.
#[derive(Clone)]
pub struct ArbiterPool {
    /// Worker arbiter handles (never empty after construction).
    workers: Arc<Vec<ArbiterHandle>>,
    /// Round-robin cursor.
    next: Arc<std::sync::atomic::AtomicUsize>,
}

impl ArbiterPool {
    /// Build a pool with `size` worker arbiters. `size == 0` is upgraded to 1.
    ///
    /// # Panics
    /// Panics if called outside an actix `System` context (arbiter threads
    /// must register with the current system).
    pub fn new(size: usize) -> Self {
        let size = size.max(1);
        let workers = (0..size)
            .map(|_| actix::prelude::Arbiter::new().handle())
            .collect::<Vec<_>>();
        Self {
            workers: Arc::new(workers),
            next: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Number of worker arbiters in the pool.
    pub fn size(&self) -> usize {
        self.workers.len()
    }

    /// Pick the next arbiter (round-robin).
    fn next_arbiter(&self) -> ArbiterHandle {
        let idx = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.workers.len();
        self.workers[idx].clone()
    }
}

impl std::fmt::Debug for ArbiterPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArbiterPool")
            .field("workers", &self.workers.len())
            .finish()
    }
}

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
    actors: Arc<RwLock<HashMap<String, BoxedActorRef>>>,
    /// System configuration
    config: ActorSystemConfig,
    /// Registry of active actors by path
    registry: Arc<RwLock<HashMap<String, BoxedActorRef>>>,
    /// Default dispatcher
    default_dispatcher: Arc<RwLock<Option<String>>>,
    /// Arbiter pool for parallel actor execution (multi-arbiter support).
    /// Lazily built on first use; size configured via `arbiter_count`.
    arbiters: Arc<RwLock<Option<ArbiterPool>>>,
    /// Number of worker arbiters to create (default: number of CPUs).
    arbiter_count: usize,
}

// Manually implement Clone, sharing certain locked data
impl Clone for ActixActorSystem {
    fn clone(&self) -> Self {
        Self {
            system: self.system.clone(),
            actors: self.actors.clone(),        // Share actors collection instead of creating a new empty map
            config: self.config.clone(),
            registry: self.registry.clone(),    // Share registry data
            default_dispatcher: self.default_dispatcher.clone(), // Share dispatcher settings
            arbiters: self.arbiters.clone(),    // Share the arbiter pool
            arbiter_count: self.arbiter_count,
        }
    }
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
        Self::with_arbiter_count(num_cpus::get()).await
    }

    /// Create a new ActixActorSystem with an explicit arbiter count.
    ///
    /// # Parameters
    /// - `arbiter_count`: number of worker arbiters; actors are spread
    ///   round-robin across them at spawn time. Values < 1 are upgraded to 1.
    pub async fn with_arbiter_count(arbiter_count: usize) -> Result<Self, SystemError> {
        Ok(Self {
            // Use Tokio runtime's spawn method instead of creating a new system
            system: Arc::new(ActixSystem::current()),
            actors: Arc::new(RwLock::new(HashMap::new())),
            config: ActorSystemConfig::default(),
            registry: Arc::new(RwLock::new(HashMap::new())),
            default_dispatcher: Arc::new(RwLock::new(None)),
            arbiters: Arc::new(RwLock::new(None)),
            arbiter_count: arbiter_count.max(1),
        })
    }

    /// Number of worker arbiters (threads) backing this system.
    pub fn arbiter_size(&self) -> usize {
        self.arbiter_count
    }

    /// Get (lazily constructing) the shared arbiter pool.
    fn arbiter_pool(&self) -> Result<ArbiterPool, SystemError> {
        // Fast path: read without constructing.
        if let Some(pool) = self.arbiters.read().ok().and_then(|g| g.clone()) {
            return Ok(pool);
        }
        let mut guard = self
            .arbiters
            .write()
            .map_err(|_| SystemError::Other(anyhow!("Failed to acquire write lock")))?;
        if let Some(pool) = guard.as_ref() {
            return Ok(pool.clone());
        }
        let pool = ArbiterPool::new(self.arbiter_count);
        *guard = Some(pool.clone());
        Ok(pool)
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
    pub async fn spawn_root_typed<A>(&self, actor: A, _config: A::Config) -> Result<Box<dyn ActorRef>, SystemError> 
    where
        A: ParrotActor<Context = ActixContext<ActixActor<A>>> + Unpin + 'static 
    {
        // Get type name as actor name
        let type_name = std::any::type_name::<A>();
        let actor_name = match type_name.rsplit("::").next() {
            Some(name) => name,
            None => "unknown"
        };
        
        // Unique path per actor instance: spawning the same actor type twice
        // used to overwrite the registry entry (both stress suites noted this
        // limitation). A short uuid suffix keeps paths collision-free while
        // staying readable.
        let path = format!("actix://{}/{}", actor_name, Uuid::new_v4().simple());
        
        // Create ActixActor wrapper
        let actor_base = ActixActor::new(actor);
        
        // Start the actor on a pooled worker arbiter (round-robin). This is
        // the multi-arbiter support: actors land on different OS threads and
        // execute in parallel; a blocking handler on one arbiter no longer
        // freezes actors on the others. Falls back to the main arbiter only
        // if the pool is unavailable.
        let addr = match self.arbiter_pool() {
            Ok(pool) => actix::Actor::start_in_arbiter(&pool.next_arbiter(), |_ctx| actor_base),
            Err(_) => actix::Actor::start(actor_base),
        };
        
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
        println!("ActixActorSystem: Starting shutdown sequence");
        
        // Stop all actors
        {
            let actors = match self.actors.read() {
                Ok(actors) => actors,
                Err(_) => return Err(SystemError::Other(anyhow!("Failed to acquire read lock"))),
            };
            
            println!("ActixActorSystem: Stopping {} actors", actors.len());
            
            for actor_ref in actors.values() {
                // Spawn a task to stop the actor
                let actor_ref_clone = actor_ref.clone_boxed();
                tokio::spawn(async move {
                    let _ = actor_ref_clone.stop().await;
                });
            }
        }
        
        // Wait for actors to stop (could add a timeout here)
        println!("ActixActorSystem: Waiting for actors to stop");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        
        // Stop the Actix system
        println!("ActixActorSystem: Stopping the Actix system");
        actix::System::current().stop();
        println!("ActixActorSystem: System shutdown initiated");
        tracing::info!("ActixActorSystem: System shutdown initiated");
        
        Ok(())
    }
    
    /// Get status of the actor system
    /// 
    /// # Returns
    /// Current system status
    pub fn status(&self) -> SystemStatus {
        SystemStatus {
            state: SystemState::Running,
            active_actors: self.actors.read().map(|actors| actors.len()).unwrap_or(0),
            // TODO: calculate actual uptime based on system start timestamp
            uptime: Duration::from_secs(0),
            resources: SystemResources {
                // Placeholder resource metrics; integrate real data as needed
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 1,
            },
        }
    }
} 