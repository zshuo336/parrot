//! Bounded ownership for a created result that has not reached its facade caller.
use super::ActorSystemImpl;
use crate::owned_payload::{
    OwnedPayload, PendingCleanupPermit, PublicationInput, cleanup_diagnostic, panic_message,
    pending_cleanup_permit,
};
use parrot_api::{
    engine::AsyncRootCohort,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    task::Poll,
};

pub(in crate::system) trait CreationOutput: Send + Sync + 'static {
    fn retire(&self) -> BoxedFuture<'_, ActorResult<()>>;
}
impl CreationOutput for BoxedActorRef {
    fn retire(&self) -> BoxedFuture<'_, ActorResult<()>> {
        self.stop()
    }
}
impl CreationOutput for Arc<dyn AsyncRootCohort> {
    fn retire(&self) -> BoxedFuture<'_, ActorResult<()>> {
        self.stop()
    }
}

#[derive(Clone)]
pub(in crate::system) struct CreationRetirement {
    runtime: tokio::runtime::Handle,
    permit: Arc<PendingCleanupPermit>,
    context: Option<parrot_api::trace::TraceContext>,
}
impl CreationRetirement {
    pub(in crate::system) async fn reserve(
        runtime: &tokio::runtime::Handle,
        slots: &Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self {
            runtime: runtime.clone(),
            permit: Arc::new(pending_cleanup_permit(slots).await),
            context: parrot_api::trace::TraceContext::current(),
        }
    }
}

// Poll and dispose an arbitrary borrowed stop future in separate unwind scopes.
async fn stop<T: CreationOutput>(value: &T) -> ActorResult<()> {
    let future = catch_unwind(AssertUnwindSafe(|| value.retire()))
        .map_err(|payload| ActorError::Panic(panic_message(payload)))?;
    let mut future = PublicationInput::new(future);
    let result = std::future::poll_fn(|cx| {
        match catch_unwind(AssertUnwindSafe(|| future.value_mut().as_mut().poll(cx))) {
            Ok(polled) => polled,
            Err(payload) => Poll::Ready(Err(ActorError::Panic(panic_message(payload)))),
        }
    })
    .await;
    let mut result = PublicationInput::new(result);
    let disposal = future.retire();
    let result = result.take();
    if result.is_err() { result } else { disposal }
}

struct Unpublished<T: CreationOutput, E: Send + 'static> {
    result: Option<OwnedPayload<Result<T, E>>>,
    retirement: Option<CreationRetirement>,
    finished: Option<tokio::sync::oneshot::Sender<()>>,
}
impl<T: CreationOutput, E: Send + 'static> Unpublished<T, E> {
    fn new(result: OwnedPayload<Result<T, E>>, retirement: CreationRetirement) -> Self {
        Self {
            result: Some(result),
            retirement: Some(retirement),
            finished: None,
        }
    }
    fn failed(&self) -> bool {
        self.result.as_ref().unwrap().value().is_err()
    }
    fn take(mut self) -> Result<T, E> {
        self.result.take().unwrap().take()
    }
    async fn retire(mut self) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.finished = Some(sender);
        drop(self);
        let _ = receiver.await;
    }
}

pub(in crate::system) async fn callback_creation<T, E>(
    future: BoxedFuture<'_, Result<T, E>>,
    owner: &OwnedPayload<ActorSystemImpl>,
    retirement: &CreationRetirement,
) -> Result<T, E>
where
    T: CreationOutput,
    E: From<ActorError> + Send + 'static,
{
    // A borrowed future's destructor can fail after returning a live actor.
    // Install cancellation-safe retirement before running that destructor.
    let (result, cleanup) = super::super::snapshot::protected_callback(future, |result| {
        Unpublished::new(owner.protect(result), retirement.clone())
    })
    .await;
    match cleanup {
        Ok(()) => result.take(),
        Err(error) if result.failed() => {
            cleanup_diagnostic(|| {
                tracing::warn!(error = %error, "creation future cleanup failed after operation error");
            });
            result.take()
        }
        Err(error) => {
            result.retire().await;
            Err(error.into())
        }
    }
}
impl<T: CreationOutput, E: Send + 'static> Drop for Unpublished<T, E> {
    fn drop(&mut self) {
        let Some(result) = self.result.take() else {
            return;
        };
        let retirement = self.retirement.take().unwrap();
        let finished = self.finished.take();
        // Reserve before starting native creation. Cancellation can dispatch one
        // terminal owner without an unbounded task queue or a held disposal slot.
        retirement.runtime.spawn(parrot_api::trace::scope(retirement.context, async move {
            let _permit = retirement.permit;
            if let Ok(value) = result.value() {
                let outcome = PublicationInput::new(stop(value).await);
                if let Err(error) = outcome.value() {
                    cleanup_diagnostic(|| tracing::warn!(error = %error, "unpublished facade creation stop failed"));
                }
            }
            let _ = result.retire().await;
            cleanup_diagnostic(|| {
                if let Some(finished) = finished { let _ = finished.send(()); }
            });
        }));
    }
}

pub(in crate::system) async fn finish_creation<T, E>(
    owner: OwnedPayload<ActorSystemImpl>,
    result: Result<T, E>,
    retirement: CreationRetirement,
) -> Result<T, E>
where
    T: CreationOutput,
    E: From<ActorError> + Send + 'static,
{
    let result = Unpublished::new(owner.protect(result), retirement);
    match owner.retire().await {
        Ok(()) => result.take(),
        Err(error) if result.failed() => {
            cleanup_diagnostic(
                || tracing::warn!(error = %error, "selected runtime cleanup failed after operation error"),
            );
            result.take()
        }
        Err(error) => {
            result.retire().await;
            Err(error.into())
        }
    }
}
