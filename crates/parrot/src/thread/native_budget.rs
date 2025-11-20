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

/// Move an implicit shutdown join off the dropping thread, which may itself
/// be the worker. The process-owned lifecycle executor survives host shutdown.
pub(crate) fn retire_join(
    handle: std::thread::JoinHandle<Result<(), String>>,
    reservation: Option<NativeWorkerReservation>,
) {
    let owner = Arc::new(Mutex::new(Some((handle, reservation))));
    let queued = owner.clone();
    let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let runtime = crate::root_cohort::lifecycle_runtime().map_err(|_| ())?;
        drop(runtime.spawn_blocking(move || {
            let (handle, reservation) = queued.lock().unwrap().take().expect("unclaimed OS join");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.join()))
                .map_err(crate::owned_payload::panic_message)
                .and_then(|joined| joined.map_err(crate::owned_payload::panic_message))
                .and_then(|result| result);
            if let Some(reservation) = reservation {
                reservation.release_after_join();
            }
            if let Err(error) = result {
                diagnostic(
                    || tracing::error!(%error, "native worker failed during implicit shutdown"),
                );
            }
        }));
        Ok::<_, ()>(())
    }));
    if !matches!(dispatched, Ok(Ok(()))) {
        // Dispatch failure must not drop an unjoined worker's reservation or
        // attempt a synchronous self-join. Retain it before any diagnostic.
        std::mem::forget(owner);
        if let Err(payload) = dispatched {
            let _ = crate::owned_payload::panic_message(payload);
        }
        diagnostic(|| tracing::error!("native join dispatch failed; retaining worker budget"));
    }
}

fn diagnostic(report: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(report)) {
        let _ = crate::owned_payload::panic_message(payload);
    }
}
