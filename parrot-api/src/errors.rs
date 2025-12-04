//! # Actor System Error Types
//! 
//! This module defines the error types used throughout the Parrot actor system.
//! It provides a comprehensive error handling infrastructure that enables proper
//! error propagation and handling across the actor hierarchy.
//!
//! ## Design Philosophy
//!
//! The error system follows these principles:
//! - Type Safety: Strongly typed errors for compile-time error handling
//! - Context Preservation: Errors maintain relevant context information
//! - Error Recovery: Support for supervision and error recovery strategies
//! - Error Classification: Clear categorization of different error types
//!
//! ## Core Components
//!
//! - `ActorError`: Main error enum covering all actor system errors
//! - Error variants for specific failure scenarios:
//!   - Initialization failures
//!   - Message handling errors
//!   - Actor lifecycle errors
//!   - Timeouts
//!
//! ## Usage Example
//!
//! ```rust
//! use parrot_api::errors::ActorError;
//!
//! fn handle_actor_error(error: ActorError) {
//!     match error {
//!         ActorError::InitializationError(msg) => {
//!             // Handle initialization failure
//!             println!("Actor failed to initialize: {}", msg);
//!         }
//!         ActorError::Timeout => {
//!             // Handle timeout
//!             println!("Operation timed out");
//!         }
//!         _ => {
//!             // Handle other errors
//!             println!("Unexpected error: {}", error);
//!         }
//!     }
//! }
//! ```

use thiserror::Error;

/// Core error type for the actor system.
///
/// This enum represents all possible error conditions that can occur
/// during actor system operation. It is used throughout the system
/// for error propagation and handling.
#[derive(Error, Debug)]
pub enum ActorError {
    /// Error during actor initialization.
    ///
    /// This error occurs when an actor fails to properly initialize,
    /// such as failing to establish required resources or invalid
    /// configuration.
    ///
    /// # Parameters
    /// * String - Detailed error message explaining the initialization failure
    #[error("Actor initialization failed: {0}")]
    InitializationError(String),
    
    /// Error during message processing.
    ///
    /// This error occurs when an actor fails to process a message,
    /// such as invalid message format or processing logic failure.
    ///
    /// # Parameters
    /// * String - Detailed error message explaining the handling failure
    #[error("Message handling failed: {0}")]
    MessageHandlingError(String),
    
    /// Actor has been stopped.
    ///
    /// This error indicates that an operation was attempted on a
    /// stopped actor. This is a normal part of actor lifecycle
    /// management.
    #[error("Actor stopped")]
    Stopped,
    
    /// Operation timeout.
    ///
    /// This error occurs when an operation fails to complete within
    /// its specified timeout period. This can happen during message
    /// processing, actor creation, or system operations.
    #[error("Timeout")]
    Timeout,
    
    /// Process message error.
    ///
    /// This error occurs when a message is processed with an error.
    #[error("Process message error: {0}")]
    ProcessMessageError(String),

    /// Reply channel error.
    ///
    /// This error occurs when a reply channel fails to send a message.
    #[error("Reply channel error: {0}")]
    ReplyChannelError(String),

    /// Catch-all for other errors.
    ///
    /// This variant wraps any other error types that may occur
    /// during actor system operation. It preserves the original
    /// error context through error source chaining.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

