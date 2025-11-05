//! Open runtime dispatch through the existing application-facing facade.
use super::{ActorSystemImpl, ParrotActorSystem, retire_registrations};
use parrot_api::{
    engine::{
        AsyncEngineRuntime, AsyncFactoryRuntime, AsyncLocalRuntime, FactoryOptions, InstanceConfig,
        LocalRuntimeFactory, RuntimeActor, RuntimeFactory, SpawnOptions,
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
        let system = ActorSystemImpl::from_runtime(runtime);
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
        let mut systems = self.systems.write().unwrap();
        let retired_gateway = match &system {
            ActorSystemImpl::Thread(thread) => self
                .receptionist_gateway()
                .and_then(|gateway| thread.swap_receptionist_gateway(gateway)),
            ActorSystemImpl::Actix(actix) => self
                .receptionist_gateway()
                .and_then(|gateway| actix.swap_receptionist_gateway(gateway)),
            ActorSystemImpl::Runtime(_) => None,
        };
        let old = systems.insert(name.clone(), system);
        if set_as_default || self.default_system.read().unwrap().is_none() {
            *self.default_system.write().unwrap() = Some(name.clone());
        }
        self.topology.set(&name, child_topology.as_ref());
        drop(systems);
        drop(lifecycle);
        drop(topology);
        if let Err(error) = retire_registrations([retired_gateway]) {
            // An old registration still belongs to this result path. Retire it
            // outside locks, even when the propagated gateway failed disposal.
            let _ = retire_registrations([old]);
            return Err(error);
        }
        Ok(old)
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
        match &self.runtime {
            Some(runtime) => {
                crate::owned_payload::OwnedPayload::new(value, runtime, &self.cleanup_slots)
            }
            None => crate::owned_payload::OwnedPayload::unbound(value),
        }
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
        Box::pin(async move {
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
        })
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
        Box::pin(async move {
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
        })
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
        Box::pin(async move {
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
        })
    }
}
impl AsyncEngineRuntime for ParrotActorSystem {
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
impl AsyncFactoryRuntime for ParrotActorSystem {
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.factory_selected(self.get_creation_system(None), factory, options)
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
