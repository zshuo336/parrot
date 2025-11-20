//! Stage disposal receipts outlive cancellation and veto generation reuse.
use crate::error_receipt::ActorReceipt;
use parrot_api::{
    errors::ActorError,
    trace::{SpanGuard, SpanStatus},
    types::ActorResult,
};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub(crate) struct StageFailures(Mutex<Option<ActorReceipt>>);
impl StageFailures {
    pub(crate) fn record(&self, error: ActorReceipt) {
        let mut first = self.0.lock().unwrap();
        if first.is_none() {
            *first = Some(error);
        } else {
            drop(first);
            drop(error);
        }
    }
    pub(crate) fn failed(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }
    pub(crate) fn result(&self) -> ActorResult<()> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .map_or(Ok(()), |error| Err(error.into_error()))
    }
}

pub(crate) struct StageCleanup {
    failures: Arc<StageFailures>,
    first: Option<ActorReceipt>,
    cancelled: bool,
    batch: Option<parrot_api::batch_dispatch::BatchCleanup>,
    pub(crate) recording: Option<SpanGuard>,
}
impl StageCleanup {
    pub(crate) fn new(failures: Arc<StageFailures>) -> Self {
        Self {
            failures,
            first: None,
            cancelled: false,
            batch: None,
            recording: None,
        }
    }
    pub(crate) fn borrow(&mut self) -> crate::typed_supervision::DispatchCleanup<'_> {
        crate::typed_supervision::DispatchCleanup::new(&mut self.first, &mut self.cancelled)
    }
    pub(crate) fn batch(&mut self) -> parrot_api::batch_dispatch::BatchCleanup {
        self.batch.get_or_insert_with(Default::default).clone()
    }
    pub(crate) fn finish_recording(&mut self, status: SpanStatus) {
        if let Some(recording) = self.recording.take() {
            recording.finish(if self.first.is_some() || self.failures.failed() {
                SpanStatus::Panicked
            } else {
                status
            });
        }
    }
}
impl Drop for StageCleanup {
    fn drop(&mut self) {
        if let Some(message) = self.batch.as_ref().and_then(|batch| batch.failure()) {
            self.first
                .get_or_insert_with(|| ActorReceipt::from_error(ActorError::Panic(message)));
        }
        self.finish_recording(if std::thread::panicking() {
            SpanStatus::Panicked
        } else {
            SpanStatus::Cancelled
        });
        if let Some(error) = self.first.take() {
            self.failures.record(error);
        }
    }
}

pub(crate) fn outcome<T>(result: Result<ActorResult<T>, String>) -> ActorResult<T> {
    result.unwrap_or_else(|message| Err(ActorError::Panic(message)))
}
