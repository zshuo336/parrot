//! Cancellation ends a cleanup waiter, not ownership of its detached values.
use futures::FutureExt;
use parrot::thread::{
    BackpressureStrategy,
    envelope::AskEnvelope,
    error::MailboxError,
    mailbox::{
        Mailbox, MailboxItem, mpsc::MpscMailbox, spsc::SpscMailbox,
        spsc_ringbuf::SpscRingbufMailbox,
    },
};
use parrot_api::{address::ActorPath, types::BoxedMessage};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(5);
type Counts = Arc<[AtomicUsize; 3]>;

struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

struct Value {
    index: usize,
    drops: Counts,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<mpsc::Receiver<()>>,
}
impl Drop for Value {
    fn drop(&mut self) {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        if let Some(release) = &self.release {
            release
                .recv_timeout(LIMIT)
                .expect("guardian must release destructor");
        }
        self.drops[self.index].fetch_add(1, Ordering::SeqCst);
        if self.index < 2 {
            panic!("independent mailbox value {}", self.index);
        }
    }
}

fn mailbox(kind: usize, capacity: usize) -> Arc<dyn Mailbox> {
    let path = ActorPath::for_test("/cleanup-cancellation");
    match kind {
        0 => Arc::new(MpscMailbox::new(capacity, path)),
        1 => Arc::new(SpscMailbox::new(capacity, path)),
        _ => Arc::new(SpscRingbufMailbox::new(capacity, path)),
    }
}

fn counts() -> Counts {
    Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)))
}

fn blocked(drops: &Counts) -> (Value, Release, tokio::sync::oneshot::Receiver<()>) {
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, released) = mpsc::channel();
    (
        Value {
            index: 0,
            drops: drops.clone(),
            entered: Some(entered),
            release: Some(released),
        },
        Release(Some(release)),
        started,
    )
}

async fn retired(drops: &Counts, expected: [usize; 3]) {
    tokio::time::timeout(LIMIT, async {
        while drops.each_ref().map(|count| count.load(Ordering::SeqCst)) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every detached value must actually be attempted");
}

async fn close_cancelled(kind: usize) {
    let mailbox = mailbox(kind, 3);
    let drops = counts();
    let (first, release, started) = blocked(&drops);
    mailbox
        .push(Box::new(first), BackpressureStrategy::Error)
        .await
        .unwrap();
    for index in 1..3 {
        mailbox
            .push(
                Box::new(Value {
                    index,
                    drops: drops.clone(),
                    entered: None,
                    release: None,
                }),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
    }
    assert_eq!(mailbox.len().await, 3);
    let mut close = Box::pin(mailbox.close());
    assert!(futures::poll!(&mut close).is_pending());
    tokio::time::timeout(LIMIT, started).await.unwrap().unwrap();
    assert_eq!(drops[0].load(Ordering::SeqCst), 0, "destructor is blocked");
    assert!(mailbox.is_closed().await);
    assert_eq!(mailbox.len().await, 0);
    assert!(
        futures::poll!(&mut close).is_pending(),
        "empty queue is not completion"
    );
    assert!(catch_unwind(AssertUnwindSafe(|| drop(close))).is_ok());
    assert_eq!(
        drops[0].load(Ordering::SeqCst),
        0,
        "cancel is not completion"
    );
    drop(release);
    retired(&drops, [1, 1, 1]).await;
    // Keep the mailbox alive until all attempts finish so final Drop cannot
    // conceal cleanup lost by the cancelled close operation.
    assert!(mailbox.pop().await.is_none());
    assert!(catch_unwind(AssertUnwindSafe(|| drop(mailbox))).is_ok());
    assert_eq!(
        drops.each_ref().map(|count| count.load(Ordering::SeqCst)),
        [1, 1, 1]
    );
}

async fn push(
    mailbox: &dyn Mailbox,
    value: BoxedMessage,
    strategy: BackpressureStrategy,
    entry: usize,
) -> Result<(), MailboxError> {
    match entry {
        0 => mailbox.push(value, strategy).await,
        1 => mailbox.push_item(MailboxItem::Plain(value), strategy).await,
        2 => {
            mailbox
                .push_ask(AskEnvelope::with_boxed(value).0, strategy)
                .await
        }
        3 => {
            mailbox
                .push_item(MailboxItem::Ask(AskEnvelope::with_boxed(value).0), strategy)
                .await
        }
        _ => mailbox
            .push_with_priority(value, strategy, entry == 5)
            .await
            .map(|_| ()),
    }
}

async fn rejected(kind: usize) {
    for category in 0..3 {
        for entry in 0..6 {
            for cancel in [false, true] {
                let mailbox = mailbox(kind, 1);
                if category == 2 {
                    mailbox.close().await;
                } else {
                    mailbox
                        .push(Box::new(17_u64), BackpressureStrategy::Error)
                        .await
                        .unwrap();
                }
                let drops = counts();
                let (value, release, started) = blocked(&drops);
                let strategy = if category == 1 {
                    BackpressureStrategy::DropNewest
                } else {
                    BackpressureStrategy::Error
                };
                let mut pending =
                    Box::pin(push(mailbox.as_ref(), Box::new(value), strategy, entry));
                assert!(
                    futures::poll!(&mut pending).is_pending(),
                    "rejection must await disposal"
                );
                tokio::time::timeout(LIMIT, started).await.unwrap().unwrap();
                assert_eq!(drops[0].load(Ordering::SeqCst), 0);
                assert!(
                    futures::poll!(&mut pending).is_pending(),
                    "blocked disposal cannot finish rejection"
                );
                if category != 2 {
                    assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 17);
                    mailbox
                        .push(Box::new(91_u64), BackpressureStrategy::Error)
                        .await
                        .unwrap();
                    assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 91);
                }
                if cancel {
                    assert!(catch_unwind(AssertUnwindSafe(|| drop(pending))).is_ok());
                    assert_eq!(drops[0].load(Ordering::SeqCst), 0);
                    drop(release);
                } else {
                    drop(release);
                    let outcome =
                        tokio::time::timeout(LIMIT, AssertUnwindSafe(pending).catch_unwind())
                            .await
                            .unwrap()
                            .expect("disposal failure must retain rejection category");
                    assert!(matches!(
                        (category, outcome),
                        (0, Err(MailboxError::Full { capacity: 1 }))
                            | (1, Err(MailboxError::Dropped { capacity: 1 }))
                            | (2, Err(MailboxError::Closed))
                    ));
                }
                retired(&drops, [1, 0, 0]).await;
                assert_eq!(mailbox.len().await, 0);
                mailbox.close().await;
                drop(mailbox);
                assert_eq!(
                    drops.each_ref().map(|count| count.load(Ordering::SeqCst)),
                    [1, 0, 0]
                );
            }
        }
    }
}

fn isolated(name: &str, body: impl std::future::Future<Output = ()>) {
    if std::env::var("PARROT_MAILBOX_CLEANUP_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(body);
        println!("MAILBOX_CLEANUP_COMPLETED {name}");
        return;
    }
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let log = tempfile::NamedTempFile::new().unwrap();
    let output = log.reopen().unwrap();
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_MAILBOX_CLEANUP_CHILD", name)
            .env("RUST_BACKTRACE", "0")
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let text = std::fs::read_to_string(log.path()).unwrap();
            assert!(
                status.success(),
                "mailbox cleanup child failed: {status}\n{text}"
            );
            assert!(
                text.contains("1 passed; 0 failed; 0 ignored;"),
                "child must execute exactly one case: {text}"
            );
            assert_eq!(
                text.lines()
                    .filter(|line| *line == format!("MAILBOX_CLEANUP_COMPLETED {name}"))
                    .count(),
                1,
                "child must complete the requested scenario: {text}"
            );
            return;
        }
        assert!(Instant::now() < deadline, "mailbox cleanup child timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

macro_rules! contract {
    ($name:ident, $exercise:ident, $kind:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), $exercise($kind));
        }
    };
}
contract!(
    mpsc_cancelled_close_retires_detached_values,
    close_cancelled,
    0
);
contract!(
    spsc_cancelled_close_retires_detached_values,
    close_cancelled,
    1
);
contract!(
    ring_cancelled_close_retires_detached_values,
    close_cancelled,
    2
);
contract!(
    mpsc_rejection_waits_for_disposal_despite_cancellation,
    rejected,
    0
);
contract!(
    spsc_rejection_waits_for_disposal_despite_cancellation,
    rejected,
    1
);
contract!(
    ring_rejection_waits_for_disposal_despite_cancellation,
    rejected,
    2
);
