//! A facade shutdown waiter never owns the remaining engines' retirement.
use super::{ActorSystemImpl, ParrotActorSystem};
use futures::FutureExt;
use parrot_api::{system::SystemError, types::BoxedFuture};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;

type Outcome = Option<Result<(), String>>;
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
        self.publish(Err(
            "facade shutdown timed out; cleanup remains owned".into()
        ));
    }
    async fn retire<F: std::future::Future<Output = Result<(), String>>>(
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
    fn finish(&self, result: Result<(), String>) {
        self.lifecycle.finished.store(true, Ordering::Release);
        self.publish(result);
    }
    fn publish(&self, result: Result<(), String>) {
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
        self.publish(Err("facade shutdown owner ended before completion".into()));
    }
}
impl ParrotActorSystem {
    /// Join one owned shutdown operation. Starts when polled; cancelling any
    /// waiter leaves all engine and mirror retirement owned by the operation.
    /// Repeated/concurrent calls retain the first result, including timeout.
    /// A positive system_shutdown timeout bounds the receipt, not cleanup;
    /// zero leaves deadlines to the engines. The host executor must stay alive.
    /// Completion first observed at or after the facade deadline is a timeout.
    pub fn shutdown_runtime(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async move {
            let mut receipt = {
                let mut state = self.lifecycle.receipt.lock().unwrap();
                if let Some(receipt) = &*state {
                    receipt.subscribe()
                } else {
                    let runtime = self
                        .runtime
                        .clone()
                        .or_else(|| tokio::runtime::Handle::try_current().ok())
                        .ok_or_else(|| {
                            SystemError::InitializationError(
                                "facade shutdown requires a Tokio executor".into(),
                            )
                        })?;
                    let timeout = self.config.timeouts.system_shutdown;
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
                        .map(|(name, system)| (name.clone(), system.clone()))
                        .collect();
                    systems.sort_by(|a, b| a.0.cmp(&b.0));
                    let mut seen = std::collections::HashSet::new();
                    systems.retain(|(_, system)| seen.insert(system.identity()));
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
                    return result.map_err(|error| SystemError::Other(anyhow::anyhow!(error)));
                }
                receipt.changed().await.map_err(|_| {
                    SystemError::Other(anyhow::anyhow!("facade shutdown receipt closed"))
                })?;
            }
        })
    }
}

async fn retire_engine(name: String, system: ActorSystemImpl) -> Result<(), String> {
    let result = std::panic::AssertUnwindSafe(async move {
        system.shutdown().await.map_err(|error| error.to_string())
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|payload| Err(crate::owned_payload::panic_message(payload)));
    result.map_err(|error| format!("engine {name:?}: {error}"))
}

async fn retire_all(
    systems: Vec<(String, ActorSystemImpl)>,
    mirrors: Arc<crate::mirror::MirrorRegistry>,
) -> Result<(), String> {
    let engines = futures::future::join_all(
        systems
            .into_iter()
            .map(|(name, system)| retire_engine(name, system)),
    );
    let mirrors = std::panic::AssertUnwindSafe(mirrors.shutdown()).catch_unwind();
    let (engines, mirrors) = tokio::join!(engines, mirrors);
    let mut errors: Vec<_> = engines.into_iter().filter_map(Result::err).collect();
    match mirrors {
        Ok(Ok(())) => {}
        Ok(Err(error)) => errors.push(format!("mirrors: {error}")),
        Err(payload) => errors.push(format!(
            "mirrors: {}",
            crate::owned_payload::panic_message(payload)
        )),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
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

    fn timeout() -> Outcome {
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
                for result in [Ok(()), Err("late cleanup error".into())] {
                    let (owner, state, observed) = owner();
                    let deadline = Instant::now() + Duration::from_millis(10);
                    tokio::time::advance(Duration::from_millis(10) + extra).await;
                    owner
                        .retire(std::future::ready(result.clone()), Some(deadline))
                        .await;
                    assert!(state.finished(), "actual retirement must finish");
                    let actual = observed.borrow().clone();
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
                for result in [Ok(()), Err("late cleanup error".into())] {
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
                    let actual = observed.borrow().clone();
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
        for result in [Ok(()), Err("early cleanup error".into())] {
            let (owner, state, observed) = owner();
            owner
                .retire(
                    std::future::ready(result.clone()),
                    Some(Instant::now() + Duration::from_secs(1)),
                )
                .await;
            assert!(state.finished());
            assert_eq!(*observed.borrow(), Some(result));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn no_facade_deadline_preserves_pending_work_and_actual_result() {
        for result in [Ok(()), Err("unbounded cleanup error".into())] {
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
            assert_eq!(*observed.borrow(), Some(result));
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
        for result in [Ok(()), Err("late cleanup error".into())] {
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
            assert_eq!(*observed.borrow(), timeout());
            assert!(!state.finished(), "public timeout is not actual retirement");
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            let late = state.receipt.lock().unwrap().as_ref().unwrap().subscribe();
            assert_eq!(*late.borrow(), timeout());
            send.send(result).unwrap();
            retirement.await;
            assert!(state.finished());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(*observed.borrow(), timeout());
            assert_eq!(*late.borrow(), timeout());
        }
    }
}
