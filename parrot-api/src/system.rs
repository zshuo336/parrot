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

/// System error type
#[derive(thiserror::Error, Debug)]
pub enum SystemError {
    #[error("System initialization failed: {0}")]
    InitializationError(String),
    
    #[error("Actor creation failed: {0}")]
    ActorCreationError(String),
    
    #[error("System is shutting down")]
    ShuttingDown,
    
    #[error(transparent)]
    ActorError(#[from] ActorError),
    
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Actor system configuration
#[derive(Debug, Clone)]
pub struct ActorSystemConfig {
    /// System name
    pub name: String,
    /// Runtime configuration
    pub runtime_config: RuntimeConfig,
    /// System guardian configuration
    pub guardian_config: GuardianConfig,
    /// System default timeout settings
    pub timeouts: SystemTimeouts,
}

/// System guardian configuration
#[derive(Debug, Clone)]
pub struct GuardianConfig {
    /// Maximum restart count
    pub max_restarts: u32,
    /// Restart time window
    pub restart_window: Duration,
    /// Supervision strategy
    pub supervision_strategy: SupervisorStrategyType,
}

/// System timeout settings
#[derive(Debug, Clone)]
pub struct SystemTimeouts {
    /// Actor creation timeout
    pub actor_creation: Duration,
    /// Message handling timeout
    pub message_handling: Duration,
    /// System shutdown timeout
    pub system_shutdown: Duration,
}

/// Actor system trait
#[async_trait]
pub trait ActorSystem: Send + Sync + 'static {
    /// Start the system
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError> where Self: Sized;
    
    /// Create a top-level actor with specific type
    async fn spawn_root_typed<A: Actor>(&self, actor: A, config: A::Config) -> Result<Box<dyn ActorRef>, SystemError>;
    
    /// Create a top-level actor from boxed components
    async fn spawn_root_boxed(
        &self,
        actor: Box<dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn Any + Send>,
    ) -> Result<Box<dyn ActorRef>, SystemError>;
    
    /// Find actor by path
    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>>;
    
    /// Broadcast message to all actors
    async fn broadcast<M: Message>(&self, msg: M) -> Result<(), SystemError>;
    
    /// Get system status
    fn status(&self) -> SystemStatus;
    
    /// Gracefully shutdown the system
    async fn shutdown(self) -> Result<(), SystemError>;
}

/// System status
#[derive(Debug, Clone)]
pub struct SystemStatus {
    /// System running state
    pub state: SystemState,
    /// Number of active actors
    pub active_actors: usize,
    /// System uptime
    pub uptime: Duration,
    /// System resource usage
    pub resources: SystemResources,
}

/// System state enum
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemState {
    Starting,
    Running,
    ShuttingDown,
    Stopped,
}

/// System resource usage
#[derive(Debug, Clone)]
pub struct SystemResources {
    /// CPU usage
    pub cpu_usage: f64,
    /// Memory usage
    pub memory_usage: usize,
    /// Thread count
    pub thread_count: usize,
} 