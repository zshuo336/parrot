//! Open runtime dispatch through the existing application-facing facade.
use super::{ActorSystemImpl, ParrotActorSystem, retire_registrations};
use crate::owned_payload::PublicationInput;
use parrot_api::{
    engine::{
        AsyncCohortRuntime, AsyncEngineRuntime, AsyncFactoryRuntime, AsyncLocalRuntime,
        AsyncRootCohort, FactoryOptions, InstanceConfig, LocalRuntimeFactory, RuntimeActor,
        RuntimeFactory, SpawnOptions,
    },
    system::{ActorSystem, SystemError, SystemStatus},
    types::{BoxedActorRef, BoxedFuture},
};
use std::{any::Any, sync::Arc};

impl ActorSystemImpl {
    fn from_runtime(runtime: Arc<dyn AsyncEngineRuntime>) -> Self {
        // Canonicalize known engines, so mixed typed/neutral aliases share the
        // same registry identity and receptionist propagation. External engines
        // retain their actual trait object and use Arc identity for aliases.
        let erased: Arc<dyn Any + Send + Sync> = runtime.clone();
        match erased.downcast::<crate::thread::ThreadActorSystem>() {
            Ok(thread) => Self::Thread(thread),
            Err(erased) => match erased.downcast::<crate::actix::ActixActorSystem>() {
                Ok(actix) => Self::Actix((*actix).clone()),
                Err(_) => Self::Runtime(runtime),
            },
        }
    }
    fn runtime(&self) -> &dyn AsyncEngineRuntime {
        match self {
            Self::Thread(system) => system.as_ref(),
            Self::Actix(system) => system,
            Self::Runtime(system) => system.as_ref(),
        }
    }
    fn into_runtime(self) -> Arc<dyn AsyncEngineRuntime> {
        match self {
            Self::Thread(system) => system,
            Self::Actix(system) => Arc::new(system),
            Self::Runtime(system) => system,
        }
    }
}
impl ParrotActorSystem {
    /// Execute I/O on a named engine's independent executor.
    pub fn run_io_on(
        &self,
        name: &str,
        future: BoxedFuture<
            'static,
            parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
        >,
    ) -> BoxedFuture<'static, parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
        self.io_selected(self.get_creation_system(Some(name)), future)
    }

    fn io_selected(
        &self,
        system: Result<ActorSystemImpl, SystemError>,
        future: BoxedFuture<
            'static,
            parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
        >,
    ) -> BoxedFuture<'static, parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
        match system {
            Ok(system) if !self.lifecycle.started() => match system.runtime().io_runtime() {
                Some(io) => io.run_io(future),
                None => crate::io_runtime::reject(future),
            },
            _ => crate::io_runtime::reject(future),
        }
    }

    pub(super) fn shutdown_would_wait_for_io(&self) -> bool {
        let systems: Vec<_> = self.systems.read().unwrap().values().cloned().collect();
        systems
            .iter()
            .any(|system| system.runtime().shutdown_would_wait_for_current_task())
    }
    /// Register any asynchronous engine. Names retain the facade's opaque-key
    /// semantics. Replacement returns the previous registration without stopping
    /// it; callers retain responsibility for removed/replaced engines. Shutdown
    /// atomically closes engine registration. No user callback runs under locks.
    pub fn register_runtime(
        &self,
        name: String,
        runtime: Arc<dyn AsyncEngineRuntime>,
        set_as_default: bool,
    ) -> Result<Option<ActorSystemImpl>, SystemError> {
        let child_topology = (runtime.as_ref() as &dyn Any)
            .downcast_ref::<Self>()
            .map(|facade| facade.topology.clone());
        self.register_system(
            name,
            PublicationInput::new(ActorSystemImpl::from_runtime(runtime)),
            child_topology,
            set_as_default,
        )
    }
    pub(super) fn register_system(
        &self,
        name: String,
        mut system: PublicationInput<ActorSystemImpl>,
        child_topology: Option<Arc<super::topology::Node>>,
        set_as_default: bool,
    ) -> Result<Option<ActorSystemImpl>, SystemError> {
        // Keep rejected and displaced inputs outside every registry lock scope.
        let mut retired_gateway = PublicationInput::new(None);
        let mut retired_remote = PublicationInput::new(None);
        let mut old = PublicationInput::new(None);
        let result = (|| {
            let topology = super::topology::mutation();
            let lifecycle = self.lifecycle.receipt.lock().unwrap();
            if lifecycle.is_some() {
                drop(lifecycle);
                return Err(SystemError::ShuttingDown);
            }
            if child_topology
                .as_ref()
                .is_some_and(|child| !self.topology.accepts(child))
            {
                return Err(SystemError::InitializationError(
                    "facade runtime registration would create an ownership cycle".into(),
                ));
            }
            let mut systems = self
                .systems
                .write()
                .map_err(|_| SystemError::Other(anyhow::anyhow!("systems lock poisoned")))?;
            *retired_gateway.value_mut() = match system.value() {
                ActorSystemImpl::Thread(thread) => self
                    .receptionist_gateway()
                    .and_then(|gateway| thread.swap_receptionist_gateway(gateway)),
                ActorSystemImpl::Actix(actix) => self
                    .receptionist_gateway()
                    .and_then(|gateway| actix.swap_receptionist_gateway(gateway)),
                ActorSystemImpl::Runtime(_) => None,
            };
            *retired_remote.value_mut() = match system.value() {
                ActorSystemImpl::Thread(thread) => self
                    .remote_engine_gateway()
                    .and_then(|gateway| thread.swap_remote_gateway(gateway)),
                ActorSystemImpl::Actix(actix) => self
                    .remote_engine_gateway()
                    .and_then(|gateway| actix.swap_remote_gateway(gateway)),
                ActorSystemImpl::Runtime(_) => None,
            };
            *old.value_mut() = systems.insert(name.clone(), system.take());
            if set_as_default || self.default_system.read().unwrap().is_none() {
                *self.default_system.write().unwrap() = Some(name.clone());
            }
            self.topology.set(&name, child_topology.as_ref());
            drop(systems);
            drop(lifecycle);
            drop(topology);
            Ok(())
        })();
        let input_cleanup = system.retire().map_err(SystemError::ActorError);
        let service_cleanup = retire_registrations([retired_gateway.take()]);
        let remote_cleanup = retire_registrations([retired_remote.take()]);
        if let Err(error) = input_cleanup
            .and(result)
            .and(service_cleanup)
            .and(remote_cleanup)
        {
            // An old registration still belongs to this result path. Retire it
            // outside locks, even when the propagated gateway failed disposal.
            let _ = retire_registrations([old.take()]);
            return Err(error);
        }
        Ok(old.take())
    }
    /// Capture a runtime without retaining a facade registry lock. Discovery
    /// does not transfer exclusive ownership or bypass the runtime's shutdown.
    pub fn runtime(&self, name: &str) -> Result<Arc<dyn AsyncEngineRuntime>, SystemError> {
        self.get_system_impl(name)
            .map(ActorSystemImpl::into_runtime)
    }
    /// Remove one registration and return its engine without shutting it down.
    /// Removing the default selects the first remaining name in lexical order.
    pub fn unregister_system(&self, name: &str) -> Result<Option<ActorSystemImpl>, SystemError> {
        let _topology = super::topology::mutation();
        let lifecycle = self.lifecycle.receipt.lock().unwrap();
        if lifecycle.is_some() {
            return Err(SystemError::ShuttingDown);
        }
        let mut systems = self.systems.write().unwrap();
        let old = systems.remove(name);
        self.topology.set(name, None);
        let mut default = self.default_system.write().unwrap();
        if default.as_deref() == Some(name) {
            *default = systems.keys().min().cloned();
        }
        Ok(old)
    }
    pub(super) fn get_creation_system(
        &self,
        name: Option<&str>,
    ) -> Result<ActorSystemImpl, SystemError> {
        let lifecycle = self.lifecycle.receipt.lock().unwrap();
        if lifecycle.is_some() {
            return Err(SystemError::ShuttingDown);
        }
        match name {
            Some(name) => self.get_system_impl(name),
            None => self.get_default_system_impl(),
        }
    }
    pub(super) fn own<T: Send + 'static>(&self, value: T) -> crate::owned_payload::OwnedPayload<T> {
        crate::owned_payload::OwnedPayload::new(value, &self.runtime, &self.cleanup_slots)
    }
    /// Create a root group on a named engine captured at call time. The group
    /// retains that engine even when registrations or the default later change.
    pub fn root_cohort_on(
        &self,
        name: &str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, SystemError>> {
        self.cohort_selected(self.get_cohort_system(Some(name)), strategy)
    }

    fn get_cohort_system(&self, name: Option<&str>) -> Result<ActorSystemImpl, SystemError> {
        let mut system = self.get_creation_system(name)?;
        let mut seen = std::collections::HashSet::new();
        loop {
            if !seen.insert(system.identity()) {
                return Err(SystemError::InitializationError(
                    "facade root cohort selection revisited a runtime".into(),
                ));
            }
            let nested = (system.runtime() as &dyn Any)
                .downcast_ref::<Self>()
                .map(|facade| facade.get_creation_system(None));
            match nested {
                Some(selected) => system = selected?,
                None => return Ok(system),
            }
        }
    }

    fn cohort_selected(
        &self,
        system: Result<ActorSystemImpl, SystemError>,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, SystemError>> {
        let strategy = self.own(strategy);
        Box::pin(parrot_api::trace::capture(async move {
            let system = match system {
                Ok(system) if !self.lifecycle.started() => system,
                result => {
                    let error = result.err().unwrap_or(SystemError::ShuttingDown);
                    strategy.retire().await.map_err(SystemError::ActorError)?;
                    return Err(error);
                }
            };
            let Some(runtime) = system.runtime().cohort_runtime() else {
                strategy.retire().await.map_err(SystemError::ActorError)?;
                return Err(SystemError::ActorCreationError(
                    "selected runtime does not expose root cohorts".into(),
                ));
            };
            runtime.root_cohort(strategy.take()).await
        }))
    }
    /// Named engine creation with a real underlying path. Selection is captured
    /// at call time; later replacement does not redirect this pending creation.
    pub fn spawn_on<'a>(
        &'a self,
        name: &str,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.spawn_selected(self.get_creation_system(Some(name)), actor, config, options)
    }
    fn spawn_selected<'a>(
        &'a self,
        system: Result<ActorSystemImpl, SystemError>,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let actor = self.own(actor);
        let config = self.own(config);
        Box::pin(parrot_api::trace::capture(async move {
            let system = match system {
                Ok(system) if !self.lifecycle.started() => system,
                result => {
                    let error = result.err().unwrap_or(SystemError::ShuttingDown);
                    let (actor, config) = tokio::join!(actor.retire(), config.retire());
                    actor.and(config).map_err(SystemError::ActorError)?;
                    return Err(error);
                }
            };
            if let Err(error) = options.validate() {
                let (actor, config) = tokio::join!(actor.retire(), config.retire());
                actor.and(config).map_err(SystemError::ActorError)?;
                return Err(error);
            }
            system
                .runtime()
                .spawn(actor.take(), config.take(), options)
                .await
        }))
    }
    /// Named-engine factory creation, preserving the selected runtime's real
    /// logical identity and parent-independent root supervision.
    pub fn spawn_factory_on<'a>(
        &'a self,
        name: &str,
        factory: RuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.factory_selected(self.get_creation_system(Some(name)), factory, options)
    }
    fn factory_selected<'a>(
        &'a self,
        system: Result<ActorSystemImpl, SystemError>,
        factory: RuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let factory = self.own(factory);
        Box::pin(parrot_api::trace::capture(async move {
            let system = match system {
                Ok(system) if !self.lifecycle.started() => system,
                result => {
                    let error = result.err().unwrap_or(SystemError::ShuttingDown);
                    factory.retire().await.map_err(SystemError::ActorError)?;
                    return Err(error);
                }
            };
            if let Err(error) = options.validate() {
                factory.retire().await.map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let Some(runtime) = system.runtime().factory_runtime() else {
                factory.retire().await.map_err(SystemError::ActorError)?;
                return Err(SystemError::ActorCreationError(
                    "selected runtime does not expose factories".into(),
                ));
            };
            runtime.spawn_factory(factory.take(), options).await
        }))
    }
    /// Named-engine Local factory creation, preserving the selected runtime's real
    /// logical identity and parent-independent root supervision.
    pub fn spawn_local_factory_on<'a>(
        &'a self,
        name: &str,
        factory: LocalRuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.local_factory_selected(self.get_creation_system(Some(name)), factory, options)
    }
    fn local_factory_selected<'a>(
        &'a self,
        system: Result<ActorSystemImpl, SystemError>,
        factory: LocalRuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        let factory = self.own(factory);
        Box::pin(parrot_api::trace::capture(async move {
            let system = match system {
                Ok(system) if !self.lifecycle.started() => system,
                result => {
                    let error = result.err().unwrap_or(SystemError::ShuttingDown);
                    factory.retire().await.map_err(SystemError::ActorError)?;
                    return Err(error);
                }
            };
            if let Err(error) = options.validate() {
                factory.retire().await.map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let Some(runtime) = system.runtime().local_runtime() else {
                factory.retire().await.map_err(SystemError::ActorError)?;
                return Err(SystemError::ActorCreationError(
                    "selected runtime does not expose Local factories".into(),
                ));
            };
            runtime.spawn_local_factory(factory.take(), options).await
        }))
    }
}
impl AsyncEngineRuntime for ParrotActorSystem {
    fn shutdown_would_wait_for_current_task(&self) -> bool {
        self.shutdown_would_wait_for_io()
    }
    fn io_runtime(&self) -> Option<&dyn parrot_api::engine::AsyncIoRuntime> {
        self.get_creation_system(None).ok().and_then(|system| {
            system
                .runtime()
                .io_runtime()
                .is_some()
                .then_some(self as &dyn parrot_api::engine::AsyncIoRuntime)
        })
    }
    fn cohort_runtime(&self) -> Option<&dyn AsyncCohortRuntime> {
        self.get_creation_system(None).ok().and_then(|system| {
            system
                .runtime()
                .cohort_runtime()
                .is_some()
                .then_some(self as &dyn AsyncCohortRuntime)
        })
    }
    fn local_runtime(&self) -> Option<&dyn AsyncLocalRuntime> {
        self.get_creation_system(None).ok().and_then(|system| {
            system
                .runtime()
                .local_runtime()
                .is_some()
                .then_some(self as &dyn AsyncLocalRuntime)
        })
    }
    fn factory_runtime(&self) -> Option<&dyn AsyncFactoryRuntime> {
        self.get_creation_system(None).ok().and_then(|system| {
            system
                .runtime()
                .factory_runtime()
                .is_some()
                .then_some(self as &dyn AsyncFactoryRuntime)
        })
    }
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.spawn_selected(self.get_creation_system(None), actor, config, options)
    }
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async move {
            let default = self.get_default_system_name().ok();
            let mut systems: Vec<_> = self
                .systems
                .read()
                .unwrap()
                .iter()
                .map(|(name, system)| (name.clone(), system.clone()))
                .collect();
            systems.sort_by(|a, b| {
                (Some(&a.0) != default.as_ref(), &a.0).cmp(&(Some(&b.0) != default.as_ref(), &b.0))
            });
            let mut seen = std::collections::HashSet::new();
            for (_, system) in systems {
                if seen.insert(system.identity())
                    && let Some(reference) = system.runtime().get_actor(path).await
                {
                    return Some(reference);
                }
            }
            None
        })
    }
    fn status(&self) -> SystemStatus {
        ActorSystem::status(self)
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        self.shutdown_runtime()
    }
}
impl parrot_api::engine::AsyncIoRuntime for ParrotActorSystem {
    fn run_io(
        &self,
        future: BoxedFuture<
            'static,
            parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
        >,
    ) -> BoxedFuture<'static, parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
        self.io_selected(self.get_creation_system(None), future)
    }
}

impl AsyncFactoryRuntime for ParrotActorSystem {
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.factory_selected(self.get_creation_system(None), factory, options)
    }
}

impl AsyncCohortRuntime for ParrotActorSystem {
    fn root_cohort(
        &self,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> BoxedFuture<'_, Result<Arc<dyn AsyncRootCohort>, SystemError>> {
        self.cohort_selected(self.get_cohort_system(None), strategy)
    }
}

impl AsyncLocalRuntime for ParrotActorSystem {
    fn spawn_local_factory<'a>(
        &'a self,
        factory: LocalRuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.local_factory_selected(self.get_creation_system(None), factory, options)
    }
}
