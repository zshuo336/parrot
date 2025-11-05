//! Ownership of a context's direct call into an arbitrary actor reference.
//!
//! Each framework-owned value is retired independently. The delegate owns the
//! input after invocation; panics entirely inside arbitrary delegate code
//! (including its own double-panic destructors) cannot be repaired here.
use crate::{
    context_tasks::ContextTasks,
    owned_payload::{OwnedPayload, panic_message},
};
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

fn failure(stage: &'static str, payload: Box<dyn std::any::Any + Send>) -> ActorError {
    ActorError::Panic(format!(
        "context message {stage}: {}",
        panic_message(payload)
    ))
}

// Polling and destruction are separate user-code boundaries. In particular,
// catch_unwind around an async block that owns both values is not sufficient:
// a poll panic can otherwise unwind through a second, panicking destructor.
struct Value<'a, T> {
    failures: &'a ContextTasks,
    value: Option<T>,
    stage: &'static str,
}
impl<'a, T> Value<'a, T> {
    fn new(value: T, stage: &'static str, failures: &'a ContextTasks) -> Self {
        Self {
            failures,
            value: Some(value),
            stage,
        }
    }
    fn get(&self) -> &T {
        self.value.as_ref().expect("live context value")
    }
    fn take(&mut self) -> T {
        self.value.take().expect("live context value")
    }
    fn dispose(&mut self) -> ActorResult<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.value.take()))).map_err(
            |payload| {
                let error = failure(self.stage, payload);
                if let ActorError::Panic(message) = &error {
                    self.failures.record_message_disposal_failure(message);
                }
                error
            },
        )
    }
}
impl<T> Drop for Value<'_, T> {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}

fn finish<T>(
    result: ActorResult<T>,
    failures: &ContextTasks,
    disposal: impl FnOnce() -> ActorResult<()>,
) -> ActorResult<T> {
    // A successful reply may itself have a hostile destructor. Protect it
    // before running cleanup, and preserve the original error without formatting
    // or replacing its source. Only a previously successful result is overridden.
    let mut result = Value::new(result, "result disposal", failures);
    match disposal() {
        Err(error) if result.get().is_ok() => {
            let _ = result.dispose();
            Err(error)
        }
        _ => result.take(),
    }
}

struct Delegate<'a, 'c, T> {
    future: Value<'c, BoxedFuture<'a, ActorResult<T>>>,
}
impl<T> Future for Delegate<'_, '_, T> {
    type Output = ActorResult<T>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future
                .value
                .as_mut()
                .expect("live context delegate")
                .as_mut()
                .poll(cx)
        }));
        let result = match outcome {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result,
            Err(payload) => Err(failure("poll", payload)),
        };
        Poll::Ready(finish(result, self.future.failures, || {
            self.future.dispose()
        }))
    }
}

pub(crate) fn call<T: Send + 'static, I: Send + 'static>(
    target: OwnedPayload<BoxedActorRef>,
    message: OwnedPayload<I>,
    failures: &ContextTasks,
    invoke: impl for<'a> FnOnce(&'a dyn ActorRef, I) -> BoxedFuture<'a, ActorResult<T>> + Send,
) -> impl Future<Output = ActorResult<T>> + Send {
    parrot_api::trace::capture(async move {
        // Before first poll the existing inputs still own asynchronous retirement.
        // Once invoked, retain the target on this execution thread until its borrowed
        // delegate is fully disposed. Cancellation drops the delegate before target.
        let mut target = Value::new(target.take(), "target disposal", failures);
        let result = {
            let constructed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                invoke(target.get().as_ref(), message.take())
            }));
            match constructed {
                Ok(future) => {
                    Delegate {
                        future: Value::new(future, "future disposal", failures),
                    }
                    .await
                }
                Err(payload) => Err(failure("construction", payload)),
            }
        };
        finish(result, failures, || target.dispose())
    })
}
