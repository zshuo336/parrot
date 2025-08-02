//! # Actor System Core
//! 
//! This module defines the core Actor System infrastructure for the Parrot framework.
//! The Actor System is the top-level container that manages actor lifecycle, messaging,
//! and resource allocation.
//!
//! ## Design Philosophy
//!
//! The Actor System is designed with these principles:
//! - Centralized Management: Single point of control for actor lifecycle
//! - Resource Efficiency: Controlled allocation and cleanup of system resources
//! - Fault Tolerance: System-wide error handling and recovery
//! - Scalability: Support for distributed actor deployment
//!
//! ## Core Components
//!
//! - `ActorSystem`: Main trait defining system capabilities
//! - `ActorSystemConfig`: System-wide configuration
//! - `GuardianConfig`: Top-level supervision settings
//! - `SystemTimeouts`: Timeout configurations
//!
//! ## Usage Example
//!
//! ```rust
//! use parrot_api::system::{ActorSystem, ActorSystemConfig, SystemTimeouts};
//! use std::time::Duration;
//!
//! // Configure the system
//! let config = ActorSystemConfig {
//!     name: "my-system".to_string(),
//!     timeouts: SystemTimeouts {
//!         actor_creation: Duration::from_secs(5),
//!         message_handling: Duration::from_secs(30),
//!         system_shutdown: Duration::from_secs(60),
//!     },
//!     ..Default::default()
//! };
//!
//! // Start the system
//! let system = MyActorSystem::start(config).await?;
//!
//! // Create actors
//! let actor = system.spawn_root_typed(MyActor::new(), actor_config).await?;
//! ```

use std::time::Duration;
use std::any::Any;
use async_trait::async_trait;
use crate::actor::Actor;
use crate::errors::ActorError;
use crate::address::{ActorPath, ActorRef};
use crate::runtime::RuntimeConfig;
use crate::message::Message;
use crate::supervisor::SupervisorStrategyType;
use crate::context::ActorContext;

/// Errors that can occur during Actor System operations.
///
/// This enum covers various failure scenarios in the system,
/// from initialization to actor management.
#[derive(thiserror::Error, Debug)]
pub enum SystemError {
    /// Failed to initialize the actor system
    #[error("System initialization failed: {0}")]
    InitializationError(String),
    
    /// Failed to create a new actor
    #[error("Actor creation failed: {0}")]
    ActorCreationError(String),
    
    /// System is in shutdown process
    #[error("System is shutting down")]
    ShuttingDown,
    
    /// Error from actor execution
    #[error(transparent)]
    ActorError(#[from] ActorError),
    
    /// Other unexpected errors
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Configuration for the Actor System.
///
/// This structure defines all system-wide settings including:
/// - System identification
/// - Runtime parameters
/// - Supervision policies
/// - Timing constraints
#[derive(Debug, Clone, Default)]
pub struct ActorSystemConfig {
    /// Unique name for this actor system instance
    pub name: String,
    
    /// Configuration for the underlying runtime
    pub runtime_config: RuntimeConfig,
    
    /// Configuration for the system's root guardian
    pub guardian_config: GuardianConfig,
    
    /// System-wide timeout settings
    pub timeouts: SystemTimeouts,
}

/// Configuration for the system's root guardian actor.
///
/// The guardian is a special system actor that supervises
/// all top-level user actors.
#[derive(Debug, Clone, Default)]
pub struct GuardianConfig {
    /// Maximum number of restarts allowed for supervised actors
    pub max_restarts: u32,
    
    /// Time window for counting restarts
    pub restart_window: Duration,
    
    /// Strategy for handling supervised actor failures
    pub supervision_strategy: SupervisorStrategyType,
}

/// System-wide timeout configurations.
///
/// These timeouts provide safety boundaries for various
/// system operations to prevent resource leaks.
#[derive(Debug, Clone, Default)]
pub struct SystemTimeouts {
    /// Maximum time allowed for actor creation
    pub actor_creation: Duration,
    
    /// Maximum time allowed for message processing
    pub message_handling: Duration,
    
    /// Maximum time allowed for system shutdown
    pub system_shutdown: Duration,
}

/// Core trait defining Actor System capabilities.
///
/// This trait provides the primary interface for:
/// - System lifecycle management
/// - Actor creation and supervision
/// - Message routing and delivery
/// - Resource monitoring and control
#[async_trait]
pub trait ActorSystem: Send + Sync + 'static {
    /// Initializes and starts the actor system.
    ///
    /// This method:
    /// 1. Initializes system resources
    /// 2. Starts the root guardian
    /// 3. Prepares the system for actor creation
    ///
    /// # Parameters
    /// * `config` - System configuration
    ///
    /// # Returns
    /// * `Ok(Self)` - Successfully initialized system
    /// * `Err(SystemError)` - Initialization failed
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError> where Self: Sized;
    
    /// Creates a new top-level actor with type information.
    ///
    /// This method is used for creating actors when the concrete
    /// type is known at compile time.
    ///
    /// # Type Parameters
    /// * `A` - Actor type implementing the Actor trait
    ///
    /// # Parameters
    /// * `actor` - Actor instance
    /// * `config` - Actor configuration
    ///
    /// # Returns
    /// Reference to the created actor or error
    async fn spawn_root_typed<A: Actor>(&self, actor: A, config: A::Config) -> Result<Box<dyn ActorRef>, SystemError>;
    
    /// Creates a new top-level actor from type-erased components.
    ///
    /// This method is used when actor types are determined at runtime
    /// or when implementing dynamic actor creation.
    ///
    /// # Parameters
    /// * `actor` - Boxed actor instance
    /// * `config` - Type-erased configuration
    ///
    /// # Returns
    /// Reference to the created actor or error
    async fn spawn_root_boxed(
        &self,
        actor: Box<dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn Any + Send>,
    ) -> Result<Box<dyn ActorRef>, SystemError>;
    
    /// Locates an actor by its path.
    ///
    /// # Parameters
    /// * `path` - Actor path to search for
    ///
    /// # Returns
    /// * `Some(ActorRef)` - Actor was found
    /// * `None` - Actor does not exist
    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>>;
    
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
    async fn broadcast<M: Message + Clone>(&self, msg: M) -> Result<(), SystemError>;
    
    /// Retrieves current system status.
    ///
    /// Returns information about:
    /// - System state
    /// - Active actors
    /// - Resource usage
    /// - Uptime
    fn status(&self) -> SystemStatus;
    
    /// Initiates graceful system shutdown.
    ///
    /// This process:
    /// 1. Stops accepting new actors
    /// 2. Delivers pending messages
    /// 3. Stops all actors
    /// 4. Releases system resources
    ///
    /// # Returns
    /// Success or failure of the shutdown operation
    async fn shutdown(self) -> Result<(), SystemError>;
}

/// Current status of the actor system.
///
/// This structure provides a snapshot of system health
/// and resource utilization.
#[derive(Debug, Clone)]
pub struct SystemStatus {
    /// Current operational state
    pub state: SystemState,
    
    /// Number of actors currently running
    pub active_actors: usize,
    
    /// Time since system start
    pub uptime: Duration,
    
    /// Current resource utilization
    pub resources: SystemResources,
}

/// Possible states of the actor system.
///
/// Represents the lifecycle stages of the system from
/// startup to shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemState {
    /// System is initializing
    Starting,
    /// System is fully operational
    Running,
    /// System is performing shutdown
    ShuttingDown,
    /// System has terminated
    Stopped,
}

/// Resource usage statistics for the actor system.
///
/// Provides metrics for monitoring system health
/// and performance.
#[derive(Debug, Clone)]
pub struct SystemResources {
    /// Percentage of CPU utilization
    pub cpu_usage: f64,
    
    /// Bytes of memory in use
    pub memory_usage: usize,
    
    /// Number of active threads
    pub thread_count: usize,
} 