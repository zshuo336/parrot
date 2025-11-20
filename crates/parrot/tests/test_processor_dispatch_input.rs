use async_trait::async_trait;
use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ProcessorInterface, ThreadActor, ThreadActorConfig,
    ThreadContext,
    envelope::AskEnvelope,
    error::MailboxError,
    mailbox::{Mailbox, ScheduleState, WakeHook, mpsc::MpscMailbox},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex, Weak,
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
#[derive(Default)]
struct Counts {
    calls: AtomicUsize,
    drops: AtomicUsize,
    unlocked: AtomicUsize,
    payloads: AtomicUsize,
    predicates: AtomicUsize,
    panic_predicate: AtomicUsize,
    barrier: AtomicBool,
    release: AtomicBool,
    block_drop: AtomicBool,
    drop_entered: AtomicBool,
    drop_release: (Mutex<bool>, Condvar),
    processor: Mutex<Weak<ActorProcessor<Probe>>>,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("dispatch input recursive payload destructor");
    }
}
struct Input {
    counts: Arc<Counts>,
    mode: u8,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        let processor = self.counts.processor.lock().unwrap().upgrade().unwrap();
        let mut start = Box::pin(processor.start_actor());
        if poll_once(start.as_mut()).is_ready() {
            self.counts.unlocked.fetch_add(1, Ordering::SeqCst);
        }
        drop(start);
        if self.counts.block_drop.load(Ordering::SeqCst) {
            self.counts.drop_entered.store(true, Ordering::SeqCst);
            let (gate, changed) = &self.counts.drop_release;
            let mut released = gate.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
        }
        match self.mode {
            0 => {}
            1 => panic!("dispatch input destructor"),
            2 => std::panic::panic_any(Recursive(self.counts.clone())),
            _ => unreachable!(),
        }
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
        let number = self.counts.predicates.fetch_add(1, Ordering::SeqCst) + 1;
        assert_ne!(
            self.counts.panic_predicate.load(Ordering::SeqCst),
            number,
            "batch input predicate panic"
        );
        matches!(self.mode, Mode::Batch)
            && !(self.counts.barrier.load(Ordering::SeqCst) && message.is::<Input>())
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.counts.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if message.downcast_ref::<u64>() == Some(&0) {
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
}
fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(std::task::Waker::noop()))
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("dispatch input ownership stalled")
}
fn caught(action: impl FnOnce()) -> Option<String> {
    std::panic::catch_unwind(AssertUnwindSafe(action))
        .err()
        .map(|payload| {
            let text = payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string input panic".into());
            if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(secondary);
            }
            text
        })
}
fn isolated(test: &str) -> bool {
    const CHILD: &str = "PARROT_DISPATCH_INPUT_CHILD";
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
        "input child failed (status={status}, timed_out={timed_out}): {}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    true
}
fn mailbox() -> Arc<dyn Mailbox> {
    Arc::new(MpscMailbox::new(8, ActorPath::placeholder("/input")))
}
async fn request(
    mailbox: &Arc<dyn Mailbox>,
    payload: BoxedMessage,
) -> parrot::thread::reply::ReplyReceiver {
    let (envelope, reply) = AskEnvelope::new(payload);
    mailbox
        .push_ask(envelope, BackpressureStrategy::Error)
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
        ThreadContext::new_for_test("/input"),
        "/input".into(),
        ThreadActorConfig {
            message_batch_size: Some(if matches!(mode, Mode::Single) { 1 } else { 4 }),
            ..Default::default()
        },
    ));
    *counts.processor.lock().unwrap() = Arc::downgrade(&processor);
    bounded(processor.initialize_and_start()).await.unwrap();
    (processor, counts)
}
fn input(counts: &Arc<Counts>, mode: u8) -> BoxedMessage {
    Box::new(Input {
        counts: counts.clone(),
        mode,
    })
}
async fn retirement(processor: &Arc<ActorProcessor<Probe>>, mode: u8) {
    let first = bounded(processor.clone().retire_erased()).await;
    let second = bounded(processor.retirement()).await;
    if mode == 0 {
        first.unwrap();
        second.unwrap();
    } else {
        let error = first.unwrap_err().to_string();
        assert!(
            error.contains("dispatch input") || error.contains("payload destructor panicked"),
            "{error}"
        );
        assert_eq!(second.unwrap_err().to_string(), error);
    }
}
async fn waiting(mode: Mode, panic: u8, erased: bool) {
    let (processor, counts) = fixture(mode).await;
    let active_mailbox = mailbox();
    let active_reply = request(&active_mailbox, Box::new(0_u64)).await;
    let mut active = Box::pin(processor.process_batch_of_messages(&active_mailbox, 1, false));
    assert!(poll_once(active.as_mut()).is_pending());
    assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
    let waiting_mailbox = mailbox();
    let reply = request(&waiting_mailbox, input(&counts, panic)).await;
    let mut waiting: Pin<Box<dyn Future<Output = _> + Send + '_>> = if erased {
        processor
            .clone()
            .process_batch_erased(waiting_mailbox.clone(), 1, false)
    } else {
        Box::pin(processor.process_batch_of_messages(&waiting_mailbox, 1, false))
    };
    assert!(poll_once(waiting.as_mut()).is_pending());
    assert!(waiting_mailbox.is_empty().await, "input was not dequeued");
    let escaped = caught(|| drop(waiting));
    counts.release.store(true, Ordering::SeqCst);
    bounded(active).await.unwrap();
    drop(bounded(active_reply).await.unwrap().unwrap());
    assert!(
        escaped.is_none(),
        "waiting input escaped cancellation: {escaped:?}"
    );
    assert!(
        bounded(reply).await.is_err(),
        "cancelled dispatch published a fabricated reply"
    );
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(panic == 2)
    );
    assert_eq!(
        processor.has_failed(),
        panic != 0,
        "lost or fabricated input cleanup failure"
    );
    let next = request(&waiting_mailbox, Box::new(1_u64)).await;
    let result = bounded(processor.process_batch_of_messages(&waiting_mailbox, 1, false)).await;
    if panic == 0 {
        result.unwrap();
        assert_eq!(
            *bounded(next)
                .await
                .unwrap()
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            1
        );
        assert_eq!(counts.calls.load(Ordering::SeqCst), 2);
    } else {
        assert!(result.is_err());
        assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
        // The request stayed in the mailbox because failed admission consumed nothing.
        drop(waiting_mailbox.pop_item().await);
        assert!(bounded(next).await.is_err());
    }
    retirement(&processor, panic).await;
}
async fn predicate(panic: u8, number: usize) {
    let (processor, counts) = fixture(Mode::Batch).await;
    counts.panic_predicate.store(number, Ordering::SeqCst);
    let mailbox = mailbox();
    let first = request(&mailbox, input(&counts, panic)).await;
    let second = if number == 2 {
        Some(request(&mailbox, input(&counts, panic)).await)
    } else {
        None
    };
    let mut execution = Box::pin(processor.process_batch_of_messages(&mailbox, number, false));
    let mut result = None;
    // Retire an escaped recursive payload before a reporting assertion unwinds.
    let escaped = caught(|| result = Some(poll_once(execution.as_mut())));
    let disposal = caught(|| drop(execution));
    assert!(
        escaped.is_none() && matches!(result, Some(Poll::Ready(Err(_)))),
        "predicate failure escaped or stayed pending: {escaped:?}"
    );
    assert!(
        disposal.is_none(),
        "predicate future disposal escaped: {disposal:?}"
    );
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), number);
    assert_eq!(
        counts.unlocked.load(Ordering::SeqCst),
        number,
        "predicate input disposed under execution lock"
    );
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        if panic == 2 { number } else { 0 }
    );
    assert!(bounded(first).await.unwrap().is_err());
    if let Some(second) = second {
        assert!(bounded(second).await.unwrap().is_err());
    }
    assert!(processor.has_failed());
    // Even healthy input disposal cannot resume a predicate panic.
    let error = bounded(processor.clone().retire_erased()).await;
    if panic == 0 {
        error.unwrap();
    } else {
        assert!(error.is_err());
    }
}
async fn lookahead(panic: u8) {
    let (processor, counts) = fixture(Mode::Batch).await;
    counts.barrier.store(true, Ordering::SeqCst);
    let mailbox = mailbox();
    let first = request(&mailbox, Box::new(0_u64)).await;
    let second = request(&mailbox, input(&counts, panic)).await;
    let mut execution = Box::pin(processor.process_batch_of_messages(&mailbox, 2, false));
    assert!(poll_once(execution.as_mut()).is_pending());
    assert!(mailbox.is_empty().await);
    assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
    let escaped = caught(|| drop(execution));
    assert!(
        escaped.is_none(),
        "lookahead cancellation escaped: {escaped:?}"
    );
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.unlocked.load(Ordering::SeqCst),
        1,
        "lookahead disposed under execution lock"
    );
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(panic == 2)
    );
    assert!(bounded(first).await.is_err());
    assert!(bounded(second).await.is_err());
    retirement(&processor, panic).await;
}

#[derive(Debug)]
struct PausingMailbox {
    inner: MpscMailbox,
    pops: AtomicUsize,
}
#[async_trait]
impl Mailbox for PausingMailbox {
    async fn push(
        &self,
        message: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> Result<(), MailboxError> {
        self.inner.push(message, strategy).await
    }
    async fn pop(&self) -> Option<BoxedMessage> {
        if self.pops.fetch_add(1, Ordering::SeqCst) == 2 {
            std::future::pending::<()>().await;
        }
        self.inner.pop().await
    }
    async fn is_empty(&self) -> bool {
        self.inner.is_empty().await
    }
    async fn signal_ready(&self) {
        self.inner.signal_ready().await;
    }
    fn path(&self) -> &ActorPath {
        self.inner.path()
    }
    fn capacity(&self) -> usize {
        self.inner.capacity()
    }
    async fn len(&self) -> usize {
        self.inner.len().await
    }
    fn seal(&self) {
        self.inner.seal();
    }
    async fn close(&self) {
        self.inner.close().await;
    }
    fn set_processor(&self, processor: Arc<dyn ProcessorInterface>) {
        self.inner.set_processor(processor);
    }
    fn get_processor(&self) -> Option<Arc<dyn ProcessorInterface>> {
        self.inner.get_processor()
    }
    fn has_processor(&self) -> bool {
        self.inner.has_processor()
    }
    fn set_wake_hook(&self, hook: WakeHook) {
        self.inner.set_wake_hook(hook);
    }
    fn fire_wake_hook(&self) {
        self.inner.fire_wake_hook();
    }
    fn schedule_state(&self) -> &ScheduleState {
        self.inner.schedule_state()
    }
}
async fn collecting(panic: u8) {
    let (processor, counts) = fixture(Mode::Batch).await;
    let concrete = Arc::new(PausingMailbox {
        inner: MpscMailbox::new(8, ActorPath::placeholder("/input")),
        pops: AtomicUsize::new(0),
    });
    let mailbox: Arc<dyn Mailbox> = concrete.clone();
    let first = request(&mailbox, input(&counts, panic)).await;
    let second = request(&mailbox, input(&counts, panic)).await;
    let mut execution = Box::pin(processor.process_batch_of_messages(&mailbox, 3, false));
    assert!(poll_once(execution.as_mut()).is_pending());
    assert_eq!(concrete.pops.load(Ordering::SeqCst), 3);
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    let escaped = caught(|| drop(execution));
    assert!(
        escaped.is_none(),
        "batch collection cancellation escaped: {escaped:?}"
    );
    assert_eq!(counts.drops.load(Ordering::SeqCst), 2);
    assert_eq!(
        counts.unlocked.load(Ordering::SeqCst),
        2,
        "collected input disposed under execution lock"
    );
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        if panic == 2 { 2 } else { 0 }
    );
    assert!(bounded(first).await.is_err());
    assert!(bounded(second).await.is_err());
    retirement(&processor, panic).await;
}
async fn delivered(mode: Mode) {
    let (processor, counts) = fixture(mode).await;
    let mailbox = mailbox();
    let message = Box::new(Input {
        counts: counts.clone(),
        mode: 0,
    });
    let pointer = message.as_ref() as *const Input;
    let reply = request(&mailbox, message).await;
    assert_eq!(
        bounded(processor.process_batch_of_messages(&mailbox, 1, false))
            .await
            .unwrap(),
        (1, 0)
    );
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    assert!(!processor.has_failed());
    let message = bounded(reply)
        .await
        .unwrap()
        .unwrap()
        .downcast::<Input>()
        .expect("input type was replaced");
    assert_eq!(message.as_ref() as *const Input, pointer);
    assert!(Arc::ptr_eq(&message.counts, &counts));
    drop(message);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.unlocked.load(Ordering::SeqCst), 1);
    retirement(&processor, 0).await;
}

async fn blocking_disposal(panic: u8) {
    let (processor, counts) = fixture(Mode::Batch).await;
    counts.block_drop.store(true, Ordering::SeqCst);
    let mailbox: Arc<dyn Mailbox> = Arc::new(PausingMailbox {
        inner: MpscMailbox::new(8, ActorPath::placeholder("/input")),
        pops: AtomicUsize::new(1),
    });
    let reply = request(&mailbox, input(&counts, panic)).await;
    let owned_processor = processor.clone();
    let mut execution = Box::pin(async move {
        owned_processor
            .process_batch_of_messages(&mailbox, 2, false)
            .await
    });
    assert!(poll_once(execution.as_mut()).is_pending());
    let dropper = std::thread::spawn(move || caught(|| drop(execution)));
    bounded(async {
        while !counts.drop_entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut retiring = processor.clone().retire_erased();
    let before = poll_once(retiring.as_mut());
    let premature = before.is_ready();
    // Release before reporting any assertion, so a failed candidate cannot
    // strand the native cleanup thread or the test runtime.
    *counts.drop_release.0.lock().unwrap() = true;
    counts.drop_release.1.notify_all();
    let escaped = dropper.join().unwrap();
    let result = match before {
        Poll::Ready(result) => result,
        Poll::Pending => bounded(retiring).await,
    };
    assert!(
        !premature,
        "retirement completed while input disposal was blocked"
    );
    assert!(
        escaped.is_none(),
        "blocked input disposal escaped: {escaped:?}"
    );
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.unlocked.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(panic == 2)
    );
    assert!(bounded(reply).await.is_err());
    if panic == 0 {
        result.unwrap();
    } else {
        assert!(result.is_err());
    }
    retirement(&processor, panic).await;
}

#[tokio::test]
async fn blocked_input_delays_successful_retirement() {
    blocking_disposal(0).await;
}
#[tokio::test]
async fn blocked_input_retains_late_failure() {
    blocking_disposal(1).await;
}
#[tokio::test]
async fn blocked_input_retains_late_recursive_failure() {
    blocking_disposal(2).await;
}

#[tokio::test]
async fn single_delivered() {
    delivered(Mode::Single).await;
}
#[tokio::test]
async fn batch_delivered() {
    delivered(Mode::Batch).await;
}
#[tokio::test]
async fn fallback_delivered() {
    delivered(Mode::Fallback).await;
}

macro_rules! waiting_tests {
    ($mode:ident, $erased:expr, $clean:ident, $panic:ident, $recursive:ident) => {
        #[tokio::test]
        async fn $clean() {
            waiting(Mode::$mode, 0, $erased).await;
        }
        #[tokio::test]
        async fn $panic() {
            waiting(Mode::$mode, 1, $erased).await;
        }
        #[tokio::test]
        async fn $recursive() {
            waiting(Mode::$mode, 2, $erased).await;
        }
    };
}
waiting_tests!(
    Single,
    false,
    single_wait_clean,
    single_wait_panic,
    single_wait_recursive
);
waiting_tests!(
    Batch,
    false,
    batch_wait_clean,
    batch_wait_panic,
    batch_wait_recursive
);
waiting_tests!(
    Fallback,
    false,
    fallback_wait_clean,
    fallback_wait_panic,
    fallback_wait_recursive
);
waiting_tests!(
    Single,
    true,
    single_erased_clean,
    single_erased_panic,
    single_erased_recursive
);
waiting_tests!(
    Batch,
    true,
    batch_erased_clean,
    batch_erased_panic,
    batch_erased_recursive
);
waiting_tests!(
    Fallback,
    true,
    fallback_erased_clean,
    fallback_erased_panic,
    fallback_erased_recursive
);
macro_rules! isolated_tests {
    ($function:ident $(, $extra:expr)?; $clean:ident, $panic:ident, $recursive:ident) => {
        #[tokio::test] async fn $clean() { if !isolated(stringify!($clean)) { $function(0 $(, $extra)?).await; } }
        #[tokio::test] async fn $panic() { if !isolated(stringify!($panic)) { $function(1 $(, $extra)?).await; } }
        #[tokio::test] async fn $recursive() { if !isolated(stringify!($recursive)) { $function(2 $(, $extra)?).await; } }
    };
}
isolated_tests!(predicate, 1; first_predicate_clean, first_predicate_panic, first_predicate_recursive);
isolated_tests!(predicate, 2; second_predicate_clean, second_predicate_panic, second_predicate_recursive);
isolated_tests!(lookahead; lookahead_clean, lookahead_panic, lookahead_recursive);
isolated_tests!(collecting; collecting_clean, collecting_panic, collecting_recursive);
