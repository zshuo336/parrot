//! # Parrot Actor Framework API
//! 
//! Parrot is a language-agnostic Actor framework API specification that provides a unified interface
//! for building distributed, concurrent, and fault-tolerant applications using the Actor model.
//! 
//! ## Design Principles
//! 
//! - **Language Agnostic**: The API is designed to be implemented in any programming language while
//!   maintaining consistent behavior and semantics.
//! - **Type Safety**: Strong typing ensures compile-time correctness of actor interactions.
//! - **Fault Tolerance**: Built-in supervision strategies and error handling mechanisms.
//! - **Scalability**: Support for distributed systems and concurrent processing.
//! - **Flexibility**: Extensible design allowing custom implementations while maintaining core guarantees.
//! 
//! ## Core Components
//! 
//! The framework consists of several core components:
//! 
//! - **Actor System**: The top-level container that manages actor lifecycle and resources
//! - **Actors**: Basic units of computation that process messages
//! - **Messages**: Typed communication protocol between actors
//! - **Supervision**: Hierarchical error handling and recovery strategies
//! - **Streams**: Support for reactive stream processing
//! 
//! ## Usage Example
//! 
//! ```rust
//! use parrot_api::{Actor, ActorSystem, Message};
//! 
//! // Define a message
//! #[derive(Message)]
//! struct Ping;
//! 
//! // Define an actor
//! struct PingActor;
//! 
//! impl Actor for PingActor {
//!     // Actor implementation
//! }
//! 
//! // Create and use the actor system
//! async fn example() {
//!     let system = ActorSystem::new();
//!     let actor = system.spawn_actor(PingActor::new());
//!     actor.send(Ping).await;
//! }
//! ```
//! 
//! ## Module Organization
//! 
//! - [`actor`]: Core actor traits and implementations
//! - [`address`]: Actor addressing and location transparency
//! - [`context`]: Actor execution context and lifecycle management
//! - [`message`]: Message definitions and handling
//! - [`supervisor`]: Supervision strategies and fault tolerance
//! - [`system`]: Actor system management and configuration
//! - [`runtime`]: Runtime configuration and execution environment
//! - [`errors`]: Error types and handling
//! - [`stream`]: Stream processing capabilities
//! - [`types`]: Common type definitions

pub mod actor;
pub mod address;
pub mod context;
pub mod message;
pub mod supervisor;
pub mod system;
pub mod runtime;
pub mod errors;
pub mod stream;
pub mod types;

pub use actor::{Actor, ActorConfig, ActorFactory, ActorState};
pub use address::{ActorPath, ActorRef};
pub use context::ActorContext;
pub use message::{Message, MessageEnvelope, MessageOptions, MessagePriority};
pub use supervisor::{SupervisionDecision, SupervisorStrategy};
pub use system::{ActorSystem, ActorSystemConfig, SystemError}; 
pub use stream::{StreamHandler, StreamRegistry, StreamRegistryExt, ActorStreamHandler};

// Re-export the derive macro
pub use parrot_api_derive::Message; 