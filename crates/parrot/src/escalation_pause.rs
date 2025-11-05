//! A bounded first-cause pause, owned by one dynamic actor generation.
use parrot_api::errors::ActorError;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Debug, Default)]
pub(crate) struct EscalationPause {
    cause: Mutex<Option<Arc<ActorError>>>,
    changed: Notify,
}
impl EscalationPause {
    pub(crate) fn begin(&self, error: Arc<ActorError>) -> bool {
        let mut cause = self.cause.lock().unwrap();
        if cause.is_some() {
            return false;
        }
        *cause = Some(error);
        self.changed.notify_waiters();
        true
    }
    pub(crate) fn pending(&self) -> Option<Arc<ActorError>> {
        self.cause.lock().unwrap().clone()
    }
    pub(crate) fn resume(&self, error: &Arc<ActorError>) {
        let mut cause = self.cause.lock().unwrap();
        if cause
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, error))
        {
            cause.take();
            self.changed.notify_waiters();
        }
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
            if self.pending().is_some() {
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
