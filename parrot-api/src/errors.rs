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

    /// Operation timeout with a detailed message.
    #[error("Timeout: {0}")]
    TimeoutDetail(String),

    /// Actor not found at the given path.
    #[error("Actor not found: {0}")]
    ActorNotFound(String),

    /// Internal system error.
    #[error("Internal error: {0}")]
    InternalError(String),

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

    /// Panic error.
    ///
    /// This error occurs when a panic occurs in an actor.
    #[error("Panic: {0}")]
    Panic(String),

    /// Catch-all for other errors.
    ///
    /// This variant wraps any other error types that may occur
    /// during actor system operation. It preserves the original
    /// error context through error source chaining.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全部 11 个变体的 Display 精确格式（稳定错误面，防止无意识改动）。
    #[test]
    fn display_all_variants() {
        assert_eq!(
            ActorError::InitializationError("bad cfg".into()).to_string(),
            "Actor initialization failed: bad cfg"
        );
        assert_eq!(
            ActorError::MessageHandlingError("bad msg".into()).to_string(),
            "Message handling failed: bad msg"
        );
        assert_eq!(ActorError::Stopped.to_string(), "Actor stopped");
        assert_eq!(ActorError::Timeout.to_string(), "Timeout");
        assert_eq!(
            ActorError::TimeoutDetail("5s".into()).to_string(),
            "Timeout: 5s"
        );
        assert_eq!(
            ActorError::ActorNotFound("/user/x".into()).to_string(),
            "Actor not found: /user/x"
        );
        assert_eq!(
            ActorError::InternalError("oops".into()).to_string(),
            "Internal error: oops"
        );
        assert_eq!(
            ActorError::ProcessMessageError("p".into()).to_string(),
            "Process message error: p"
        );
        assert_eq!(
            ActorError::ReplyChannelError("closed".into()).to_string(),
            "Reply channel error: closed"
        );
        assert_eq!(
            ActorError::Panic("boom".into()).to_string(),
            "Panic: boom"
        );
        let other = ActorError::Other(anyhow::anyhow!("inner"));
        assert_eq!(other.to_string(), "inner");
    }

    /// Other 变体保留错误源链（source chaining）。
    #[test]
    fn other_variant_preserves_source_chain() {
        use std::error::Error as _;
        // anyhow 包装一个带自身 source 的 std error，链应透传
        #[derive(Debug, thiserror::Error)]
        #[error("middle")]
        struct Middle(#[source] std::io::Error);
        let io_err = std::io::Error::other("root cause");
        let inner = Middle(io_err);
        let err = ActorError::Other(anyhow::Error::new(inner));
        let src = err.source();
        assert!(src.is_some(), "anyhow-wrapped std error must chain through");
        // thiserror transparent 跳过 anyhow 层，链到 Middle 的错误链
        let s = src.unwrap().to_string();
        assert!(
            s == "middle" || s == "root cause",
            "source chain should reach Middle or its root, got {s}"
        );
        // 链最终必须到达 root cause
        let mut cur = err.source();
        let mut found_root = false;
        while let Some(e) = cur {
            if e.to_string() == "root cause" {
                found_root = true;
                break;
            }
            cur = e.source();
        }
        assert!(found_root, "root cause must be reachable via source chain");
    }

    /// From<anyhow::Error> 转换。
    #[test]
    fn from_anyhow_conversion() {
        let e: ActorError = anyhow::anyhow!("wrapped").into();
        assert!(matches!(e, ActorError::Other(_)));
        assert_eq!(e.to_string(), "wrapped");
    }

    /// Debug 输出包含变体名（日志可辨识）。
    #[test]
    fn debug_names_are_stable() {
        assert!(format!("{:?}", ActorError::Stopped).contains("Stopped"));
        assert!(format!("{:?}", ActorError::Timeout).contains("Timeout"));
        assert!(
            format!("{:?}", ActorError::TimeoutDetail("x".into()))
                .contains("TimeoutDetail")
        );
        assert!(
            format!("{:?}", ActorError::Panic("x".into())).contains("Panic")
        );
    }

    /// 所有变体可跨线程发送（ActorError: Send 断言）。
    #[test]
    fn all_variants_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ActorError>();
        let e = ActorError::Panic("p".into());
        std::thread::spawn(move || {
            let _ = e.to_string();
        })
        .join()
        .unwrap();
    }

    /// 常见判别模式（supervisor 决策输入）。
    #[test]
    fn variant_matching_for_supervision() {
        let cases: Vec<(ActorError, &str)> = vec![
            (ActorError::Panic("p".into()), "panic"),
            (ActorError::Stopped, "stopped"),
            (ActorError::Timeout, "timeout"),
            (ActorError::InitializationError("i".into()), "init"),
        ];
        for (e, kind) in cases {
            let detected = match e {
                ActorError::Panic(_) => "panic",
                ActorError::Stopped => "stopped",
                ActorError::Timeout | ActorError::TimeoutDetail(_) => "timeout",
                _ => "init",
            };
            assert_eq!(detected, kind);
        }
    }

    /// Timeout 与 TimeoutDetail 的判别一致性。
    #[test]
    fn timeout_variants_distinguishable() {
        let a = ActorError::Timeout;
        let b = ActorError::TimeoutDetail("detail".into());
        assert!(matches!(a, ActorError::Timeout));
        assert!(!matches!(a, ActorError::TimeoutDetail(_)));
        assert!(matches!(b, ActorError::TimeoutDetail(_)));
    }
}
