//! A native facade group keeps one physical engine and one shared recovery scope.
use super::{ActorSystemImpl, ParrotActorSystem, SingleActorRef};
use crate::{
    root_cohort::RootCohort,
    thread::typed::{SingleDispatch, SingleVariant},
};
use parrot_api::{
    engine::{
        AsyncLocalRootCohort, AsyncRootCohort, InstanceConfig, RuntimeActor, RuntimeFactory,
        SpawnOptions,
    },
    message::Message,
    supervisor::SupervisorStrategyType,
    system::SystemError,
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::sync::Arc;

enum NativeSystem {
    Thread(Arc<crate::thread::ThreadActorSystem>),
    Actix(Arc<crate::actix::ActixActorSystem>),
}

/// A facade-created native group accepting static, dynamic and Local members.
/// All members share the original engine's actual scope, budget and barriers.
/// Dropping a handle does not stop members; call `stop` or engine shutdown.
pub struct FacadeRootCohort {
    system: NativeSystem,
    group: RootCohort,
    dynamic: Arc<dyn AsyncRootCohort>,
}

impl ParrotActorSystem {
    /// Capture the default native engine and create one mixed-protocol root group.
    /// External dynamic-only groups remain available through `cohort_runtime`.
    pub fn native_root_cohort(
        &self,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<FacadeRootCohort>, SystemError>> {
        self.native_cohort_selected(self.get_leaf_creation_system(None), strategy)
    }

    /// Capture a named native engine; later catalog/default changes do not redirect it.
    pub fn native_root_cohort_on(
        &self,
        name: &str,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<FacadeRootCohort>, SystemError>> {
        self.native_cohort_selected(self.get_leaf_creation_system(Some(name)), strategy)
    }

    fn native_cohort_selected(
        &self,
        selected: Result<ActorSystemImpl, SystemError>,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<FacadeRootCohort>, SystemError>> {
        let strategy = self.own(strategy);
        Box::pin(parrot_api::trace::capture(async move {
            let selected = match selected {
                Ok(system) if !self.lifecycle.started() => system,
                result => {
                    strategy.retire().await.map_err(SystemError::ActorError)?;
                    return Err(result.err().unwrap_or(SystemError::ShuttingDown));
                }
            };
            let (system, group, dynamic) = match selected {
                ActorSystemImpl::Thread(system) => {
                    let group = system.root_cohort(strategy.take()).await?;
                    let dynamic = system.bind_root_cohort(group.clone());
                    (NativeSystem::Thread(system), group, dynamic)
                }
                ActorSystemImpl::Actix(system) => {
                    let system = Arc::new(system);
                    let group = system.root_cohort(strategy.take()).await?;
                    let dynamic = system.bind_root_cohort(group.clone());
                    (NativeSystem::Actix(system), group, dynamic)
                }
                ActorSystemImpl::Runtime(_) => {
                    strategy.retire().await.map_err(SystemError::ActorError)?;
                    return Err(SystemError::ActorCreationError(
                        "selected runtime has no native static root cohort bridge".into(),
                    ));
                }
            };
            Ok(Arc::new(FacadeRootCohort {
                system,
                group,
                dynamic,
            }))
        }))
    }
}

impl FacadeRootCohort {
    /// Admit a static instance under this group's shared failure policy.
    pub fn spawn_typed<'a, A, M>(
        &'a self,
        actor: A,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        match &self.system {
            NativeSystem::Thread(system) => {
                Box::pin(system.spawn_typed_in_cohort(actor, path, &self.group))
            }
            NativeSystem::Actix(system) => {
                Box::pin(system.spawn_typed_in_cohort(actor, path, &self.group))
            }
        }
    }

    /// Admit a repeatable static root with the group's original recovery barriers.
    pub fn spawn_typed_factory<'a, A, M, F>(
        &'a self,
        factory: F,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        match &self.system {
            NativeSystem::Thread(system) => {
                Box::pin(system.spawn_typed_factory_in_cohort(factory, path, &self.group))
            }
            NativeSystem::Actix(system) => {
                Box::pin(system.spawn_typed_factory_in_cohort(factory, path, &self.group))
            }
        }
    }

    /// Admit a single-protocol instance to the same native group.
    pub fn spawn_typed_single<'a, A, M>(
        &'a self,
        actor: A,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<SingleActorRef<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
    {
        self.spawn_typed::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
        )
    }

    /// Admit a repeatable single-protocol root to the same native group.
    pub fn spawn_typed_single_factory<'a, A, M, F>(
        &'a self,
        factory: F,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<SingleActorRef<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_typed_factory::<SingleDispatch<A, M>, SingleVariant<A, M>, _>(
            move || SingleDispatch::new(factory()),
            path,
        )
    }
}

impl AsyncRootCohort for FacadeRootCohort {
    fn members(&self) -> parrot_api::context::ReadOnlyChildrenVec {
        self.group.members()
    }
    fn stop(&self) -> BoxedFuture<'static, ActorResult<()>> {
        self.group.stop()
    }
    fn local_runtime(&self) -> Option<&dyn AsyncLocalRootCohort> {
        self.dynamic.local_runtime()
    }
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.dynamic.spawn(actor, config, options)
    }
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.dynamic.spawn_factory(factory, options)
    }
}
