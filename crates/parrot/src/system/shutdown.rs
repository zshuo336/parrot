//! A facade shutdown waiter never owns the remaining engines' retirement.
use super::{ActorSystemImpl, ParrotActorSystem};
mod errors;
mod future;
use crate::{
    error_receipt::Receipt,
    owned_payload::{PublicationInput, panic_message},
};
pub use errors::{ShutdownErrors, ShutdownFailure};
use future::ShutdownFuture;
use futures::FutureExt;
use parrot_api::{system::SystemError, types::BoxedFuture};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;

#[cfg(test)]
mod host_exit_contracts;

type Outcome = Option<Result<(), ShutdownErrors>>;
#[derive(Default)]
pub(super) struct Lifecycle {
    pub(super) receipt: Mutex<Option<watch::Sender<Outcome>>>,
    finished: AtomicBool,
}
impl Lifecycle {
    pub(super) fn started(&self) -> bool {
        self.receipt.lock().unwrap().is_some()
    }
    pub(super) fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}
struct Completion {
    lifecycle: Arc<Lifecycle>,
    receipt: watch::Sender<Outcome>,
}
impl Completion {
    fn publish_timeout(&self) {
        self.publish(Err(ShutdownErrors::message(
            "facade shutdown timed out; cleanup remains owned",
        )));
    }
    async fn retire<F: std::future::Future<Output = Result<(), ShutdownErrors>>>(
        self,
        work: F,
        deadline: Option<tokio::time::Instant>,
    ) {
        tokio::pin!(work);
        let result = if let Some(deadline) = deadline {
            tokio::select! {
                result = &mut work => {
                    // A ready cleanup can win even when the timer is also
                    // ready, or its last poll can consume the remaining budget.
                    if tokio::time::Instant::now() >= deadline {
                        self.publish_timeout();
                    }
                    result
                },
                _ = tokio::time::sleep_until(deadline) => {
                    self.publish_timeout();
                    work.await
                }
            }
        } else {
            work.await
        };
        self.finish(result);
    }
    fn finish(&self, result: Result<(), ShutdownErrors>) {
        self.lifecycle.finished.store(true, Ordering::Release);
        self.publish(result);
    }
    fn publish(&self, result: Result<(), ShutdownErrors>) {
        self.receipt.send_if_modified(|value| {
            if value.is_some() {
                return false;
            }
            *value = Some(result);
            true
        });
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        self.publish(Err(ShutdownErrors::message(
            "facade shutdown owner ended before completion",
        )));
    }
}
impl ParrotActorSystem {
    /// Join one owned shutdown operation. Starts when polled; cancelling any
    /// waiter leaves all engine and mirror retirement owned by the operation.
    /// Repeated/concurrent calls retain the first result, including timeout.
    /// A positive system_shutdown timeout bounds the receipt, not cleanup;
    /// zero leaves deadlines to the engines. An independent lifecycle executor
    /// owns the operation even after the facade's original host exits.
    /// Completion first observed at or after the facade deadline is a timeout.
    /// Completed failures are collected in [`ShutdownErrors`] with engine context
    /// and original causes. Formatting the aggregate uses cached safe diagnostics.
    /// A published timeout supersedes late outcomes, whose errors are safely disposed.
    pub fn shutdown_runtime(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async move {
            let mut receipt = {
                let mut state = self.lifecycle.receipt.lock().unwrap();
                if let Some(receipt) = &*state {
                    receipt.subscribe()
                } else {
                    let runtime = self.runtime.clone();
                    let timeout = self.config.value().timeouts.system_shutdown;
                    let deadline = if timeout.is_zero() {
                        None
                    } else {
                        Some(
                            tokio::time::Instant::now()
                                .checked_add(timeout)
                                .ok_or_else(|| {
                                    SystemError::InitializationError(
                                        "facade shutdown deadline exceeds clock range".into(),
                                    )
                                })?,
                        )
                    };
                    let mut systems: Vec<_> = self
                        .systems
                        .read()
                        .unwrap()
                        .iter()
                        .map(|(name, system)| (name.clone(), PublicationInput::new(system.clone())))
                        .collect();
                    systems.sort_by(|a, b| a.0.cmp(&b.0));
                    let mut seen = std::collections::HashSet::new();
                    systems.retain(|(_, system)| seen.insert(system.value().identity()));
                    let mirrors = self.mirrors.clone();
                    let (sender, receipt) = watch::channel(None);
                    *state = Some(sender.clone());
                    let completion = Completion {
                        lifecycle: self.lifecycle.clone(),
                        receipt: sender,
                    };
                    runtime.spawn(completion.retire(retire_all(systems, mirrors), deadline));
                    receipt
                }
            };
            loop {
                if let Some(result) = receipt.borrow_and_update().clone() {
                    return result.map_err(|error| SystemError::Other(anyhow::Error::new(error)));
                }
                receipt.changed().await.map_err(|_| {
                    SystemError::Other(anyhow::anyhow!("facade shutdown receipt closed"))
                })?;
            }
        })
    }
}

async fn retire_engine(
    name: String,
    system: PublicationInput<ActorSystemImpl>,
) -> Vec<ShutdownFailure> {
    // Keep the runtime owner outside both callback and future unwind boundaries.
    let mut errors = match catch_unwind(AssertUnwindSafe(|| -> BoxedFuture<'_, _> {
        match system.value() {
            ActorSystemImpl::Runtime(runtime) => runtime.shutdown(),
            ActorSystemImpl::Thread(runtime) => Box::pin(async {
                runtime
                    .shutdown_internal()
                    .await
                    .map_err(crate::error_receipt::public_system_error)
            }),
            ActorSystemImpl::Actix(runtime) => Box::pin(runtime.clone().shutdown()),
        }
    })) {
        Ok(future) => ShutdownFuture::new(future).await,
        Err(payload) => vec![("shutdown construction", future::panic_receipt(payload))],
    };
    if let Err(error) = system.retire() {
        errors.push((
            "runtime handle disposal",
            Receipt::new(SystemError::ActorError(error)),
        ));
    }
    errors
        .into_iter()
        .map(|(operation, error)| {
            ShutdownFailure::from_receipt(Some(name.clone()), operation, error)
        })
        .collect()
}

async fn retire_all(
    // Protect each owner before the spawned task's first poll: host shutdown
    // may cancel the whole snapshot before retire_engine has ever run.
    systems: Vec<(String, PublicationInput<ActorSystemImpl>)>,
    mirrors: Arc<crate::mirror::MirrorRegistry>,
) -> Result<(), ShutdownErrors> {
    let engines = futures::future::join_all(
        systems
            .into_iter()
            .map(|(name, system)| retire_engine(name, system)),
    );
    let mirrors = std::panic::AssertUnwindSafe(mirrors.shutdown()).catch_unwind();
    let (engines, mirrors) = tokio::join!(engines, mirrors);
    let mut errors: Vec<_> = engines.into_iter().flatten().collect();
    match mirrors {
        Ok(Ok(())) => {}
        Ok(Err(error)) => errors.push(ShutdownFailure::new(
            None,
            "mirrors",
            SystemError::Other(anyhow::anyhow!(error)),
        )),
        Err(payload) => errors.push(ShutdownFailure::new(
            None,
            "mirrors",
            SystemError::Other(anyhow::anyhow!(panic_message(payload))),
        )),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ShutdownErrors::new(errors))
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::sync::oneshot;
    use tokio::time::Instant;

    fn owner() -> (Completion, Arc<Lifecycle>, watch::Receiver<Outcome>) {
        let (receipt, observed) = watch::channel(None);
        let lifecycle = Arc::new(Lifecycle {
            receipt: Mutex::new(Some(receipt.clone())),
            finished: AtomicBool::new(false),
        });
        (
            Completion {
                lifecycle: lifecycle.clone(),
                receipt,
            },
            lifecycle,
            observed,
        )
    }

    fn text(outcome: &Outcome) -> Option<Result<(), String>> {
        outcome
            .as_ref()
            .map(|result| result.as_ref().map(|_| ()).map_err(ToString::to_string))
    }

    fn timeout() -> Option<Result<(), String>> {
        Some(Err(
            "facade shutdown timed out; cleanup remains owned".into()
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn ready_retirement_at_or_after_deadline_keeps_timeout() {
        let mut incorrect = Vec::new();
        // The production select is unbiased. Run the entire fixed matrix;
        // never retry a failed case until a favorable branch is selected.
        for sample in 0..32 {
            for extra in [Duration::ZERO, Duration::from_millis(1)] {
                for result in [Ok(()), Err(ShutdownErrors::message("late cleanup error"))] {
                    let (owner, state, observed) = owner();
                    let deadline = Instant::now() + Duration::from_millis(10);
                    tokio::time::advance(Duration::from_millis(10) + extra).await;
                    owner
                        .retire(std::future::ready(result.clone()), Some(deadline))
                        .await;
                    assert!(state.finished(), "actual retirement must finish");
                    let actual = text(&observed.borrow());
                    if actual != timeout() {
                        incorrect.push((sample, extra, result, actual));
                    }
                }
            }
        }
        assert!(
            incorrect.is_empty(),
            "ready facade cleanup must not hide expiry: {incorrect:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn both_ready_repoll_keeps_timeout() {
        let mut incorrect = Vec::new();
        for sample in 0..16 {
            for extra in [Duration::ZERO, Duration::from_millis(1)] {
                for result in [Ok(()), Err(ShutdownErrors::message("late cleanup error"))] {
                    let (owner, state, observed) = owner();
                    let deadline = Instant::now() + Duration::from_millis(10);
                    let (send, receive) = oneshot::channel();
                    let retirement = owner.retire(
                        async move { receive.await.expect("cleanup release") },
                        Some(deadline),
                    );
                    tokio::pin!(retirement);
                    assert!(futures::poll!(retirement.as_mut()).is_pending());
                    assert!(observed.borrow().is_none());
                    tokio::time::advance(Duration::from_millis(10) + extra).await;
                    send.send(result.clone()).unwrap();
                    retirement.await;
                    assert!(state.finished());
                    let actual = text(&observed.borrow());
                    if actual != timeout() {
                        incorrect.push((sample, extra, result, actual));
                    }
                }
            }
        }
        assert!(
            incorrect.is_empty(),
            "facade both-ready repoll must not hide expiry: {incorrect:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn early_retirement_preserves_success_and_error() {
        for result in [Ok(()), Err(ShutdownErrors::message("early cleanup error"))] {
            let (owner, state, observed) = owner();
            owner
                .retire(
                    std::future::ready(result.clone()),
                    Some(Instant::now() + Duration::from_secs(1)),
                )
                .await;
            assert!(state.finished());
            assert_eq!(text(&observed.borrow()), text(&Some(result)));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn no_facade_deadline_preserves_pending_work_and_actual_result() {
        for result in [
            Ok(()),
            Err(ShutdownErrors::message("unbounded cleanup error")),
        ] {
            let (owner, state, observed) = owner();
            let (send, receive) = oneshot::channel();
            let retirement =
                owner.retire(async move { receive.await.expect("cleanup release") }, None);
            tokio::pin!(retirement);
            assert!(futures::poll!(retirement.as_mut()).is_pending());
            tokio::time::advance(Duration::from_secs(86_400)).await;
            assert!(futures::poll!(retirement.as_mut()).is_pending());
            assert!(
                observed.borrow().is_none(),
                "zero timeout must remain unbounded"
            );
            assert!(!state.finished());
            send.send(result.clone()).unwrap();
            retirement.await;
            assert!(state.finished());
            assert_eq!(text(&observed.borrow()), text(&Some(result)));
        }
    }

    struct Cleanup(Arc<AtomicUsize>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn public_timeout_stays_sticky_while_cleanup_remains_owned() {
        for result in [Ok(()), Err(ShutdownErrors::message("late cleanup error"))] {
            let (owner, state, observed) = owner();
            let (send, receive) = oneshot::channel();
            let drops = Arc::new(AtomicUsize::new(0));
            let cleanup = Cleanup(drops.clone());
            let retirement = owner.retire(
                async move {
                    let _cleanup = cleanup;
                    receive.await.expect("cleanup release")
                },
                Some(Instant::now() + Duration::from_millis(10)),
            );
            tokio::pin!(retirement);
            assert!(futures::poll!(retirement.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(11)).await;
            assert!(
                futures::poll!(retirement.as_mut()).is_pending(),
                "timeout must retain the pending cleanup future"
            );
            assert_eq!(text(&observed.borrow()), timeout());
            assert!(!state.finished(), "public timeout is not actual retirement");
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            let late = state.receipt.lock().unwrap().as_ref().unwrap().subscribe();
            assert_eq!(text(&late.borrow()), timeout());
            send.send(result).unwrap();
            retirement.await;
            assert!(state.finished());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(text(&observed.borrow()), timeout());
            assert_eq!(text(&late.borrow()), timeout());
        }
    }
}
