//! Optional owner-thread execution through the production runtime catalog.
use super::FactoryOptions;
use crate::{
    local_typed::LocalBoxedFuture,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};

/// Object-safe local behavior. The actor, its futures, and their destructors stay
/// on one owner thread. Only messages and replies cross the Send boundary.
/// Context clones identify one generation and cannot reopen its child scope.
/// This is a local messaging/lifecycle face with owner-thread stream hooks;
/// the capability remains optional and does not claim that every engine
/// supports local execution.
pub trait LocalActor: 'static {
    fn init<'a>(&'a mut self, _context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn before_stop<'a>(
        &'a mut self,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn init_local_streams<'a>(
        &'a mut self,
        _context: &'a mut crate::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_started<'a>(
        &'a mut self,
        _id: u64,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_item<'a>(
        &'a mut self,
        _id: u64,
        _item: Box<dyn std::any::Any + 'static>,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_finished<'a>(
        &'a mut self,
        _id: u64,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_error<'a>(
        &'a mut self,
        _id: u64,
        _error: crate::errors::ActorError,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    /// Receives the original dynamic value, including ActorTerminated events
    /// when this actor watches another real engine reference.
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>>;
}
/// Exists only on the selected owner; deliberately has no Send requirement.
pub type LocalRuntimeActor = Box<dyn LocalActor>;
/// Send captures move to an owner before invocation. No Sync bound is needed;
/// all calls are serialized there. Create Rc/RefCell state inside this callback.
pub type LocalRuntimeFactory = Box<dyn Fn() -> LocalRuntimeActor + Send>;

/// Optional local factory capability of an AsyncEngineRuntime. Lookup, status,
/// and shutdown use that same runtime and its real registry/lifecycle identities.
pub trait AsyncLocalRuntime: Send + Sync {
    /// Admit a repeatable local root. Success acknowledges mailbox/registry
    /// admission; construction and async init may still be pending. A construction
    /// failure terminates the reference; init/handler failures use the bounded
    /// supervision policy. Final failures remain in the stop receipt.
    /// Zero max_restarts disables replacements. Failed messages are not replayed;
    /// unread messages and the logical identity survive successful replacements.
    /// Inputs are owned at call time, including unpolled cancellation or rejection.
    /// Synchronous constructors/destructors must terminate cooperatively: a stop
    /// deadline reports timeout without abandoning their eventual cleanup.
    fn spawn_local_factory<'a>(
        &'a self,
        factory: LocalRuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, crate::system::SystemError>>;
}

/// Optional engine-neutral Local child admission in one real parent generation.
/// Returned references use the parent's native scope, registry and supervision.
pub trait AsyncLocalActorSpawner: Send + Sync {
    /// Admit a Local child factory. None inherits the live parent strategy;
    /// Some overrides it. Paths are generated beneath the parent's real path.
    /// Parent retirement joins child cleanup; an old generation cannot admit
    /// new children after stop/replacement. Inputs are owned at call time,
    /// including unpolled cancellation and rejected admission.
    /// Success acknowledges admission before construction/init, as for Local roots.
    fn spawn_local_factory<'a>(
        &'a self,
        factory: LocalRuntimeFactory,
        strategy: Option<crate::supervisor::SupervisorStrategyType>,
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>>;
}
