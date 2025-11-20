//! A staged output releases construction capacity while retaining its invocation.
use super::{ConstructionControl, ConstructionError, Factory, Invocation, panic_message};
use parrot_api::types::BoxedFuture;
use std::sync::Arc;
use tokio::sync::{Semaphore, watch};

const MAX_LIFECYCLE_DISCARD_WAITERS: usize = 1024;

fn lifecycle_discard_waiters() -> Arc<Semaphore> {
    static WAITERS: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    WAITERS
        .get_or_init(|| Arc::new(Semaphore::new(MAX_LIFECYCLE_DISCARD_WAITERS)))
        .clone()
}

// Protect cancellation before the cleanup/publication worker is ever polled,
// including submission to a runtime that has already shut down. The output and
// its final factory capture must never unwind through each other's destructors.
struct OutputLease<A> {
    context: Option<parrot_api::trace::TraceContext>,
    actor: Option<A>,
    invocation: Option<Invocation<A>>,
}
impl<A> OutputLease<A> {
    fn new(
        actor: A,
        invocation: Invocation<A>,
        context: Option<parrot_api::trace::TraceContext>,
    ) -> Self {
        Self {
            context,
            actor: Some(actor),
            invocation: Some(invocation),
        }
    }
    fn take_actor(&mut self) -> A {
        self.actor.take().expect("one prepared transfer")
    }
    fn record_failure(&self, error: &str) {
        if let Some(invocation) = &self.invocation {
            invocation.record_failure(error);
        }
    }
    fn retire(&mut self) -> Result<(), String> {
        let _scope = crate::owned_payload::DisposalScope::enter();
        parrot_api::trace::in_scope(self.context.clone(), || {
            let output = self.actor.take().map_or(Ok(()), |actor| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor))).map_err(
                    |panic| {
                        format!(
                            "prepared output disposal panicked: {}",
                            panic_message(panic)
                        )
                    },
                )
            });
            if let Err(error) = &output {
                self.record_failure(error);
            }
            // Store the failure before releasing the active invocation. If no
            // factory owner survives, this may also dispose its final capture.
            let capture = self.invocation.take().map_or(Ok(()), |mut invocation| {
                super::retirement::invocation_finished(&mut invocation)
            });
            output.and(capture)
        })
    }
}
impl<A> Drop for OutputLease<A> {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}
struct Discard<A> {
    output: OutputLease<A>,
    finished: watch::Sender<Option<Result<(), String>>>,
    _pending: Option<crate::owned_payload::PendingCleanupPermit>,
}
impl<A> Drop for Discard<A> {
    fn drop(&mut self) {
        self.finished.send_replace(Some(self.output.retire()));
    }
}

pub(crate) struct Prepared<A: Send + 'static> {
    context: Option<parrot_api::trace::TraceContext>,
    parts: Option<(A, Invocation<A>)>,
    control: Arc<ConstructionControl>,
    workers: Arc<Semaphore>,
    cleanup: Arc<Semaphore>,
    runtime: tokio::runtime::Handle,
}
impl<A: Send + 'static> Prepared<A> {
    /// Move a prepared value synchronously into a lifecycle owner. The caller
    /// must immediately store it in an independently retired input slot, with
    /// no await or user callback between transfer and storage.
    pub(crate) fn adopt(mut self) -> A {
        let (actor, invocation) = self.parts.take().expect("one prepared adoption");
        drop(invocation);
        actor
    }

    fn start_discard(&mut self) -> BoxedFuture<'static, Result<(), String>> {
        let Some((actor, invocation)) = self.parts.take() else {
            return Box::pin(async { Ok(()) });
        };
        let cleanup = self.cleanup.clone();
        let (finished, receipt) = watch::channel(None);
        let mut owner = Discard {
            output: OutputLease::new(actor, invocation, self.context.clone()),
            finished,
            _pending: None,
        };
        if crate::owned_payload::DisposalScope::active() {
            drop(owner);
        } else if let Some(pending) =
            crate::owned_payload::try_pending_cleanup_permit(&self.cleanup)
        {
            owner._pending = Some(pending);
            self.runtime.spawn(async move {
                // Normal retirement remains bounded. If the host cancels this task,
                // the same owner isolates destructors and reports actual disposal.
                let permit = cleanup.acquire_owned().await;
                let _ = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    drop(owner);
                })
                .await;
            });
        } else {
            // Saturation applies backpressure at the owner boundary. The
            // synchronous owner still publishes the actual disposal receipt.
            drop(owner);
        }
        wait_receipt(receipt)
    }

    /// Retire an already-prepared output from a retained lifecycle owner.
    ///
    /// Raw `discard` preserves synchronous backpressure for callers that own
    /// the final value. Cohort recovery is different: its driver must remain
    /// schedulable while pending cleanup capacity is exhausted. Keep the
    /// output in one bounded waiter task until admission, then hand it to the
    /// normal blocking cleanup owner. Dropping the returned receipt cannot
    /// cancel that owner or lose its actual disposal result.
    pub(crate) fn discard_in_lifecycle(mut self) -> BoxedFuture<'static, Result<(), String>> {
        let Some((actor, invocation)) = self.parts.take() else {
            return Box::pin(async { Ok(()) });
        };
        let cleanup = self.cleanup.clone();
        let runtime = self.runtime.clone();
        let (finished, receipt) = watch::channel(None);
        let mut owner = Discard {
            output: OutputLease::new(actor, invocation, self.context.clone()),
            finished,
            _pending: None,
        };
        if crate::owned_payload::DisposalScope::active() {
            drop(owner);
            return wait_receipt(receipt);
        }
        if let Some(pending) = crate::owned_payload::try_pending_cleanup_permit(&cleanup) {
            owner._pending = Some(pending);
            runtime.spawn(async move {
                let permit = cleanup.acquire_owned().await;
                let _ = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    drop(owner);
                })
                .await;
            });
            return wait_receipt(receipt);
        }
        let Ok(waiter) = lifecycle_discard_waiters().try_acquire_owned() else {
            // The bounded lifecycle waiter pool is itself saturated. Preserve
            // the final-owner rule instead of creating an unbounded task list.
            drop(owner);
            return wait_receipt(receipt);
        };
        runtime.spawn(async move {
            let pending = crate::owned_payload::pending_cleanup_permit(&cleanup).await;
            owner._pending = Some(pending);
            let permit = cleanup.acquire_owned().await;
            let _ = tokio::task::spawn_blocking(move || {
                let _waiter = waiter;
                let _permit = permit;
                drop(owner);
            })
            .await;
        });
        wait_receipt(receipt)
    }
    pub(crate) fn discard(mut self) -> BoxedFuture<'static, Result<(), String>> {
        self.start_discard()
    }
    /// Caller retains this operation through cancellation until publication or
    /// discard has actually returned. Publication does not activate a cohort.
    pub(crate) fn publish<T: Send + 'static, E: Send + 'static>(
        mut self,
        publish: impl FnOnce(A) -> Result<T, E> + Send + 'static,
    ) -> impl std::future::Future<Output = Result<T, ConstructionError<E>>> + Send + 'static {
        parrot_api::trace::scope(self.context.clone(), async move {
            let control = self.control.clone();
            let workers = self.workers.clone();
            let permit = tokio::select! {
                biased;
                _ = control.cancelled() => {
                    self.discard().await.map_err(ConstructionError::DisposalPanicked)?;
                    return Err(ConstructionError::Cancelled);
                },
                permit = workers.acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(error) => {
                        self.discard().await.map_err(ConstructionError::DisposalPanicked)?;
                        return Err(ConstructionError::WorkerFailed(error.to_string()));
                    }
                },
            };
            let (actor, invocation) = self.parts.take().expect("one prepared publication");
            let mut output = OutputLease::new(actor, invocation, self.context.clone());
            tokio::task::spawn_blocking(super::scoped_blocking(self.context.clone(), move || {
                let _permit = permit;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if control.is_cancelled() {
                        let _scope = crate::owned_payload::DisposalScope::enter();
                        drop(output.take_actor());
                        Err(ConstructionError::Cancelled)
                    } else {
                        publish(output.take_actor()).map_err(ConstructionError::Publication)
                    }
                }))
                .unwrap_or_else(|panic| {
                    Err(ConstructionError::DisposalPanicked(format!(
                        "prepared publication or output disposal panicked: {}",
                        panic_message(panic)
                    )))
                });
                if let Err(ConstructionError::DisposalPanicked(error)) = &result {
                    output.record_failure(error);
                }
                output
                    .retire()
                    .map_err(ConstructionError::DisposalPanicked)?;
                result
            }))
            .await
            .map_err(|error| {
                ConstructionError::WorkerFailed(format!(
                    "prepared publication worker failed: {}",
                    crate::owned_payload::join_error(error)
                ))
            })?
        })
    }
}

fn wait_receipt(
    mut receipt: watch::Receiver<Option<Result<(), String>>>,
) -> BoxedFuture<'static, Result<(), String>> {
    Box::pin(async move {
        loop {
            if let Some(result) = receipt.borrow_and_update().clone() {
                return result;
            }
            receipt
                .changed()
                .await
                .map_err(|_| "prepared cleanup owner ended without a receipt".to_owned())?;
        }
    })
}
impl<A: Send + 'static> Drop for Prepared<A> {
    fn drop(&mut self) {
        // The invocation remains in the owned cleanup task. Factory::retire
        // cannot finish merely because a group drops its staging handle.
        drop(self.start_discard());
    }
}
impl<A: Send + 'static> Factory<A> {
    pub(crate) fn prepare<E: Send + 'static>(
        &self,
        control: Arc<ConstructionControl>,
    ) -> impl std::future::Future<Output = Result<Prepared<A>, ConstructionError<E>>> + Send + '_
    {
        parrot_api::trace::capture(async move {
            let cleanup_reservation = if crate::owned_payload::DisposalScope::active() {
                self.admission.clone().try_acquire_owned().map_err(|_| {
                    ConstructionError::WorkerFailed(
                        "cleanup capacity unavailable during reentrant preparation".into(),
                    )
                })?
            } else {
                tokio::select! {
                    biased;
                    _ = control.cancelled() => return Err(ConstructionError::Cancelled),
                    permit = self.admission.clone().acquire_owned() => permit
                        .map_err(|error| ConstructionError::WorkerFailed(error.to_string()))?,
                }
            };
            let invocation = {
                let mut state = self.state.lock().unwrap();
                if state.disposal.is_some() {
                    return Err(ConstructionError::Cancelled);
                }
                let function = state.function.clone().ok_or(ConstructionError::Cancelled)?;
                state.active += 1;
                Invocation {
                    function: Some(function),
                    state: self.state.clone(),
                    changed: self.changed.clone(),
                    cleanup: Some(cleanup_reservation),
                }
            };
            let permit = tokio::select! {
                biased;
                _ = control.cancelled() => return Err(ConstructionError::Cancelled),
                permit = self.workers.clone().acquire_owned() => permit.map_err(|error| ConstructionError::WorkerFailed(error.to_string()))?,
            };
            let workers = self.workers.clone();
            let cleanup = self.cleanup.clone();
            let runtime = tokio::runtime::Handle::current();
            let check = control.clone();
            let context = parrot_api::trace::TraceContext::current();
            let prepared =
                tokio::task::spawn_blocking(super::scoped_blocking(context.clone(), move || {
                    let _permit = permit;
                    if control.is_cancelled() {
                        return Err(ConstructionError::Cancelled);
                    }
                    let actor = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        invocation.function.as_ref().expect("live invocation")()
                    }))
                    .map_err(|panic| {
                        ConstructionError::ConstructorPanicked(format!(
                            "actor factory panicked: {}",
                            panic_message(panic)
                        ))
                    })?;
                    Ok(Prepared {
                        context,
                        parts: Some((actor, invocation)),
                        control,
                        workers,
                        cleanup,
                        runtime,
                    })
                }))
                .await
                .map_err(|error| {
                    ConstructionError::WorkerFailed(format!(
                        "factory preparation worker failed: {}",
                        crate::owned_payload::join_error(error)
                    ))
                })??;
            if check.is_cancelled() {
                prepared
                    .discard()
                    .await
                    .map_err(ConstructionError::DisposalPanicked)?;
                Err(ConstructionError::Cancelled)
            } else {
                Ok(prepared)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    #[derive(Default)]
    struct Gate {
        entered: tokio::sync::Notify,
        open: Mutex<bool>,
        changed: Condvar,
    }
    impl Gate {
        fn block(&self) {
            self.entered.notify_one();
            let (_guard, timeout) = self
                .changed
                .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(4), |open| {
                    !*open
                })
                .unwrap();
            assert!(!timeout.timed_out(), "test gate not released");
        }
        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
        async fn entered(&self) {
            tokio::time::timeout(Duration::from_secs(2), self.entered.notified())
                .await
                .unwrap();
        }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Output {
        dropped: Arc<AtomicUsize>,
        gate: Option<Arc<Gate>>,
        panic: bool,
    }
    impl Drop for Output {
        fn drop(&mut self) {
            if let Some(gate) = &self.gate {
                gate.block();
            }
            self.dropped.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "prepared output destructor panic");
        }
    }
    fn factory(
        dropped: &Arc<AtomicUsize>,
        captures: &Arc<AtomicUsize>,
        gate: Option<Arc<Gate>>,
        panic: bool,
    ) -> Arc<Factory<Output>> {
        let dropped = dropped.clone();
        let capture = Capture(captures.clone());
        Factory::new(
            move || {
                let _ = &capture;
                Output {
                    dropped: dropped.clone(),
                    gate: gate.clone(),
                    panic,
                }
            },
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
        )
    }

    #[tokio::test]
    async fn saturated_discard_preserves_output_and_final_capture_receipts() {
        struct FinalCapture {
            drops: Arc<AtomicUsize>,
            panic: bool,
        }
        impl Drop for FinalCapture {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                assert!(!self.panic, "final prepared capture failure");
            }
        }

        for (output_panics, capture_panics) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let drops = Arc::new(AtomicUsize::new(0));
            let captures = Arc::new(AtomicUsize::new(0));
            let cleanup = Arc::new(Semaphore::new(1));
            let capture = FinalCapture {
                drops: captures.clone(),
                panic: capture_panics,
            };
            let observed = drops.clone();
            let factory = Factory::new(
                move || {
                    let _ = &capture;
                    Output {
                        dropped: observed.clone(),
                        gate: None,
                        panic: output_panics,
                    }
                },
                Arc::new(Semaphore::new(1)),
                cleanup.clone(),
            );
            let prepared = factory.prepare::<()>(Arc::default()).await.unwrap();
            drop(factory);
            let pending: Vec<_> = (0..crate::owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT)
                .map(|_| crate::owned_payload::try_pending_cleanup_permit(&cleanup).unwrap())
                .collect();
            assert!(crate::owned_payload::try_pending_cleanup_permit(&cleanup).is_none());

            // Discard starts before its observer is polled, including overflow.
            let receipt = prepared.discard();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(captures.load(Ordering::SeqCst), 1);
            drop(pending);
            let result = receipt.await;
            match (output_panics, capture_panics) {
                (false, false) => result.unwrap(),
                (true, _) => assert!(
                    result
                        .unwrap_err()
                        .contains("prepared output disposal panicked")
                ),
                (false, true) => assert!(
                    result
                        .unwrap_err()
                        .contains("final prepared capture failure")
                ),
            }
        }
    }

    #[tokio::test]
    async fn prepared_cohort_releases_worker_capacity_but_retains_factory_captures() {
        let drops = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let factory = factory(&drops, &captures, None, false);
        let first = factory.prepare::<()>(Arc::default()).await.unwrap();
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            factory.prepare::<()>(Arc::default()),
        )
        .await
        .expect("a staged output must not retain the sole constructor permit")
        .unwrap();
        let retiring = factory.retire();
        tokio::pin!(retiring);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut retiring)
                .await
                .is_err()
        );
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        first.discard().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut retiring)
                .await
                .is_err()
        );
        second.discard().await.unwrap();
        retiring.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn cancelled_discard_waiter_still_owns_output_until_destructor_returns() {
        let drops = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let factory = factory(&drops, &captures, Some(gate.clone()), false);
        let prepared = factory.prepare::<()>(Arc::default()).await.unwrap();
        let discarded = prepared.discard();
        gate.entered().await;
        drop(discarded);
        let retiring = factory.retire();
        tokio::pin!(retiring);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut retiring)
                .await
                .is_err()
        );
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        gate.release();
        retiring.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn discarded_output_panic_remains_a_failed_retirement_without_a_waiter() {
        let drops = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let factory = factory(&drops, &captures, None, true);
        drop(factory.prepare::<()>(Arc::default()).await.unwrap());
        let error = factory.retire().await.unwrap_err();
        assert!(error.contains("prepared output disposal panicked"));
        assert_eq!(factory.retire().await.unwrap_err(), error);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn cancelled_prepared_generation_never_invokes_publication() {
        let drops = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let factory = factory(&drops, &captures, None, false);
        let control = Arc::new(ConstructionControl::default());
        let prepared = factory.prepare::<()>(control.clone()).await.unwrap();
        control.cancel();
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let result = prepared
            .publish(move |output| -> Result<(), ()> {
                called.fetch_add(1, Ordering::SeqCst);
                drop(output);
                Ok(())
            })
            .await;
        assert!(matches!(result, Err(ConstructionError::Cancelled)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        factory.retire().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn prepared_publication_keeps_capture_lease_until_callback_really_returns() {
        let drops = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let factory = factory(&drops, &captures, None, false);
        let first = factory.prepare::<()>(Arc::default()).await.unwrap();
        let second = factory.prepare::<()>(Arc::default()).await.unwrap();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let published = tokio::spawn(async move {
            first
                .publish(move |output| -> Result<(), ()> {
                    blocked.block();
                    drop(output);
                    Ok(())
                })
                .await
        });
        gate.entered().await;
        let retiring = factory.retire();
        tokio::pin!(retiring);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut retiring)
                .await
                .is_err()
        );
        gate.release();
        published.await.unwrap().unwrap();
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        second
            .publish(|output| -> Result<(), ()> {
                drop(output);
                Ok(())
            })
            .await
            .unwrap();
        retiring.await.unwrap();
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn admission_bounds_staged_outputs_and_cancellation_keeps_state_unpublished() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let factory = Factory::new(
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
            },
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
        );

        let first = factory.prepare::<()>(Arc::default()).await.unwrap();
        let second = factory.prepare::<()>(Arc::default()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(factory.admission.available_permits(), 0);
        assert_eq!(factory.state.lock().unwrap().active, 2);

        let control = Arc::new(ConstructionControl::default());
        let mut third = Box::pin(factory.prepare::<()>(control.clone()));
        assert!(futures::poll!(&mut third).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(factory.state.lock().unwrap().active, 2);

        control.cancel();
        assert!(matches!(third.await, Err(ConstructionError::Cancelled)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(factory.state.lock().unwrap().active, 2);

        first.discard().await.unwrap();
        assert_eq!(factory.admission.available_permits(), 1);
        second.discard().await.unwrap();
        assert_eq!(factory.admission.available_permits(), 2);
        assert_eq!(factory.state.lock().unwrap().active, 0);
        factory.retire().await.unwrap();
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    #[test]
    fn prepared_shutdown_isolates_output_and_final_capture_panics() {
        const NAME: &str = "constructor::prepared::shutdown_tests::prepared_shutdown_isolates_output_and_final_capture_panics";
        const CHILD: &str = "PARROT_PREPARED_SHUTDOWN_CHILD";
        if let Ok(mode) = std::env::var(CHILD) {
            struct Panicking(Arc<AtomicUsize>);
            impl Drop for Panicking {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    panic!("prepared shutdown input destructor");
                }
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let outputs = Arc::new(AtomicUsize::new(0));
            let captures = Arc::new(AtomicUsize::new(0));
            let output = outputs.clone();
            let capture = Panicking(captures.clone());
            let cleanup = Arc::new(Semaphore::new(1));
            let permit = cleanup.clone().try_acquire_owned().unwrap();
            let factory = Factory::new(
                move || {
                    let _ = &capture;
                    Panicking(output.clone())
                },
                Arc::new(Semaphore::new(1)),
                cleanup,
            );
            let prepared = runtime
                .block_on(factory.prepare::<()>(Arc::default()))
                .unwrap();
            // Leave the invocation as the sole owner of the factory capture.
            drop(factory);
            match mode.as_str() {
                "already-stopped" => {
                    runtime.shutdown_timeout(Duration::from_secs(1));
                    drop(prepared);
                }
                "never-polled" => {
                    drop(prepared);
                    runtime.shutdown_timeout(Duration::from_secs(1));
                }
                "waiting-permit" => {
                    drop(prepared);
                    runtime.block_on(async {
                        tokio::task::yield_now().await;
                    });
                    assert_eq!(outputs.load(Ordering::SeqCst), 0);
                    assert_eq!(captures.load(Ordering::SeqCst), 0);
                    runtime.shutdown_timeout(Duration::from_secs(1));
                }
                _ => panic!("unknown owned child mode"),
            }
            drop(permit);
            assert_eq!(outputs.load(Ordering::SeqCst), 1);
            assert_eq!(captures.load(Ordering::SeqCst), 1);
            println!("PREPARED_SHUTDOWN_RETIRED");
            return;
        }
        use std::process::{Command, Stdio};
        struct Logs(std::path::PathBuf);
        impl Drop for Logs {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        for mode in ["already-stopped", "never-polled", "waiting-permit"] {
            let logs = Logs(
                std::env::temp_dir()
                    .join(format!("parrot-prepared-shutdown-{}", uuid::Uuid::new_v4())),
            );
            std::fs::create_dir(&logs.0).unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env(CHILD, mode)
                .stdout(Stdio::from(
                    std::fs::File::create(logs.0.join("stdout")).unwrap(),
                ))
                .stderr(Stdio::from(
                    std::fs::File::create(logs.0.join("stderr")).unwrap(),
                ))
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            let (status, timed_out) = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break (status, false);
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    break (child.wait().unwrap(), true);
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let read = |name| {
                use std::io::Read;
                let mut bytes = Vec::new();
                std::fs::File::open(logs.0.join(name))
                    .unwrap()
                    .take(256 * 1024)
                    .read_to_end(&mut bytes)
                    .unwrap();
                String::from_utf8_lossy(&bytes).into_owned()
            };
            let stdout = read("stdout");
            let stderr = read("stderr");
            assert!(
                !timed_out,
                "{mode}: owned child timed out: {stdout}\n{stderr}"
            );
            assert!(
                status.success(),
                "{mode}: owned child failed {status}: {stdout}\n{stderr}"
            );
            assert!(
                stdout.contains("PREPARED_SHUTDOWN_RETIRED"),
                "{mode}: {stdout}\n{stderr}"
            );
        }
    }
}
