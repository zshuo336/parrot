//! Engine-neutral bounded constructors and cancellation-safe capture retirement.
pub(crate) mod owned;
pub(crate) mod prepared;
mod retirement;
use crate::owned_payload::{join_error, panic_message};
use parrot_api::types::BoxedFuture;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Semaphore, watch};

#[derive(Debug)]
pub(crate) enum ConstructionError<E> {
    Cancelled,
    ConstructorPanicked(String),
    DisposalPanicked(String),
    WorkerFailed(String),
    Publication(E),
}

#[derive(Default)]
pub(crate) struct ConstructionControl {
    cancelled: AtomicBool,
    deadline: Option<tokio::time::Instant>,
    notify: tokio::sync::Notify,
}
impl ConstructionControl {
    pub(crate) fn with_deadline(deadline: tokio::time::Instant) -> Self {
        Self {
            deadline: Some(deadline),
            ..Self::default()
        }
    }
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    }
    pub(crate) async fn cancelled(&self) {
        let changed = self.notify.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        match self.deadline {
            Some(deadline) => tokio::select! {
                _ = changed => {},
                _ = tokio::time::sleep_until(deadline) => {},
            },
            None => changed.await,
        }
    }
}

struct FactoryState<A> {
    capture_context: Option<parrot_api::trace::TraceContext>,
    function: Option<Arc<dyn Fn() -> A + Send + Sync>>,
    disposal: Option<watch::Receiver<Option<Result<(), String>>>>,
    active: usize,
    retirement_failure: Option<String>,
    fallback: Option<watch::Sender<Option<Result<(), String>>>>,
}
pub(crate) struct Factory<A> {
    state: Arc<Mutex<FactoryState<A>>>,
    changed: Arc<tokio::sync::Notify>,
    workers: Arc<Semaphore>,
    cleanup: Arc<Semaphore>,
}
struct Invocation<A> {
    function: Option<Arc<dyn Fn() -> A + Send + Sync>>,
    state: Arc<Mutex<FactoryState<A>>>,
    changed: Arc<tokio::sync::Notify>,
}
impl<A> Drop for Invocation<A> {
    fn drop(&mut self) {
        let _ = retirement::invocation_finished(self);
    }
}
impl<A: Send + 'static> Factory<A> {
    pub(crate) fn new(
        function: impl Fn() -> A + Send + Sync + 'static,
        workers: Arc<Semaphore>,
        cleanup: Arc<Semaphore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(Mutex::new(FactoryState {
                capture_context: parrot_api::trace::TraceContext::current(),
                function: Some(Arc::new(function)),
                disposal: None,
                active: 0,
                retirement_failure: None,
                fallback: None,
            })),
            changed: Arc::default(),
            workers,
            cleanup,
        })
    }
    /// The caller is a retained lifecycle owner, not the user's cancellable waiter.
    pub(crate) fn construct<T: Send + 'static, E: Send + 'static>(
        &self,
        control: Arc<ConstructionControl>,
        publish: impl FnOnce(A) -> Result<T, E> + Send + 'static,
    ) -> impl std::future::Future<Output = Result<T, ConstructionError<E>>> + Send + '_ {
        parrot_api::trace::capture(async move {
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
                }
            };
            let permit = tokio::select! {
                biased;
                _ = control.cancelled() => return Err(ConstructionError::Cancelled),
                permit = self.workers.clone().acquire_owned() => permit.map_err(|e| ConstructionError::WorkerFailed(e.to_string()))?,
            };
            tokio::task::spawn_blocking(scoped_blocking(
                parrot_api::trace::TraceContext::current(),
                move || {
                    let _permit = permit;
                    let result = if control.is_cancelled() {
                        Err(ConstructionError::Cancelled)
                    } else {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            invocation
                                .function
                                .as_ref()
                                .expect("live factory invocation")()
                        }))
                        .map_err(|payload| {
                            ConstructionError::ConstructorPanicked(format!(
                                "actor factory panicked: {}",
                                panic_message(payload)
                            ))
                        })
                        .and_then(|actor| {
                            // Output destruction/publication has a separate error class:
                            // failed construction is not itself failed resource cleanup.
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                if control.is_cancelled() {
                                    drop(actor);
                                    Err(ConstructionError::Cancelled)
                                } else {
                                    publish(actor).map_err(ConstructionError::Publication)
                                }
                            }))
                            .unwrap_or_else(|payload| {
                                Err(ConstructionError::DisposalPanicked(format!(
                                    "factory publication or output disposal panicked: {}",
                                    panic_message(payload)
                                )))
                            })
                        })
                    };
                    // Keep the invocation (and its function clone) inside the bounded
                    // worker until publication/discard has really returned.
                    drop(invocation);
                    result
                },
            ))
            .await
            .map_err(|e| {
                ConstructionError::WorkerFailed(format!("factory worker failed: {}", join_error(e)))
            })?
        })
    }
}

// Scope owns the callback before dispatch. Cancelling an unstarted blocking
// job must dispose its captures under the same identity as executing it would.
fn scoped_blocking<F, R>(
    context: Option<parrot_api::trace::TraceContext>,
    operation: F,
) -> impl FnOnce() -> R + Send + 'static
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let work = parrot_api::trace::scope(context, async move { operation() });
    move || {
        futures::FutureExt::now_or_never(work).expect("synchronous constructor work cannot suspend")
    }
}

#[cfg(test)]
#[path = "constructor/trace_contracts.rs"]
mod trace_contracts;

#[cfg(test)]
mod scoped_worker_contracts {
    use super::*;
    use parrot_api::trace::TraceContext;
    use std::time::Duration;

    struct Capture {
        observed: Arc<Mutex<Vec<Option<TraceContext>>>>,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            self.observed.lock().unwrap().push(TraceContext::current());
        }
    }

    #[test]
    fn aborted_queued_blocking_callback_disposes_captures_under_its_owner() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        for traced in [false, true] {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let context = traced.then(TraceContext::root);
            let capture = Capture {
                observed: observed.clone(),
            };
            let (entered, ready) = std::sync::mpsc::channel();
            let (release, gate) = std::sync::mpsc::channel();
            let blocker = runtime.spawn_blocking(move || {
                entered.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(5))
                    .expect("blocking queue test gate must be released");
            });
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let callback = scoped_blocking(context.clone(), move || -> () {
                std::hint::black_box(&capture);
                panic!("aborted queued callback must never execute");
            });
            let queued = runtime.spawn_blocking(callback);
            queued.abort();
            release.send(()).unwrap();
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    blocker.await.unwrap();
                    assert!(queued.await.unwrap_err().is_cancelled());
                })
                .await
                .expect("queued callback independent liveness watchdog");
            });
            assert_eq!(
                *observed.lock().unwrap(),
                vec![context],
                "unstarted blocking callback disposal must retain its owner identity"
            );
            assert!(TraceContext::current().is_none());
            eprintln!("PARROT_CONSTRUCTOR_TRACE_CASE_OK case=queued-abort traced={traced}");
        }
    }
}

#[cfg(test)]
mod shared_cancellation_tests {
    use super::*;
    use std::{future::Future, time::Duration};
    #[tokio::test]
    async fn cancellation_wakes_every_already_registered_constructor() {
        let control = Arc::new(ConstructionControl::default());
        let mut tasks = tokio::task::JoinSet::new();
        let mut ready = Vec::new();
        for _ in 0..8 {
            let child = control.clone();
            let (send, receive) = tokio::sync::oneshot::channel();
            ready.push(receive);
            tasks.spawn(async move {
                let wait = child.cancelled();
                tokio::pin!(wait);
                let mut send = Some(send);
                std::future::poll_fn(|cx| {
                    let result = wait.as_mut().poll(cx);
                    if result.is_pending()
                        && let Some(send) = send.take()
                    {
                        let _ = send.send(());
                    }
                    result
                })
                .await;
            });
        }
        for receive in ready {
            receive.await.unwrap();
        }
        control.cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
        })
        .await
        .expect("all eight registered waiters must wake");
    }
    #[tokio::test]
    async fn deadline_wakes_a_constructor_without_an_explicit_cancel_call() {
        let control = ConstructionControl::with_deadline(
            tokio::time::Instant::now() + Duration::from_millis(10),
        );
        tokio::time::timeout(Duration::from_secs(1), control.cancelled())
            .await
            .unwrap();
        assert!(control.is_cancelled());
    }
}

#[cfg(test)]
mod retirement_tests;
