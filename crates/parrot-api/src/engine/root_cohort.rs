//! Optional, engine-bound supervision domains for top-level actors.
use super::{InstanceConfig, LocalRuntimeFactory, RuntimeActor, RuntimeFactory, SpawnOptions};
use crate::{
    context::ReadOnlyChildrenVec,
    supervisor::SupervisorStrategyType,
    system::SystemError,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::sync::Arc;

/// Creates independent groups on this runtime. Existing runtime implementations
/// opt in through `AsyncEngineRuntime::cohort_runtime`.
pub trait AsyncCohortRuntime: Send + Sync {
    /// Protect strategy captures at call time, including unpolled cancellation.
    /// The returned group stays bound to this engine across catalog replacements.
    fn root_cohort(
        &self,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, SystemError>>;
}

/// System-owned roots sharing one policy, restart budget and recovery barriers.
/// Members remain roots (no synthetic parent). Dropping this handle does not
/// stop members. Factories preserve their real references and paths on recovery;
/// preconstructed instances cannot be recreated. Use factories for restart.
pub trait AsyncRootCohort: Send + Sync {
    /// Snapshot admitted members, including pending first constructors.
    fn members(&self) -> ReadOnlyChildrenVec;

    /// Close admission at call time and join actual member/policy retirement.
    /// Dropping the returned waiter does not cancel cleanup.
    fn stop(&self) -> BoxedFuture<'static, ActorResult<()>>;

    /// Admit an instance under the group's policy. Inputs follow the runtime's
    /// call-time ownership and cancellation contract; init may still be pending.
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>>;

    /// Admit a repeatable constructor. Restart limits belong to the group,
    /// so this method takes path options without a second member restart budget.
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>>;

    /// Optional owner-thread factory admission in this same group.
    fn local_runtime(&self) -> Option<&dyn AsyncLocalRootCohort> {
        None
    }
}

/// Local state is constructed, polled and retired on its selected owner thread.
/// Only the constructor crosses the admission boundary; returned refs are Send.
pub trait AsyncLocalRootCohort: Send + Sync {
    /// Admit a Local factory with the group's policy and shared recovery barriers.
    /// Protect constructor captures before returning the future.
    fn spawn_local_factory<'a>(
        &'a self,
        factory: LocalRuntimeFactory,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>>;
}
