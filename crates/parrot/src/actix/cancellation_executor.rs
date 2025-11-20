//! Prestarted, bounded handoff for cancellation that runs on an owned worker.
use parrot_api::system::SystemError;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

type Job = Box<dyn FnOnce() + Send>;
const CAPACITY: usize = 4096;
const WORKERS: usize = 2;

struct Executor {
    sender: Option<mpsc::SyncSender<Job>>,
    threads: Vec<std::thread::JoinHandle<()>>,
    slots: Arc<Semaphore>,
}

impl Executor {
    fn with_spawner(
        capacity: usize,
        mut spawn: impl FnMut(usize, Job) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Arc<Self>, String> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut executor = Self {
            sender: Some(sender),
            threads: Vec::with_capacity(WORKERS),
            slots: Arc::new(Semaphore::new(capacity)),
        };
        for index in 0..WORKERS {
            let receiver = receiver.clone();
            let job: Job = Box::new(move || {
                loop {
                    let received = receiver
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .recv();
                    let Ok(job) = received else { break };
                    if let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                    {
                        let _ = crate::owned_payload::panic_message(payload);
                    }
                }
            });
            let thread =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| spawn(index, job)))
                    .map_err(crate::owned_payload::panic_message)
                    .and_then(|result| result.map_err(|error| error.to_string()))?;
            executor.threads.push(thread);
        }
        Ok(Arc::new(executor))
    }

    fn reserve(self: &Arc<Self>) -> Result<Arc<Reservation>, SystemError> {
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            SystemError::InitializationError(format!(
                "Actix cancellation capacity exhausted ({CAPACITY} live or retiring systems)"
            ))
        })?;
        Ok(Arc::new(Reservation {
            executor: self.clone(),
            _permit: permit,
        }))
    }
}

#[cfg(test)]
#[path = "cancellation_executor/contracts.rs"]
mod contracts;

impl Drop for Executor {
    fn drop(&mut self) {
        // Failed partial startup and isolated test executors close their queues
        // before joining. The production executor stays owned for process life.
        drop(self.sender.take());
        for thread in self.threads.drain(..) {
            if let Err(payload) = thread.join() {
                let _ = crate::owned_payload::panic_message(payload);
            }
        }
    }
}

pub(super) struct Reservation {
    executor: Arc<Executor>,
    _permit: OwnedSemaphorePermit,
}
impl Reservation {
    pub(super) fn new() -> Result<Arc<Self>, SystemError> {
        static SHARED: OnceLock<Mutex<Option<Arc<Executor>>>> = OnceLock::new();
        let mut shared = SHARED.get_or_init(|| Mutex::new(None)).lock().unwrap();
        if shared.is_none() {
            *shared = Some(
                Executor::with_spawner(CAPACITY, |index, job| {
                    std::thread::Builder::new()
                        .name(format!("parrot-actix-cancel-{index}"))
                        .spawn(job)
                })
                .map_err(|error| {
                    SystemError::InitializationError(format!(
                        "Actix cancellation executor initialization failed: {error}"
                    ))
                })?,
            );
        }
        shared.as_ref().unwrap().reserve()
    }

    pub(super) fn submit(self: Arc<Self>, job: impl FnOnce() + Send + 'static) {
        let sender = self.executor.sender.as_ref().unwrap().clone();
        // One terminal owner per registry, and its startup reservation stays in
        // the job. Thus the queue cannot be full, even when both workers block.
        let work: Job = Box::new(move || {
            let _reservation = self;
            job();
        });
        if let Err(error) = sender.try_send(work) {
            // The process-owned executor never closes; retain cleanup even if
            // an internal queue invariant is violated.
            match error {
                mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job) => job(),
            }
        }
    }
}
