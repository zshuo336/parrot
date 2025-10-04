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
//! use parrot_api::supervisor::DefaultSupervisorStrategyFactory;
//! use std::time::Duration;
//!
//! // Create a one-for-one strategy
//! let strategy = DefaultSupervisorStrategyFactory::one_for_one(
//!     3,  // max restarts
//!     Duration::from_secs(60)  // within time window
//! );
//! ```
//!
//! Note: registering the strategy with an actor system is
//! implementation-specific; refer to the concrete system's builder API.

use crate::address::ActorRef;
use crate::errors::ActorError;
use async_trait::async_trait;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

/// Why an actor terminated (M3 supervision: public death reason).
///
/// Carried by engine `Terminated` notifications so DeathWatch consumers
/// can distinguish graceful stops from panics and escalations — parity
/// with Akka's public `Terminated`/`DeathPact` distinction and Erlang's
/// process exit reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeathReason {
    /// Normal termination (`stop` / system shutdown / mailbox closed).
    Normal,
    /// The actor panicked; the payload message is the formatted panic.
    Panic(String),
    /// The actor was killed by the system (forced removal).
    Killed,
    /// The actor was stopped as part of an escalation cascade; the payload
    /// message explains the originating failure.
    Escalated(String),
}

impl std::fmt::Display for DeathReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeathReason::Normal => write!(f, "normal"),
            DeathReason::Panic(m) => write!(f, "panic: {m}"),
            DeathReason::Killed => write!(f, "killed"),
            DeathReason::Escalated(m) => write!(f, "escalated: {m}"),
        }
    }
}

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
    /// use parrot_api::supervisor::{BasicDecisionFn, SupervisionDecision};
    /// use parrot_api::errors::ActorError;
    ///
    /// let decider = BasicDecisionFn::new(|error: &ActorError| {
    ///     match error {
    ///         ActorError::Timeout => SupervisionDecision::Restart,
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
#[derive(Debug, Clone, Copy, Default)]
pub enum DefaultStrategy {
    /// Always terminates the failed actor
    #[default]
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
    /// use parrot_api::supervisor::DefaultSupervisorStrategyFactory;
    /// use std::time::Duration;
    ///
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
    /// use parrot_api::supervisor::DefaultSupervisorStrategyFactory;
    /// use std::time::Duration;
    ///
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

impl Default for SupervisorStrategyType {
    fn default() -> Self {
        Self::Default(DefaultStrategy::default())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{ActorPath, ActorRef};
    use crate::errors::ActorError;
    use crate::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
    use std::time::Duration;

    /// 一个恒死的占位 ActorRef，用于 handle_failure 参数传递。
    #[derive(Debug)]
    struct DeadRef;

    #[async_trait]
    impl ActorRef for DeadRef {
        fn send<'a>(
            &'a self,
            _msg: BoxedMessage,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Err(ActorError::Stopped) })
        }
        fn send_with_timeout<'a>(
            &'a self,
            _msg: BoxedMessage,
            _t: Option<std::time::Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Err(ActorError::Stopped) })
        }
        fn deliver<'a>(
            &'a self,
            _msg: BoxedMessage,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Err(ActorError::Stopped) })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Err(ActorError::Stopped) })
        }
        fn path(&self) -> String {
            "dead://test".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { false })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(DeadRef)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn dead_ref() -> Box<dyn ActorRef> {
        Box::new(DeadRef)
    }

    fn any_err() -> ActorError {
        ActorError::MessageHandlingError("boom".into())
    }

    // ---------------- DeathReason ----------------

    #[test]
    fn death_reason_display_variants() {
        assert_eq!(DeathReason::Normal.to_string(), "normal");
        assert_eq!(
            DeathReason::Panic("x".into()).to_string(),
            "panic: x"
        );
        assert_eq!(DeathReason::Killed.to_string(), "killed");
        assert_eq!(
            DeathReason::Escalated("cascade".into()).to_string(),
            "escalated: cascade"
        );
    }

    #[test]
    fn death_reason_equality_and_clone() {
        let a = DeathReason::Panic("p".into());
        assert_eq!(a.clone(), DeathReason::Panic("p".into()));
        assert_ne!(DeathReason::Normal, DeathReason::Killed);
        assert_ne!(DeathReason::Panic("a".into()), DeathReason::Panic("b".into()));
    }

    // ---------------- BasicDecisionFn ----------------

    #[test]
    fn basic_decision_fn_dispatches_by_error() {
        let d = BasicDecisionFn::new(|e| match e {
            ActorError::Timeout => SupervisionDecision::Resume,
            _ => SupervisionDecision::Stop,
        });
        assert_eq!(d.decide(&ActorError::Timeout), SupervisionDecision::Resume);
        assert_eq!(
            d.decide(&ActorError::InitializationError("x".into())),
            SupervisionDecision::Stop
        );
    }

    #[test]
    fn basic_decision_fn_debug_and_clone() {
        let d = BasicDecisionFn::new(|_| SupervisionDecision::Restart);
        assert!(format!("{:?}", d).contains("BasicDecisionFn"));
        let d2 = d.clone();
        assert_eq!(d2.decide(&any_err()), SupervisionDecision::Restart);
    }

    // ---------------- DefaultStrategy ----------------

    #[tokio::test]
    async fn default_strategy_all_four_decisions() {
        let cases = [
            (DefaultStrategy::StopOnFailure, SupervisionDecision::Stop),
            (DefaultStrategy::RestartOnFailure, SupervisionDecision::Restart),
            (DefaultStrategy::ResumeOnFailure, SupervisionDecision::Resume),
            (DefaultStrategy::EscalateFailure, SupervisionDecision::Escalate),
        ];
        for (strategy, want) in cases {
            assert_eq!(
                strategy.handle_failure(dead_ref(), &any_err(), 0).await,
                want,
                "strategy {:?}",
                strategy
            );
        }
    }

    #[tokio::test]
    async fn default_strategy_default_is_stop() {
        assert!(matches!(
            DefaultStrategy::default(),
            DefaultStrategy::StopOnFailure
        ));
        assert_eq!(
            DefaultStrategy::default()
                .handle_failure(dead_ref(), &any_err(), 42)
                .await,
            SupervisionDecision::Stop
        );
    }

    // ---------------- OneForOne / OneForAll ----------------

    #[tokio::test]
    async fn one_for_one_within_budget_uses_decider() {
        let s = DefaultSupervisorStrategyFactory::one_for_one(3, Duration::from_secs(60));
        // failure_count == max_restarts: 3 > 3 false → decider (default restart)
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 3).await,
            SupervisionDecision::Restart
        );
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 0).await,
            SupervisionDecision::Restart
        );
    }

    #[tokio::test]
    async fn one_for_one_over_budget_stops() {
        let s = DefaultSupervisorStrategyFactory::one_for_one(3, Duration::from_secs(60));
        // failure_count > max_restarts → Stop（无论 decider）
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 4).await,
            SupervisionDecision::Stop
        );
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 1000).await,
            SupervisionDecision::Stop
        );
    }

    #[tokio::test]
    async fn one_for_one_zero_budget_always_stops_after_first() {
        let s = DefaultSupervisorStrategyFactory::one_for_one(0, Duration::from_secs(1));
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 1).await,
            SupervisionDecision::Stop
        );
        // failure_count=0（首次失败）仍走 decider
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 0).await,
            SupervisionDecision::Restart
        );
    }

    #[tokio::test]
    async fn one_for_one_custom_decider_overrides() {
        let s = OneForOneStrategy {
            max_restarts: 10,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(|e| {
                if matches!(e, ActorError::Panic(_)) {
                    SupervisionDecision::Escalate
                } else {
                    SupervisionDecision::Resume
                }
            }),
        };
        assert_eq!(
            s.handle_failure(dead_ref(), &ActorError::Panic("p".into()), 1)
                .await,
            SupervisionDecision::Escalate
        );
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 1).await,
            SupervisionDecision::Resume
        );
    }

    #[tokio::test]
    async fn one_for_all_budget_boundary_matches_one_for_one() {
        let s = DefaultSupervisorStrategyFactory::one_for_all(2, Duration::from_secs(30));
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 2).await,
            SupervisionDecision::Restart
        );
        assert_eq!(
            s.handle_failure(dead_ref(), &any_err(), 3).await,
            SupervisionDecision::Stop
        );
    }

    #[test]
    fn strategy_debug_impls_show_params() {
        let s = DefaultSupervisorStrategyFactory::one_for_one(5, Duration::from_secs(10));
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("max_restarts"));
        assert!(dbg.contains("decider"));

        let s2 = DefaultSupervisorStrategyFactory::one_for_all(5, Duration::from_secs(10));
        assert!(format!("{:?}", s2).contains("OneForAllStrategy"));
    }

    #[test]
    fn strategy_clone_preserves_behavior() {
        let s = DefaultSupervisorStrategyFactory::one_for_one(1, Duration::from_secs(1));
        let c = s.clone();
        assert_eq!(c.max_restarts, 1);
        assert_eq!(c.within, Duration::from_secs(1));
    }

    // ---------------- SupervisorStrategyType（枚举分发） ----------------

    #[tokio::test]
    async fn strategy_type_dispatches_to_inner() {
        let cases: Vec<(SupervisorStrategyType, SupervisionDecision)> = vec![
            (
                SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure),
                SupervisionDecision::Resume,
            ),
            (
                SupervisorStrategyType::OneForOne(
                    DefaultSupervisorStrategyFactory::one_for_one(0, Duration::from_secs(1)),
                ),
                SupervisionDecision::Stop, // failure_count=1 > max=0
            ),
            (
                SupervisorStrategyType::OneForAll(
                    DefaultSupervisorStrategyFactory::one_for_all(9, Duration::from_secs(1)),
                ),
                SupervisionDecision::Restart, // decider 默认
            ),
        ];
        for (ty, want) in cases {
            assert_eq!(ty.handle_failure(dead_ref(), &any_err(), 1).await, want);
        }
    }

    #[test]
    fn strategy_type_default_is_default_stop() {
        assert!(matches!(
            SupervisorStrategyType::default(),
            SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure)
        ));
    }

    // ---------------- 每错误类型 × 决策矩阵 ----------------

    #[tokio::test]
    async fn decider_sees_all_error_variants() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let d = BasicDecisionFn::new(move |e| {
            seen2.lock().unwrap().push(format!("{:?}", e));
            SupervisionDecision::Restart
        });
        let errors = vec![
            ActorError::InitializationError("i".into()),
            ActorError::MessageHandlingError("m".into()),
            ActorError::Stopped,
            ActorError::Timeout,
            ActorError::TimeoutDetail("t".into()),
            ActorError::ActorNotFound("n".into()),
            ActorError::InternalError("in".into()),
            ActorError::ProcessMessageError("p".into()),
            ActorError::ReplyChannelError("r".into()),
            ActorError::Panic("pan".into()),
        ];
        for e in &errors {
            assert_eq!(d.decide(e), SupervisionDecision::Restart);
        }
        assert_eq!(seen.lock().unwrap().len(), errors.len());
        // anyhow 包装（Other）也必须可用
        assert_eq!(
            d.decide(&ActorError::Other(anyhow::anyhow!("o"))),
            SupervisionDecision::Restart
        );
    }

    // ---------------- ActorPath 占位（辅助覆盖地址构造） ----------------

    #[test]
    fn actor_path_placeholder_in_strategy_context() {
        // 策略本身不读 path；此测试保证 DeadRef 的 path 稳定可用于日志
        let p = ActorPath::placeholder("dead://test");
        assert_eq!(p.path(), "dead://test");
    }
}
