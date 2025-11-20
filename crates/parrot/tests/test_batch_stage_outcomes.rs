//! Completed batch results remain owned when their user future fails disposal.
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
    errors::ActorError,
    trace::{SpanKind, SpanStatus, TraceRecorder},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Counts {
    calls: AtomicUsize,
    polls: AtomicUsize,
    futures: AtomicUsize,
    drops: [AtomicUsize; 4],
    pointers: [AtomicUsize; 4],
    payloads: AtomicUsize,
    business_pointer: AtomicUsize,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("batch recursive payload disposal");
    }
}
fn fail(kind: u8, counts: &Arc<Counts>, message: &'static str) {
    match kind {
        0 => {}
        1 => panic!("{message}"),
        2 => std::panic::panic_any(Recursive(counts.clone())),
        _ => unreachable!(),
    }
}
struct Reply {
    index: usize,
    panic: u8,
    counts: Arc<Counts>,
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.counts.drops[self.index].fetch_add(1, Ordering::SeqCst);
        fail(self.panic, &self.counts, "batch result disposal");
    }
}
#[derive(Debug)]
struct BusinessError(u64);
impl std::fmt::Display for BusinessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original batch business error {}", self.0)
    }
}
impl std::error::Error for BusinessError {}

struct BatchFuture {
    output: Option<Vec<ActorResult<BoxedMessage>>>,
    counts: Arc<Counts>,
    panic: u8,
}
impl Future for BatchFuture {
    type Output = Vec<ActorResult<BoxedMessage>>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.counts.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(self.output.take().expect("batch polled after completion"))
    }
}
impl Drop for BatchFuture {
    fn drop(&mut self) {
        self.counts.futures.fetch_add(1, Ordering::SeqCst);
        fail(self.panic, &self.counts, "batch future disposal");
    }
}
struct Probe {
    counts: Arc<Counts>,
    future_panic: u8,
    reply_panics: [u8; 4],
    business_error: bool,
    extra: bool,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn can_batch_message(&self, _: &BoxedMessage) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("batch fixture unexpectedly used serial dispatch")
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        self.counts.calls.fetch_add(1, Ordering::SeqCst);
        let count = messages.len() + usize::from(self.extra);
        let mut output = Vec::new();
        for index in 0..count {
            if self.business_error && index == 1 {
                let error = anyhow::Error::new(BusinessError(731));
                self.counts.business_pointer.store(
                    error.downcast_ref::<BusinessError>().unwrap() as *const BusinessError as usize,
                    Ordering::SeqCst,
                );
                output.push(Err(ActorError::Other(error)));
            } else {
                let reply = Box::new(Reply {
                    index,
                    panic: self.reply_panics[index],
                    counts: self.counts.clone(),
                });
                self.counts.pointers[index]
                    .store(reply.as_ref() as *const Reply as usize, Ordering::SeqCst);
                output.push(Ok(reply as BoxedMessage));
            }
        }
        Box::pin(BatchFuture {
            output: Some(output),
            counts: self.counts.clone(),
            panic: self.future_panic,
        })
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("batch stage ownership stalled")
}
fn caught<T>(result: std::thread::Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(payload) => {
            if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(secondary);
            }
            panic!("batch stage disposal escaped the processor");
        }
    }
}
fn isolated(test: &str) -> bool {
    const FLAG: &str = "PARROT_BATCH_STAGE_OUTCOMES";
    if std::env::var(FLAG).as_deref() == Ok(test) {
        return false;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(FLAG, test)
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
    let deadline = Instant::now() + Duration::from_secs(15);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    assert!(
        status.success() && !timed_out,
        "batch stage subprocess (status={status}, timed_out={timed_out}): {}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    true
}

#[derive(Clone, Copy)]
enum Delivery {
    Live,
    Tell,
    Cancelled,
}
async fn run(
    erased: bool,
    future_panic: u8,
    reply_panics: [u8; 4],
    business_error: bool,
    extra: bool,
    delivery: Delivery,
) {
    let counts = Arc::new(Counts::default());
    let processor = Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(Probe {
            counts: counts.clone(),
            future_panic,
            reply_panics,
            business_error,
            extra,
        }),
        ThreadContext::new_for_test("/batch-stage-outcomes"),
        "/batch-stage-outcomes".into(),
        ThreadActorConfig {
            message_batch_size: Some(4),
            ..Default::default()
        },
    ));
    bounded(processor.initialize_and_start()).await.unwrap();
    let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
        8,
        ActorPath::placeholder("/batch-stage-outcomes"),
    ));
    let recorder = TraceRecorder::new(16).unwrap();
    let root = recorder.root();
    let mut receivers = Vec::new();
    let outcome = root
        .scope(async {
            for index in 0..3_u8 {
                let message: BoxedMessage = Box::new(index);
                let message = if matches!(delivery, Delivery::Tell) {
                    message
                } else {
                    let (envelope, receiver) = AskEnvelope::new(message);
                    if matches!(delivery, Delivery::Live) {
                        receivers.push(receiver);
                    } else {
                        drop(receiver);
                    }
                    Box::new(envelope)
                };
                mailbox
                    .push(message, BackpressureStrategy::Error)
                    .await
                    .unwrap();
            }
            if erased {
                caught(
                    bounded(
                        AssertUnwindSafe(processor.clone().process_batch_erased(
                            mailbox.clone(),
                            3,
                            false,
                        ))
                        .catch_unwind(),
                    )
                    .await,
                )
            } else {
                caught(
                    bounded(
                        AssertUnwindSafe(processor.process_batch_of_messages(&mailbox, 3, false))
                            .catch_unwind(),
                    )
                    .await,
                )
            }
        })
        .await;
    let failed = future_panic != 0;
    assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts.polls.load(Ordering::SeqCst), 1);
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    if failed {
        assert!(outcome.is_err(), "future cleanup failure was hidden");
    } else {
        assert_eq!(outcome.unwrap(), (3, usize::from(business_error)));
    }
    let discarded = failed && !business_error;
    for (index, receiver) in receivers.into_iter().enumerate() {
        let result = bounded(receiver).await.expect("batch reply abandoned");
        if discarded {
            let error = result.expect_err("failed future published a success");
            let text = error.to_string();
            assert!(
                text.contains(if future_panic == 1 {
                    "batch future disposal"
                } else {
                    "panic payload destructor panicked"
                }),
                "lost first cleanup failure: {text}"
            );
        } else if business_error && index == 1 {
            let error = result.expect_err("business error became a success");
            let mut source: &(dyn std::error::Error + 'static) = &error;
            let mut original = None;
            for _ in 0..16 {
                if let Some(value) = source.downcast_ref::<BusinessError>() {
                    original = Some(value as *const BusinessError as usize);
                    break;
                }
                match source.source() {
                    Some(next) => source = next,
                    None => break,
                }
            }
            assert_eq!(
                original,
                Some(counts.business_pointer.load(Ordering::SeqCst)),
                "lost original concrete business error identity"
            );
        } else {
            let reply = result.unwrap().downcast::<Reply>().ok().unwrap();
            let same = reply.index == index
                && Arc::ptr_eq(&reply.counts, &counts)
                && reply.as_ref() as *const Reply as usize
                    == counts.pointers[index].load(Ordering::SeqCst)
                && counts.drops[index].load(Ordering::SeqCst) == 0;
            caught(std::panic::catch_unwind(AssertUnwindSafe(|| drop(reply))));
            assert!(
                same,
                "completed reply was replaced or disposed before delivery"
            );
        }
    }
    for index in 0..3 + usize::from(extra) {
        assert_eq!(
            counts.drops[index].load(Ordering::SeqCst),
            usize::from(!(business_error && index == 1)),
            "result {index} was not retired exactly once"
        );
    }
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(future_panic == 2)
            + if discarded {
                reply_panics[..3 + usize::from(extra)]
                    .iter()
                    .filter(|&&kind| kind == 2)
                    .count()
            } else {
                0
            }
    );
    recorder.close();
    let snapshot = recorder.snapshot();
    assert!(snapshot.terminal);
    if matches!(delivery, Delivery::Tell) {
        assert!(snapshot.records.is_empty());
    } else {
        assert_eq!(snapshot.records.len(), 1);
        let record = &snapshot.records[0];
        assert_eq!(record.kind, SpanKind::BusinessBatch);
        assert_eq!(record.trace_id, root.trace_id());
        assert_eq!(record.parent_span_id, Some(root.span_id()));
        assert_eq!(
            record.status,
            if failed {
                SpanStatus::Panicked
            } else if business_error {
                SpanStatus::Error
            } else {
                SpanStatus::Ok
            }
        );
    }
    if failed {
        mailbox
            .push(Box::new(0_u8), BackpressureStrategy::Error)
            .await
            .unwrap();
        assert!(
            bounded(processor.process_batch_of_messages(&mailbox, 1, false))
                .await
                .is_err()
        );
        assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
    } else {
        mailbox
            .push(Box::new(0_u8), BackpressureStrategy::Error)
            .await
            .unwrap();
        assert_eq!(
            bounded(processor.process_batch_of_messages(&mailbox, 1, false))
                .await
                .unwrap(),
            (1, 0),
            "healthy batch prevented later business"
        );
        assert_eq!(counts.calls.load(Ordering::SeqCst), 2);
        assert_eq!(counts.futures.load(Ordering::SeqCst), 2);
        assert_eq!(counts.drops[0].load(Ordering::SeqCst), 2);
    }
    for _ in 0..2 {
        let retirement = bounded(processor.clone().retire_erased()).await;
        if failed {
            let text = retirement.unwrap_err().to_string();
            assert!(
                text.contains(if future_panic == 1 {
                    "batch future disposal"
                } else {
                    "panic payload destructor panicked"
                }),
                "retirement lost first failure: {text}"
            );
        } else {
            retirement.unwrap();
        }
    }
}

macro_rules! case {
    ($name:ident, $erased:expr, $future:expr, $replies:expr, $business:expr, $extra:expr, $delivery:ident) => {
        #[tokio::test]
        async fn $name() {
            if isolated(stringify!($name)) {
                return;
            }
            run(
                $erased,
                $future,
                $replies,
                $business,
                $extra,
                Delivery::$delivery,
            )
            .await;
        }
    };
}
case!(healthy_live, false, 0, [0; 4], false, false, Live);
case!(healthy_erased, true, 0, [0; 4], false, false, Live);
case!(healthy_tell, false, 0, [0; 4], false, false, Tell);
case!(healthy_cancelled, true, 0, [0; 4], false, false, Cancelled);
case!(healthy_business_error, false, 0, [0; 4], true, false, Live);
case!(
    healthy_business_error_erased,
    true,
    0,
    [0; 4],
    true,
    false,
    Live
);
case!(failed_live, false, 1, [0; 4], false, false, Live);
case!(failed_erased, true, 1, [0; 4], false, false, Live);
case!(recursive_future_live, false, 2, [0; 4], false, false, Live);
case!(recursive_future_erased, true, 2, [0; 4], false, false, Live);
case!(ordinary_results, false, 1, [1, 1, 0, 0], false, false, Live);
case!(
    ordinary_results_erased,
    true,
    1,
    [1, 1, 0, 0],
    false,
    false,
    Tell
);
case!(
    recursive_results,
    false,
    1,
    [2, 2, 0, 0],
    false,
    false,
    Live
);
case!(
    recursive_results_erased,
    true,
    1,
    [2, 2, 0, 0],
    false,
    false,
    Cancelled
);
case!(mixed_results, false, 2, [1, 2, 0, 0], false, false, Live);
case!(
    mixed_results_erased,
    true,
    2,
    [1, 2, 0, 0],
    false,
    false,
    Live
);
case!(business_error, false, 1, [0; 4], true, false, Live);
case!(business_error_erased, true, 1, [0; 4], true, false, Live);
case!(
    business_recursive_future,
    false,
    2,
    [0; 4],
    true,
    false,
    Live
);
case!(
    business_recursive_future_erased,
    true,
    2,
    [0; 4],
    true,
    false,
    Live
);
case!(extra_discarded, false, 1, [1, 0, 0, 2], false, true, Live);
case!(
    extra_discarded_erased,
    true,
    1,
    [2, 0, 0, 1],
    false,
    true,
    Live
);
