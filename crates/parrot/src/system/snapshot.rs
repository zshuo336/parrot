//! Borrowed callbacks and independently protected registry snapshot owners.
use super::ActorSystemImpl;
use crate::owned_payload::{OwnedPayload, PublicationInput};
use parrot_api::{
    address::ActorPath,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    task::Poll,
};

pub(super) async fn protected_callback<R, S>(
    future: BoxedFuture<'_, R>,
    protect: impl FnOnce(R) -> S,
) -> (S, ActorResult<()>) {
    let mut future = PublicationInput::new(future);
    let outcome = std::future::poll_fn(|cx| {
        match catch_unwind(AssertUnwindSafe(|| future.value_mut().as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    })
    .await;
    let mut outcome = PublicationInput::new(outcome);
    match outcome.take() {
        Err(payload) => {
            let mut payload = PublicationInput::new(payload);
            let _ = future.retire();
            std::panic::resume_unwind(payload.take())
        }
        Ok(value) => {
            let value = protect(value);
            (value, future.retire())
        }
    }
}

pub(super) async fn callback<R: Send + 'static>(future: BoxedFuture<'_, R>) -> R {
    let (mut value, cleanup) = protected_callback(future, PublicationInput::new).await;
    match cleanup {
        Ok(()) => value.take(),
        Err(error) => {
            let _ = value.retire();
            panic!("lookup future retirement failed: {error}");
        }
    }
}

pub(super) async fn result_callback<R>(future: BoxedFuture<'_, ActorResult<R>>) -> ActorResult<R> {
    let (mut result, cleanup) = protected_callback(future, PublicationInput::new).await;
    match cleanup {
        Err(error) if result.value().is_ok() => {
            let _ = result.retire();
            Err(error)
        }
        _ => result.take(),
    }
}

pub(super) async fn lookup(
    owner: OwnedPayload<ActorSystemImpl>,
    path: &ActorPath,
) -> Option<BoxedActorRef> {
    let result = owner.value().get_actor(path).await;
    super::runtime::finish_selected(owner, Ok::<_, ActorError>(result))
        .await
        .unwrap_or_else(|error| panic!("lookup owner retirement failed: {error}"))
}
