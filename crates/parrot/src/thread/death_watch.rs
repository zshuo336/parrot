//! Managed Thread subscriptions. Legacy and common notifications have separate
//! bounded channels so old Terminated consumers remain source compatible.
use super::{
    config::BackpressureStrategy, error::SystemError, mailbox::WeakMailboxRef, system::Terminated,
};
use parrot_api::{
    address::ActorRef,
    context::ActorTerminated,
    death_watch::{DeathWatch, TerminationRecipient},
    errors::ActorError,
    supervisor::DeathReason,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

pub(crate) const WATCH_LIMIT: usize = 1024;
#[derive(Clone)]
pub(crate) struct ThreadWatches {
    pub common: DeathWatch,
    pub legacy: DeathWatch,
}
impl ThreadWatches {
    pub fn dynamic(cell: &Arc<Mutex<WeakMailboxRef>>) -> Self {
        let watches = Self {
            common: DeathWatch::new(WATCH_LIMIT),
            legacy: DeathWatch::new(WATCH_LIMIT),
        };
        for (handle, legacy) in [(&watches.common, false), (&watches.legacy, true)] {
            handle
                .set_recipient(Arc::new(MailboxRecipient {
                    cell: Arc::downgrade(cell),
                    legacy,
                }))
                .expect("fresh recipient");
        }
        watches
    }
    pub fn close(&self) {
        self.common.close();
        self.legacy.close();
    }
    pub fn suspend(&self) {
        self.common.suspend();
        self.legacy.suspend();
    }
    pub fn resume(&self) -> ActorResult<()> {
        self.common.resume()?;
        self.legacy.resume()
    }
}
struct MailboxRecipient {
    cell: Weak<Mutex<WeakMailboxRef>>,
    legacy: bool,
}
impl TerminationRecipient for MailboxRecipient {
    fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let cell = self.cell.upgrade().ok_or(ActorError::Stopped)?;
            let weak = cell.lock().unwrap().clone();
            let mailbox = weak.upgrade().ok_or(ActorError::Stopped)?;
            let message: BoxedMessage = if self.legacy {
                Box::new(Terminated {
                    path: event.path,
                    reason: event.reason,
                })
            } else {
                Box::new(event)
            };
            mailbox
                .push_with_priority(message, BackpressureStrategy::Block, true)
                .await
                .map(|_| ())
                .map_err(|error| ActorError::InternalError(error.to_string()))
        })
    }
}
struct LegacyTypedRecipient(Weak<dyn ActorRef>);
impl TerminationRecipient for LegacyTypedRecipient {
    fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let actor = self.0.upgrade().ok_or(ActorError::Stopped)?;
            actor
                .deliver(Box::new(Terminated {
                    path: event.path,
                    reason: event.reason,
                }))
                .await
        })
    }
}
pub(crate) fn install_typed_legacy(handle: &DeathWatch, reference: &Arc<dyn ActorRef>) {
    handle
        .set_recipient(Arc::new(LegacyTypedRecipient(Arc::downgrade(reference))))
        .expect("fresh typed legacy recipient");
}
pub(crate) async fn notify_legacy(
    handle: &DeathWatch,
    path: &str,
    reason: DeathReason,
    timeout: Duration,
) -> Result<(), SystemError> {
    handle
        .terminated_checked(
            ActorTerminated {
                generation: handle.id(),
                path: path.into(),
                reason,
            },
            timeout,
        )
        .await
        .map_err(|error| match error {
            ActorError::Timeout => {
                SystemError::Timeout("termination notification deadline expired".into())
            }
            ActorError::TimeoutDetail(message) => SystemError::Timeout(message),
            error => SystemError::ShutdownError(error.to_string()),
        })
}
