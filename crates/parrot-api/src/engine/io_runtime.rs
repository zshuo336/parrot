//! Optional, engine-owned execution for Send I/O futures.
use crate::{errors::ActorError, types::{ActorResult, BoxedFuture, BoxedMessage}};
use std::future::Future;

/// A separate I/O executor owned and retired by an actor engine.
/// Admission begins when the returned future is polled. Cancelling that future
/// cancels its work; engine shutdown cancels admitted I/O and joins its cleanup
/// and workers. Polling and disposal must cooperate: blocking user code cannot
/// be forcibly terminated. Unaccepted inputs are still retired on cancellation.
pub trait AsyncIoRuntime: Send + Sync {
    fn run_io(&self, future: BoxedFuture<'static, ActorResult<BoxedMessage>>)
        -> BoxedFuture<'static, ActorResult<BoxedMessage>>;
}

/// Typed convenience over the object-safe I/O capability.
pub trait AsyncIoRuntimeExt: AsyncIoRuntime {
    fn run_io_typed<F, T>(&self, future: F) -> BoxedFuture<'static, ActorResult<T>>
    where
        F: Future<Output = ActorResult<T>> + Send + 'static,
        T: Send + 'static,
    {
        let result = self.run_io(Box::pin(async move {
            future.await.map(|value| Box::new(value) as BoxedMessage)
        }));
        Box::pin(async move {
            result.await?.downcast::<T>().map(|value| *value).map_err(|_| {
                ActorError::InternalError("I/O executor returned a different output type".into())
            })
        })
    }
}
impl<R: AsyncIoRuntime + ?Sized> AsyncIoRuntimeExt for R {}
