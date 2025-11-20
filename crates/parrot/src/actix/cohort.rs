//! Engine-neutral groups use the native registry and recovery owners.
use super::{ActixActorSystem, system::BoxedActixActor};
use crate::{owned_payload::OwnedPayload, root_cohort::RootCohort};
use parrot_api::{
    engine::{
        AsyncCohortRuntime, AsyncLocalRootCohort, AsyncRootCohort, InstanceConfig,
        LocalRuntimeFactory, RuntimeActor, RuntimeFactory, SpawnOptions,
    },
    supervisor::SupervisorStrategyType,
    system::SystemError,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::sync::Arc;

struct Cohort {
    system: ActixActorSystem,
    group: RootCohort,
}

impl AsyncCohortRuntime for ActixActorSystem {
    fn root_cohort(
        &self,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, SystemError>> {
        let group = self.root_cohort(strategy);
        let system = self.clone();
        Box::pin(async move {
            let group = group.await.map_err(SystemError::ActorError)?;
            Ok(Arc::new(Cohort { system, group }) as Arc<dyn AsyncRootCohort>)
        })
    }
}

impl AsyncRootCohort for Cohort {
    fn members(&self) -> parrot_api::context::ReadOnlyChildrenVec {
        self.group.members()
    }

    fn stop(&self) -> BoxedFuture<'static, ActorResult<()>> {
        self.group.stop()
    }

    fn local_runtime(&self) -> Option<&dyn AsyncLocalRootCohort> {
        Some(self)
    }

    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let actor = OwnedPayload::new(actor, &self.system.runtime, &self.system.cleanup_slots);
        let config = OwnedPayload::new(config, &self.system.runtime, &self.system.cleanup_slots);
        Box::pin(parrot_api::trace::capture(async move {
            if let Err(error) = options.validate() {
                let (actor, config) = tokio::join!(actor.retire(), config.retire());
                actor.and(config).map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let path = options
                .path
                .unwrap_or_else(|| format!("actix://runtime/{}", uuid::Uuid::new_v4().simple()));
            self.system
                .spawn_root_in_cohort(
                    BoxedActixActor {
                        inner: actor.take(),
                    },
                    config.take(),
                    &path,
                    &self.group,
                )
                .await
                .map_err(SystemError::ActorError)
        }))
    }

    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let factory = crate::owned_payload::OwnedPayload::new(
            factory,
            &self.system.runtime,
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
                .unwrap_or_else(|| format!("actix://runtime/{}", uuid::Uuid::new_v4().simple()));
            self.system
                .spawn_root_factory_in_cohort(
                    move || BoxedActixActor {
                        inner: constructor(),
                    },
                    &path,
                    &self.group,
                )
                .await
                .map_err(SystemError::ActorError)
        }))
    }
}

impl AsyncLocalRootCohort for Cohort {
    fn spawn_local_factory<'a>(
        &'a self,
        factory: LocalRuntimeFactory,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.system
            .spawn_local_factory_in_cohort(factory, options, &self.group)
    }
}
