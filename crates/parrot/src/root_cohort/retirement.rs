//! Process-owned driver for root retirement, independent of business hosts.
use super::*;
use std::sync::mpsc;
use tokio::{
    runtime::Runtime,
    sync::{OwnedSemaphorePermit, oneshot, watch},
};

const CAPACITY: usize = 65536;
type Job = Box<dyn FnOnce() + Send>;
type Completion = watch::Receiver<Option<Result<(), ActorReceipt>>>;

struct Driver {
    runtime: Handle,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    slots: Arc<Semaphore>,
}
impl Driver {
    fn shared() -> ActorResult<Arc<Self>> {
        static SHARED: OnceLock<Mutex<Option<Arc<Driver>>>> = OnceLock::new();
        let mut shared = SHARED.get_or_init(|| Mutex::new(None)).lock().unwrap();
        if shared.is_none() {
            *shared = Some(
                Self::start(
                    CAPACITY,
                    || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .max_blocking_threads(512)
                            .build()
                    },
                    |job| {
                        std::thread::Builder::new()
                            .name("parrot-root-retire".into())
                            .spawn(job)
                    },
                )
                .map_err(|error| {
                    ActorError::InitializationError(format!(
                        "lifecycle retirement driver initialization failed: {error}"
                    ))
                })?,
            );
        }
        Ok(shared.as_ref().unwrap().clone())
    }

    fn start(
        capacity: usize,
        build: impl FnOnce() -> std::io::Result<Runtime> + Send + 'static,
        spawn: impl FnOnce(Job) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Arc<Self>, String> {
        let (ready, receive_ready) = mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel();
        let job: Job = Box::new(move || {
            let runtime = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build))
                .map_err(crate::owned_payload::panic_message)
                .and_then(|result| result.map_err(|error| error.to_string()));
            match runtime {
                Ok(runtime) => {
                    if ready.send(Ok(runtime.handle().clone())).is_ok() {
                        runtime.block_on(async {
                            let _ = stopped.await;
                        });
                    }
                    drop(runtime);
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            }
        });
        let thread = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| spawn(job)))
            .map_err(crate::owned_payload::panic_message)
            .and_then(|result| result.map_err(|error| error.to_string()))?;
        match receive_ready.recv() {
            Ok(Ok(runtime)) => Ok(Arc::new(Self {
                runtime,
                stop: Some(stop),
                thread: Some(thread),
                slots: Arc::new(Semaphore::new(capacity)),
            })),
            result => {
                let error = match result {
                    Ok(Err(error)) => error,
                    Err(error) => error.to_string(),
                    Ok(Ok(_)) => unreachable!(),
                };
                drop(stop);
                if let Err(payload) = thread.join() {
                    let _ = crate::owned_payload::panic_message(payload);
                }
                Err(error)
            }
        }
    }
    fn reserve(self: &Arc<Self>) -> ActorResult<Arc<Reservation>> {
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            ActorError::InitializationError(format!(
                "root retirement capacity exhausted ({CAPACITY} retained or retiring groups)"
            ))
        })?;
        Ok(Arc::new(Reservation {
            driver: self.clone(),
            _permit: permit,
        }))
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        // Production is held by SHARED for process life. Startup rollback and
        // isolated tests close the driver, then join all of its runtime threads.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take()
            && let Err(payload) = thread.join()
        {
            let _ = crate::owned_payload::panic_message(payload);
        }
    }
}

struct Reservation {
    driver: Arc<Driver>,
    _permit: OwnedSemaphorePermit,
}
impl Reservation {
    fn new() -> ActorResult<Arc<Self>> {
        Driver::shared()?.reserve()
    }
}

pub(super) fn runtime() -> ActorResult<Handle> {
    Ok(Driver::shared()?.runtime.clone())
}

pub(super) struct Retirement {
    reservation: Arc<Reservation>,
    completion: Mutex<Option<Completion>>,
}
struct Publish(watch::Sender<Option<Result<(), ActorReceipt>>>);
impl Drop for Publish {
    fn drop(&mut self) {
        self.0.send_if_modified(|value| {
            if value.is_some() {
                return false;
            }
            *value = Some(Err(ActorReceipt::from_error(ActorError::InternalError(
                "root retirement task ended without completing disposal".into(),
            ))));
            true
        });
    }
}
impl Retirement {
    pub(super) fn new() -> ActorResult<Arc<Self>> {
        Ok(Arc::new(Self {
            reservation: Reservation::new()?,
            completion: Mutex::new(None),
        }))
    }
    pub(super) fn start(
        &self,
        scope: Arc<ChildScope>,
        owner: Weak<RootCohorts>,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        let completion = {
            let mut slot = self.completion.lock().unwrap();
            if let Some(completion) = &*slot {
                completion.clone()
            } else {
                let (finished, completion) = watch::channel(None);
                *slot = Some(completion.clone());
                let publish = Publish(finished);
                let reservation = self.reservation.clone();
                drop(self.reservation.driver.runtime.spawn(async move {
                    let _reservation = reservation;
                    let result = scope.join().await.map_err(ActorReceipt::from_error);
                    if let Some(owner) = owner.upgrade() {
                        owner.remove(&scope);
                    }
                    publish.0.send_replace(Some(result));
                }));
                completion
            }
        };
        Box::pin(async move { wait(completion).await })
    }
}
async fn wait(mut completion: Completion) -> ActorResult<()> {
    loop {
        if let Some(result) = completion.borrow_and_update().clone() {
            return result.map_err(ActorReceipt::into_error);
        }
        completion.changed().await.map_err(|_| {
            ActorError::InternalError("root retirement driver lost its terminal receipt".into())
        })?;
    }
}

#[cfg(test)]
mod saturation_contracts;
#[cfg(test)]
mod tests;
