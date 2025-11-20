use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ProcessorInterface, ThreadActor, ThreadActorConfig,
    ThreadContext,
    envelope::AskEnvelope,
    mailbox::{Mailbox, mpsc::MpscMailbox},
    processor::ProcessorStatus,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
fn processor() -> Arc<ActorProcessor<Echo>> {
    Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(Echo),
        ThreadContext::new_for_test("/failure-recording"),
        "/failure-recording".into(),
        ThreadActorConfig::default(),
    ))
}
fn without_executor() -> Arc<ActorProcessor<Echo>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let processor = {
        let _entered = runtime.enter();
        processor()
    };
    drop(runtime);
    processor
}

#[derive(Clone, Copy)]
enum Disposal {
    Clean,
    Panic,
    RecursivePanic,
}
struct RecursivePanic;
impl Drop for RecursivePanic {
    fn drop(&mut self) {
        panic!("failure error panic payload destructor failed");
    }
}
#[derive(Default)]
struct Observations {
    drops: AtomicUsize,
    reentered: AtomicUsize,
    diagnostics: AtomicUsize,
    finished: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}
struct UserError {
    observations: Arc<Observations>,
    reenter: Option<Weak<ActorProcessor<Echo>>>,
    disposal: Disposal,
}
impl fmt::Debug for UserError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.observations.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("recording invoked user Debug");
    }
}
impl fmt::Display for UserError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.observations.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("recording invoked user Display");
    }
}
impl std::error::Error for UserError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.observations.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("recording invoked user source");
    }
}
impl Drop for UserError {
    fn drop(&mut self) {
        self.observations.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(processor) = self.reenter.as_ref().and_then(Weak::upgrade) {
            assert!(
                processor.has_failed(),
                "failure not published before disposal"
            );
            assert!(!processor.begin_escalation(Arc::new(ActorError::Stopped)));
            self.observations.reentered.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(finished) = self.observations.finished.lock().unwrap().take() {
            let _ = finished.send(());
        }
        match self.disposal {
            Disposal::Clean => {}
            Disposal::Panic => panic!("failure error destructor failed"),
            Disposal::RecursivePanic => std::panic::panic_any(RecursivePanic),
        }
    }
}
fn error(
    observations: &Arc<Observations>,
    disposal: Disposal,
    reenter: Option<&Arc<ActorProcessor<Echo>>>,
) -> Arc<ActorError> {
    Arc::new(ActorError::Other(anyhow::Error::new(UserError {
        observations: observations.clone(),
        reenter: reenter.map(Arc::downgrade),
        disposal,
    })))
}
fn no_unwind(action: impl FnOnce(), message: &str) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(action)) {
        // The old implementation can return a recursively panicking payload.
        std::mem::forget(payload);
        panic!("{message}");
    }
}
fn first_failure(processor: &ActorProcessor<Echo>) {
    processor.fail_from_escalation(Arc::new(ActorError::Panic("first failure".into())));
    assert!(processor.has_failed());
    assert_eq!(processor.get_status(), ProcessorStatus::Failed);
}

#[test]
fn duplicate_disposal_can_reenter_the_actual_processor() {
    const FLAG: &str = "PARROT_FAILURE_RECORDING_REENTRY";
    if std::env::var(FLAG).as_deref() == Ok("child") {
        let processor = without_executor();
        first_failure(&processor);
        let observations = Arc::new(Observations::default());
        processor.fail_from_escalation(error(&observations, Disposal::Clean, Some(&processor)));
        assert_eq!(observations.drops.load(Ordering::SeqCst), 1);
        assert_eq!(observations.reentered.load(Ordering::SeqCst), 1);
        assert_eq!(observations.diagnostics.load(Ordering::SeqCst), 0);
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "duplicate_disposal_can_reenter_the_actual_processor",
            "--nocapture",
        ])
        .env(FLAG, "child")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "reentrant error disposal failed: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("reentrant error disposal deadlocked the failure lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn duplicate_disposal(disposal: Disposal) {
    let processor = without_executor();
    first_failure(&processor);
    let observations = Arc::new(Observations::default());
    no_unwind(
        || processor.fail_from_escalation(error(&observations, disposal, None)),
        "duplicate error destructor escaped failure recording",
    );
    assert!(processor.has_failed(), "failure mutex became poisoned");
    assert!(!processor.begin_escalation(Arc::new(ActorError::Stopped)));
    assert_eq!(observations.drops.load(Ordering::SeqCst), 1);
    assert_eq!(observations.diagnostics.load(Ordering::SeqCst), 0);
}
#[test]
fn duplicate_panicking_error_does_not_escape_or_poison() {
    duplicate_disposal(Disposal::Panic);
}
#[test]
fn duplicate_recursive_panic_is_isolated() {
    duplicate_disposal(Disposal::RecursivePanic);
}

fn retained_disposal(disposal: Disposal) {
    let processor = without_executor();
    let observations = Arc::new(Observations::default());
    processor.fail_from_escalation(error(&observations, disposal, None));
    assert_eq!(observations.drops.load(Ordering::SeqCst), 0);
    no_unwind(
        || drop(processor),
        "retained first error destructor escaped processor destruction",
    );
    assert_eq!(observations.drops.load(Ordering::SeqCst), 1);
    assert_eq!(observations.diagnostics.load(Ordering::SeqCst), 0);
}
#[test]
fn retained_first_error_panic_is_isolated_without_executor() {
    retained_disposal(Disposal::Panic);
}
#[test]
fn retained_first_recursive_panic_is_isolated_without_executor() {
    retained_disposal(Disposal::RecursivePanic);
}

fn shutdown_disposal(retained: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let observations = Arc::new(Observations::default());
    no_unwind(
        || {
            let _entered = runtime.enter();
            let processor = processor();
            if !retained {
                first_failure(&processor);
            }
            processor.fail_from_escalation(error(&observations, Disposal::RecursivePanic, None));
            drop(processor);
            assert_eq!(observations.drops.load(Ordering::SeqCst), 0);
        },
        "unpolled error cleanup escaped its owner",
    );
    no_unwind(
        || drop(runtime),
        "runtime shutdown failed to isolate unpolled error disposal",
    );
    assert_eq!(observations.drops.load(Ordering::SeqCst), 1);
    assert_eq!(observations.diagnostics.load(Ordering::SeqCst), 0);
}
#[test]
fn runtime_shutdown_retires_unpolled_duplicate_error() {
    shutdown_disposal(false);
}
#[test]
fn runtime_shutdown_retires_unpolled_retained_error() {
    shutdown_disposal(true);
}

#[tokio::test]
async fn live_runtime_isolates_duplicate_error_disposal() {
    let processor = processor();
    processor.initialize_and_start().await.unwrap();
    first_failure(&processor);
    let observations = Arc::new(Observations::default());
    let (finished, receipt) = tokio::sync::oneshot::channel();
    *observations.finished.lock().unwrap() = Some(finished);
    no_unwind(
        || processor.fail_from_escalation(error(&observations, Disposal::Panic, None)),
        "live duplicate error destructor escaped failure recording",
    );
    tokio::time::timeout(Duration::from_secs(5), receipt)
        .await
        .expect("duplicate error disposal was abandoned")
        .unwrap();
    assert!(processor.has_failed());
    assert_eq!(observations.drops.load(Ordering::SeqCst), 1);
    processor.retire_erased().await.unwrap();
}

#[test]
fn retained_error_respects_external_arc_ownership() {
    let processor = without_executor();
    let observations = Arc::new(Observations::default());
    let original = error(&observations, Disposal::Clean, None);
    processor.fail_from_escalation(original.clone());
    drop(processor);
    assert_eq!(observations.drops.load(Ordering::SeqCst), 0);
    drop(original);
    assert_eq!(observations.drops.load(Ordering::SeqCst), 1);
    assert_eq!(observations.diagnostics.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn healthy_generation_replies_before_failure_and_rejects_afterwards() {
    let processor = processor();
    processor.initialize_and_start().await.unwrap();
    let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
        2,
        ActorPath::placeholder("/failure-recording"),
    ));
    let (envelope, reply) = AskEnvelope::new(Box::new(83_u64));
    mailbox
        .push(Box::new(envelope), BackpressureStrategy::Error)
        .await
        .unwrap();
    assert_eq!(
        processor
            .process_batch_of_messages(&mailbox, 1, false)
            .await
            .unwrap(),
        (1, 0)
    );
    let response = reply.await.unwrap().unwrap();
    assert_eq!(*response.downcast::<u64>().unwrap(), 83);
    first_failure(&processor);
    mailbox
        .push(Box::new(97_u64), BackpressureStrategy::Error)
        .await
        .unwrap();
    assert!(
        processor
            .process_batch_of_messages(&mailbox, 1, false)
            .await
            .is_err()
    );
    assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 97);
    processor.retire_erased().await.unwrap();
}
