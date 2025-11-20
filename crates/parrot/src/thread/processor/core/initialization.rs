use super::*;

pub(super) struct InitializationAttempt<'a, A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    processor: &'a ActorProcessor<A>,
    finished: bool,
}
impl<'a, A> InitializationAttempt<'a, A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    pub(super) fn new(processor: &'a ActorProcessor<A>) -> Self {
        Self {
            processor,
            finished: false,
        }
    }
    pub(super) fn finish(mut self) {
        self.finished = true;
    }
}
impl<A> Drop for InitializationAttempt<'_, A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    fn drop(&mut self) {
        if !self.finished {
            self.processor
                .stats
                .errors_encountered
                .fetch_add(1, Ordering::SeqCst);
            let error = self
                .processor
                .stage_failures
                .result()
                .err()
                .unwrap_or_else(|| {
                    ActorError::InitializationError(format!(
                        "actor initialization at {} was abandoned before completion",
                        self.processor.path
                    ))
                });
            self.processor.record_failure(Arc::new(error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    struct PendingInit;
    impl Future for PendingInit {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }
    impl Drop for PendingInit {
        fn drop(&mut self) {
            panic!("abandoned init disposal failed");
        }
    }
    struct Probe;
    impl Actor for Probe {
        type Config = parrot_api::actor::EmptyConfig;
        type Context = ThreadContext<Self>;
        fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(PendingInit)
        }
        fn receive_message<'a>(
            &'a mut self,
            _: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { unreachable!() })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    #[tokio::test]
    async fn abandoned_attempt_retains_real_stage_disposal_failure_class() {
        let processor = ActorProcessor::new(
            ThreadActor::new_for_test(Probe),
            ThreadContext::new_for_test("/abandoned-initialization"),
            "/abandoned-initialization".into(),
            Default::default(),
        );
        let mut attempt = Box::pin(processor.initialize_actor());
        assert!(
            attempt
                .as_mut()
                .poll(&mut Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        drop(attempt);
        assert!(matches!(
            processor.stage_failures.result(),
            Err(ActorError::Panic(_))
        ));
        let failure = processor.failure_cause.lock().unwrap();
        let error = &failure
            .as_ref()
            .expect("abandoned initialization has no failure receipt")
            .error;
        assert!(
            matches!(error.error().as_ref(), ActorError::Panic(message) if message.contains("abandoned init disposal failed")),
            "real stage failure class was overwritten: {error:?}"
        );
    }
}
