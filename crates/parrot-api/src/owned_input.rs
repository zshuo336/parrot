//! Direct API inputs remain owned outside borrowed user callback unwind scopes.
use crate::{errors::ActorError, types::ActorResult};

pub(crate) fn call<T>(callback: impl FnOnce() -> T) -> ActorResult<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)).map_err(|payload| {
        let text = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        let mut message: String = text.chars().take(1024).collect();
        if message.len() < text.len() {
            message.push('…');
        }
        if let Err(secondary) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        {
            message.push_str("; panic payload destructor panicked");
            std::mem::forget(secondary);
        }
        ActorError::Panic(message)
    })
}

pub(crate) struct Input<T>(Option<T>);
impl<T> Input<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Some(value))
    }
    pub(crate) fn value(&self) -> &T {
        self.0.as_ref().expect("unconsumed API input")
    }
    pub(crate) fn value_mut(&mut self) -> &mut T {
        self.0.as_mut().expect("unconsumed API input")
    }
    pub(crate) fn take(&mut self) -> T {
        self.0.take().expect("unconsumed API input")
    }
    pub(crate) fn retire(&mut self) -> ActorResult<()> {
        match self.0.take() {
            Some(value) => call(|| drop(value)),
            None => Ok(()),
        }
    }
    pub(crate) fn finish<R>(mut self, result: ActorResult<R>) -> ActorResult<R> {
        let cleanup = self.retire();
        match result {
            // A secondary cleanup error cannot replace a metadata/admission error.
            Err(error) => Err(error),
            Ok(value) => match cleanup {
                Ok(()) => Ok(value),
                Err(error) => {
                    // A failed final input retirement must not publish success.
                    // The suppressed output gets its own independent Drop boundary.
                    drop(Input::new(value));
                    Err(error)
                }
            },
        }
    }
}
impl<T> Drop for Input<T> {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

/// Poll and retire a delegate's existing allocation at independent unwind
/// boundaries. Capture ownership before returning so an unpolled cancellation
/// also retires it safely. The inner future stays pinned throughout its lifetime.
pub(crate) fn resolve<'a, T: 'a>(
    future: crate::types::BoxedFuture<'a, ActorResult<T>>,
) -> impl std::future::Future<Output = ActorResult<T>> + Send + 'a {
    let mut future = Input::new(future);
    async move {
        let result =
            std::future::poll_fn(|cx| match call(|| future.value_mut().as_mut().poll(cx)) {
                Ok(poll) => poll,
                Err(error) => std::task::Poll::Ready(Err(error)),
            })
            .await;
        // Preserve a transport/poll error if retirement also fails; suppress and
        // independently retire a successful reply when the delegate cannot retire.
        future.finish(result)
    }
}
