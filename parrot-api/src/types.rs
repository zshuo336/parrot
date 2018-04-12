//! # Common Type Definitions
//! 
//! This module provides common type definitions and aliases used throughout the Parrot actor system.
//! These types form the foundation for actor references, message passing, and asynchronous operations.
//!
//! ## Design Philosophy
//!
//! The type system is designed with these principles:
//! - Type Safety: Strong typing for compile-time correctness
//! - Ergonomics: Convenient aliases for common complex types
//! - Flexibility: Generic type parameters for extensibility
//! - Performance: Zero-cost abstractions through type aliases
//!
//! ## Core Types
//!
//! - Actor References: `BoxedActorRef`, `WeakActorTarget`
//! - Message Types: `BoxedMessage`
//! - Result Types: `ActorResult`
//! - Future Types: `BoxedFuture`
//!
//! ## Usage Example
//!
//! ```rust
//! use parrot_api::types::{BoxedActorRef, ActorResult, BoxedMessage};
//!
//! async fn handle_message(
//!     actor: BoxedActorRef,
//!     message: BoxedMessage
//! ) -> ActorResult<BoxedMessage> {
//!     // Process message and return result
//!     Ok(message)
//! }
//! ```

use crate::address::ActorRef;
use crate::errors::ActorError;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Type-erased reference to an actor.
///
/// This type represents a boxed trait object of an actor reference,
/// allowing for dynamic dispatch and type erasure while maintaining
/// the actor's interface. Used for storing and passing actor references
/// throughout the system.
pub type BoxedActorRef = Box<dyn ActorRef>;

/// Thread-safe shared reference to an actor.
///
/// This type provides a reference-counted, thread-safe handle to an actor.
/// Used for sharing actor references across multiple owners while ensuring
/// proper cleanup when all references are dropped.
pub type WeakActorTarget = Arc<dyn ActorRef>;

/// Type-erased message container.
///
/// This type represents a boxed trait object that can hold any message type
/// implementing Send. Used for type erasure in the message passing system
/// while maintaining thread safety.
pub type BoxedMessage = Box<dyn Any + Send>;

/// Result type for actor operations.
///
/// This type combines a generic success type with the actor system's error type.q
/// Used throughout the system for operations that can fail, providing
/// consistent error handling.
///
/// # Type Parameters
/// * `T` - The success type of the result
pub type ActorResult<T> = Result<T, ActorError>;

/// Pinned, boxed future type for async operations.
///
/// This type represents a pinned, boxed future that can be sent across threads.
/// Used for representing asynchronous operations in the actor system.
///
/// # Type Parameters
/// * `'a` - Lifetime of the future
/// * `T` - The type of value the future resolves to
pub type BoxedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
