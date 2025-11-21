//! A prestarted owner joins workers independently of shutdown waiters.
use crate::thread::native_budget::NativeWorkerReservation;
use parrot_api::{errors::ActorError, system::SystemError, types::ActorResult};
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use tokio::sync::{Semaphore, watch};

type Exit = Result<(), String>;
type Job = Box<dyn FnOnce() + Send>;

// Helper joins are user-visible retirement work. Keep their pending owners
// behind the same bounded cleanup accounting as actor/config payloads instead
// of adding an unbounded task to Tokio's process-wide blocking queue for every
// retiring pool.
const HELPER_CLEANUP_SLOTS: usize = 512;
fn helper_cleanup_slots() -> &'static Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(HELPER_CLEANUP_SLOTS)))
}

struct HelperJoin {
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    reservations: Arc<Mutex<Vec<NativeWorkerReservation>>>,
    finished: watch::Sender<Option<Exit>>,
    scheduled: AtomicBool,
    changed: Condvar,
}
struct HelperJoinJob {
    owner: Arc<HelperJoin>,
}
impl Drop for HelperJoinJob {
    fn drop(&mut self) {
        self.owner.join_blocking();
    }
}
impl HelperJoin {
    fn join_thread(
        thread: std::thread::JoinHandle<()>,
        reservations: &Arc<Mutex<Vec<NativeWorkerReservation>>>,
    ) -> Exit {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| thread.join()))
            .map_err(crate::owned_payload::panic_message)
            .and_then(|result| result.map_err(crate::owned_payload::panic_message))
            .map_err(|message| format!("Actix retirement thread failed: {message}"));
        for reservation in reservations.lock().unwrap().drain(..) {
            reservation.release_after_join();
        }
        result
    }

    fn join_blocking(&self) {
        let mut slot = self.thread.lock().unwrap();
        let Some(thread) = slot.take() else {
            while self.finished.borrow().is_none() {
                slot = self.changed.wait(slot).unwrap();
            }
            return;
        };
        drop(slot);
        let result = Self::join_thread(thread, &self.reservations);
        // Receipt wakers may synchronously reenter join. Publish outside the
        // join lock, and notify blocking observers even if a waker panics.
        let notified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.finished.send_replace(Some(result));
        }));
        {
            let _slot = self.thread.lock().unwrap();
            self.changed.notify_all();
        }
        if let Err(payload) = notified {
            let _ = crate::owned_payload::panic_message(payload);
        }
    }
    fn dispatch(self: &Arc<Self>, dispatch: impl FnOnce(HelperJoinJob)) {
        if !self.scheduled.swap(true, Ordering::AcqRel) {
            let job = HelperJoinJob {
                owner: self.clone(),
            };
            // A failed or cancelled dispatch drops this guard. It claims the
            // shared OS join or waits for the cancellation owner's actual join.
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                dispatch(job);
            })) {
                let message = crate::owned_payload::panic_message(payload);
                tracing::warn!(error = %message, "Actix helper join dispatch failed; actual join receipt retained");
            }
        }
    }

    fn dispatch_in_lifecycle(self: &Arc<Self>, runtime: &tokio::runtime::Handle) {
        let context = parrot_api::trace::TraceContext::current();
        self.dispatch(|job| {
            let slots = helper_cleanup_slots().clone();
            // Production pools retain at least one native worker reservation
            // through this helper's OS join. That existing process budget also
            // bounds these once-per-helper admission waiters.
            runtime.spawn(async move {
                // A synchronous takeover can finish without returning any
                // pending quota. Its completed job must stop waiting here.
                let pending = tokio::select! {
                    biased;
                    _ = wait(job.owner.finished.subscribe()) => return,
                    pending = crate::owned_payload::pending_cleanup_permit(&slots) => pending,
                };
                let permit = tokio::select! {
                    biased;
                    _ = wait(job.owner.finished.subscribe()) => return,
                    permit = slots.acquire_owned() => permit,
                };
                let _ = tokio::task::spawn_blocking(move || {
                    let _pending = pending;
                    let _permit = permit;
                    parrot_api::trace::in_scope(context, || {
                        let _ = crate::owned_payload::retire_synchronously(job);
                    });
                })
                .await;
            });
        });
    }
}
impl Drop for HelperJoin {
    fn drop(&mut self) {
        let Some(thread) = self.thread.get_mut().unwrap().take() else {
            return;
        };

        // A lifecycle task can be cancelled before it creates HelperJoinJob.
        // Keep the native handle and reservations owned by an independent OS
        // joiner instead of allowing JoinHandle::drop to detach the thread.
        let owner = Arc::new(Mutex::new(Some((
            thread,
            self.reservations.clone(),
            self.finished.clone(),
        ))));
        let queued = owner.clone();
        let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::thread::Builder::new()
                .name("parrot-actix-retire-fallback".into())
                .spawn(move || {
                    let Some((thread, reservations, finished)) = queued.lock().unwrap().take()
                    else {
                        return;
                    };
                    let result = Self::join_thread(thread, &reservations);
                    finished.send_replace(Some(result));
                })
        }));
        if !matches!(spawned, Ok(Ok(_))) {
            // There is no safe synchronous fallback from a destructor. Keep
            // the exact join owner and its native leases alive for teardown.
            std::mem::forget(owner);
        }
    }
}

async fn wait(mut receipt: watch::Receiver<Option<Exit>>) -> Exit {
    loop {
        if let Some(result) = receipt.borrow_and_update().clone() {
            return result;
        }
        receipt
            .changed()
            .await
            .map_err(|_| "Actix retirement owner ended without a cleanup receipt".to_owned())?;
    }
}

struct RetireWorkers {
    owners: Vec<actix::Arbiter>,
    finished: watch::Sender<Option<Exit>>,
    // Also retained by HelperJoin on successful startup. On startup failure
    // this guard keeps every lease until all business workers have been joined.
    _reservations: Arc<Mutex<Vec<NativeWorkerReservation>>>,
}
impl Drop for RetireWorkers {
    fn drop(&mut self) {
        for owner in &self.owners {
            owner.stop();
        }
        let mut first = None;
        let mut failures = 0;
        for (index, owner) in self.owners.drain(..).enumerate() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.join()))
                .map_err(crate::owned_payload::panic_message)
                .and_then(|result| result.map_err(crate::owned_payload::panic_message));
            if let Err(message) = result {
                failures += 1;
                first.get_or_insert_with(|| format!("worker {index}: {message}"));
            }
        }
        let result = first.map_or(Ok(()), |message| {
            Err(format!(
                "Actix worker failed during shutdown ({failures} worker joins): {message}"
            ))
        });
        self.finished.send_replace(Some(result));
    }
}

pub(super) struct Retirement {
    runtime: tokio::runtime::Handle,
    signal: Mutex<Option<mpsc::Sender<()>>>,
    finished: watch::Receiver<Option<Exit>>,
    helper: Arc<HelperJoin>,
}
impl Retirement {
    #[cfg(test)]
    pub(super) fn new(owners: Vec<actix::Arbiter>) -> Result<Self, SystemError> {
        Self::with_spawner_and_reservations(owners, Vec::new(), |job| {
            std::thread::Builder::new()
                .name("parrot-actix-retire".into())
                .spawn(job)
        })
    }

    pub(super) fn new_with_reservations(
        owners: Vec<actix::Arbiter>,
        reservations: Vec<NativeWorkerReservation>,
    ) -> Result<Self, SystemError> {
        Self::with_spawner_and_reservations(owners, reservations, |job| {
            std::thread::Builder::new()
                .name("parrot-actix-retire".into())
                .spawn(job)
        })
    }

    #[cfg(test)]
    fn with_spawner(
        owners: Vec<actix::Arbiter>,
        spawn: impl FnOnce(Job) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Self, SystemError> {
        Self::with_spawner_and_reservations(owners, Vec::new(), spawn)
    }

    fn with_spawner_and_reservations(
        owners: Vec<actix::Arbiter>,
        reservations: Vec<NativeWorkerReservation>,
        spawn: impl FnOnce(Job) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Self, SystemError> {
        let (finished, receipt) = watch::channel(None);
        let reservations = Arc::new(Mutex::new(reservations));
        // Install rollback before dispatch: failed thread creation drops its
        // closure on the constructing thread, which is outside this worker set.
        let workers = RetireWorkers {
            owners,
            finished,
            _reservations: reservations.clone(),
        };
        let runtime = crate::root_cohort::lifecycle_runtime()?;
        let (signal, receive) = mpsc::channel();
        let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spawn(Box::new(move || {
                let _ = receive.recv();
                drop(workers);
            }))
        }))
        .map_err(crate::owned_payload::panic_message)
        .and_then(|result| result.map_err(|error| error.to_string()));
        match dispatched {
            Ok(thread) => Ok(Self {
                runtime,
                signal: Mutex::new(Some(signal)),
                finished: receipt,
                helper: Arc::new(HelperJoin {
                    thread: Mutex::new(Some(thread)),
                    reservations,
                    finished: watch::channel(None).0,
                    scheduled: AtomicBool::new(false),
                    changed: Condvar::new(),
                }),
            }),
            Err(message) => {
                let cleanup = match receipt.borrow().as_ref() {
                    Some(Ok(())) => String::new(),
                    Some(Err(error)) => format!("; {error}"),
                    None => "; retirement owner ended without a cleanup receipt".into(),
                };
                Err(SystemError::InitializationError(format!(
                    "Actix retirement worker initialization failed: {message}{cleanup}"
                )))
            }
        }
    }

    pub(super) fn request(&self) {
        let signal = self.signal.lock().unwrap().take();
        drop(signal);
    }

    #[cfg(test)]
    pub(super) fn receipt(&self) -> impl Future<Output = ActorResult<()>> + Send + 'static {
        let receipt = self.finished.clone();
        async move { wait(receipt).await.map_err(ActorError::InternalError) }
    }

    pub(super) async fn join(&self) -> ActorResult<()> {
        let workers = wait(self.finished.clone()).await;
        self.helper.dispatch_in_lifecycle(&self.runtime);
        let helper = wait(self.helper.finished.subscribe()).await;
        Self::combine(workers, helper)
    }

    #[cfg(test)]
    async fn join_with(&self, dispatch: impl FnOnce(HelperJoinJob)) -> ActorResult<()> {
        let workers = wait(self.finished.clone()).await;
        // Never await the helper until its complete business-worker join pass
        // has finished. Cancellation cannot make another observer bypass it.
        self.helper.dispatch(dispatch);
        let helper = wait(self.helper.finished.subscribe()).await;
        Self::combine(workers, helper)
    }

    pub(super) fn join_blocking(&self) -> ActorResult<()> {
        self.request();
        // A cancelled executor may still own a queued blocking job. Claim the
        // handle here before that job starts; its later Drop observes this join.
        self.helper.join_blocking();
        let workers = self.finished.borrow().clone().unwrap_or_else(|| {
            Err("Actix retirement owner ended without a cleanup receipt".into())
        });
        let helper = self.helper.finished.borrow().clone().unwrap();
        Self::combine(workers, helper)
    }

    fn combine(workers: Exit, helper: Exit) -> ActorResult<()> {
        match (workers, helper) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(message), Ok(())) | (Ok(()), Err(message)) => {
                Err(ActorError::InternalError(message))
            }
            (Err(workers), Err(helper)) => {
                Err(ActorError::InternalError(format!("{workers}; {helper}")))
            }
        }
    }

    pub(super) fn owned_threads(&self, workers: usize) -> usize {
        let workers = if self.finished.borrow().is_none() {
            workers
        } else {
            0
        };
        workers + usize::from(self.helper.finished.borrow().is_none())
    }
}
impl Drop for Retirement {
    fn drop(&mut self) {
        self.request();
        if !self.helper.scheduled.load(Ordering::Acquire) {
            let helper = self.helper.clone();
            let workers = self.finished.clone();
            let runtime = self.runtime.clone();
            // The last pool owner may run on an owned worker or without any
            // current runtime. Keep its helper handle until an independent join.
            self.runtime.spawn(async move {
                let _ = wait(workers).await;
                helper.dispatch_in_lifecycle(&runtime);
            });
        }
    }
}

#[cfg(test)]
#[path = "pool_retirement/contracts.rs"]
mod contracts;

#[cfg(test)]
#[path = "pool_retirement/helper_contracts.rs"]
mod helper_contracts;

#[cfg(test)]
#[path = "pool_retirement/saturation_contracts.rs"]
mod saturation_contracts;
