//! # Actor Supervision System
//! 
//! This module provides the supervision infrastructure for the Parrot actor system,
//! implementing fault tolerance through hierarchical error handling and recovery strategies.
//!
//! ## Design Philosophy
//!
//! The supervision system is based on these principles:
//! - Hierarchical: Actors form a tree where parents supervise children
//! - Declarative: Supervision strategies are defined separately from business logic
//! - Flexible: Multiple strategies available for different failure scenarios
//! - Predictable: Clear rules for error handling and recovery
//!
//! ## Core Components
//!
//! - `SupervisorStrategy`: Base trait for implementing supervision strategies
//! - `SupervisionDecision`: Possible actions when handling failures
//! - `DecisionFn`: Custom failure handling logic
//! - Built-in strategies:
//!   - `DefaultStrategy`: Simple predefined behaviors
//!   - `OneForOneStrategy`: Independent child handling
//!   - `OneForAllStrategy`: Coordinated child handling
//!
//! ## Usage Example
//!
//! ```rust
//! use parrot_api::supervisor::{
//!     SupervisorStrategy,
//!     OneForOneStrategy,
//!     DefaultSupervisorStrategyFactory,
//! };
//! use std::time::Duration;
//!
//! // Create a one-for-one strategy
//! let strategy = DefaultSupervisorStrategyFactory::one_for_one(
//!     3,  // max restarts
//!     Duration::from_secs(60)  // within time window
//! );
//!
//! // Use in an actor system
//! let system = ActorSystem::new()
//!     .with_supervisor_strategy(strategy);
//! ```

use std::time::Duration;
use std::fmt::Debug;
use std::sync::Arc;
use async_trait::async_trait;
use crate::errors::ActorError;
use crate::address::ActorRef;

/// Decisions available to supervisors when handling actor failures.
///
/// When an actor fails, its supervisor must decide how to handle the failure.
/// This enum represents the possible decisions that can be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisionDecision {
    /// Continue actor execution, maintaining current state.
    ///
    /// Use when the error is transient and doesn't affect actor state.
    Resume,
    
    /// Recreate the actor, resetting its state to initial values.
    ///
    /// Use when the actor's state may be corrupted.
    Restart,
    
    /// Terminate the actor permanently.
    ///
    /// Use when the error is unrecoverable or the actor is no longer needed.
    Stop,
    
    /// Forward the error to the parent supervisor.
    ///
    /// Use when the error needs to be handled at a higher level.
    Escalate,
}

/// Trait for implementing custom failure handling logic.
///
/// This trait allows you to define how specific errors should be handled
/// by implementing custom decision functions.
pub trait DecisionFn: Send + Sync + Debug + 'static {
    /// Determines how to handle a specific actor error.
    ///
    /// # Parameters
    /// * `error` - The error that occurred
    ///
    /// # Returns
    /// The supervision decision to apply
    fn decide(&self, error: &ActorError) -> SupervisionDecision;
}

/// Basic implementation of the DecisionFn trait using a closure.
///
/// This type wraps a function pointer or closure in a thread-safe,
/// clonable container for use in supervision strategies.
#[derive(Clone)]
pub struct BasicDecisionFn(Arc<dyn Fn(&ActorError) -> SupervisionDecision + Send + Sync>);

impl Debug for BasicDecisionFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BasicDecisionFn(<function>)")
    }
}

impl BasicDecisionFn {
    /// Creates a new BasicDecisionFn from a function or closure.
    ///
    /// # Parameters
    /// * `f` - Function that maps errors to supervision decisions
    ///
    /// # Examples
    ///
    /// ```rust
    /// let decider = BasicDecisionFn::new(|error| {
    ///     match error {
    ///         ActorError::Timeout(_) => SupervisionDecision::Restart,
    ///         _ => SupervisionDecision::Stop,
    ///     }
    /// });
    /// ```
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&ActorError) -> SupervisionDecision + Send + Sync + 'static,
    {
        Self(Arc::new(f))
    }
}

impl DecisionFn for BasicDecisionFn {
    fn decide(&self, error: &ActorError) -> SupervisionDecision {
        (self.0)(error)
    }
}

/// Core trait for implementing actor supervision strategies.
///
/// This trait defines how a supervisor should handle failures of its
/// child actors. Different implementations can provide various recovery
/// and error handling behaviors.
#[async_trait]
pub trait SupervisorStrategy: Send + Sync + Debug + Clone + 'static {
    /// Handles a failure of a supervised actor.
    ///
    /// This method is called when a supervised actor encounters an error.
    /// The implementation should decide how to handle the failure based on
    /// the error type, actor state, and failure history.
    ///
    /// # Parameters
    /// * `failed_actor` - Reference to the failed actor
    /// * `error` - The error that occurred
    /// * `failure_count` - Number of failures for this actor
    ///
    /// # Returns
    /// The decision on how to handle the failure
    async fn handle_failure(
        &self,
        failed_actor: Box<dyn ActorRef>,
        error: &ActorError,
        failure_count: u32,
    ) -> SupervisionDecision;
}

/// Simple, predefined supervision strategies for common cases.
///
/// These strategies provide basic error handling behaviors without
/// the need for custom configuration.
#[derive(Debug, Clone, Copy)]
pub enum DefaultStrategy {
    /// Always terminates the failed actor
    StopOnFailure,
    /// Always attempts to restart the failed actor
    RestartOnFailure,
    /// Always continues actor execution
    ResumeOnFailure,
    /// Always forwards errors to parent
    EscalateFailure,
}

#[async_trait]
impl SupervisorStrategy for DefaultStrategy {
    async fn handle_failure(
        &self,
        _failed_actor: Box<dyn ActorRef>,
        _error: &ActorError,
        _failure_count: u32,
    ) -> SupervisionDecision {
        match self {
            DefaultStrategy::StopOnFailure => SupervisionDecision::Stop,
            DefaultStrategy::RestartOnFailure => SupervisionDecision::Restart,
            DefaultStrategy::ResumeOnFailure => SupervisionDecision::Resume,
            DefaultStrategy::EscalateFailure => SupervisionDecision::Escalate,
        }
    }
}

/// Supervision strategy that handles each child actor independently.
///
/// This strategy allows each child actor to fail and recover independently
/// of its siblings. It's useful when child actors don't have dependencies
/// on each other.
#[derive(Clone)]
pub struct OneForOneStrategy {
    /// Maximum number of restarts allowed within the time window
    pub max_restarts: u32,
    
    /// Time window for counting restarts
    pub within: Duration,
    
    /// Function for making supervision decisions
    pub decider: BasicDecisionFn,
}

impl Debug for OneForOneStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneForOneStrategy")
            .field("max_restarts", &self.max_restarts)
            .field("within", &self.within)
            .field("decider", &self.decider)
            .finish()
    }
}

#[async_trait]
impl SupervisorStrategy for OneForOneStrategy {
    async fn handle_failure(
        &self,
        _failed_actor: Box<dyn ActorRef>,
        error: &ActorError,
        failure_count: u32,
    ) -> SupervisionDecision {
        if failure_count > self.max_restarts {
            SupervisionDecision::Stop
        } else {
            self.decider.decide(error)
        }
    }
}

/// Supervision strategy that handles all child actors as a group.
///
/// When one child actor fails, the supervision decision is applied to
/// all child actors. This is useful when child actors have dependencies
/// on each other and need to be coordinated.
#[derive(Clone)]
pub struct OneForAllStrategy {
    /// Maximum number of restarts allowed within the time window
    pub max_restarts: u32,
    
    /// Time window for counting restarts
    pub within: Duration,
    
    /// Function for making supervision decisions
    pub decider: BasicDecisionFn,
}

impl Debug for OneForAllStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneForAllStrategy")
            .field("max_restarts", &self.max_restarts)
            .field("within", &self.within)
            .field("decider", &self.decider)
            .finish()
    }
}

#[async_trait]
impl SupervisorStrategy for OneForAllStrategy {
    async fn handle_failure(
        &self,
        _failed_actor: Box<dyn ActorRef>,
        error: &ActorError,
        failure_count: u32,
    ) -> SupervisionDecision {
        if failure_count > self.max_restarts {
            SupervisionDecision::Stop
        } else {
            self.decider.decide(error)
        }
    }
}

/// Factory for creating common supervisor strategy configurations.
///
/// This factory provides convenient methods for creating pre-configured
/// supervision strategies with sensible defaults.
pub struct DefaultSupervisorStrategyFactory;

impl DefaultSupervisorStrategyFactory {
    /// Creates a default decision function that always restarts actors.
    fn create_default_decider() -> BasicDecisionFn {
        BasicDecisionFn::new(|_| SupervisionDecision::Restart)
    }

    /// Creates a one-for-one strategy with specified parameters.
    ///
    /// # Parameters
    /// * `max_restarts` - Maximum number of restart attempts
    /// * `within` - Time window for counting restarts
    ///
    /// # Examples
    ///
    /// ```rust
    /// let strategy = DefaultSupervisorStrategyFactory::one_for_one(
    ///     3,
    ///     Duration::from_secs(60)
    /// );
    /// ```
    pub fn one_for_one(max_restarts: u32, within: Duration) -> OneForOneStrategy {
        OneForOneStrategy {
            max_restarts,
            within,
            decider: Self::create_default_decider(),
        }
    }

    /// Creates a one-for-all strategy with specified parameters.
    ///
    /// # Parameters
    /// * `max_restarts` - Maximum number of restart attempts
    /// * `within` - Time window for counting restarts
    ///
    /// # Examples
    ///
    /// ```rust
    /// let strategy = DefaultSupervisorStrategyFactory::one_for_all(
    ///     3,
    ///     Duration::from_secs(60)
    /// );
    /// ```
    pub fn one_for_all(max_restarts: u32, within: Duration) -> OneForAllStrategy {
        OneForAllStrategy {
            max_restarts,
            within,
            decider: Self::create_default_decider(),
        }
    }
}

/// Enumeration of all available supervision strategy types.
///
/// This enum provides a unified type for handling different
/// supervision strategies in the system.
#[derive(Debug, Clone)]
pub enum SupervisorStrategyType {
    /// Simple predefined strategy
    Default(DefaultStrategy),
    /// Independent child handling strategy
    OneForOne(OneForOneStrategy),
    /// Coordinated child handling strategy
    OneForAll(OneForAllStrategy),
}

#[async_trait]
impl SupervisorStrategy for SupervisorStrategyType {
    async fn handle_failure(
        &self,
        failed_actor: Box<dyn ActorRef>,
        error: &ActorError,
        failure_count: u32,
    ) -> SupervisionDecision {
        match self {
            Self::Default(s) => s.handle_failure(failed_actor, error, failure_count).await,
            Self::OneForOne(s) => s.handle_failure(failed_actor, error, failure_count).await,
            Self::OneForAll(s) => s.handle_failure(failed_actor, error, failure_count).await,
        }
    }
} 