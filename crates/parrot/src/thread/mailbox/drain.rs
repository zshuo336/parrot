//! Process-wide drain admission, retained through the actual native join.
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const CAPACITY: usize = 32;

pub(super) struct DrainPermit(Option<OwnedSemaphorePermit>);

impl From<OwnedSemaphorePermit> for DrainPermit {
    fn from(permit: OwnedSemaphorePermit) -> Self {
        Self(Some(permit))
    }
}

impl Drop for DrainPermit {
    fn drop(&mut self) {
        crate::owned_payload::release_quota(self.0.take());
    }
}

struct Join {
    thread: Option<std::thread::JoinHandle<()>>,
    _permit: DrainPermit,
}

impl Drop for Join {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take()
            && let Err(payload) = thread.join()
        {
            let _ = crate::owned_payload::panic_message(payload);
        }
    }
}

pub(super) enum Reservation {
    Inline,
    Native {
        permit: DrainPermit,
        runtime: tokio::runtime::Handle,
    },
}

impl Reservation {
    pub(super) async fn acquire() -> Self {
        // A destructor can synchronously close another mailbox. It must not
        // wait for a slot still held by its own outer disposal.
        if crate::owned_payload::DisposalScope::active() {
            return Self::Inline;
        }
        match crate::root_cohort::lifecycle_runtime() {
            Ok(runtime) => {
                static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
                let slots = SLOTS.get_or_init(|| Arc::new(Semaphore::new(CAPACITY)));
                Self::Native {
                    permit: slots.clone().acquire_owned().await.unwrap().into(),
                    runtime,
                }
            }
            // Match the existing OS-thread-exhaustion contract: retain values
            // and complete synchronously outside all mailbox bookkeeping locks.
            Err(_) => Self::Inline,
        }
    }

    pub(super) fn run(self, job: impl FnOnce() + Send + 'static) {
        let Self::Native { permit, runtime } = self else {
            job();
            return;
        };
        // Thread creation failure drops the submitted closure. Keep its user
        // values in a separate owner until creation is known to have succeeded.
        let retained = Arc::new(Mutex::new(Some(job)));
        let detached = retained.clone();
        match std::thread::Builder::new()
            .name("parrot-mailbox-drain".into())
            .spawn(move || {
                // This thread only disposes values. Keep the reentry marker
                // through TLS teardown, which happens after this closure exits.
                std::mem::forget(crate::owned_payload::DisposalScope::enter());
                let job = detached.lock().unwrap().take().unwrap();
                if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
                    let _ = crate::owned_payload::panic_message(payload);
                }
            }) {
            Ok(thread) => {
                // One lease bounds both the native thread and its queued or
                // active join, including thread-local destructor completion.
                let owner = Arc::new(Mutex::new(Some(Join {
                    thread: Some(thread),
                    _permit: permit,
                })));
                let queued = owner.clone();
                let dispatch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runtime.spawn_blocking(move || {
                        let join = queued.lock().unwrap().take();
                        drop(join);
                    })
                }));
                if let Err(payload) = dispatch {
                    let join = owner.lock().unwrap().take();
                    drop(join);
                    let _ = crate::owned_payload::panic_message(payload);
                }
            }
            Err(_) => {
                let job = retained.lock().unwrap().take().unwrap();
                job();
                drop(permit);
            }
        }
    }
}

#[cfg(test)]
mod permit_wake_contracts;
