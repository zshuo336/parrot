//! Unpublished instance inputs are retired outside caller and registry locks.
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedFuture},
};
use std::sync::Arc;
use tokio::sync::{Semaphore, watch};

type Receipt = watch::Receiver<Option<Result<(), String>>>;

/// Input ownership inside an already bounded, synchronous publication owner.
/// Do not acquire another executor permit here: factory/child publication may
/// already hold the last one. Retire each input independently after releasing
/// registry locks, and return its actual disposal result.
pub(crate) struct PublicationInput<T>(Option<T>);
impl<T> PublicationInput<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Some(value))
    }
    pub(crate) fn value(&self) -> &T {
        self.0.as_ref().expect("unconsumed publication input")
    }
    pub(crate) fn value_mut(&mut self) -> &mut T {
        self.0.as_mut().expect("unconsumed publication input")
    }
    pub(crate) fn take(&mut self) -> T {
        self.0.take().expect("unconsumed publication input")
    }
    pub(crate) fn retire(mut self) -> ActorResult<()> {
        self.dispose()
    }
    fn dispose(&mut self) -> ActorResult<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.0.take()))).map_err(
            |payload| {
                ActorError::InternalError(format!(
                    "unpublished actor input disposal failed: {}",
                    panic_message(payload)
                ))
            },
        )
    }
}
impl<T> Drop for PublicationInput<T> {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}

/// Never run arbitrary formatting code, or let a panic payload's destructor
/// unwind a resource owner. Recursive destructor panics leak the final payload.
pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let text = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    let mut message: String = text.chars().take(1024).collect();
    if message.len() < text.len() {
        message.push('…');
    }
    if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
    {
        message.push_str("; panic payload destructor panicked");
        std::mem::forget(secondary);
    }
    message
}

pub(crate) fn join_error(error: tokio::task::JoinError) -> String {
    if error.is_panic() {
        panic_message(error.into_panic())
    } else {
        error.to_string()
    }
}

// Installed before dispatch: cancellation of an unpolled cleanup task must also
// retire its value without unwinding through another capture's destructor.
struct Disposal<T> {
    context: Option<parrot_api::trace::TraceContext>,
    value: Option<T>,
    finished: watch::Sender<Option<Result<(), String>>>,
}
impl<T> Drop for Disposal<T> {
    fn drop(&mut self) {
        parrot_api::trace::in_scope(self.context.clone(), || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.value.take())))
                    .map_err(|payload| {
                        format!(
                            "unpublished actor input disposal failed: {}",
                            panic_message(payload)
                        )
                    });
            self.finished.send_replace(Some(result));
        });
    }
}

pub(crate) struct OwnedPayload<T: Send + 'static> {
    // Ownership, not a later executor or cancellation caller, identifies cleanup.
    context: Option<parrot_api::trace::TraceContext>,
    value: Option<T>,
    runtime: Option<tokio::runtime::Handle>,
    slots: Arc<Semaphore>,
    // Inputs transferred without an observer need no completion allocation.
    finished: std::sync::OnceLock<watch::Sender<Option<Result<(), String>>>>,
}
impl<T: Send + 'static> OwnedPayload<T> {
    pub(crate) fn new(value: T, runtime: &tokio::runtime::Handle, slots: &Arc<Semaphore>) -> Self {
        Self {
            context: parrot_api::trace::TraceContext::current(),
            value: Some(value),
            runtime: Some(runtime.clone()),
            slots: slots.clone(),
            finished: std::sync::OnceLock::new(),
        }
    }
    /// Unbound contexts have no system cleanup executor. Use the caller's
    /// executor when present, with a shared bounded pool. Without an executor,
    /// disposal is synchronous but still isolates each input's destructor.
    pub(crate) fn unbound(value: T) -> Self {
        static SLOTS: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
        Self {
            context: parrot_api::trace::TraceContext::current(),
            value: Some(value),
            runtime: tokio::runtime::Handle::try_current().ok(),
            slots: SLOTS.get_or_init(|| Arc::new(Semaphore::new(1))).clone(),
            finished: std::sync::OnceLock::new(),
        }
    }
    pub(crate) fn value(&self) -> &T {
        self.value.as_ref().expect("unconsumed payload")
    }
    pub(crate) fn take(mut self) -> T {
        let value = self.value.take().expect("unconsumed payload");
        if let Some(finished) = self.finished.get() {
            finished.send_replace(Some(Ok(())));
        }
        value
    }
    pub(crate) fn receipt(&self) -> BoxedFuture<'static, ActorResult<()>> {
        wait(self.completion().subscribe())
    }
    fn completion(&self) -> &watch::Sender<Option<Result<(), String>>> {
        self.finished.get_or_init(|| watch::channel(None).0)
    }
    pub(crate) fn retire(self) -> BoxedFuture<'static, ActorResult<()>> {
        if !std::mem::needs_drop::<T>() {
            // Drop completes synchronously and notifies any existing observers.
            // There is no disposal work that needs a new completion channel.
            drop(self);
            return Box::pin(async { Ok(()) });
        }
        let receipt = self.receipt();
        // Start before returning the waiter; cancelling it cannot cancel disposal.
        drop(self);
        receipt
    }
}
impl<T: Send + 'static> Drop for OwnedPayload<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else {
            return;
        };
        if !std::mem::needs_drop::<T>() {
            drop(value);
            if let Some(finished) = self.finished.get() {
                finished.send_replace(Some(Ok(())));
            }
            return;
        }
        let slots = self.slots.clone();
        let finished = self.completion().clone();
        let disposal = Disposal {
            context: self.context.clone(),
            value: Some(value),
            finished: finished.clone(),
        };
        let Some(runtime) = &self.runtime else {
            drop(disposal);
            return;
        };
        runtime.spawn(async move {
            // These private pools remain open through system retirement.
            let permit = slots.acquire_owned().await;
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                drop(disposal);
            })
            .await
            .map_err(join_error);
            if let Err(error) = result {
                // Disposal normally publishes the more specific payload result.
                if finished.borrow().is_none() {
                    finished.send_replace(Some(Err(error)));
                }
            }
        });
    }
}
fn wait(mut receipt: Receipt) -> BoxedFuture<'static, ActorResult<()>> {
    Box::pin(async move {
        loop {
            if let Some(result) = receipt.borrow_and_update().clone() {
                return result.map_err(ActorError::InternalError);
            }
            receipt.changed().await.map_err(|_| {
                ActorError::InternalError(
                    "unpublished input cleanup owner ended without a receipt".into(),
                )
            })?;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Hostile(Arc<AtomicUsize>);
    impl Drop for Hostile {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("unpolled disposal failure");
        }
    }

    #[test]
    fn unpolled_cleanup_retains_failure_receipt_when_runtime_shuts_down() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::new(
            Hostile(count.clone()),
            runtime.handle(),
            &Arc::new(Semaphore::new(1)),
        );
        let receipt = value.retire();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        // No block_on: the cleanup future has never been polled.
        drop(runtime);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let error = futures::executor::block_on(receipt).unwrap_err();
        assert!(
            error.to_string().contains("unpolled disposal failure"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn cancelled_retirement_waiter_does_not_cancel_input_destruction() {
        let count = Arc::new(AtomicUsize::new(0));
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let value = OwnedPayload::new(
            Hostile(count.clone()),
            &tokio::runtime::Handle::current(),
            &slots,
        );
        let receipt = value.receipt();
        drop(value.retire());
        tokio::task::yield_now().await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(permit);
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), receipt)
            .await
            .unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unpolled disposal failure")
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod receipt_contracts {
    use super::*;
    use std::{
        cell::Cell,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Hostile(Arc<AtomicUsize>);
    impl Drop for Hostile {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("receipt contract disposal failed");
        }
    }
    fn ready(mut receipt: BoxedFuture<'static, ActorResult<()>>) -> ActorResult<()> {
        match receipt
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                panic!("completed ownership transfer/disposal left an unresolved receipt")
            }
        }
    }
    #[tokio::test]
    async fn take_resolves_polled_and_unpolled_receipts_without_disposal() {
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::new(
            Counted(count.clone()),
            &tokio::runtime::Handle::current(),
            &Arc::new(Semaphore::new(1)),
        );
        let mut first = value.receipt();
        let second = value.receipt();
        let abandoned = value.receipt();
        assert!(futures::poll!(&mut first).is_pending());
        drop(abandoned);
        let transferred = value.take();
        ready(first).unwrap();
        ready(second).unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "take transfers ownership, not disposal"
        );
        drop(transferred);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn simultaneous_receipts_share_one_terminal_result() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let value = OwnedPayload::new(42u64, runtime.handle(), &Arc::new(Semaphore::new(1)));
        let barrier = std::sync::Barrier::new(4);
        let receipts = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        value.receipt()
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(value.take(), 42);
        for receipt in receipts {
            ready(receipt).unwrap();
        }
    }
    #[test]
    fn trivial_drop_resolves_receipts_even_after_executor_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let value = OwnedPayload::new(42u64, &handle, &Arc::new(Semaphore::new(1)));
        let receipt = value.receipt();
        drop(value);
        ready(receipt).unwrap();
    }
    #[test]
    fn closed_executor_still_disposes_and_reports_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::new(
            Hostile(count.clone()),
            &handle,
            &Arc::new(Semaphore::new(1)),
        );
        let first = value.receipt();
        let second = value.retire();
        for receipt in [first, second] {
            assert!(
                ready(receipt)
                    .unwrap_err()
                    .to_string()
                    .contains("receipt contract disposal failed")
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn unbound_without_runtime_preserves_disposal_failure() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::unbound(Hostile(count.clone()));
        let first = value.receipt();
        let second = value.retire();
        for receipt in [first, second] {
            assert!(
                ready(receipt)
                    .unwrap_err()
                    .to_string()
                    .contains("receipt contract disposal failed")
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn unobserved_drop_still_disposes_exactly_once() {
        let count = Arc::new(AtomicUsize::new(0));
        drop(OwnedPayload::new(
            Counted(count.clone()),
            &tokio::runtime::Handle::current(),
            &Arc::new(Semaphore::new(1)),
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            while count.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unobserved input was not disposed");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    struct Blocking {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        count: Arc<AtomicUsize>,
    }
    impl Drop for Blocking {
        fn drop(&mut self) {
            self.entered.take().unwrap().send(()).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(3))
                .expect("test must release destructor");
            self.count.fetch_add(1, Ordering::SeqCst);
            panic!("blocked disposal failed");
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_waiter_does_not_complete_or_abort_blocked_disposal() {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::new(
            Blocking {
                entered: Some(entered),
                release: blocked,
                count: count.clone(),
            },
            &tokio::runtime::Handle::current(),
            &Arc::new(Semaphore::new(1)),
        );
        let mut observer = value.receipt();
        let abandoned = value.retire();
        tokio::time::timeout(Duration::from_secs(3), started)
            .await
            .unwrap()
            .unwrap();
        drop(abandoned);
        assert!(futures::poll!(&mut observer).is_pending());
        assert_eq!(count.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(3), observer)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("blocked disposal failed"));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn send_only_inputs_still_transfer_across_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<OwnedPayload<Cell<u64>>>();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let value = OwnedPayload::new(
            Cell::new(42u64),
            runtime.handle(),
            &Arc::new(Semaphore::new(1)),
        );
        let receipt = value.receipt();
        assert_eq!(
            std::thread::spawn(move || value.take().get())
                .join()
                .unwrap(),
            42
        );
        ready(receipt).unwrap();
    }
}

#[cfg(test)]
mod trivial_retirement_contracts {
    use super::*;
    use std::{
        cell::Cell,
        task::{Context, Poll, Waker},
    };

    fn completed(mut receipt: BoxedFuture<'static, ActorResult<()>>) {
        match receipt
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending => panic!("synchronous retirement left an observer pending"),
        }
    }

    #[test]
    fn unpolled_retirement_completes_both_existing_observers() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let slots = Arc::new(Semaphore::new(0));
        let input = OwnedPayload::new(42u64, runtime.handle(), &slots);
        let mut polled = input.receipt();
        let unpolled = input.receipt();
        assert!(
            polled
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let returned = input.retire();
        // No executor progress and no cleanup permits are available.
        completed(polled);
        completed(unpolled);
        completed(returned);
        assert_eq!(slots.available_permits(), 0);
    }

    #[test]
    fn cancelled_retirement_after_executor_shutdown_completes_observers() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let input = OwnedPayload::new([7u8; 64], &handle, &Arc::new(Semaphore::new(0)));
        let first = input.receipt();
        let second = input.receipt();
        drop(input.retire());
        completed(first);
        completed(second);
    }

    #[test]
    fn send_only_input_retirement_notifies_origin_thread_before_waiter_poll() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let input = OwnedPayload::new(
            Cell::new(42u64),
            runtime.handle(),
            &Arc::new(Semaphore::new(0)),
        );
        let observer = input.receipt();
        let returned = std::thread::spawn(move || input.retire()).join().unwrap();
        completed(observer);
        completed(returned);
    }

    #[test]
    fn unbound_trivial_retirement_completes_without_executor() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let input = OwnedPayload::unbound(42u64);
        let observer = input.receipt();
        let returned = input.retire();
        completed(observer);
        completed(returned);
    }
}

#[cfg(test)]
mod trace_disposal_tests {
    use super::*;
    use parrot_api::trace::{self, TraceContext};
    use std::sync::Mutex;
    struct Probe(Arc<Mutex<Vec<Option<TraceContext>>>>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(TraceContext::current());
        }
    }
    fn check(mode: u8) {
        for traced in [true, false] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let events = Arc::new(Mutex::new(Vec::new()));
            let parent = traced.then(TraceContext::root);
            let foreign = TraceContext::root();
            let payload = trace::in_scope(parent.clone(), || {
                if mode == 2 {
                    OwnedPayload::unbound(Probe(events.clone()))
                } else {
                    OwnedPayload::new(
                        Probe(events.clone()),
                        runtime.handle(),
                        &Arc::new(Semaphore::new(1)),
                    )
                }
            });
            let receipt = foreign.in_scope(|| payload.retire());
            if mode == 0 {
                runtime.block_on(receipt).unwrap();
            } else {
                foreign.in_scope(|| drop(runtime));
                futures::executor::block_on(receipt).unwrap();
            }
            assert_eq!(*events.lock().unwrap(), vec![parent]);
            assert!(TraceContext::current().is_none());
        }
    }
    #[test]
    fn background_disposal_keeps_construction_context() {
        check(0);
    }
    #[test]
    fn unpolled_runtime_disposal_keeps_construction_context() {
        check(1);
    }
    #[test]
    fn synchronous_disposal_keeps_construction_context() {
        check(2);
    }
}
