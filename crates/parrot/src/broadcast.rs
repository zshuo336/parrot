//! Broadcast keeps one protected source and bounds user cloning off the driver.
use crate::owned_payload::{OwnedPayload, join_error, panic_message};
use parrot_api::{errors::ActorError, types::ActorResult};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub(crate) struct Failure {
    operation: crate::error_receipt::Receipt<anyhow::Error>,
    summary: String,
    cleanup: crate::error_receipt::ActorReceipt,
}
impl std::fmt::Debug for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; broadcast source cleanup: {}",
            self.summary,
            crate::error_receipt::actor_summary(&self.cleanup)
        )
    }
}
impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.operation.error().as_ref())
    }
}

pub(crate) async fn finish<T, E>(
    source: Input<T>,
    operation: Result<(), E>,
    summarize: fn(&E) -> String,
) -> Result<(), E>
where
    T: Send + 'static,
    E: std::error::Error + Send + Sync + From<anyhow::Error> + 'static,
{
    // Keep an arbitrary delivery error protected while source disposal runs.
    let operation = OwnedPayload::unbound(operation);
    let cleanup = source.retire().await;
    match (operation.take(), cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(anyhow::Error::new(error).into()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(anyhow::Error::new(Failure {
            summary: summarize(&error),
            operation: crate::error_receipt::Receipt::new(anyhow::Error::new(error)),
            cleanup: crate::error_receipt::ActorReceipt::from_error(cleanup),
        })
        .into()),
    }
}

pub(crate) struct Input<T: Send + 'static> {
    source: Option<OwnedPayload<T>>,
    workers: Arc<Semaphore>,
}
impl<T: Send + 'static> Input<T> {
    pub(crate) fn new(source: OwnedPayload<T>, workers: Arc<Semaphore>) -> Self {
        Self {
            source: Some(source),
            workers,
        }
    }
    pub(crate) async fn copy_with<R: Send + 'static>(
        &mut self,
        copy: impl FnOnce(&T) -> ActorResult<R> + Send + 'static,
    ) -> ActorResult<OwnedPayload<R>> {
        let permit = self
            .workers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ActorError::Stopped)?;
        let source = self.source.take().expect("one broadcast copy at a time");
        let trace = parrot_api::trace::TraceContext::current();
        let (source, result) = tokio::task::spawn_blocking(move || {
            parrot_api::trace::in_scope(trace, || {
                let _permit = permit;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| copy(source.value())))
                        .unwrap_or_else(|panic| {
                            Err(ActorError::Panic(format!(
                                "broadcast clone panicked: {}",
                                panic_message(panic)
                            )))
                        })
                        .map(|value| source.protect(value));
                (source, result)
            })
        })
        .await
        .map_err(|error| {
            ActorError::InternalError(format!(
                "broadcast clone worker failed: {}",
                join_error(error)
            ))
        })?;
        self.source = Some(source);
        result
    }
    pub(crate) async fn retire(mut self) -> ActorResult<()> {
        match self.source.take() {
            Some(source) => source.retire().await,
            None => Ok(()),
        }
    }
}
impl<T: Clone + Send + 'static> Input<T> {
    pub(crate) async fn copy(&mut self) -> ActorResult<OwnedPayload<T>> {
        self.copy_with(|source| Ok(source.clone())).await
    }
}
