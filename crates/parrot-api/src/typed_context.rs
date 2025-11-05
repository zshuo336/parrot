//! Generation-owned services for static typed actors.
use crate::{
    context::ReadOnlyChildrenVec, supervisor::SupervisorStrategyType, types::BoxedActorRef,
};
use std::{any::Any, sync::Arc};

/// An engine supplies identity and ownership services; business messages still
/// use the actor's concrete `Msg` and `Reply` types.
pub trait TypedContextServices: Any + Send + Sync {
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
