//! Process-wide admission for framework-owned native worker threads.
//!
//! The per-system scheduler limits protect one configuration. This budget
//! covers independently-created Thread and Actix systems and is held until the
//! corresponding OS join has completed.

use std::sync::{Arc, Mutex, OnceLock, mpsc};
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
        // Self-owned workers cannot start another joiner under PID exhaustion.
        // Establish that independent owner before admitting a business worker.
        prepare_join_fallback()?;
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

trait PendingJoin: Send {
    fn is_finished(&self) -> bool;
    fn finish(self: Box<Self>);
}

struct DeferredJoin<T, F> {
    thread: std::thread::JoinHandle<T>,
    finished: F,
}

impl<T: Send + 'static, F: FnOnce(std::thread::Result<T>) + Send> PendingJoin
    for DeferredJoin<T, F>
{
    fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    fn finish(self: Box<Self>) {
        (self.finished)(self.thread.join());
    }
}

struct JoinFallback {
    sender: mpsc::Sender<Box<dyn PendingJoin>>,
    _thread: std::thread::JoinHandle<()>,
}

impl JoinFallback {
    fn shared() -> Result<Arc<Self>, SystemError> {
        static SHARED: OnceLock<Mutex<Option<Arc<JoinFallback>>>> = OnceLock::new();
        let mut shared = SHARED.get_or_init(|| Mutex::new(None)).lock().unwrap();
        if let Some(owner) = &*shared {
            return Ok(owner.clone());
        }
        let (sender, receiver) = mpsc::channel::<Box<dyn PendingJoin>>();
        let thread = std::thread::Builder::new()
            .name("parrot-native-join-owner".into())
            .spawn(move || {
                let mut pending: Vec<Box<dyn PendingJoin>> = Vec::new();
                loop {
                    let received = if pending.is_empty() {
                        receiver.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    } else {
                        receiver.recv_timeout(std::time::Duration::from_millis(1))
                    };
                    match received {
                        Ok(job) => pending.push(job),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) if pending.is_empty() => return,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                    // Bound intake per pass so a stream of exits cannot starve
                    // joins already ready to return their native reservations.
                    pending.extend(receiver.try_iter().take(63));
                    let mut index = 0;
                    while index < pending.len() {
                        if !pending[index].is_finished() {
                            index += 1;
                            continue;
                        }
                        let job = pending.swap_remove(index);
                        if let Err(payload) =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.finish()))
                        {
                            let message = crate::owned_payload::panic_message(payload);
                            diagnostic(|| tracing::error!(error = %message, "native join completion failed"));
                        }
                    }
                }
            })
            .map_err(|error| {
                SystemError::ThreadSetupError(format!(
                    "native join owner initialization failed: {error}"
                ))
            })?;
        let owner = Arc::new(Self {
            sender,
            _thread: thread,
        });
        *shared = Some(owner.clone());
        Ok(owner)
    }
}

pub(crate) fn prepare_join_fallback() -> Result<(), SystemError> {
    JoinFallback::shared().map(|_| ())
}

/// Transfer an already admitted native owner without allocating another OS
/// thread. Production callers retain native reservations through this join;
/// those process-wide reservations also bound the number of queued owners.
pub(crate) fn defer_join<T: Send + 'static>(
    thread: std::thread::JoinHandle<T>,
    finished: impl FnOnce(std::thread::Result<T>) + Send + 'static,
) {
    let job: Box<dyn PendingJoin> = Box::new(DeferredJoin { thread, finished });
    match JoinFallback::shared() {
        Ok(owner) => {
            if let Err(mpsc::SendError(job)) = owner.sender.send(job) {
                // The prestarted process owner should never disappear. Keep
                // the exact handle/lease if that invariant has been violated.
                std::mem::forget(job);
                diagnostic(|| tracing::error!("native join owner unexpectedly ended"));
            }
        }
        Err(error) => {
            std::mem::forget(job);
            diagnostic(
                || tracing::error!(%error, "native join owner was not prepared before admission"),
            );
        }
    }
}

fn finish_native_worker(
    joined: std::thread::Result<NativeJoinResult>,
    reservation: Option<NativeWorkerReservation>,
) {
    let result = joined
        .map_err(crate::owned_payload::panic_message)
        .and_then(|result| result);
    if let Some(reservation) = reservation {
        reservation.release_after_join();
    }
    if let Err(error) = result {
        diagnostic(|| tracing::error!(%error, "native worker failed during implicit shutdown"));
    }
}

fn join_native_worker(
    handle: std::thread::JoinHandle<NativeJoinResult>,
    reservation: Option<NativeWorkerReservation>,
) {
    let joined = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.join()))
        .unwrap_or_else(Err);
    finish_native_worker(joined, reservation);
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

        defer_join(handle, move |joined| {
            finish_native_worker(joined, reservation)
        });
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
mod fallback_contracts;

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
