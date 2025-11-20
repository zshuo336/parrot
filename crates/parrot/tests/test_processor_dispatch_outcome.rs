use futures::FutureExt;
use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ProcessorInterface, ThreadActor, ThreadActorConfig,
    ThreadContext,
    envelope::AskEnvelope,
    mailbox::{Mailbox, mpsc::MpscMailbox},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    task::Context,
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Single,
    Batch,
    Fallback,
}
#[derive(Clone, Copy, Debug)]
enum Delivery {
    Tell,
    Cancelled,
    Live,
}
#[derive(Default)]
struct Counts {
    calls: AtomicUsize,
    drops: AtomicUsize,
    unlocked: AtomicUsize,
    payloads: AtomicUsize,
    pointer: AtomicUsize,
    lookahead: AtomicUsize,
    processor: Mutex<Weak<ActorProcessor<Probe>>>,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("orphan reply panic payload destructor");
    }
}
struct Reply {
    counts: Arc<Counts>,
    panic: u8,
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        let processor = self.counts.processor.lock().unwrap().upgrade().unwrap();
        let mut start = Box::pin(processor.start_actor());
        if start
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_ready()
        {
            self.counts.unlocked.fetch_add(1, Ordering::SeqCst);
        }
        drop(start);
        match self.panic {
            0 => {}
            1 => panic!("orphan reply destructor"),
            2 => std::panic::panic_any(Recursive(self.counts.clone())),
            _ => unreachable!(),
        }
    }
}
struct Probe {
    counts: Arc<Counts>,
    mode: Mode,
    panic: u8,
    invalid: bool,
}
struct Rejected(Arc<Counts>);
impl Drop for Rejected {
    fn drop(&mut self) {
        self.0.lookahead.fetch_add(1, Ordering::SeqCst);
        let processor = self.0.processor.lock().unwrap().upgrade().unwrap();
        let mut start = Box::pin(processor.start_actor());
        assert!(
            start
                .as_mut()
                .poll(&mut Context::from_waker(std::task::Waker::noop()))
                .is_ready(),
            "lookahead cleanup held an execution lock"
        );
        panic!("lookahead cleanup failure");
    }
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        matches!(self.mode, Mode::Batch) && !message.is::<Rejected>()
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.counts.calls.fetch_add(1, Ordering::SeqCst);
        let reply = Box::new(Reply {
            counts: self.counts.clone(),
            panic: message.downcast_ref::<u8>().copied().unwrap_or(self.panic),
        });
        self.counts
            .pointer
            .store(reply.as_ref() as *const Reply as usize, Ordering::SeqCst);
        let reply: BoxedMessage = reply;
        Box::pin(async { Ok(reply) })
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        Box::pin(async move {
            let mut messages = messages;
            if self.invalid {
                messages.push(Box::new(()));
            }
            let mut output = Vec::new();
            for message in messages {
                output.push(self.receive_message(message, ctx).await);
            }
            output
        })
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("outcome owner stalled")
}
fn escaped(payload: Box<dyn std::any::Any + Send>) -> ! {
    if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(secondary);
    }
    panic!("orphan result destructor escaped direct dispatch");
}
fn isolated(test: &str) -> bool {
    const CHILD: &str = "PARROT_DISPATCH_OUTCOME_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return false;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let drain = |mut stream: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut output = Vec::new();
            std::io::Read::read_to_end(&mut stream, &mut output).unwrap();
            output
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    assert!(
        status.success() && !timed_out,
        "independent result disposal failed (status={status}, timed_out={timed_out}): {}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    true
}
async fn fixture(
    mode: Mode,
    panic: u8,
    invalid: bool,
) -> (Arc<ActorProcessor<Probe>>, Arc<Counts>, Arc<dyn Mailbox>) {
    let counts = Arc::new(Counts::default());
    let processor = Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(Probe {
            counts: counts.clone(),
            mode,
            panic,
            invalid,
        }),
        ThreadContext::new_for_test("/dispatch-outcome"),
        "/dispatch-outcome".into(),
        ThreadActorConfig {
            message_batch_size: Some(if matches!(mode, Mode::Single) { 1 } else { 4 }),
            ..Default::default()
        },
    ));
    *counts.processor.lock().unwrap() = Arc::downgrade(&processor);
    bounded(processor.initialize_and_start()).await.unwrap();
    let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
        8,
        ActorPath::placeholder("/dispatch-outcome"),
    ));
    (processor, counts, mailbox)
}
async fn orphan(mode: Mode, delivery: Delivery, panic: u8) {
    orphan_execution(mode, delivery, panic, false).await;
}
async fn orphan_execution(mode: Mode, delivery: Delivery, panic: u8, erased: bool) {
    let (processor, counts, mailbox) = fixture(mode, panic, false).await;
    let (message, receiver) = AskEnvelope::new(Box::new(()));
    let receiver = if matches!(delivery, Delivery::Tell) {
        mailbox
            .push(Box::new(()), BackpressureStrategy::Error)
            .await
            .unwrap();
        drop(message);
        drop(receiver);
        None
    } else {
        mailbox
            .push(Box::new(message), BackpressureStrategy::Error)
            .await
            .unwrap();
        if matches!(delivery, Delivery::Cancelled) {
            drop(receiver);
            None
        } else {
            Some(receiver)
        }
    };
    let outcome = if erased {
        bounded(
            AssertUnwindSafe(
                processor
                    .clone()
                    .process_batch_erased(mailbox.clone(), 1, false),
            )
            .catch_unwind(),
        )
        .await
    } else {
        bounded(
            AssertUnwindSafe(processor.process_batch_of_messages(&mailbox, 1, false))
                .catch_unwind(),
        )
        .await
    }
    .unwrap_or_else(|payload| escaped(payload));
    if let Some(receiver) = receiver {
        assert_eq!(outcome.unwrap(), (1, 0));
        assert_eq!(
            counts.drops.load(Ordering::SeqCst),
            0,
            "delivered output was retired by the engine"
        );
        let reply = bounded(receiver)
            .await
            .unwrap()
            .unwrap()
            .downcast::<Reply>()
            .ok()
            .unwrap();
        assert!(Arc::ptr_eq(&reply.counts, &counts));
        assert_eq!(
            reply.as_ref() as *const Reply as usize,
            counts.pointer.load(Ordering::SeqCst)
        );
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| drop(reply))).is_ok());
    } else {
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.unlocked.load(Ordering::SeqCst),
            1,
            "orphan disposal held an execution lock"
        );
        assert_eq!(
            counts.payloads.load(Ordering::SeqCst),
            usize::from(panic == 2)
        );
        if panic == 0 {
            assert_eq!(outcome.unwrap(), (1, 0));
        } else {
            assert!(
                outcome.is_err(),
                "cleanup failure was reported as successful dispatch"
            );
            mailbox
                .push(Box::new(0_u8), BackpressureStrategy::Error)
                .await
                .unwrap();
            assert!(
                bounded(processor.process_batch_of_messages(&mailbox, 1, false))
                    .await
                    .is_err()
            );
            assert_eq!(
                counts.calls.load(Ordering::SeqCst),
                1,
                "failed cleanup allowed another handler"
            );
        }
    }
    let retirement = bounded(processor.clone().retire_erased()).await;
    if panic == 0 {
        retirement.unwrap();
    } else {
        let error = retirement.unwrap_err().to_string();
        assert!(
            error.contains("orphan reply destructor")
                || error.contains("panic payload destructor panicked"),
            "{error}"
        );
        assert_eq!(
            bounded(processor.retirement())
                .await
                .unwrap_err()
                .to_string(),
            error
        );
    }
}

macro_rules! contracts {
    ($clean:ident, $panic:ident, $recursive:ident, $mode:ident, $delivery:ident) => {
        #[tokio::test]
        async fn $clean() {
            orphan(Mode::$mode, Delivery::$delivery, 0).await;
        }
        #[tokio::test]
        async fn $panic() {
            orphan(Mode::$mode, Delivery::$delivery, 1).await;
        }
        #[tokio::test]
        async fn $recursive() {
            orphan(Mode::$mode, Delivery::$delivery, 2).await;
        }
    };
}
contracts!(
    single_tell_clean,
    single_tell_panic,
    single_tell_recursive,
    Single,
    Tell
);
contracts!(
    single_cancel_clean,
    single_cancel_panic,
    single_cancel_recursive,
    Single,
    Cancelled
);
contracts!(
    batch_tell_clean,
    batch_tell_panic,
    batch_tell_recursive,
    Batch,
    Tell
);
contracts!(
    batch_cancel_clean,
    batch_cancel_panic,
    batch_cancel_recursive,
    Batch,
    Cancelled
);
contracts!(
    fallback_tell_clean,
    fallback_tell_panic,
    fallback_tell_recursive,
    Fallback,
    Tell
);
contracts!(
    fallback_cancel_clean,
    fallback_cancel_panic,
    fallback_cancel_recursive,
    Fallback,
    Cancelled
);

#[tokio::test]
async fn single_live() {
    orphan(Mode::Single, Delivery::Live, 0).await;
}
#[tokio::test]
async fn batch_live() {
    orphan(Mode::Batch, Delivery::Live, 0).await;
}
#[tokio::test]
async fn fallback_live() {
    orphan(Mode::Fallback, Delivery::Live, 0).await;
}

#[tokio::test]
async fn single_erased_cancel() {
    if isolated("single_erased_cancel") {
        return;
    }
    orphan_execution(Mode::Single, Delivery::Cancelled, 2, true).await;
}
#[tokio::test]
async fn batch_erased_cancel() {
    if isolated("batch_erased_cancel") {
        return;
    }
    orphan_execution(Mode::Batch, Delivery::Cancelled, 2, true).await;
}
#[tokio::test]
async fn fallback_erased_cancel() {
    if isolated("fallback_erased_cancel") {
        return;
    }
    orphan_execution(Mode::Fallback, Delivery::Cancelled, 2, true).await;
}

async fn multiple_outputs(live_siblings: bool) {
    let (processor, counts, mailbox) = fixture(Mode::Batch, 0, false).await;
    let mut receivers = Vec::new();
    for index in 0..3 {
        let panic = if index == 0 || !live_siblings {
            2_u8
        } else {
            0_u8
        };
        let (message, receiver) = AskEnvelope::new(Box::new(panic));
        mailbox
            .push(Box::new(message), BackpressureStrategy::Error)
            .await
            .unwrap();
        if index != 0 && live_siblings {
            receivers.push(receiver);
        } else {
            drop(receiver);
        }
    }
    let outcome = bounded(
        AssertUnwindSafe(processor.process_batch_of_messages(&mailbox, 3, false)).catch_unwind(),
    )
    .await
    .unwrap_or_else(|payload| escaped(payload));
    assert!(outcome.is_err());
    assert_eq!(counts.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        counts.drops.load(Ordering::SeqCst),
        if live_siblings { 1 } else { 3 }
    );
    for receiver in receivers {
        let reply = bounded(receiver)
            .await
            .expect("completed sibling reply was abandoned")
            .unwrap();
        assert!(reply.is::<Reply>());
        drop(reply);
    }
    assert_eq!(counts.drops.load(Ordering::SeqCst), 3);
    assert_eq!(counts.unlocked.load(Ordering::SeqCst), 3);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        if live_siblings { 1 } else { 3 }
    );
    assert!(
        bounded(processor.clone().retire_erased())
            .await
            .unwrap_err()
            .to_string()
            .contains("panic payload destructor panicked")
    );
}
#[tokio::test]
async fn batch_completed_siblings_are_published_after_orphan_failure() {
    if isolated("batch_completed_siblings_are_published_after_orphan_failure") {
        return;
    }
    multiple_outputs(true).await;
}
#[tokio::test]
async fn batch_each_orphan_is_retired_independently() {
    if isolated("batch_each_orphan_is_retired_independently") {
        return;
    }
    multiple_outputs(false).await;
}

#[tokio::test]
async fn orphan_failure_rejects_and_retires_lookahead() {
    if isolated("orphan_failure_rejects_and_retires_lookahead") {
        return;
    }
    let (processor, counts, mailbox) = fixture(Mode::Batch, 2, false).await;
    let (first, receiver) = AskEnvelope::new(Box::new(()));
    drop(receiver);
    let (next, receiver) = AskEnvelope::new(Box::new(Rejected(counts.clone())));
    mailbox
        .push(Box::new(first), BackpressureStrategy::Error)
        .await
        .unwrap();
    mailbox
        .push(Box::new(next), BackpressureStrategy::Error)
        .await
        .unwrap();
    let result = bounded(
        AssertUnwindSafe(processor.process_batch_of_messages(&mailbox, 2, false)).catch_unwind(),
    )
    .await
    .unwrap_or_else(|payload| escaped(payload));
    assert!(result.is_err());
    assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts.lookahead.load(Ordering::SeqCst), 1);
    assert!(bounded(receiver).await.unwrap().is_err());
    assert!(bounded(processor.clone().retire_erased()).await.is_err());
}

#[tokio::test]
async fn invalid_batch_retires_each_discarded_result() {
    if isolated("invalid_batch_retires_each_discarded_result") {
        return;
    }
    let (processor, counts, mailbox) = fixture(Mode::Batch, 2, true).await;
    for _ in 0..2 {
        mailbox
            .push(Box::new(()), BackpressureStrategy::Error)
            .await
            .unwrap();
    }
    let outcome = bounded(
        AssertUnwindSafe(processor.process_batch_of_messages(&mailbox, 2, false)).catch_unwind(),
    )
    .await
    .unwrap_or_else(|payload| escaped(payload));
    assert!(outcome.is_err());
    assert_eq!(counts.calls.load(Ordering::SeqCst), 3);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 3);
    assert_eq!(counts.unlocked.load(Ordering::SeqCst), 3);
    assert_eq!(counts.payloads.load(Ordering::SeqCst), 3);
    let error = bounded(processor.clone().retire_erased())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("panic payload destructor panicked"),
        "{error}"
    );
}
