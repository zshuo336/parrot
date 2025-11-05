//! Separate user polling, formatting and each owned destructor before publishing a receipt.
use parrot_api::types::BoxedFuture;
use std::{
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};

#[derive(Default)]
pub(crate) struct Cleanup(AtomicBool);
impl Cleanup {
    pub(crate) fn failed(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    pub(crate) fn call<T>(&self, operation: impl FnOnce() -> T) -> Result<T, ()> {
        catch_unwind(AssertUnwindSafe(operation)).map_err(|payload| {
            self.0.store(true, Ordering::Relaxed);
            // Retire the original panic payload once. Its throwing successor
            // must not recursively unwind through another independent owner.
            if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(secondary);
            }
        })
    }
    pub(crate) fn dispose<T>(&self, value: T) {
        let _ = self.call(|| drop(value));
    }
    pub(crate) fn owned<T>(&self, value: T) -> Owned<'_, T> {
        Owned {
            value: Some(value),
            cleanup: self,
        }
    }
}
pub(crate) struct Owned<'a, T> {
    value: Option<T>,
    cleanup: &'a Cleanup,
}
impl<T> Owned<'_, T> {
    pub(crate) fn value(&self) -> &T {
        self.value.as_ref().expect("live ingress input")
    }
    pub(crate) fn take(&mut self) -> T {
        self.value.take().expect("live ingress input")
    }
    pub(crate) fn retire(&mut self) {
        self.cleanup.dispose(self.value.take());
    }
}
impl<T> Drop for Owned<'_, T> {
    fn drop(&mut self) {
        self.retire();
    }
}
struct UserFuture<'a, 'c, T> {
    future: Option<BoxedFuture<'a, T>>,
    cleanup: &'c Cleanup,
}
impl<T> Future for UserFuture<'_, '_, T> {
    type Output = Result<T, ()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = self.cleanup.call(|| {
            self.future
                .as_mut()
                .expect("live ingress future")
                .as_mut()
                .poll(cx)
        });
        if matches!(result, Ok(Poll::Pending)) {
            return Poll::Pending;
        }
        self.cleanup.dispose(self.future.take());
        Poll::Ready(match result {
            Ok(Poll::Ready(value)) => Ok(value),
            Err(()) => Err(()),
            Ok(Poll::Pending) => unreachable!(),
        })
    }
}
impl<T> Drop for UserFuture<'_, '_, T> {
    fn drop(&mut self) {
        self.cleanup.dispose(self.future.take());
    }
}
pub(crate) async fn run<'a, T>(
    cleanup: &Cleanup,
    construct: impl FnOnce() -> BoxedFuture<'a, T>,
) -> Result<T, ()> {
    let future = cleanup.call(construct)?;
    UserFuture {
        future: Some(future),
        cleanup,
    }
    .await
}

/// Observe cleanup after all younger user owners have retired, including cancellation.
pub(crate) struct CloseOnFailure<'a> {
    armed: bool,
    cleanup: &'a Cleanup,
    connection: &'a crate::transport::FrameSender,
}
impl Cleanup {
    pub(crate) fn close_on_failure<'a>(
        &'a self,
        connection: &'a crate::transport::FrameSender,
    ) -> CloseOnFailure<'a> {
        CloseOnFailure {
            armed: true,
            cleanup: self,
            connection,
        }
    }
}
impl Drop for CloseOnFailure<'_> {
    fn drop(&mut self) {
        if self.armed && self.cleanup.failed() {
            self.connection.close();
        }
    }
}

impl CloseOnFailure<'_> {
    /// A truthful terminal response replaces cancellation's connection closure.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}
