//! Single-protocol convenience methods reuse the same static facade admission.
use super::{ParrotActorSystem, SingleActorRef};
use crate::thread::typed::{SingleDispatch, SingleVariant};
use parrot_api::{
    message::Message,
    supervisor::{OneForOneStrategy, SupervisorStrategyType},
    typed::TypedReceive,
    types::{ActorResult, BoxedFuture},
};

impl ParrotActorSystem {
    /// Create a single-protocol instance; send messages with `SingleVariant::wrap`.
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

    /// Named single-protocol instance, with call-time engine selection.
    pub fn spawn_typed_single_on<'a, A, M>(
        &'a self,
        name: &str,
        actor: A,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<SingleActorRef<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
    {
        self.spawn_typed_on::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            name,
            SingleDispatch::new(actor),
            path,
        )
    }

    /// Single-protocol instance with an explicit handler policy.
    pub fn spawn_typed_single_with_strategy<'a, A, M>(
        &'a self,
        actor: A,
        path: &'a str,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'a, ActorResult<SingleActorRef<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
    {
        self.spawn_typed_with_strategy::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
            strategy,
        )
    }

    /// Named single-protocol instance with an explicit handler policy.
    pub fn spawn_typed_single_with_strategy_on<'a, A, M>(
        &'a self,
        name: &str,
        actor: A,
        path: &'a str,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'a, ActorResult<SingleActorRef<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
    {
        self.spawn_typed_with_strategy_on::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            name,
            SingleDispatch::new(actor),
            path,
            strategy,
        )
    }

    /// Repeatable single-protocol root using the native engine's default policy.
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

    /// Named repeatable single-protocol root, preserving its original typed ref.
    pub fn spawn_typed_single_factory_on<'a, A, M, F>(
        &'a self,
        name: &str,
        factory: F,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<SingleActorRef<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_typed_factory_on::<SingleDispatch<A, M>, SingleVariant<A, M>, _>(
            name,
            move || SingleDispatch::new(factory()),
            path,
        )
    }

    /// Repeatable single-protocol root with an explicit bounded restart policy.
    pub fn spawn_typed_single_factory_with_strategy<'a, A, M, F>(
        &'a self,
        factory: F,
        path: &'a str,
        strategy: OneForOneStrategy,
    ) -> BoxedFuture<'a, ActorResult<SingleActorRef<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_typed_factory_with_strategy::<SingleDispatch<A, M>, SingleVariant<A, M>, _>(
            move || SingleDispatch::new(factory()),
            path,
            strategy,
        )
    }

    /// Named repeatable single-protocol root with an explicit restart policy.
    pub fn spawn_typed_single_factory_with_strategy_on<'a, A, M, F>(
        &'a self,
        name: &str,
        factory: F,
        path: &'a str,
        strategy: OneForOneStrategy,
    ) -> BoxedFuture<'a, ActorResult<SingleActorRef<A, M>>>
    where
        A: TypedReceive<M>,
        M: Message,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.spawn_typed_factory_with_strategy_on::<SingleDispatch<A, M>, SingleVariant<A, M>, _>(
            name,
            move || SingleDispatch::new(factory()),
            path,
            strategy,
        )
    }
}
