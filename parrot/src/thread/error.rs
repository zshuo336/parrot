use std::time::Duration;
use thiserror::Error;

/// Errors related to Mailbox operations.
#[derive(Error, Debug, Clone)]
pub enum MailboxError {
    #[error("Mailbox is full (capacity: {capacity})")]
    Full { capacity: usize },
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