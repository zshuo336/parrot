use crate::{error_receipt::Receipt, owned_payload::panic_message};
use parrot_api::{errors::ActorError, system::SystemError, types::BoxedFuture};
use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    task::{Context, Poll},
};

pub(super) fn panic_receipt(payload: Box<dyn std::any::Any + Send>) -> Receipt<SystemError> {
    Receipt::new(SystemError::ActorError(ActorError::Panic(panic_message(
        payload,
    ))))
}

// The delegate stays outside catch_unwind during poll. Its independent Drop
// can then panic without unwinding through the original poll panic.
pub(super) struct ShutdownFuture<'a>(Option<BoxedFuture<'a, Result<(), SystemError>>>);
impl<'a> ShutdownFuture<'a> {
    pub(super) fn new(future: BoxedFuture<'a, Result<(), SystemError>>) -> Self {
        Self(Some(future))
    }
    fn dispose(&mut self) -> Option<Receipt<SystemError>> {
        catch_unwind(AssertUnwindSafe(|| drop(self.0.take())))
            .err()
            .map(panic_receipt)
    }
}
impl Future for ShutdownFuture<'_> {
    type Output = Vec<(&'static str, Receipt<SystemError>)>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let polled = catch_unwind(AssertUnwindSafe(|| {
            self.0
                .as_mut()
                .expect("live shutdown future")
                .as_mut()
                .poll(cx)
        }));
        let primary = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result.err().map(|error| ("shutdown", Receipt::new(error))),
            Err(payload) => Some(("shutdown poll", panic_receipt(payload))),
        };
        let mut errors: Vec<_> = primary.into_iter().collect();
        if let Some(error) = self.dispose() {
            errors.push(("shutdown future disposal", error));
        }
        Poll::Ready(errors)
    }
}
impl Drop for ShutdownFuture<'_> {
    fn drop(&mut self) {
        if let Some(error) = self.dispose() {
            tracing::warn!(error = %crate::error_receipt::public_system_summary(error.error()), "shutdown future disposal failed during cancellation");
        }
    }
}
