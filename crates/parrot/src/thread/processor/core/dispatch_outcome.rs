//! Each unpublished reply is an independent disposal owner, including when a
//! batch's supervision waiter is cancelled or a reply receiver has disappeared.
use super::*;
use tokio::sync::oneshot;

pub(super) struct DispatchOutcome {
    result: Option<ActorResult<BoxedMessage>>,
    failures: Arc<StageFailures>,
}
impl DispatchOutcome {
    pub(super) fn new(result: ActorResult<BoxedMessage>, failures: &Arc<StageFailures>) -> Self {
        Self {
            result: Some(result),
            failures: failures.clone(),
        }
    }
    pub(super) fn result(&self) -> &ActorResult<BoxedMessage> {
        self.result.as_ref().expect("unpublished dispatch result")
    }
    pub(super) fn take(&mut self) -> ActorResult<BoxedMessage> {
        self.result.take().expect("unpublished dispatch result")
    }
    pub(super) fn replace(&mut self, result: ActorResult<BoxedMessage>) {
        debug_assert!(self.result.is_none());
        self.result = Some(result);
    }
    pub(super) fn publish(
        mut self,
        reply: Option<oneshot::Sender<ActorResult<BoxedMessage>>>,
    ) -> Result<(), String> {
        if let Some(reply) = reply {
            let result = self.take();
            if let Err(result) = reply.send(result) {
                self.result = Some(result);
            }
        }
        self.retire()
    }
    fn retire(&mut self) -> Result<(), String> {
        crate::owned_payload::PublicationInput::new(self.result.take())
            .retire()
            .map_err(|error| {
                let message = format!(
                    "dispatch outcome disposal failed: {}",
                    crate::error_receipt::actor_summary(&error)
                );
                self.failures
                    .record(crate::error_receipt::ActorReceipt::from_error(
                        ActorError::Panic(message.clone()),
                    ));
                message
            })
    }
}
impl Drop for DispatchOutcome {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

impl<A> ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    pub(super) fn publish_dispatch_outcome(
        &self,
        outcome: DispatchOutcome,
        reply: Option<oneshot::Sender<ActorResult<BoxedMessage>>>,
    ) -> Result<(), SystemError> {
        let failed = outcome.result().is_err();
        outcome.publish(reply).map_err(|message| {
            if !failed {
                self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            }
            self.record_failure(Arc::new(ActorError::Panic(message.clone())));
            SystemError::WorkerStateError(message)
        })
    }
}
