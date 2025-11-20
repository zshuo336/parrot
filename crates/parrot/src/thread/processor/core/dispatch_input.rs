//! Protect dequeued inputs until the user hook accepts their ownership.
use super::*;

pub(super) struct DispatchInput {
    message: Option<BoxedMessage>,
    failures: Arc<StageFailures>,
    _execution: execution::InputLease,
}
impl DispatchInput {
    pub(super) fn new(
        message: BoxedMessage,
        failures: &Arc<StageFailures>,
        executions: &Arc<execution::Executions>,
    ) -> Self {
        Self {
            message: Some(message),
            failures: failures.clone(),
            _execution: executions.input(),
        }
    }
    pub(super) fn value(&self) -> &BoxedMessage {
        self.message.as_ref().expect("unconsumed dispatch input")
    }
    pub(super) fn take(&mut self) -> BoxedMessage {
        self.message.take().expect("unconsumed dispatch input")
    }
}
impl Drop for DispatchInput {
    fn drop(&mut self) {
        if let Err(error) =
            crate::owned_payload::PublicationInput::new(self.message.take()).retire()
        {
            self.failures
                .record(crate::error_receipt::ActorReceipt::from_error(
                    ActorError::Panic(format!(
                        "dispatch input disposal failed: {}",
                        crate::error_receipt::actor_summary(&error)
                    )),
                ));
        }
    }
}
