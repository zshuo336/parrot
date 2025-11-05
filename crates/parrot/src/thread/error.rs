use std::time::Duration;
use thiserror::Error;

/// Errors related to Mailbox operations.
#[derive(Error, Debug, Clone)]
pub enum MailboxError {
    #[error("Mailbox is full (capacity: {capacity})")]
    Full { capacity: usize },
    #[error("Newest message dropped because mailbox is full (capacity: {capacity})")]
    Dropped { capacity: usize },
    #[error("Mailbox is closed")]
    Closed,
    #[error("Failed to push message: {0}")]
    PushError(String),
    #[error("Failed to pop message: {0}")]
    PopError(String),
    #[error("Internal channel error: {0}")]
    ChannelError(String), // For underlying channel errors (e.g., flume send/recv)
}

/// Errors related to ActorRef operations (excluding ask).
#[derive(Error, Debug, Clone)]
pub enum ActorRefError {
    #[error("Mailbox error during send: {0}")]
    MailboxError(String),
    #[error("Actor not found")]
    NotFound, // TODO: Should this be part of send/ask errors or a separate find error?
}

/// Errors related to the Ask pattern.
#[derive(Error, Debug)]
pub enum AskError {
    #[error("Failed to send ask message to mailbox: {0}")]
    SendError(String),
    #[error("Failed to receive reply: {0}")]
    ReceiveError(String),
    #[error("Ask operation timed out after {0:?}")]
    Timeout(Duration),
    #[error("Actor processing failed: {0}")]
    ActorError(#[from] Box<dyn std::error::Error + Send + Sync>), // Error returned by the actor itself
    #[error("System is shutting down")]
    SystemShutdown,
    // TODO: Add ActorNotFound if ref becomes invalid?
}

/// Errors related to spawning actors.
#[derive(Error, Debug, Clone)]
pub enum SpawnError {
    #[error("Factory output disposal failed: {0}")]
    DisposalFailed(String),
    #[error("Actor creation was cancelled")]
    Cancelled,
    #[error("Actor creation timed out: {0}")]
    Timeout(String),
    #[error("Actor path already exists: {0}")]
    ActorPathAlreadyExists(String), // Assuming ActorPath implements Display
    #[error("Failed to initialize actor: {0}")]
    InitializationFailed(String),
    #[error("Scheduler error: {0}")]
    SchedulerError(String),
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("System is shutting down")]
    SystemShutdown,
}

impl From<SpawnError> for parrot_api::errors::ActorError {
    fn from(error: SpawnError) -> Self {
        match error {
            SpawnError::Cancelled | SpawnError::SystemShutdown => Self::Stopped,
            SpawnError::Timeout(message) => Self::TimeoutDetail(message),
            SpawnError::InitializationFailed(message) => Self::InitializationError(message),
            SpawnError::DisposalFailed(message) => Self::Panic(message),
            error => Self::InitializationError(error.to_string()),
        }
    }
}

/// Errors related to the Actor System itself.
#[derive(Error, Debug)]
pub enum SystemError {
    #[error("Thread setup error: {0}")]
    ThreadSetupError(String),
    #[error("Actor system is not running")]
    NotRunning,
    #[error("Actor system is already shutting down")]
    ShuttingDown,
    #[error("Failed during shutdown: {0}")]
    ShutdownError(String),
    #[error("Actor panicked: {0}")]
    Panic(String),
    #[error("Actor creation failed: {0}")]
    ActorCreationError(String),
    #[error("Actor not found: {0}")]
    ActorNotFound(String),
    #[error("Registration error: {0}")]
    RegistrationError(String),
    #[error("Configuration error: {0}")]
    ConfigError(String),
    #[error("Operation timed out: {0}")]
    Timeout(String),
    #[error("Worker state error: {0}")]
    WorkerStateError(String),
    #[error("Internal system error: {0}")]
    Other(#[from] anyhow::Error),
}

impl SystemError {
    /// Keep a known policy destructor panic distinct in lifecycle receipts.
    pub(crate) fn policy_disposal(error: parrot_api::errors::ActorError) -> Self {
        match error {
            parrot_api::errors::ActorError::Panic(message) => Self::Panic(message),
            error => Self::Other(anyhow::Error::new(error)),
        }
    }
}

impl From<SystemError> for parrot_api::errors::ActorError {
    fn from(error: SystemError) -> Self {
        match error {
            SystemError::Timeout(message) => Self::TimeoutDetail(message),
            SystemError::Panic(message) => Self::Panic(message),
            SystemError::ActorNotFound(path) => Self::ActorNotFound(path),
            error => Self::Other(anyhow::Error::new(error)),
        }
    }
}

/// Errors related to Supervision.
#[derive(Error, Debug, Clone)]
pub enum SupervisorError {
    #[error("Restart limit exceeded for actor: {0}")]
    RestartLimitExceeded(String), // Path
    #[error("Failed to restart actor {0}: {1}")]
    RestartFailed(String, String), // Path, Reason
    #[error("Escalation failed for actor: {0}")]
    EscalationFailed(String), // Path
}
