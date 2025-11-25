//! Borrowed callbacks and independently protected registry snapshot owners.
use super::ActorSystemImpl;
use crate::owned_payload::{OwnedPayload, PublicationInput};
use parrot_api::{
    address::ActorPath,
    errors::ActorError,
    types::{BoxedActorRef, BoxedFuture},
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    task::Poll,
};

pub(super) async fn callback<R: Send + 'static>(future: BoxedFuture<'_, R>) -> R {
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
    let cleanup = future.retire();
    match outcome.take() {
        Err(payload) => std::panic::resume_unwind(payload),
        Ok(value) => match cleanup {
            Ok(()) => value,
            Err(error) => {
                let _ = PublicationInput::new(value).retire();
                panic!("lookup future retirement failed: {error}");
            }
        },
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
