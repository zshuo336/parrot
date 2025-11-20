//! Retire each callback future and result before publishing its outcome.
use super::reference::call_catching;
use parrot_api::types::{ActorResult, BoxedFuture};
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};

#[derive(Default)]
pub(super) struct Cleanup(AtomicBool);
impl Cleanup {
    pub(super) fn report_to<'a>(&'a self, parent: Option<&'a Self>) -> FailureReport<'a> {
        FailureReport {
            cleanup: self,
            parent,
        }
    }
    pub(super) fn failed(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    pub(super) fn call<T>(&self, operation: impl FnOnce() -> T) -> Result<T, ()> {
        call_catching(operation).map_err(|()| self.0.store(true, Ordering::Relaxed))
    }
    fn dispose<T>(&self, value: T) {
        let _ = self.call(|| drop(value));
    }
    pub(super) fn owned<T>(&self, value: T) -> Owned<'_, T> {
        Owned {
            value: Some(value),
            cleanup: self,
        }
    }
}
// Report after the nested future has retired, including cancellation cleanup.
pub(super) struct FailureReport<'a> {
    cleanup: &'a Cleanup,
    parent: Option<&'a Cleanup>,
}
impl Drop for FailureReport<'_> {
    fn drop(&mut self) {
        if self.cleanup.failed() {
            if let Some(parent) = self.parent {
                parent.0.store(true, Ordering::Relaxed);
            }
        }
    }
}
pub(super) struct Owned<'a, T> {
    value: Option<T>,
    cleanup: &'a Cleanup,
}
impl<T> Owned<'_, T> {
    pub(super) fn value(&self) -> &T {
        self.value.as_ref().expect("owned Node callback result")
    }
    pub(super) fn retire(&mut self) {
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
                .expect("owned Node callback future")
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

// A successful value is transferred even when future disposal failed. A factory
// must adopt that value's actor reference before deciding which actors to roll back.
pub(super) async fn run<'a, T>(
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

pub(super) async fn discard<'a, T>(
    construct: impl FnOnce() -> BoxedFuture<'a, ActorResult<T>>,
    normalize: impl FnOnce(&ActorResult<T>) -> Result<(), String>,
    panic_detail: &'static str,
) -> Result<(), String> {
    let cleanup = Cleanup::default();
    let value = run(&cleanup, construct)
        .await
        .map_err(|()| panic_detail.to_owned())?;
    let mut value = cleanup.owned(value);
    let outcome = cleanup.call(|| normalize(value.value()));
    value.retire();
    if cleanup.failed() {
        let mut detail = match outcome {
            Ok(Err(detail)) => detail,
            _ => String::new(),
        };
        if !detail.is_empty() {
            detail.push_str("; ");
        }
        detail.push_str(panic_detail);
        Err(detail)
    } else {
        outcome.expect("successful Node callback normalization")
    }
}
