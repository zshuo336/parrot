use super::*;
use parrot_api::trace::TraceContext;
use std::sync::Barrier;

struct Probe;
impl Actor for Probe {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
fn processor() -> Arc<ActorProcessor<Probe>> {
    Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(Probe),
        ThreadContext::new_for_test("/failure-identity"),
        "/failure-identity".into(),
        Default::default(),
    ))
}

#[derive(Debug)]
struct Original(u64);
impl std::fmt::Display for Original {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "original {}", self.0)
    }
}
impl std::error::Error for Original {}

#[tokio::test]
async fn first_error_keeps_exact_arc_type_and_trace_context() {
    for traced in [false, true] {
        let processor = processor();
        processor.initialize_and_start().await.unwrap();
        let original = Arc::new(ActorError::Other(anyhow::Error::new(Original(83))));
        let context = traced.then(TraceContext::root);
        parrot_api::trace::in_scope(context.clone(), || {
            processor.fail_from_escalation(original.clone());
        });
        TraceContext::root().in_scope(|| {
            processor.fail_from_escalation(Arc::new(ActorError::Panic("later".into())));
        });
        {
            let first = processor.failure_cause.lock().unwrap();
            let first = first.as_ref().unwrap();
            assert!(Arc::ptr_eq(first.error.error(), &original));
            assert_eq!(first.context, context);
            let ActorError::Other(error) = first.error.error().as_ref() else {
                panic!("concrete business error category was replaced");
            };
            assert_eq!(error.downcast_ref::<Original>().unwrap().0, 83);
        }
        assert!(processor.has_failed());
        processor.retire_storage().await.unwrap();
    }
}

#[derive(Debug)]
struct LaterError {
    drops: Arc<AtomicUsize>,
}
impl std::fmt::Display for LaterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("concurrent failure")
    }
}
impl std::error::Error for LaterError {}
impl Drop for LaterError {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn concurrent_later_errors_dispose_once_and_keep_first_cause() {
    let processor = processor();
    processor.initialize_and_start().await.unwrap();
    let first = Arc::new(ActorError::Panic("first".into()));
    processor.fail_from_escalation(first.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let processor = processor.clone();
        let barrier = barrier.clone();
        let drops = drops.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..8 {
                let error = ActorError::Other(anyhow::Error::new(LaterError {
                    drops: drops.clone(),
                }));
                processor.fail_from_escalation(Arc::new(error));
            }
        }));
    }
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(drops.load(Ordering::SeqCst), 16);
    assert!(Arc::ptr_eq(
        processor
            .failure_cause
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .error
            .error(),
        &first
    ));
    processor.retire_storage().await.unwrap();
}
