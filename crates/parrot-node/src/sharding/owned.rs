//! Keep user values and delegate futures outside borrowed unwind boundaries.
use parrot_api::{errors::ActorError, types::ActorResult};

pub(super) fn call<T>(callback: impl FnOnce() -> T) -> ActorResult<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)).map_err(|payload| {
        let text = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        let mut message: String = text.chars().take(1024).collect();
        if let Err(nested) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        {
            message.push_str("; panic payload destructor panicked");
            std::mem::forget(nested);
        }
        ActorError::Panic(message)
    })
}

pub(crate) struct Owned<T>(Option<T>);
impl<T> Owned<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Some(value))
    }
    pub(super) fn get(&self) -> &T {
        self.0.as_ref().expect("owned shard input")
    }
    pub(super) fn take(&mut self) -> T {
        self.0.take().expect("owned shard input")
    }
    pub(super) fn finish<R>(mut self, result: ActorResult<R>) -> ActorResult<R> {
        let cleanup = self.retire();
        match result {
            Err(error) => Err(error),
            Ok(value) => match cleanup {
                Ok(()) => Ok(value),
                Err(error) => {
                    drop(Owned::new(value));
                    Err(error)
                }
            },
        }
    }
    pub(super) fn retire(&mut self) -> ActorResult<()> {
        match self.0.take() {
            Some(value) => call(|| drop(value)),
            None => Ok(()),
        }
    }
}
impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

pub(super) fn resolve<'a, T: Send + 'a>(
    future: parrot_api::types::BoxedFuture<'a, ActorResult<T>>,
) -> impl std::future::Future<Output = ActorResult<T>> + Send + 'a {
    let mut future = Owned::new(future);
    async move {
        let result = std::future::poll_fn(|cx| {
            match call(|| future.0.as_mut().unwrap().as_mut().poll(cx)) {
                Ok(poll) => poll,
                Err(error) => std::task::Poll::Ready(Err(error)),
            }
        })
        .await;
        future.finish(result)
    }
}
