// Integration tests for error types in parrot::thread::error

use parrot::thread::error::*; // Corrected import path
use std::time::Duration;
use anyhow::anyhow;
use thiserror::Error; // Import thiserror for the helper error

// Helper to create a dummy actor error
#[derive(Error, Debug)]
#[error("Test actor processing error")]
struct SampleActorError;

#[test]
fn test_mailbox_error_display() {
    assert_eq!(MailboxError::Full { capacity: 100 }.to_string(), "Mailbox is full (capacity: 100)");
    assert_eq!(MailboxError::Closed.to_string(), "Mailbox is closed");
    assert_eq!(MailboxError::PushError("reason".to_string()).to_string(), "Failed to push message: reason");
    assert_eq!(MailboxError::PopError("reason".to_string()).to_string(), "Failed to pop message: reason");
    assert_eq!(MailboxError::ChannelError("flume error".to_string()).to_string(), "Internal channel error: flume error");
}

#[test]
fn test_actor_ref_error_display() {
    assert_eq!(ActorRefError::MailboxError("mailbox closed".to_string()).to_string(), "Mailbox error during send: mailbox closed");
    assert_eq!(ActorRefError::NotFound.to_string(), "Actor not found");
}

#[test]
fn test_ask_error_display() {
    assert_eq!(AskError::SendError("mailbox full".to_string()).to_string(), "Failed to send ask message to mailbox: mailbox full");
    assert_eq!(AskError::ReceiveError("channel closed".to_string()).to_string(), "Failed to receive reply: channel closed");
    assert_eq!(AskError::Timeout(Duration::from_secs(5)).to_string(), "Ask operation timed out after 5s");
    let actor_err: Box<dyn std::error::Error + Send + Sync> = Box::new(SampleActorError);
    // Note: Display for ActorError source might not be perfect without downcasting, but check basic format
    assert!(AskError::ActorError(actor_err).to_string().starts_with("Actor processing failed:"));
    assert_eq!(AskError::SystemShutdown.to_string(), "System is shutting down");
}

#[test]
fn test_spawn_error_display() {
    assert_eq!(SpawnError::ActorPathAlreadyExists("actor/path".to_string()).to_string(), "Actor path already exists: actor/path");
    assert_eq!(SpawnError::InitializationFailed("start failed".to_string()).to_string(), "Failed to initialize actor: start failed");
    assert_eq!(SpawnError::SchedulerError("pool full".to_string()).to_string(), "Scheduler error: pool full");
    assert_eq!(SpawnError::InvalidConfig("bad timeout".to_string()).to_string(), "Invalid configuration: bad timeout");
}

#[test]
fn test_system_error_display() {
    assert_eq!(SystemError::NotRunning.to_string(), "Actor system is not running");
    assert_eq!(SystemError::ShuttingDown.to_string(), "Actor system is already shutting down");
    assert_eq!(SystemError::ShutdownError("join failed".to_string()).to_string(), "Failed during shutdown: join failed");
    assert_eq!(SystemError::ActorCreationError("init panic".to_string()).to_string(), "Actor creation failed: init panic");
    assert_eq!(SystemError::ActorNotFound("actor/path".to_string()).to_string(), "Actor not found: actor/path");
    assert_eq!(SystemError::RegistrationError("duplicate name".to_string()).to_string(), "Registration error: duplicate name");
    assert_eq!(SystemError::ConfigError("missing pool size".to_string()).to_string(), "Configuration error: missing pool size");
    let other_err = SystemError::Other(anyhow!("some internal issue"));
    // Check that the source error is included in the Display output
    assert!(other_err.to_string().contains("some internal issue"));
}

#[test]
fn test_supervisor_error_display() {
    assert_eq!(SupervisorError::RestartLimitExceeded("actor/path".to_string()).to_string(), "Restart limit exceeded for actor: actor/path");
    assert_eq!(SupervisorError::RestartFailed("actor/path".to_string(), "panic msg".to_string()).to_string(), "Failed to restart actor actor/path: panic msg");
    assert_eq!(SupervisorError::EscalationFailed("actor/path".to_string()).to_string(), "Escalation failed for actor: actor/path");
} 