//! Engine-neutral groups use the native registry and recovery owners.
use super::{ThreadActorSystem, system::BoxedThreadActor};
use crate::{owned_payload::OwnedPayload, root_cohort::RootCohort};
use parrot_api::{
    engine::{
        AsyncCohortRuntime, AsyncRootCohort, InstanceConfig, RuntimeActor, RuntimeFactory,
        SpawnOptions,
    },
    supervisor::SupervisorStrategyType,
    system::SystemError,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::sync::Arc;

struct Cohort {
    system: Arc<ThreadActorSystem>,
    group: RootCohort,
}

impl AsyncCohortRuntime for ThreadActorSystem {
    fn root_cohort(
        &self,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, SystemError>> {
        let strategy = OwnedPayload::new(strategy, self.retirement_runtime(), &self.cleanup_slots);
        let system = self.self_arc();
        Box::pin(parrot_api::trace::capture(async move {
            let Some(system) = system else {
                strategy.retire().await.map_err(SystemError::ActorError)?;
                return Err(SystemError::InitializationError(
                    "ThreadActorSystem requires its managed Arc identity".into(),
                ));
            };
            let group = system
                .root_cohort(strategy.take())
                .await
                .map_err(SystemError::ActorError)?;
            Ok(Arc::new(Cohort { system, group }) as Arc<dyn AsyncRootCohort>)
        }))
    }
}

impl AsyncRootCohort for Cohort {
    fn members(&self) -> parrot_api::context::ReadOnlyChildrenVec {
        self.group.members()
    }

    fn stop(&self) -> BoxedFuture<'static, ActorResult<()>> {
        self.group.stop()
    }

    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let actor = OwnedPayload::new(
            actor,
            self.system.retirement_runtime(),
            &self.system.cleanup_slots,
        );
        let config = OwnedPayload::new(
            config,
            self.system.retirement_runtime(),
            &self.system.cleanup_slots,
        );
        Box::pin(parrot_api::trace::capture(async move {
            if let Err(error) = options.validate() {
                let (actor, config) = tokio::join!(actor.retire(), config.retire());
                actor.and(config).map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let path = options
                .path
                .unwrap_or_else(|| format!("/runtime/{}", uuid::Uuid::new_v4().simple()));
            let reference = self
                .system
                .spawn_in_cohort(
                    BoxedThreadActor::new(actor.take()),
                    config.take(),
                    Default::default(),
                    path,
                    &self.group,
                )
                .await
                .map_err(|error| match error {
                    super::SpawnError::SystemShutdown => SystemError::ShuttingDown,
                    error => SystemError::ActorCreationError(error.to_string()),
                })?;
            Ok(Box::new(reference) as BoxedActorRef)
        }))
    }

    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let factory = OwnedPayload::new(
            factory,
            self.system.retirement_runtime(),
            &self.system.cleanup_slots,
        );
        Box::pin(parrot_api::trace::capture(async move {
            if let Err(error) = options.validate() {
                factory.retire().await.map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let constructor = factory.take();
            let path = options
                .path
                .unwrap_or_else(|| format!("/runtime/{}", uuid::Uuid::new_v4().simple()));
            let reference = self
                .system
                .spawn_supervised_in_cohort(
                    move || BoxedThreadActor::new(constructor()),
                    &path,
                    Default::default(),
                    &self.group,
                )
                .await
                .map_err(|error| match error {
                    super::SpawnError::SystemShutdown => SystemError::ShuttingDown,
                    error => SystemError::ActorCreationError(error.to_string()),
                })?;
            Ok(Box::new(reference) as BoxedActorRef)
        }))
    }
}
