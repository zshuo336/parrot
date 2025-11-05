//! Retirement ownership shared by static factories on both engines.
use super::Factory;
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedFuture},
};
use std::sync::Arc;

/// This owner is installed before admission. Even a cancelled rejection waiter
/// retains factory captures and unfinished construction/output disposal.
pub(crate) struct OwnedFactory<A: Send + 'static> {
    pub(crate) factory: Arc<Factory<A>>,
    runtime: tokio::runtime::Handle,
}
impl<A: Send + 'static> OwnedFactory<A> {
    pub(crate) fn new(
        factory: impl Fn() -> A + Send + Sync + 'static,
        workers: Arc<tokio::sync::Semaphore>,
        cleanup: Arc<tokio::sync::Semaphore>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            factory: Factory::new(factory, workers, cleanup),
            runtime,
        }
    }
    pub(crate) fn retire(self) -> BoxedFuture<'static, ActorResult<()>> {
        let receipt = self.factory.retire_on(Some(self.runtime.clone()));
        Box::pin(async move { receipt.await.map_err(ActorError::InternalError) })
    }
}
impl<A: Send + 'static> Drop for OwnedFactory<A> {
    fn drop(&mut self) {
        drop(self.factory.retire_on(Some(self.runtime.clone())));
    }
}
