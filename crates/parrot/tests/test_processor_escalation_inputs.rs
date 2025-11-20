use parrot::thread::{
    ActorProcessor, ProcessorInterface, ThreadActor, ThreadActorConfig, ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
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
fn processor() -> ActorProcessor<Echo> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let processor = {
        let _entered = runtime.enter();
        ActorProcessor::new(
            ThreadActor::new_for_test(Echo),
            ThreadContext::new_for_test("/escalation-input"),
            "/escalation-input".into(),
            ThreadActorConfig::default(),
        )
    };
    drop(runtime);
    processor
}
struct Recursive;
impl Drop for Recursive {
    fn drop(&mut self) {
        panic!("escalation input panic payload destructor failed");
    }
}
#[derive(Debug)]
struct BusinessError {
    drops: Arc<AtomicUsize>,
    recursive: bool,
}
impl fmt::Display for BusinessError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("input diagnostic invoked user Display");
    }
}
impl std::error::Error for BusinessError {}
impl Drop for BusinessError {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(Recursive);
        }
        panic!("escalation input error destructor failed");
    }
}
fn error(drops: &Arc<AtomicUsize>, recursive: bool) -> Arc<ActorError> {
    Arc::new(ActorError::Other(anyhow::Error::new(BusinessError {
        drops: drops.clone(),
        recursive,
    })))
}
fn no_unwind(action: impl FnOnce(), diagnosis: &str) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(action)) {
        std::mem::forget(payload);
        panic!("{diagnosis}");
    }
}
fn rejected(cancelled: bool, recursive: bool) {
    let processor = processor();
    if cancelled {
        processor.cancel_processing();
    } else {
        processor.fail_from_escalation(Arc::new(ActorError::Stopped));
    }
    let drops = Arc::new(AtomicUsize::new(0));
    no_unwind(
        || assert!(!processor.begin_escalation(error(&drops, recursive))),
        "processor rejected escalation error escaped admission",
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(!processor.begin_escalation(Arc::new(ActorError::Stopped)));
}
#[test]
fn failed_processor_isolates_rejected_error_panic() {
    rejected(false, false);
}
#[test]
fn failed_processor_isolates_rejected_recursive_panic() {
    rejected(false, true);
}
#[test]
fn cancelled_processor_isolates_rejected_error_panic() {
    rejected(true, false);
}
#[test]
fn cancelled_processor_isolates_rejected_recursive_panic() {
    rejected(true, true);
}
fn retained(recursive: bool) {
    let processor = processor();
    let drops = Arc::new(AtomicUsize::new(0));
    assert!(processor.begin_escalation(error(&drops, recursive)));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    no_unwind(
        || drop(processor),
        "processor retained escalation error escaped destruction",
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[test]
fn processor_final_release_isolates_pending_error_panic() {
    retained(false);
}
#[test]
fn processor_final_release_isolates_pending_recursive_panic() {
    retained(true);
}
