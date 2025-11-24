//! Static protocols retain their native mailbox through facade selection.
use super::{ActorSystemImpl, ParrotActorSystem};
use parrot_api::{
    errors::ActorError,
    supervisor::{OneForOneStrategy, SupervisorStrategyType},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedFuture},
};
use std::sync::Arc;

mod single;

/// Concrete reference for the native single-protocol adapter.
pub type SingleActorRef<A, M> = TypedActorRef<
    crate::thread::typed::SingleDispatch<A, M>,
    crate::thread::typed::SingleVariant<A, M>,
>;

impl ParrotActorSystem {
    /// Create a static root on the default native engine captured at call time.
    /// The concrete mailbox, typed replies, context and supervision are native.
    /// Inputs are owned immediately; admission starts when the future is polled.
    pub fn spawn_typed<'a, A, M>(
        &'a self,
        actor: A,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.typed_selected(None, actor, path, None)
    }

    /// Create a static root on a named native engine captured at call time.
    pub fn spawn_typed_on<'a, A, M>(
        &'a self,
        name: &str,
        actor: A,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.typed_selected(Some(name), actor, path, None)
    }

    /// Create a static instance with explicit native handler supervision.
    /// Restart requires a repeatable factory; an instance cannot reconstruct.
    pub fn spawn_typed_with_strategy<'a, A, M>(
        &'a self,
        actor: A,
        path: &'a str,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.typed_selected(None, actor, path, Some(strategy))
    }

    /// Named static instance with explicit native handler supervision.
    pub fn spawn_typed_with_strategy_on<'a, A, M>(
        &'a self,
        name: &str,
        actor: A,
        path: &'a str,
        strategy: SupervisorStrategyType,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        self.typed_selected(Some(name), actor, path, Some(strategy))
    }

    fn typed_selected<'a, A, M>(
        &'a self,
        name: Option<&str>,
        actor: A,
        path: &'a str,
        strategy: Option<SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let selected = self.get_leaf_creation_system(name);
        let actor = self.own(actor);
        let strategy = self.own(strategy);
        Box::pin(parrot_api::trace::capture(async move {
            let selected = match selected {
                Ok(selected) if !self.lifecycle.started() => selected,
                result => {
                    let (actor, strategy) = tokio::join!(actor.retire(), strategy.retire());
                    actor.and(strategy)?;
                    return Err(selection_error(result.err()));
                }
            };
            match selected {
                ActorSystemImpl::Thread(system) => match strategy.take() {
                    Some(strategy) => {
                        system
                            .spawn_typed_with_strategy(actor.take(), path, strategy)
                            .await
                    }
                    None => system.spawn_typed(actor.take(), path).await,
                },
                ActorSystemImpl::Actix(system) => {
                    let system = Arc::new(system);
                    match strategy.take() {
                        Some(strategy) => {
                            system
                                .spawn_typed_with_strategy(actor.take(), path, strategy)
                                .await
                        }
                        None => system.spawn_typed(actor.take(), path).await,
                    }
                }
                ActorSystemImpl::Runtime(_) => {
                    let (actor, strategy) = tokio::join!(actor.retire(), strategy.retire());
                    actor.and(strategy)?;
                    Err(unsupported())
                }
            }
        }))
    }

    /// Create a repeatable static root using the selected native default policy.
    /// Original typed references and unread messages survive reconstruction.
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
        self.typed_factory_selected(None, factory, path, None)
    }

    /// Named repeatable static root, with engine selection fixed before polling.
    pub fn spawn_typed_factory_on<'a, A, M, F>(
        &'a self,
        name: &str,
        factory: F,
        path: &'a str,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.typed_factory_selected(Some(name), factory, path, None)
    }

    /// Repeatable static root with an explicit bounded OneForOne policy.
    pub fn spawn_typed_factory_with_strategy<'a, A, M, F>(
        &'a self,
        factory: F,
        path: &'a str,
        strategy: OneForOneStrategy,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.typed_factory_selected(None, factory, path, Some(strategy))
    }

    /// Named repeatable static root with an explicit bounded OneForOne policy.
    pub fn spawn_typed_factory_with_strategy_on<'a, A, M, F>(
        &'a self,
        name: &str,
        factory: F,
        path: &'a str,
        strategy: OneForOneStrategy,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        self.typed_factory_selected(Some(name), factory, path, Some(strategy))
    }

    fn typed_factory_selected<'a, A, M, F>(
        &'a self,
        name: Option<&str>,
        factory: F,
        path: &'a str,
        strategy: Option<OneForOneStrategy>,
    ) -> BoxedFuture<'a, ActorResult<TypedActorRef<A, M>>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
        F: Fn() -> A + Send + Sync + 'static,
    {
        let selected = self.get_leaf_creation_system(name);
        let factory = self.own(factory);
        let strategy = self.own(strategy);
        Box::pin(parrot_api::trace::capture(async move {
            let selected = match selected {
                Ok(selected) if !self.lifecycle.started() => selected,
                result => {
                    let (factory, strategy) = tokio::join!(factory.retire(), strategy.retire());
                    factory.and(strategy)?;
                    return Err(selection_error(result.err()));
                }
            };
            match selected {
                ActorSystemImpl::Thread(system) => match strategy.take() {
                    Some(strategy) => {
                        system
                            .spawn_typed_factory_with_strategy(factory.take(), path, strategy)
                            .await
                    }
                    None => system.spawn_typed_factory(factory.take(), path).await,
                },
                ActorSystemImpl::Actix(system) => {
                    let system = Arc::new(system);
                    match strategy.take() {
                        Some(strategy) => {
                            system
                                .spawn_typed_factory_with_strategy(factory.take(), path, strategy)
                                .await
                        }
                        None => system.spawn_typed_factory(factory.take(), path).await,
                    }
                }
                ActorSystemImpl::Runtime(_) => {
                    let (factory, strategy) = tokio::join!(factory.retire(), strategy.retire());
                    factory.and(strategy)?;
                    Err(unsupported())
                }
            }
        }))
    }
}

fn selection_error(error: Option<parrot_api::system::SystemError>) -> ActorError {
    match error {
        None | Some(parrot_api::system::SystemError::ShuttingDown) => ActorError::Stopped,
        Some(error) => ActorError::InitializationError(error.to_string()),
    }
}

fn unsupported() -> ActorError {
    ActorError::InitializationError("selected runtime has no native static typed bridge".into())
}
