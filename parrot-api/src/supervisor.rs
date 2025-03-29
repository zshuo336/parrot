use std::time::Duration;
use std::fmt::Debug;
use std::sync::Arc;
use async_trait::async_trait;
use crate::errors::ActorError;
use crate::address::ActorRef;

/// Supervision decision for handling actor failures
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisionDecision {
    /// Resume the actor, keeping its state
    Resume,
    /// Restart the actor, resetting its state
    Restart,
    /// Stop the actor
    Stop,
    /// Escalate the failure to parent
    Escalate,
}

/// Decision function trait
pub trait DecisionFn: Send + Sync + Debug + 'static {
    fn decide(&self, error: &ActorError) -> SupervisionDecision;
}

/// Basic decision function implementation
#[derive(Clone)]
pub struct BasicDecisionFn(Arc<dyn Fn(&ActorError) -> SupervisionDecision + Send + Sync>);

impl Debug for BasicDecisionFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BasicDecisionFn(<function>)")
    }
}

impl BasicDecisionFn {
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

/// Supervisor strategy trait
#[async_trait]
pub trait SupervisorStrategy: Send + Sync + Debug + Clone + 'static {
    /// Handle actor failure
    async fn handle_failure(
        &self,
        failed_actor: Box<dyn ActorRef>,
        error: &ActorError,
        failure_count: u32,
    ) -> SupervisionDecision;
}

/// Default supervision strategies
#[derive(Debug, Clone, Copy)]
pub enum DefaultStrategy {
    /// Always stop on failure
    StopOnFailure,
    /// Always restart on failure
    RestartOnFailure,
    /// Always resume on failure
    ResumeOnFailure,
    /// Always escalate failure
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

/// One-for-one supervision strategy
#[derive(Clone)]
pub struct OneForOneStrategy {
    /// Maximum restart count
    pub max_restarts: u32,
    /// Restart time window
    pub within: Duration,
    /// Custom decision function
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

/// One-for-all supervision strategy
#[derive(Clone)]
pub struct OneForAllStrategy {
    /// Maximum restart count
    pub max_restarts: u32,
    /// Restart time window
    pub within: Duration,
    /// Custom decision function
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

/// Default supervisor strategy factory
pub struct DefaultSupervisorStrategyFactory;

impl DefaultSupervisorStrategyFactory {
    /// Create a debug-friendly function
    fn create_default_decider() -> BasicDecisionFn {
        BasicDecisionFn::new(|_| SupervisionDecision::Restart)
    }

    /// Create default one-for-one supervision strategy
    pub fn one_for_one(max_restarts: u32, within: Duration) -> OneForOneStrategy {
        OneForOneStrategy {
            max_restarts,
            within,
            decider: Self::create_default_decider(),
        }
    }

    /// Create default one-for-all supervision strategy
    pub fn one_for_all(max_restarts: u32, within: Duration) -> OneForAllStrategy {
        OneForAllStrategy {
            max_restarts,
            within,
            decider: Self::create_default_decider(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SupervisorStrategyType {
    Default(DefaultStrategy),
    OneForOne(OneForOneStrategy),
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