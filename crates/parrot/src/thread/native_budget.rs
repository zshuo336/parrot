//! Process-wide admission for framework-owned native worker threads.
//!
//! The per-system scheduler limits protect one configuration. This budget
//! covers independently-created Thread and Actix systems and is held until the
//! corresponding OS join has completed.

use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::thread::error::SystemError;

const CAPACITY: usize = parrot_config::ThreadResolved::MAX_WORKERS;

#[derive(Debug)]
pub(crate) struct NativeWorkerBudget {
    slots: Arc<Semaphore>,
    capacity: usize,
}

impl NativeWorkerBudget {
    pub(crate) fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<NativeWorkerBudget>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Arc::new(Self::with_capacity(CAPACITY)))
            .clone()
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    pub(crate) fn try_reserve(self: &Arc<Self>) -> Result<NativeWorkerReservation, SystemError> {
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            SystemError::ThreadSetupError(format!(
                "native worker budget exhausted ({} live or retiring threads)",
                self.capacity
            ))
        })?;
        Ok(NativeWorkerReservation {
            permit: Some(permit),
        })
    }
}

/// A native-worker lease. Its capacity is released only after its owner has
/// completed the OS join.
#[derive(Debug)]
pub(crate) struct NativeWorkerReservation {
    permit: Option<OwnedSemaphorePermit>,
}

impl NativeWorkerReservation {
    pub(crate) fn release_after_join(mut self) {
        drop(self.permit.take());
    }
}

type NativeJoinResult = Result<(), String>;

fn join_native_worker(
    handle: std::thread::JoinHandle<NativeJoinResult>,
    reservation: Option<NativeWorkerReservation>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.join()))
        .map_err(crate::owned_payload::panic_message)
        .and_then(|joined| joined.map_err(crate::owned_payload::panic_message))
        .and_then(|result| result);
    if let Some(reservation) = reservation {
        reservation.release_after_join();
    }
    if let Err(error) = result {
        diagnostic(|| tracing::error!(%error, "native worker failed during implicit shutdown"));
    }
}

/// Owns a native join until the OS thread has actually been joined. Its Drop
/// path is also the cancellation path for a queued Tokio blocking task.
struct NativeJoinJob {
    handle: Option<std::thread::JoinHandle<NativeJoinResult>>,
    reservation: Option<NativeWorkerReservation>,
}

impl NativeJoinJob {
    fn new(
        handle: std::thread::JoinHandle<NativeJoinResult>,
        reservation: Option<NativeWorkerReservation>,
    ) -> Self {
        Self {
            handle: Some(handle),
            reservation,
        }
    }
}

impl Drop for NativeJoinJob {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let reservation = self.reservation.take();
        if handle.thread().id() != std::thread::current().id() {
            join_native_worker(handle, reservation);
            return;
        }

        // A scheduler can be dropped by its own worker. Never self-join; move
        // the exact owner to a separate OS thread instead. The Arc keeps the
        // reservation and handle alive if thread creation itself is rejected.
        let owner = Arc::new(Mutex::new(Some((handle, reservation))));
        let queued = owner.clone();
        let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::thread::Builder::new()
                .name("parrot-native-join".into())
                .spawn(move || {
                    if let Some((handle, reservation)) = queued.lock().unwrap().take() {
                        join_native_worker(handle, reservation);
                    }
                })
        }));
        if !matches!(spawned, Ok(Ok(_))) {
            // There is no safe synchronous fallback on the worker itself.
            // Retain the owner rather than detaching the worker or releasing
            // its budget early; a later process teardown can reclaim it.
            std::mem::forget(owner);
            diagnostic(|| tracing::error!("native worker join thread creation failed"));
        }
    }
}

/// Move an implicit shutdown join off the dropping thread, which may itself
/// be the worker. The process-owned lifecycle executor survives host shutdown.
pub(crate) fn retire_join(
    handle: std::thread::JoinHandle<Result<(), String>>,
    reservation: Option<NativeWorkerReservation>,
) {
    let job = NativeJoinJob::new(handle, reservation);
    let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match crate::root_cohort::lifecycle_runtime() {
            Ok(runtime) => {
                // Dropping this task handle may cancel a queued blocking job;
                // NativeJoinJob::drop then performs the same real join.
                drop(runtime.spawn_blocking(move || drop(job)));
            }
            Err(error) => {
                diagnostic(
                    || tracing::error!(%error, "native join runtime unavailable; using owner drop"),
                );
                drop(job);
            }
        }
    }));
    if let Err(payload) = dispatched {
        let message = crate::owned_payload::panic_message(payload);
        diagnostic(
            || tracing::error!(error = %message, "native join dispatch failed; owner drop retained"),
        );
    }
}

fn diagnostic(report: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(report)) {
        let _ = crate::owned_payload::panic_message(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn join_owner_keeps_native_budget_until_the_os_join_finishes() {
        let budget = Arc::new(NativeWorkerBudget::with_capacity(1));
        let reservation = budget.try_reserve().unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (finished, observe) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            wait.recv().unwrap();
            finished.send(()).unwrap();
            Ok(())
        });
        let job = NativeJoinJob::new(handle, Some(reservation));
        let joiner = std::thread::spawn(move || drop(job));

        assert!(budget.try_reserve().is_err());
        release.send(()).unwrap();
        observe
            .recv_timeout(Duration::from_secs(2))
            .expect("native worker did not finish");
        joiner.join().unwrap();
        let reclaimed = budget.try_reserve().unwrap();
        reclaimed.release_after_join();
    }

    #[test]
    fn join_owner_contains_a_worker_failure_without_unwinding() {
        let handle = std::thread::spawn(|| Err("worker failure".into()));
        let job = NativeJoinJob::new(handle, None);
        drop(job);
    }
}
