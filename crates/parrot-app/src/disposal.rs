//! Internal unwind boundaries for a single owned user value, never a container.
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Keep future ownership outside separate poll and disposal unwind boundaries.
/// Cancellation has no result channel, so Drop logs disposal failure instead.
struct CatchFuture<F> {
    inner: Option<Pin<Box<F>>>,
}

pub(crate) enum FutureCompletion<T> {
    Ready(T),
    DisposalFailed(T),
    Panicked,
}

impl<F> CatchFuture<F> {
    fn retire(&mut self) -> bool {
        self.inner.take().is_none_or(drop_catching)
    }
}

impl<F: Future> Future for CatchFuture<F> {
    type Output = FutureCompletion<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            this.inner
                .as_mut()
                .expect("catching future polled after completion")
                .as_mut()
                .poll(cx)
        }));
        match result {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => {
                if this.retire() {
                    Poll::Ready(FutureCompletion::Ready(output))
                } else {
                    Poll::Ready(FutureCompletion::DisposalFailed(output))
                }
            }
            Err(payload) => {
                drop_catching(payload);
                this.retire();
                Poll::Ready(FutureCompletion::Panicked)
            }
        }
    }
}

impl<F> Drop for CatchFuture<F> {
    fn drop(&mut self) {
        if !self.retire() {
            tracing::warn!("application future disposal panicked during cancellation");
        }
    }
}

pub(crate) fn catch_future<F: Future>(future: F) -> impl Future<Output = Result<F::Output, ()>> {
    let future = catch_future_retaining_output(future);
    async move {
        match future.await {
            FutureCompletion::Ready(output) => Ok(output),
            FutureCompletion::DisposalFailed(output) => {
                drop_catching(output);
                Err(())
            }
            FutureCompletion::Panicked => Err(()),
        }
    }
}

/// A successful poll can transfer resource ownership before future disposal
/// fails. Keep that output available to its caller for rollback, never discard
/// an owning collection inside a single destructor unwind boundary.
pub(crate) fn catch_future_retaining_output<F: Future>(
    future: F,
) -> impl Future<Output = FutureCompletion<F::Output>> {
    CatchFuture {
        inner: Some(Box::pin(future)),
    }
}
pub(crate) fn drop_catching<T>(value: T) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value))) {
        Ok(()) => true,
        Err(payload) => {
            // A panic_any payload can itself panic during destruction. Bound
            // recursion by leaking only the second throwing payload. This does
            // not recover panic=abort or a double panic inside one value.
            if let Err(nested) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                std::mem::forget(nested);
            }
            false
        }
    }
}

// Keep ownership outside the borrowed formatting boundary: a formatting panic
// must not unwind through an error whose destructor can independently panic.
pub(crate) fn actor_error_detail(error: parrot_api::errors::ActorError) -> String {
    let mut detail =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| error.to_string())) {
            Ok(detail) => detail,
            Err(payload) => {
                drop_catching(payload);
                "actor error formatting panicked".into()
            }
        };
    if !drop_catching(error) {
        detail.push_str("; actor error disposal panicked");
    }
    detail
}

#[cfg(test)]
mod future_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[derive(Default)]
    struct State {
        address: AtomicUsize,
        polls: AtomicUsize,
        futures: AtomicUsize,
        outputs: AtomicUsize,
    }
    struct Output(Arc<State>, bool);
    impl Drop for Output {
        fn drop(&mut self) {
            self.0.outputs.fetch_add(1, Ordering::SeqCst);
            assert!(!self.1, "output disposal panic");
        }
    }
    struct Delegate {
        state: Arc<State>,
        pending: bool,
        poll_panics: bool,
        drop_panics: bool,
        output_panics: bool,
        _pinned: std::marker::PhantomPinned,
    }
    impl Future for Delegate {
        type Output = Output;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Output> {
            let this = self.as_ref().get_ref();
            let address = this as *const Self as usize;
            let previous = this.state.address.swap(address, Ordering::SeqCst);
            assert!(
                previous == 0 || previous == address,
                "delegate moved after pinning"
            );
            let count = this.state.polls.fetch_add(1, Ordering::SeqCst);
            assert!(!this.poll_panics, "delegate poll panic");
            if this.pending || count == 0 {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(Output(this.state.clone(), this.output_panics))
            }
        }
    }
    impl Drop for Delegate {
        fn drop(&mut self) {
            self.state.futures.fetch_add(1, Ordering::SeqCst);
            assert!(!self.drop_panics, "delegate disposal panic");
        }
    }
    fn delegate(state: &Arc<State>) -> Delegate {
        Delegate {
            state: state.clone(),
            pending: false,
            poll_panics: false,
            drop_panics: false,
            output_panics: false,
            _pinned: std::marker::PhantomPinned,
        }
    }
    #[tokio::test]
    async fn normal_completion_preserves_pinning_and_releases_delegate_once() {
        let state = Arc::new(State::default());
        let output = catch_future(delegate(&state)).await.ok().unwrap();
        assert_eq!(state.polls.load(Ordering::SeqCst), 2);
        assert_eq!(state.futures.load(Ordering::SeqCst), 1);
        assert_eq!(state.outputs.load(Ordering::SeqCst), 0);
        drop(output);
        assert_eq!(state.outputs.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn completion_disposal_failure_suppresses_and_independently_drops_output() {
        let state = Arc::new(State::default());
        let mut future = delegate(&state);
        future.drop_panics = true;
        future.output_panics = true;
        assert!(catch_future(future).await.is_err());
        assert_eq!(state.futures.load(Ordering::SeqCst), 1);
        assert_eq!(state.outputs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retained_completion_transfers_output_despite_future_disposal_failure() {
        let state = Arc::new(State::default());
        let mut future = delegate(&state);
        future.drop_panics = true;
        future.output_panics = true;
        let output = match catch_future_retaining_output(future).await {
            FutureCompletion::DisposalFailed(output) => output,
            _ => panic!("completed ownership must survive a failed future destructor"),
        };
        assert_eq!(state.futures.load(Ordering::SeqCst), 1);
        assert_eq!(state.outputs.load(Ordering::SeqCst), 0);
        assert!(!drop_catching(output));
        assert_eq!(state.outputs.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn poll_and_disposal_failures_have_independent_unwind_boundaries() {
        let state = Arc::new(State::default());
        let mut future = delegate(&state);
        future.poll_panics = true;
        future.drop_panics = true;
        assert!(catch_future(future).await.is_err());
        assert_eq!(state.futures.load(Ordering::SeqCst), 1);
        assert_eq!(state.outputs.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn unpolled_cancellation_retires_delegate_without_unwinding() {
        let state = Arc::new(State::default());
        let mut future = delegate(&state);
        future.drop_panics = true;
        drop(catch_future(future));
        assert_eq!(state.polls.load(Ordering::SeqCst), 0);
        assert_eq!(state.futures.load(Ordering::SeqCst), 1);
    }
}
