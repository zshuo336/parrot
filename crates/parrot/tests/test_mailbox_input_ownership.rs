//! Native mailbox admission owns pending inputs, and retires detached values independently.
#[path = "support/manager_ownership.rs"]
#[allow(dead_code)]
mod support;

use futures::FutureExt;
use parrot::thread::{
    BackpressureStrategy, ThreadActorRef, ThreadContext,
    envelope::AskEnvelope,
    error::MailboxError,
    mailbox::{
        Mailbox, MailboxItem, mpsc::MpscMailbox, spsc::SpscMailbox,
        spsc_ringbuf::SpscRingbufMailbox,
    },
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    message::MessageOptions,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(5);
type Factory = fn() -> Arc<dyn Mailbox>;
fn mpsc_box() -> Arc<dyn Mailbox> {
    Arc::new(MpscMailbox::new(1, ActorPath::for_test("/owned")))
}
fn spsc_box() -> Arc<dyn Mailbox> {
    Arc::new(SpscMailbox::new(1, ActorPath::for_test("/owned")))
}
fn ring_box() -> Arc<dyn Mailbox> {
    Arc::new(SpscRingbufMailbox::new(1, ActorPath::for_test("/owned")))
}
struct Input {
    drops: Arc<AtomicUsize>,
    reason: &'static str,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("{}", self.reason);
    }
}
fn input(drops: &Arc<AtomicUsize>) -> BoxedMessage {
    Box::new(Input {
        drops: drops.clone(),
        reason: "mailbox input destructor",
    })
}
async fn retired(drops: &AtomicUsize, expected: usize) {
    tokio::time::timeout(DEADLINE, async {
        while drops.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mailbox inputs must actually retire");
    assert_eq!(drops.load(Ordering::SeqCst), expected);
}
async fn fill(mailbox: &dyn Mailbox) {
    mailbox
        .push(Box::new(17_u64), BackpressureStrategy::Error)
        .await
        .unwrap();
}
async fn value(mailbox: &dyn Mailbox, expected: u64) {
    assert_eq!(mailbox.len().await, 1);
    assert_eq!(
        *mailbox.pop().await.unwrap().downcast::<u64>().unwrap(),
        expected
    );
    assert!(mailbox.pop().await.is_none());
}
async fn healthy(mailbox: &dyn Mailbox) {
    mailbox
        .push(Box::new(91_u64), BackpressureStrategy::Error)
        .await
        .unwrap();
    value(mailbox, 91).await;
}
fn push<'a>(
    mailbox: &'a dyn Mailbox,
    message: BoxedMessage,
    strategy: BackpressureStrategy,
    operation: usize,
) -> BoxedFuture<'a, Result<(), MailboxError>> {
    match operation {
        0 => mailbox.push(message, strategy),
        1 => mailbox.push_item(MailboxItem::Plain(message), strategy),
        2 => mailbox.push_ask(AskEnvelope::with_boxed(message).0, strategy),
        3 => mailbox.push_item(
            MailboxItem::Ask(AskEnvelope::with_boxed(message).0),
            strategy,
        ),
        _ => mailbox
            .push_with_priority(message, strategy, operation == 5)
            .map(|result| result.map(|_| ()))
            .boxed(),
    }
}
async fn pending_inputs(factory: Factory) {
    let mailbox = factory();
    fill(mailbox.as_ref()).await;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut escaped = Vec::new();
    for operation in 0..6 {
        let mut pending = push(
            mailbox.as_ref(),
            input(&drops),
            BackpressureStrategy::Block,
            operation,
        );
        assert!(
            futures::poll!(&mut pending).is_pending(),
            "capacity must be the actual wait"
        );
        if catch_unwind(AssertUnwindSafe(|| drop(pending))).is_err() {
            escaped.push(operation);
        }
    }
    retired(&drops, 6).await;
    value(mailbox.as_ref(), 17).await;
    healthy(mailbox.as_ref()).await;
    mailbox.close().await;
    assert!(
        escaped.is_empty(),
        "mailbox pending cancellation escaped: {escaped:?}"
    );
}

struct Business;
impl Actor for Business {
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
        Box::pin(async move { Ok(message) })
    }
}
async fn public_pending_inputs(factory: Factory) {
    let mailbox = factory();
    fill(mailbox.as_ref()).await;
    let reference = ThreadActorRef::<Business>::new(
        ActorPath::for_test("/owned"),
        Arc::downgrade(&(mailbox.clone() as Arc<dyn Mailbox + Send + Sync>)),
        BackpressureStrategy::Block,
        DEADLINE,
        None,
    );
    let drops = Arc::new(AtomicUsize::new(0));
    let mut escaped = Vec::new();
    for operation in 0..13 {
        let message = input(&drops);
        let mut pending: BoxedFuture<'_, ActorResult<()>> = match operation {
            0 => reference.send(message).map(|r| r.map(|_| ())).boxed(),
            1 => reference
                .send_with_timeout(message, None)
                .map(|r| r.map(|_| ()))
                .boxed(),
            2 => reference
                .send_with_timeout(message, Some(DEADLINE))
                .map(|r| r.map(|_| ()))
                .boxed(),
            3 => reference
                .send_with_options(message, MessageOptions::default())
                .map(|r| r.map(|_| ()))
                .boxed(),
            4 => reference.deliver(message),
            5 => reference.deliver_with_options(message, MessageOptions::default()),
            6 => reference.send_msg(message).boxed(),
            7 => reference
                .send_with_strategy(message, BackpressureStrategy::Block)
                .boxed(),
            8 => reference
                .send_with_priority(message, BackpressureStrategy::Block, true)
                .boxed(),
            9 => reference
                .ask_with_strategy_and_timeout(message, BackpressureStrategy::Block, DEADLINE)
                .map(|r| r.map(|_| ()))
                .boxed(),
            10 => reference.ask(message).map(|r| r.map(|_| ())).boxed(),
            11 => reference
                .ask_unbounded(message, BackpressureStrategy::Block)
                .map(|r| r.map(|_| ()))
                .boxed(),
            _ => reference
                .ask_with_timeout(message, DEADLINE)
                .map(|r| r.map(|_| ()))
                .boxed(),
        };
        assert!(futures::poll!(&mut pending).is_pending());
        if catch_unwind(AssertUnwindSafe(|| drop(pending))).is_err() {
            escaped.push(operation);
        }
    }
    retired(&drops, 13).await;
    value(mailbox.as_ref(), 17).await;
    reference.deliver(Box::new(91_u64)).await.unwrap();
    value(mailbox.as_ref(), 91).await;
    mailbox.close().await;
    assert!(
        escaped.is_empty(),
        "public pending cancellation escaped: {escaped:?}"
    );
}

async fn rejected_inputs(factory: Factory) {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut wrong = Vec::new();
    for category in 0..3 {
        let mailbox = factory();
        if category == 2 {
            mailbox.close().await;
        } else {
            fill(mailbox.as_ref()).await;
        }
        for operation in 0..6 {
            let strategy = if category == 1 {
                BackpressureStrategy::DropNewest
            } else {
                BackpressureStrategy::Error
            };
            let outcome =
                AssertUnwindSafe(push(mailbox.as_ref(), input(&drops), strategy, operation))
                    .catch_unwind()
                    .await;
            let correct = matches!(
                (&outcome, category),
                (Ok(Err(MailboxError::Full { capacity: 1 })), 0)
                    | (Ok(Err(MailboxError::Dropped { capacity: 1 })), 1)
                    | (Ok(Err(MailboxError::Closed)), 2)
            );
            if !correct {
                wrong.push((category, operation));
            }
        }
        if category != 2 {
            value(mailbox.as_ref(), 17).await;
            healthy(mailbox.as_ref()).await;
        }
        assert_eq!(mailbox.len().await, 0);
        mailbox.close().await;
    }
    retired(&drops, 18).await;
    assert!(
        wrong.is_empty(),
        "mailbox rejection changed category or escaped: {wrong:?}"
    );
}

async fn evicted_input(factory: Factory) {
    let mailbox = factory();
    let wakes = Arc::new(AtomicUsize::new(0));
    let observed = wakes.clone();
    mailbox.set_wake_hook(Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    let drops = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(input(&drops), BackpressureStrategy::Error)
        .await
        .unwrap();
    let outcome =
        AssertUnwindSafe(mailbox.push(Box::new(29_u64), BackpressureStrategy::DropOldest))
            .catch_unwind()
            .await;
    let signalled = wakes.load(Ordering::SeqCst);
    value(mailbox.as_ref(), 29).await;
    healthy(mailbox.as_ref()).await;
    mailbox.close().await;
    retired(&drops, 1).await;
    assert!(
        matches!(outcome, Ok(Err(MailboxError::PushError(ref error)))
        if error.contains("admitted") && error.contains("mailbox input destructor")),
        "eviction failure must identify committed replacement: {outcome:?}"
    );
    assert_eq!(
        signalled, 2,
        "replacement must wake the scheduler before retiring old input"
    );
}

struct BlockingDrop {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: mpsc::Receiver<()>,
    drops: Arc<AtomicUsize>,
}
impl Drop for BlockingDrop {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        let _ = self.entered.take().unwrap().send(());
        self.release
            .recv_timeout(DEADLINE)
            .expect("external release must arrive");
    }
}
async fn eviction_keeps_host_progress(factory: Factory) {
    let mailbox = factory();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, released) = mpsc::channel();
    let (host, progressed) = mpsc::channel();
    let saw_progress = Arc::new(AtomicBool::new(false));
    let saw = saw_progress.clone();
    // The external guardian always releases Drop, even when the old code blocks this runtime.
    let guardian = std::thread::spawn(move || {
        saw.store(
            progressed.recv_timeout(Duration::from_secs(1)).is_ok(),
            Ordering::SeqCst,
        );
        release.send(()).unwrap();
    });
    let drops = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(BlockingDrop {
                entered: Some(entered),
                release: released,
                drops: drops.clone(),
            }),
            BackpressureStrategy::Error,
        )
        .await
        .unwrap();
    let inspect = mailbox.clone();
    let observer = tokio::spawn(async move {
        started.await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        value(inspect.as_ref(), 29).await;
        host.send(()).unwrap_or(());
    });
    let outcome = mailbox
        .push(Box::new(29_u64), BackpressureStrategy::DropOldest)
        .await;
    observer.await.unwrap();
    guardian.join().unwrap();
    healthy(mailbox.as_ref()).await;
    mailbox.close().await;
    assert!(outcome.is_ok());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(
        saw_progress.load(Ordering::SeqCst),
        "evicted Drop blocked host timer and mailbox inspection"
    );
}

struct NestedClose {
    mailbox: Arc<dyn Mailbox>,
    completed: Arc<AtomicBool>,
}
impl Drop for NestedClose {
    fn drop(&mut self) {
        let finished = futures::executor::block_on(tokio::time::timeout(
            Duration::from_millis(500),
            self.mailbox.close(),
        ))
        .is_ok();
        self.completed.store(finished, Ordering::SeqCst);
    }
}
async fn reentrant_cleanup(factory: Factory) {
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let nested = factory();
    let drops = Arc::new(AtomicUsize::new(0));
    nested
        .push(
            Box::new(Counted(drops.clone())),
            BackpressureStrategy::Error,
        )
        .await
        .unwrap();
    let mailbox = factory();
    let completed = Arc::new(AtomicBool::new(false));
    mailbox
        .push(
            Box::new(NestedClose {
                mailbox: nested.clone(),
                completed: completed.clone(),
            }),
            BackpressureStrategy::Error,
        )
        .await
        .unwrap();
    mailbox
        .push(Box::new(29_u64), BackpressureStrategy::DropOldest)
        .await
        .unwrap();
    value(mailbox.as_ref(), 29).await;
    mailbox.close().await;
    retired(&drops, 1).await;
    assert!(nested.is_closed().await);
    assert_eq!(nested.len().await, 0);
    assert!(
        completed.load(Ordering::SeqCst),
        "nested cleanup waited for its own executor slot"
    );
}

#[tokio::test]
async fn mpsc_reentrant_cleanup() {
    reentrant_cleanup(mpsc_box).await;
}
#[tokio::test]
async fn spsc_reentrant_cleanup() {
    reentrant_cleanup(spsc_box).await;
}
#[tokio::test]
async fn ring_reentrant_cleanup() {
    reentrant_cleanup(ring_box).await;
}

fn independent_close_panics(factory: Factory) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mailbox = factory();
        let drops = Arc::new(AtomicUsize::new(0));
        mailbox
            .push(
                Box::new(Input {
                    drops: drops.clone(),
                    reason: "first detached input",
                }),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
        mailbox
            .push(
                Box::new(Input {
                    drops: drops.clone(),
                    reason: "second detached input",
                }),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
        let result = AssertUnwindSafe(mailbox.close()).catch_unwind().await;
        assert_eq!(
            drops.load(Ordering::SeqCst),
            2,
            "both detached values must retire"
        );
        assert!(mailbox.is_closed().await);
        assert_eq!(mailbox.len().await, 0);
        let payload = result.expect_err("close must report disposal failures");
        let text = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            text.contains("first detached input") && text.contains("second detached input"),
            "close must preserve independent failures: {text}"
        );
    });
}
macro_rules! engine_contracts {
    ($pending:ident, $public:ident, $reject:ident, $evict:ident, $blocking:ident, $factory:ident) => {
        #[tokio::test]
        async fn $pending() {
            pending_inputs($factory).await;
        }
        #[tokio::test]
        async fn $public() {
            public_pending_inputs($factory).await;
        }
        #[tokio::test]
        async fn $reject() {
            rejected_inputs($factory).await;
        }
        #[tokio::test]
        async fn $evict() {
            evicted_input($factory).await;
        }
        #[tokio::test]
        async fn $blocking() {
            eviction_keeps_host_progress($factory).await;
        }
    };
}
engine_contracts!(
    mpsc_pending,
    mpsc_public_pending,
    mpsc_rejection,
    mpsc_eviction,
    mpsc_blocking_eviction,
    mpsc_box
);
engine_contracts!(
    spsc_pending,
    spsc_public_pending,
    spsc_rejection,
    spsc_eviction,
    spsc_blocking_eviction,
    spsc_box
);
engine_contracts!(
    ring_pending,
    ring_public_pending,
    ring_rejection,
    ring_eviction,
    ring_blocking_eviction,
    ring_box
);

#[test]
fn mpsc_independent_close_failures() {
    support::isolated("mpsc_independent_close_failures", || {
        independent_close_panics(|| Arc::new(MpscMailbox::new(2, ActorPath::for_test("/drain"))))
    });
}
#[test]
fn spsc_independent_close_failures() {
    support::isolated("spsc_independent_close_failures", || {
        independent_close_panics(|| Arc::new(SpscMailbox::new(2, ActorPath::for_test("/drain"))))
    });
}
#[test]
fn ring_independent_close_failures() {
    support::isolated("ring_independent_close_failures", || {
        independent_close_panics(|| {
            Arc::new(SpscRingbufMailbox::new(2, ActorPath::for_test("/drain")))
        })
    });
}
#[tokio::test]
async fn healthy_mailboxes_keep_fifo_and_exact_replies() {
    for factory in [mpsc_box as Factory, spsc_box, ring_box] {
        let mailbox = factory();
        let (envelope, reply) = AskEnvelope::with_boxed(Box::new(73_u64));
        mailbox
            .push_ask(envelope, BackpressureStrategy::Error)
            .await
            .unwrap();
        let mut envelope = match mailbox.pop_item().await.unwrap() {
            MailboxItem::Ask(envelope) => envelope,
            MailboxItem::Plain(message) => *message
                .downcast::<AskEnvelope>()
                .expect("legacy ask representation"),
        };
        envelope
            .take_reply()
            .unwrap()
            .send(Ok(Box::new(79_u64)))
            .unwrap();
        assert_eq!(
            *reply.await.unwrap().unwrap().downcast::<u64>().unwrap(),
            79
        );
        healthy(mailbox.as_ref()).await;
        mailbox.close().await;
    }
}
