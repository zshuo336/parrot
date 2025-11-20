//! Generation-owned services for static typed actors.
use crate::{
    context::ReadOnlyChildrenVec,
    errors::ActorError,
    receptionist::{ReceptionistKey, ReceptionistStream},
    supervisor::SupervisorStrategyType,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::{any::Any, sync::Arc};

/// An engine supplies identity and ownership services; business messages still
/// use the actor's concrete `Msg` and `Reply` types.
pub trait TypedContextServices: Any + Send + Sync {
    /// Optional generation-owned service publication. Foreign contexts retain
    /// the default unavailable result; custom gateways define their own cleanup.
    fn receptionist_register(
        &self,
        _key: ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        Box::pin(async { Err(ActorError::InternalError("receptionist not enabled".into())) })
    }
    fn receptionist_deregister(
        &self,
        _key: &ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        Box::pin(async { Err(ActorError::InternalError("receptionist not enabled".into())) })
    }
    fn receptionist_subscribe(
        &self,
        _key: ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<ReceptionistStream>> {
        Box::pin(async { Err(ActorError::InternalError("receptionist not enabled".into())) })
    }

    /// Optional path resolution through the owning engine. A remote reference
    /// does not establish liveness or successful delivery. Foreign contexts
    /// without this capability return None.
    fn resolve_actor(&self, _path: &str) -> Option<BoxedActorRef> {
        None
    }

    /// Optional Local child creation in this exact generation. Query before
    /// constructing inputs; the default exposes no Local executor.
    fn local_spawner(&self) -> Option<&dyn crate::engine::AsyncLocalActorSpawner> {
        None
    }

    fn path(&self) -> &str;
    fn actor_ref(&self) -> Option<BoxedActorRef>;
    fn parent(&self) -> Option<BoxedActorRef>;
    fn children(&self) -> ReadOnlyChildrenVec;
    fn stop(&self);
    fn set_child_strategy(&self, strategy: SupervisorStrategyType);
}

/// Clones refer to one physical generation. Retaining a context cannot reopen
/// child admission after that generation stops or is replaced.
#[derive(Clone)]
pub struct TypedContext(Arc<dyn TypedContextServices>);
impl TypedContext {
    /// Publish this actor's path using the gateway captured at invocation. Native
    /// contexts with a built-in gateway withdraw all publications before their
    /// generation's retirement completes. Calls first polled after retirement
    /// cannot publish.
    pub fn receptionist_register(
        &self,
        key: ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        self.0.receptionist_register(key)
    }
    /// Remove this generation's binding from the gateway captured at invocation.
    /// An obsolete call cannot remove another generation's built-in binding.
    pub fn receptionist_deregister(
        &self,
        key: &ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        self.0.receptionist_deregister(key)
    }
    /// Subscribe through the gateway captured at invocation. Existing streams
    /// remain attached to that gateway after replacement or actor retirement.
    pub fn receptionist_subscribe(
        &self,
        key: ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<ReceptionistStream>> {
        self.0.receptionist_subscribe(key)
    }

    /// Resolve an exact local identity or an explicit remote path through this
    /// generation's owning engine. Native Thread/Actix contexts reject lookup
    /// after generation retirement or engine shutdown. Remote lookup requires
    /// an injected message gateway and does not check remote actor liveness.
    /// The returned dynamic reference is an explicit messaging bridge; the
    /// actor's primary typed message channel and dispatch remain unchanged.
    pub fn resolve_actor(&self, path: &str) -> Option<BoxedActorRef> {
        self.0.resolve_actor(path)
    }

    /// Discover Local child creation without depending on concrete engine types.
    pub fn local_spawner(&self) -> Option<&dyn crate::engine::AsyncLocalActorSpawner> {
        self.0.local_spawner()
    }

    /// Construct an engine or test context from its service implementation.
    pub fn from_services<S: TypedContextServices>(services: Arc<S>) -> Self {
        Self(services)
    }
    pub fn path(&self) -> &str {
        self.0.path()
    }
    pub fn actor_ref(&self) -> Option<BoxedActorRef> {
        self.0.actor_ref()
    }
    pub fn parent(&self) -> Option<BoxedActorRef> {
        self.0.parent()
    }
    pub fn children(&self) -> ReadOnlyChildrenVec {
        self.0.children()
    }
    /// Request self-stop without waiting on the current handler's retirement.
    pub fn stop(&self) {
        self.0.stop();
    }
    pub fn set_child_strategy(&self, strategy: SupervisorStrategyType) {
        self.0.set_child_strategy(strategy);
    }
    /// Engine extension boundary. This never erases a business message.
    #[doc(hidden)]
    pub fn services(&self) -> &(dyn Any + Send + Sync) {
        self.0.as_ref()
    }
}
