//! A bounded first-cause pause, owned by one dynamic actor generation.
use crate::error_receipt::ActorReceipt;
use parrot_api::errors::ActorError;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Debug, Default)]
pub(crate) struct EscalationPause {
    cause: Mutex<Option<ActorReceipt>>,
    changed: Notify,
}

#[cfg(test)]
mod ownership_tests;

impl EscalationPause {
    pub(crate) fn begin(&self, error: Arc<ActorError>) -> bool {
        let error = ActorReceipt::new(error);
        let accepted = {
            let mut cause = self.cause.lock().unwrap();
            if cause.is_some() {
                false
            } else {
                *cause = Some(error.clone());
                true
            }
        };
        // Wakers and rejected error destructors may reenter this pause.
        if accepted {
            self.changed.notify_waiters();
        }
        drop(error);
        accepted
    }
    pub(crate) fn pending(&self) -> Option<Arc<ActorError>> {
        self.cause
            .lock()
            .unwrap()
            .as_ref()
            .map(|cause| cause.error().clone())
    }
    pub(crate) fn is_pending(&self) -> bool {
        self.cause.lock().unwrap().is_some()
    }
    pub(crate) fn resume(&self, error: &Arc<ActorError>) {
        let removed = {
            let mut cause = self.cause.lock().unwrap();
            if cause
                .as_ref()
                .is_some_and(|current| current.same_error(error))
            {
                cause.take()
            } else {
                None
            }
        };
        if removed.is_some() {
            self.changed.notify_waiters();
        }
        drop(removed);
    }
    pub(crate) async fn ready(&self) {
        self.guard(std::future::ready(())).await;
    }
    pub(crate) async fn guard<F: std::future::Future>(&self, stage: F) -> F::Output {
        tokio::pin!(stage);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_pending() {
                changed.await;
                continue;
            }
            tokio::select! {
                biased;
                _ = changed => {},
                output = &mut stage => return output,
            }
        }
    }
}
