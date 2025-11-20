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
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Single,
    Batch,
    Fallback,
}
#[derive(Clone, Copy, Debug)]
enum Seal {
    Pause,
    Stop,
    Cancel,
    Fail,
}
type PredicateSeal = (Weak<dyn ProcessorInterface>, Seal, u64);

#[derive(Default)]
struct Counts {
    calls: Mutex<Vec<u64>>,
    predicates: Mutex<Vec<u64>>,
    batches: Mutex<Vec<usize>>,
    entered: AtomicBool,
    release: AtomicBool,
    stopped: AtomicUsize,
    seal_on_predicate: Mutex<Option<PredicateSeal>>,
    payload_drops: AtomicUsize,
    lock_free_drops: AtomicUsize,
    barrier: AtomicUsize,
}
struct RecursivePanic;
impl Drop for RecursivePanic {
    fn drop(&mut self) {
        panic!("rejected payload's panic value also failed");
    }
}
struct Dropping {
    processor: Weak<ActorProcessor<Probe>>,
    counts: Arc<Counts>,
    panic_drop: bool,
}
impl Drop for Dropping {
    fn drop(&mut self) {
        self.counts.payload_drops.fetch_add(1, Ordering::SeqCst);
        let processor = self.processor.upgrade().unwrap();
        let mut reenter = Box::pin(processor.start_actor());
        if poll_once(reenter.as_mut()).is_ready() {
            self.counts.lock_free_drops.fetch_add(1, Ordering::SeqCst);
        }
        drop(reenter);
        if self.panic_drop {
            std::panic::panic_any(RecursivePanic);
        }
    }
}
fn message_id(message: &BoxedMessage) -> u64 {
    if message.is::<Dropping>() {
        1
    } else {
        *message.downcast_ref::<u64>().unwrap()
    }
}
struct Probe {
    counts: Arc<Counts>,
    mode: Mode,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        let id = message_id(message);
        self.counts.predicates.lock().unwrap().push(id);
        let mut admission = self.counts.seal_on_predicate.lock().unwrap();
        if admission
            .as_ref()
            .is_some_and(|(_, _, trigger)| *trigger == id)
        {
            let (processor, seal, _) = admission.take().unwrap();
            drop(admission);
            let erased = processor.upgrade().unwrap();
            let processor = erased
                .as_any_ref()
                .downcast_ref::<ActorProcessor<Probe>>()
                .unwrap();
            match seal {
                Seal::Pause => processor.pause(),
                Seal::Cancel => processor.cancel_processing(),
                Seal::Fail => processor.fail_from_escalation(Arc::new(ActorError::Panic(
                    "policy closed admission".into(),
                ))),
                Seal::Stop => {
                    let mut stop = Box::pin(processor.stop_actor());
                    assert!(poll_once(stop.as_mut()).is_pending());
                }
            }
        }
        id != self.counts.barrier.load(Ordering::SeqCst) as u64 && matches!(self.mode, Mode::Batch)
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let id = message_id(&message);
        self.counts.calls.lock().unwrap().push(id);
        Box::pin(async move {
            if id == 0 {
                self.counts.entered.store(true, Ordering::SeqCst);
                std::future::poll_fn(|_| {
                    if self.counts.release.load(Ordering::SeqCst) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            }
            Ok(message)
        })
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        self.counts.batches.lock().unwrap().push(messages.len());
        Box::pin(async move {
            let mut results = Vec::new();
            for message in messages {
                results.push(self.receive_message(message, context).await);
            }
            results
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.counts.stopped.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(std::task::Waker::noop()))
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("dispatch admission did not finish")
}
fn mailbox() -> Arc<dyn Mailbox> {
    Arc::new(MpscMailbox::new(
        4,
        ActorPath::placeholder("/dispatch-admission"),
    ))
}
async fn request(mailbox: &Arc<dyn Mailbox>, id: u64) -> parrot::thread::reply::ReplyReceiver {
    request_boxed(mailbox, Box::new(id)).await
}
async fn request_boxed(
    mailbox: &Arc<dyn Mailbox>,
    payload: BoxedMessage,
) -> parrot::thread::reply::ReplyReceiver {
    let (envelope, reply) = AskEnvelope::new(payload);
    mailbox
        .push(Box::new(envelope), BackpressureStrategy::Error)
        .await
        .unwrap();
    reply
}
async fn fixture(mode: Mode) -> (Arc<ActorProcessor<Probe>>, Arc<Counts>) {
    let counts = Arc::new(Counts::default());
    let processor = Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(Probe {
            counts: counts.clone(),
            mode,
        }),
        ThreadContext::new_for_test("/dispatch-admission"),
        "/dispatch-admission".into(),
        ThreadActorConfig {
            message_batch_size: Some(if matches!(mode, Mode::Single) { 1 } else { 4 }),
            ..Default::default()
        },
    ));
    processor.initialize_and_start().await.unwrap();
    (processor, counts)
}

fn weak_processor(processor: &Arc<ActorProcessor<Probe>>) -> Weak<dyn ProcessorInterface> {
    let erased: Arc<dyn ProcessorInterface> = processor.clone();
    Arc::downgrade(&erased)
}

async fn waiting_call(mode: Mode, seal: Seal) {
    let (processor, counts) = fixture(mode).await;
    let active_mailbox = mailbox();
    let waiting_mailbox = mailbox();
    let active_reply = request(&active_mailbox, 0).await;
    let waiting_reply = request(&waiting_mailbox, 1).await;
    let mut active = Box::pin(processor.process_batch_of_messages(&active_mailbox, 1, false));
    assert!(poll_once(active.as_mut()).is_pending());
    assert!(counts.entered.load(Ordering::SeqCst));
    let mut waiting = Box::pin(processor.process_batch_of_messages(&waiting_mailbox, 1, false));
    assert!(poll_once(waiting.as_mut()).is_pending());
    assert!(waiting_mailbox.is_empty().await, "waiter was not dequeued");
    let policies_before = counts.predicates.lock().unwrap().clone();
    let mut stop = None;
    match seal {
        Seal::Pause => processor.pause(),
        Seal::Cancel => processor.cancel_processing(),
        Seal::Fail => {
            processor.fail_from_escalation(Arc::new(ActorError::Panic("sealed generation".into())))
        }
        Seal::Stop => {
            let mut stopping = Box::pin(processor.stop_actor());
            assert!(poll_once(stopping.as_mut()).is_pending());
            assert_eq!(processor.get_status(), ProcessorStatus::Stopping);
            stop = Some(stopping);
        }
    }
    counts.release.store(true, Ordering::SeqCst);
    bounded(active).await.unwrap();
    assert!(
        bounded(active_reply).await.unwrap().is_ok(),
        "already entered work was discarded"
    );
    let waiting_outcome = if let Some(stop) = stop {
        let (waiting, stop) = bounded(async { tokio::join!(waiting, stop) }).await;
        stop.unwrap();
        waiting
    } else {
        bounded(waiting).await
    };
    let waiting_reply = bounded(waiting_reply).await;
    let observed = counts.calls.lock().unwrap().clone();
    let policies_after = counts.predicates.lock().unwrap().clone();
    // Finish cleanup before reporting an expected baseline assertion failure.
    if matches!(seal, Seal::Pause) {
        processor.resume();
        let followup = request(&waiting_mailbox, 2).await;
        bounded(processor.process_batch_of_messages(&waiting_mailbox, 1, false))
            .await
            .unwrap();
        assert_eq!(
            *bounded(followup)
                .await
                .unwrap()
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            2
        );
        assert!(
            !processor.has_failed(),
            "pause rejection poisoned the live generation"
        );
    }
    bounded(processor.stop_actor()).await.unwrap();
    bounded(processor.clone().retire_erased()).await.unwrap();
    assert_eq!(
        observed,
        vec![0],
        "{mode:?}/{seal:?} entered waiting user handler: {waiting_outcome:?}"
    );
    assert_eq!(
        policies_after, policies_before,
        "{mode:?}/{seal:?} entered waiting user batch predicate"
    );
    assert!(
        matches!(waiting_reply, Ok(Err(_))),
        "dequeued request was not explicitly rejected: {waiting_reply:?}"
    );
}

async fn cancelled_entry(mode: Mode) {
    let (processor, counts) = fixture(mode).await;
    let mailbox = mailbox();
    let mut reply = request(&mailbox, 1).await;
    processor.cancel_processing();
    let outcome = bounded(processor.process_batch_of_messages(&mailbox, 1, false)).await;
    let untouched = !mailbox.is_empty().await;
    let reply_pending = matches!(
        reply.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    );
    let observed = counts.calls.lock().unwrap().clone();
    let predicates = counts.predicates.lock().unwrap().clone();
    bounded(processor.stop_actor()).await.unwrap();
    bounded(processor.clone().retire_erased()).await.unwrap();
    assert!(
        outcome.is_err(),
        "{mode:?} accepted a new dispatch after cancellation: {outcome:?}"
    );
    assert!(
        untouched && reply_pending,
        "cancelled entry consumed the queued request"
    );
    assert!(
        observed.is_empty() && predicates.is_empty(),
        "cancelled entry ran user code"
    );
}

async fn normal_entry(mode: Mode) {
    let (processor, counts) = fixture(mode).await;
    let mailbox = mailbox();
    let reply = request(&mailbox, 1).await;
    assert_eq!(
        bounded(processor.process_batch_of_messages(&mailbox, 1, false))
            .await
            .unwrap(),
        (1, 0)
    );
    assert_eq!(
        *bounded(reply)
            .await
            .unwrap()
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        1
    );
    assert_eq!(*counts.calls.lock().unwrap(), vec![1]);
    assert_eq!(
        *counts.batches.lock().unwrap(),
        if matches!(mode, Mode::Batch) {
            vec![1]
        } else {
            vec![]
        }
    );
    bounded(processor.stop_actor()).await.unwrap();
    bounded(processor.clone().retire_erased()).await.unwrap();
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
}

async fn collected_group(seal: Seal) {
    let (processor, counts) = fixture(Mode::Batch).await;
    let mailbox = mailbox();
    let a = request(&mailbox, 1).await;
    let b = request(&mailbox, 2).await;
    *counts.seal_on_predicate.lock().unwrap() = Some((weak_processor(&processor), seal, 2));
    let outcome = bounded(processor.process_batch_of_messages(&mailbox, 2, false)).await;
    let replies = [bounded(a).await.unwrap(), bounded(b).await.unwrap()];
    let seen = counts.calls.lock().unwrap().clone();
    let batches = counts.batches.lock().unwrap().clone();
    let failed = processor.has_failed();
    if matches!(seal, Seal::Pause) && !failed {
        processor.resume();
        let followup = request(&mailbox, 3).await;
        bounded(processor.process_batch_of_messages(&mailbox, 1, false))
            .await
            .unwrap();
        assert!(bounded(followup).await.unwrap().is_ok());
    }
    bounded(processor.stop_actor()).await.unwrap();
    bounded(processor.clone().retire_erased()).await.unwrap();
    assert!(
        seen.is_empty() && batches.is_empty(),
        "{seal:?} ran a sealed collected group: {outcome:?}"
    );
    assert!(
        replies.iter().all(Result::is_err),
        "rejected group published success"
    );
    assert_eq!(
        failed,
        matches!(seal, Seal::Fail),
        "admission rejection poisoned the generation"
    );
}

async fn rejected_payload(mode: Mode, panic_drop: bool) {
    let (processor, counts) = fixture(mode).await;
    let active_mailbox = mailbox();
    let waiting_mailbox = mailbox();
    let active_reply = request(&active_mailbox, 0).await;
    let waiting_reply = request_boxed(
        &waiting_mailbox,
        Box::new(Dropping {
            processor: Arc::downgrade(&processor),
            counts: counts.clone(),
            panic_drop,
        }),
    )
    .await;
    let mut active = Box::pin(processor.process_batch_of_messages(&active_mailbox, 1, false));
    assert!(poll_once(active.as_mut()).is_pending());
    let mut waiting = Box::pin(processor.process_batch_of_messages(&waiting_mailbox, 1, false));
    assert!(poll_once(waiting.as_mut()).is_pending());
    processor.pause();
    counts.release.store(true, Ordering::SeqCst);
    bounded(active).await.unwrap();
    assert!(bounded(active_reply).await.unwrap().is_ok());
    use futures::FutureExt;
    let result = bounded(std::panic::AssertUnwindSafe(waiting).catch_unwind()).await;
    let escaped = result.is_err();
    if let Err(payload) = result {
        // Keep a baseline panic payload from recursively unwinding the test runner.
        std::mem::forget(payload);
    }
    let disposal_before_reply = counts.payload_drops.load(Ordering::SeqCst);
    let reply = bounded(waiting_reply).await;
    let expected_error = matches!(&reply, Ok(Err(error)) if if panic_drop {
        matches!(error, ActorError::Panic(message) if message.contains("rejected dispatch") && message.contains("panic payload destructor panicked"))
    } else { matches!(error, ActorError::Stopped) });
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(reply))) {
        std::mem::forget(payload);
    }
    let failed = processor.has_failed();
    bounded(processor.stop_actor()).await.unwrap();
    bounded(processor.clone().retire_erased()).await.unwrap();
    assert!(!escaped, "rejected payload disposal escaped dispatch");
    assert_eq!(
        disposal_before_reply, 1,
        "dispatch completed before rejected input disposal"
    );
    assert_eq!(counts.payload_drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.lock_free_drops.load(Ordering::SeqCst),
        1,
        "rejected destructor ran under execution lock"
    );
    assert!(
        expected_error,
        "rejection lost its actual error class or disposal diagnostic"
    );
    assert_eq!(
        failed, panic_drop,
        "cleanup failure receipt did not match actual disposal"
    );
}

async fn rejected_lookahead(panic_drop: bool) {
    let (processor, counts) = fixture(Mode::Batch).await;
    let mailbox = mailbox();
    let first = request(&mailbox, 3).await;
    let next = request_boxed(
        &mailbox,
        Box::new(Dropping {
            processor: Arc::downgrade(&processor),
            counts: counts.clone(),
            panic_drop,
        }),
    )
    .await;
    counts.barrier.store(1, Ordering::SeqCst);
    *counts.seal_on_predicate.lock().unwrap() = Some((weak_processor(&processor), Seal::Pause, 1));
    use futures::FutureExt;
    let outcome = bounded(
        std::panic::AssertUnwindSafe(processor.process_batch_of_messages(&mailbox, 2, false))
            .catch_unwind(),
    )
    .await;
    let escaped = outcome.is_err();
    let dispatch_failed = matches!(&outcome, Ok(Err(_)));
    if let Err(payload) = outcome {
        std::mem::forget(payload);
    }
    let first = bounded(first).await;
    let next = bounded(next).await;
    let explicit_rejection = matches!(&first, Ok(Err(ActorError::Stopped)))
        && matches!(&next, Ok(Err(error)) if if panic_drop {
            matches!(error, ActorError::Panic(message) if message.contains("rejected dispatch") && message.contains("panic payload destructor panicked"))
        } else { matches!(error, ActorError::Stopped) });
    if let Err(payload) =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop((first, next))))
    {
        std::mem::forget(payload);
    }
    let failed = processor.has_failed();
    bounded(processor.stop_actor()).await.unwrap();
    bounded(processor.clone().retire_erased()).await.unwrap();
    assert!(!escaped, "lookahead disposal escaped dispatch");
    assert!(counts.calls.lock().unwrap().is_empty());
    assert!(explicit_rejection, "lookahead lost its explicit rejection");
    assert_eq!(counts.payload_drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.lock_free_drops.load(Ordering::SeqCst), 1);
    assert_eq!(failed, panic_drop);
    assert_eq!(
        dispatch_failed, panic_drop,
        "lookahead cleanup failure was hidden"
    );
}

#[tokio::test]
async fn rejected_lookahead_is_disposed_before_reply_outside_locks() {
    rejected_lookahead(false).await;
}
#[tokio::test]
async fn rejected_lookahead_recursive_panic_fails_the_dispatch_receipt() {
    rejected_lookahead(true).await;
}

#[tokio::test]
async fn collected_batch_pause_rejects_without_poisoning_resume() {
    collected_group(Seal::Pause).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_collected_batch_pause_does_not_invoke_stop_supervision() {
    use parrot::thread::{ThreadActorSystem, config::SupervisorStrategy};
    use parrot_api::address::ActorRef;

    let system = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    let actor = system
        .spawn_at(
            Probe {
                counts: counts.clone(),
                mode: Mode::Batch,
            },
            "/managed-dispatch-admission",
            None,
            ThreadActorConfig {
                message_batch_size: Some(4),
                supervisor_strategy: Some(SupervisorStrategy::Stop),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // A real request establishes that this registered generation has started.
    bounded(actor.send(Box::new(9u64))).await.unwrap();
    counts.calls.lock().unwrap().clear();
    counts.batches.lock().unwrap().clear();
    let own_mailbox = system.get_mailbox(&actor.path()).unwrap();
    // Receiving a reply does not join its scheduler run. Reserve the real
    // idle slot before manually driving this registered processor elsewhere.
    bounded(async {
        while !own_mailbox.schedule_state().try_enqueue() {
            // A failed enqueue sets WAKE; let that extra empty run drain.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(own_mailbox.is_empty().await);
    let erased = own_mailbox.get_processor().unwrap();
    let processor = erased
        .as_any_ref()
        .downcast_ref::<ActorProcessor<Probe>>()
        .expect("real generation processor");
    let mailbox = mailbox();
    let a = request(&mailbox, 1).await;
    let b = request(&mailbox, 2).await;
    *counts.seal_on_predicate.lock().unwrap() = Some((Arc::downgrade(&erased), Seal::Pause, 2));
    let outcome = bounded(processor.process_batch_of_messages(&mailbox, 2, false)).await;
    let replies = [bounded(a).await.unwrap(), bounded(b).await.unwrap()];
    let failed = processor.has_failed();
    let invoked =
        !counts.calls.lock().unwrap().is_empty() || !counts.batches.lock().unwrap().is_empty();
    let same_generation = system
        .get_mailbox(&actor.path())
        .is_some_and(|current| Arc::ptr_eq(&current, &own_mailbox));
    if !failed {
        processor.resume();
    }
    assert!(
        !own_mailbox.schedule_state().release(false),
        "unexpected producer woke the fixture's reserved schedule slot"
    );
    if !failed {
        let followup = bounded(actor.send(Box::new(3u64))).await.unwrap();
        assert_eq!(*followup.downcast::<u64>().unwrap(), 3);
    }
    bounded(system.shutdown_internal()).await.unwrap();
    assert!(!invoked, "paused managed group entered business callbacks");
    assert!(replies.iter().all(Result::is_err));
    assert!(
        !failed,
        "admission rejection invoked managed Stop supervision"
    );
    assert!(
        same_generation,
        "pause rejection replaced the registered generation"
    );
    assert!(
        outcome.is_ok(),
        "clean managed rejection became fatal: {outcome:?}"
    );
}
#[tokio::test]
async fn collected_batch_stop_rejects_without_invoking_business() {
    collected_group(Seal::Stop).await;
}
#[tokio::test]
async fn collected_batch_cancellation_rejects_without_invoking_business() {
    collected_group(Seal::Cancel).await;
}
#[tokio::test]
async fn collected_batch_failure_retains_the_failed_generation() {
    collected_group(Seal::Fail).await;
}

macro_rules! contracts {
    ($module:ident, $mode:expr) => {
        mod $module {
            use super::*;
            #[tokio::test]
            async fn pause_rejects_waiting_user_callbacks_but_resume_still_dispatches() {
                waiting_call($mode, Seal::Pause).await;
            }
            #[tokio::test]
            async fn stop_rejects_waiting_user_callbacks_before_actual_shutdown() {
                waiting_call($mode, Seal::Stop).await;
            }
            #[tokio::test]
            async fn cancellation_rejects_waiting_user_callbacks() {
                waiting_call($mode, Seal::Cancel).await;
            }
            #[tokio::test]
            async fn failure_rejects_waiting_user_callbacks() {
                waiting_call($mode, Seal::Fail).await;
            }
            #[tokio::test]
            async fn cancellation_before_entry_preserves_the_mailbox() {
                cancelled_entry($mode).await;
            }
            #[tokio::test]
            async fn live_generation_preserves_real_reply_and_handler_selection() {
                normal_entry($mode).await;
            }
            #[tokio::test]
            async fn rejected_payload_disposal_runs_once_outside_execution_locks() {
                rejected_payload($mode, false).await;
            }
            #[tokio::test]
            async fn rejected_payload_recursive_panic_is_isolated_and_recorded() {
                rejected_payload($mode, true).await;
            }
        }
    };
}
contracts!(single, Mode::Single);
contracts!(batch, Mode::Batch);
contracts!(fallback, Mode::Fallback);
