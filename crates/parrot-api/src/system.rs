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
//! use parrot_api::system::{ActorSystemConfig, SystemTimeouts};
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
//! // Start the system with a concrete implementation (see engine crates):
//! // let system = MyActorSystem::start(config).await?;
//! # let _ = config;
//! ```

use crate::actor::Actor;
use crate::address::{ActorPath, ActorRef};
use crate::context::ActorContext;
use crate::errors::ActorError;
use crate::message::Message;
use crate::runtime::RuntimeConfig;
use crate::supervisor::SupervisorStrategyType;
use async_trait::async_trait;
use std::any::Any;
use std::time::Duration;

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
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError>
    where
        Self: Sized;

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
    async fn spawn_root_typed<A: Actor>(
        &self,
        actor: A,
        config: A::Config,
    ) -> Result<Box<dyn ActorRef>, SystemError>;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::ActorPath;
    use crate::context::ActorContext;

    use crate::runtime::{LoadBalancingStrategy, RuntimeConfig, SchedulerConfig};
    use crate::supervisor::{DefaultStrategy, SupervisorStrategyType};
    use crate::types::{ActorResult, BoxedFuture, BoxedMessage};

    // ---------------- 配置类型 ----------------

    #[test]
    fn system_config_default_is_empty() {
        let c = ActorSystemConfig::default();
        assert!(c.name.is_empty());
        assert_eq!(c.timeouts.actor_creation, Duration::ZERO);
        assert_eq!(c.guardian_config.max_restarts, 0);
        assert!(matches!(
            c.guardian_config.supervision_strategy,
            SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure)
        ));
    }

    #[test]
    fn system_config_full_construction_and_clone() {
        let c = ActorSystemConfig {
            name: "sys".into(),
            runtime_config: RuntimeConfig {
                worker_threads: Some(8),
                io_threads: None,
                scheduler_config: SchedulerConfig {
                    task_queue_capacity: 64,
                    task_timeout: Duration::from_secs(1),
                    load_balancing: LoadBalancingStrategy::Random,
                },
            },
            guardian_config: GuardianConfig {
                max_restarts: 5,
                restart_window: Duration::from_secs(60),
                supervision_strategy: SupervisorStrategyType::Default(
                    DefaultStrategy::RestartOnFailure,
                ),
            },
            timeouts: SystemTimeouts {
                actor_creation: Duration::from_secs(2),
                message_handling: Duration::from_secs(3),
                system_shutdown: Duration::from_secs(4),
            },
        };
        let c2 = c.clone();
        assert_eq!(c2.name, "sys");
        assert_eq!(c2.guardian_config.max_restarts, 5);
        assert_eq!(c2.timeouts.system_shutdown, Duration::from_secs(4));
    }

    #[test]
    fn system_timeouts_default_zero() {
        let t = SystemTimeouts::default();
        assert_eq!(t.actor_creation, Duration::ZERO);
        assert_eq!(t.message_handling, Duration::ZERO);
        assert_eq!(t.system_shutdown, Duration::ZERO);
    }

    // ---------------- SystemError ----------------

    #[test]
    fn system_error_display_variants() {
        assert_eq!(
            SystemError::InitializationError("x".into()).to_string(),
            "System initialization failed: x"
        );
        assert_eq!(
            SystemError::ActorCreationError("y".into()).to_string(),
            "Actor creation failed: y"
        );
        assert_eq!(
            SystemError::ShuttingDown.to_string(),
            "System is shutting down"
        );
        assert_eq!(
            SystemError::ActorError(crate::errors::ActorError::Timeout).to_string(),
            "Timeout"
        );
        assert_eq!(SystemError::Other(anyhow::anyhow!("z")).to_string(), "z");
    }

    #[test]
    fn system_error_from_actor_error() {
        let e: SystemError = crate::errors::ActorError::Stopped.into();
        assert!(matches!(e, SystemError::ActorError(_)));
    }

    // ---------------- SystemState / SystemStatus / SystemResources ----------------

    #[test]
    fn system_state_variants_distinct() {
        let states = [
            SystemState::Starting,
            SystemState::Running,
            SystemState::ShuttingDown,
            SystemState::Stopped,
        ];
        for (i, a) in states.iter().enumerate() {
            for b in states.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        // Clone/Copy/Debug/Eq
        let s = SystemState::Running;
        assert_eq!(s, s.clone());
    }

    #[test]
    fn system_status_and_resources_constructible() {
        let st = SystemStatus {
            state: SystemState::Running,
            active_actors: 7,
            uptime: Duration::from_secs(12),
            resources: SystemResources {
                cpu_usage: 33.3,
                memory_usage: 4096,
                thread_count: 9,
            },
        };
        let c = st.clone();
        assert_eq!(c.active_actors, 7);
        assert_eq!(c.resources.thread_count, 9);
        assert_eq!(c.uptime, Duration::from_secs(12));
    }

    // ---------------- ActorSystem trait 对象契约 ----------------

    struct NopSystem {
        status: SystemStatus,
    }

    #[derive(Debug)]
    struct NopActor;

    impl crate::actor::Actor for NopActor {
        type Config = crate::actor::EmptyConfig;
        type Context = dyn ActorContext;
        fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _c: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }
        fn state(&self) -> crate::actor::ActorState {
            crate::actor::ActorState::Running
        }
    }

    #[derive(Debug, Clone)]
    struct PingMsg;
    impl crate::message::Message for PingMsg {
        type Result = u32;
        fn extract_result(r: BoxedMessage) -> ActorResult<u32> {
            r.downcast::<u32>()
                .map(|b| *b)
                .map_err(|_| crate::errors::ActorError::MessageHandlingError("t".into()))
        }
    }

    #[async_trait]
    impl ActorSystem for NopSystem {
        async fn start(_config: ActorSystemConfig) -> Result<Self, SystemError>
        where
            Self: Sized,
        {
            Ok(NopSystem {
                status: SystemStatus {
                    state: SystemState::Starting,
                    active_actors: 0,
                    uptime: Duration::ZERO,
                    resources: SystemResources {
                        cpu_usage: 0.0,
                        memory_usage: 0,
                        thread_count: 0,
                    },
                },
            })
        }
        async fn spawn_root_typed<A: crate::actor::Actor>(
            &self,
            _actor: A,
            _config: A::Config,
        ) -> Result<Box<dyn ActorRef>, SystemError> {
            Err(SystemError::ActorCreationError("nop".into()))
        }
        async fn spawn_root_boxed(
            &self,
            _actor: Box<
                dyn crate::actor::Actor<
                    Config = Box<dyn std::any::Any + Send>,
                    Context = dyn ActorContext,
                >,
            >,
            _config: Box<dyn std::any::Any + Send>,
        ) -> Result<Box<dyn ActorRef>, SystemError> {
            Err(SystemError::ActorCreationError("nop-boxed".into()))
        }
        async fn get_actor(&self, _path: &ActorPath) -> Option<Box<dyn ActorRef>> {
            None
        }
        async fn broadcast<M: crate::message::Message + Clone>(
            &self,
            _msg: M,
        ) -> Result<(), SystemError> {
            Ok(())
        }
        fn status(&self) -> SystemStatus {
            self.status.clone()
        }
        async fn shutdown(self) -> Result<(), SystemError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn actor_system_trait_contract() {
        let s = NopSystem::start(ActorSystemConfig::default())
            .await
            .unwrap();
        // spawn_typed 错误路径
        assert!(s
            .spawn_root_typed(NopActor, crate::actor::EmptyConfig)
            .await
            .is_err());
        // get_actor 找不到
        assert!(s
            .get_actor(&ActorPath::placeholder("x://y"))
            .await
            .is_none());
        // broadcast 成功
        s.broadcast(PingMsg).await.unwrap();
        // status 快照
        assert_eq!(s.status().active_actors, 0);
        // shutdown 成功
        s.shutdown().await.unwrap();
    }
}
