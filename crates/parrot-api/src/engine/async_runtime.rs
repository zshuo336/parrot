//! Production asynchronous runtime boundary, using the existing Actor and ActorRef.
use crate::{
    actor::Actor,
    context::ActorContext,
    system::{SystemError, SystemStatus},
    types::{BoxedActorRef, BoxedFuture},
};
use std::any::Any;

/// An asynchronous actor whose context is supplied by the selected runtime.
pub type RuntimeActor = Box<dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>;
/// Instance configuration is retired by creation; Actor::init has no config argument.
/// Construct application state before supplying the instance, or use a runtime factory.
pub type InstanceConfig = Box<dyn Any + Send>;

/// Root instance admission options, independent of concrete engine types.
#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    /// Exact logical path to reserve in the engine registry. None generates a path.
    /// Explicit paths must be nonempty, at most 4096 UTF-8 bytes, and contain no
    /// control characters. They are opaque local identities, not remote routes.
    pub path: Option<String>,
}
impl SpawnOptions {
    /// Request an exact path. Validation occurs during owned creation.
    pub fn at(path: impl Into<String>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    /// Validate the common local-path contract without rewriting the identity.
    pub fn validate(&self) -> Result<(), SystemError> {
        if self
            .path
            .as_ref()
            .is_some_and(|p| p.is_empty() || p.len() > 4096 || p.chars().any(char::is_control))
        {
            return Err(SystemError::ActorCreationError(
                "runtime path must contain 1..=4096 UTF-8 bytes and no control characters".into(),
            ));
        }
        Ok(())
    }
}

/// Object-safe asynchronous boundary implemented by production runtimes.
///
/// Uses the full existing Actor/ActorRef lifecycle and messaging contract. No
/// blocking receive adapter or synthetic reference is involved. Instances are
/// Send; this interface does not claim to support owner-thread-only (!Send) actors.
///
/// Spawn protects actor/config ownership before returning a future. Dropping an
/// unpolled or pending future must retire inputs; after admission, cancellation
/// must revoke publication/stop the admitted instance without abandoning cleanup.
/// A successful spawn acknowledges admission, not completion of async init.
/// Messages wait for init; its failure terminates the reference normally.
///
/// Shutdown starts when polled. Once started, cancelling its waiter cannot cancel
/// cleanup. Repeated/concurrent shutdown joins the same operation and outcome;
/// success means admitted actors and workers have retired. Timeouts/errors remain
/// errors on subsequent calls. The host runtime must stay alive for async cleanup.
/// Any lets facades preserve native identity and recognize known facade cycles.
/// Existing static implementations require no additional method.
pub trait AsyncEngineRuntime: Any + Send + Sync {
    /// Optional owner-thread capability. Absence does not consume any inputs.
    /// Local state is constructed on the selected owner, never moved into a Send actor.
    fn local_runtime(&self) -> Option<&dyn super::AsyncLocalRuntime> {
        None
    }

    /// Optional repeatable-constructor capability. Query before constructing inputs.
    fn factory_runtime(&self) -> Option<&dyn AsyncFactoryRuntime> {
        None
    }

    /// Admit an instance and return its real engine reference.
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>>;

    /// Resolve the actual local registry identity, including pending admission.
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>>;

    /// Snapshot the runtime's current native status.
    fn status(&self) -> SystemStatus;

    /// Join the runtime's owned, idempotent shutdown operation.
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>>;
}

/// Repeatable constructor. Captured application configuration creates each fresh
/// generation; constructors run on bounded workers and must terminate cooperatively.
pub type RuntimeFactory = Box<dyn Fn() -> RuntimeActor + Send + Sync>;

/// Root factory admission and one-for-one restart budget.
#[derive(Clone, Debug)]
pub struct FactoryOptions {
    /// Registry path options, shared with instance admission.
    pub spawn: SpawnOptions,
    /// Number of replacements allowed in the rolling window (zero disables restart).
    pub max_restarts: u32,
    /// Positive rolling restart window.
    pub within: std::time::Duration,
}
impl Default for FactoryOptions {
    fn default() -> Self {
        Self {
            spawn: SpawnOptions::default(),
            max_restarts: 3,
            within: std::time::Duration::from_secs(60),
        }
    }
}
impl FactoryOptions {
    /// Validate before admission, without consuming a constructor.
    pub fn validate(&self) -> Result<(), SystemError> {
        self.spawn.validate()?;
        if self.within.is_zero() {
            return Err(SystemError::ActorCreationError(
                "factory restart window must be positive".into(),
            ));
        }
        Ok(())
    }
}

/// Optional production factory capability. Captures have owned retirement from
/// the call itself, including unpolled cancellation and rejected admission.
/// Successful creation returns a stable logical reference after initial construction;
/// async init may still be pending. Replacements preserve path, lookup and watches.
pub trait AsyncFactoryRuntime: Send + Sync {
    /// Create a root logical actor with a bounded restart policy.
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>>;
}

/// Engine-neutral creation of real children in the current parent's scope.
/// Parent termination waits for child retirement. Constructors enable replacement;
/// preconstructed instances cannot be recreated. None inherits the parent's policy.
/// Inputs are protected before returning the future; cancellation retains cleanup.
/// Child paths are generated under the parent's real logical path.
pub trait AsyncActorSpawner: Send + Sync {
    /// Admit a preconstructed child. Config follows the instance retirement contract.
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        strategy: Option<crate::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, crate::types::ActorResult<BoxedActorRef>>;
    /// Admit a repeatable child constructor under the parent's supervision.
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        strategy: Option<crate::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, crate::types::ActorResult<BoxedActorRef>>;
}
