//! Direct callers need input ownership before the first poll as well.
use crate::{
    error_receipt::ActorReceipt, owned_payload::PublicationInput, thread::stage::StageFailures,
};
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedMessage},
};
use std::sync::Arc;

pub(super) struct MessageInput {
    message: Option<BoxedMessage>,
    failures: Arc<StageFailures>,
}
impl MessageInput {
    pub(super) fn new(message: BoxedMessage, failures: Arc<StageFailures>) -> Self {
        Self {
            message: Some(message),
            failures,
        }
    }
    pub(super) fn value(&self) -> &BoxedMessage {
        self.message.as_ref().expect("unconsumed direct message")
    }
    pub(super) fn take(&mut self) -> BoxedMessage {
        self.message.take().expect("unconsumed direct message")
    }
    pub(super) fn retire(mut self) -> ActorResult<()> {
        self.dispose()
    }
    fn dispose(&mut self) -> ActorResult<()> {
        record_cleanup(
            PublicationInput::new(self.message.take()).retire(),
            &self.failures,
            "direct message input disposal failed",
        )
    }
}
impl Drop for MessageInput {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}

pub(super) fn record_cleanup(
    result: ActorResult<()>,
    failures: &StageFailures,
    phase: &str,
) -> ActorResult<()> {
    if let Err(error) = result {
        failures.record(ActorReceipt::from_error(ActorError::Panic(format!(
            "{phase}: {}",
            crate::error_receipt::actor_summary(&error)
        ))));
        failures.result()
    } else {
        Ok(())
    }
}
