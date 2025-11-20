//! Closing a mailbox must reject pending producers and leave no buffered work.
use futures::FutureExt;
use parrot::thread::config::BackpressureStrategy;
use parrot::thread::error::MailboxError;
use parrot::thread::mailbox::{
    Mailbox, mpsc::MpscMailbox, spsc::SpscMailbox, spsc_ringbuf::SpscRingbufMailbox,
};
use parrot_api::address::ActorPath;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

async fn close_pending(mailbox: Arc<dyn Mailbox>) {
    mailbox
        .push(Box::new(1_u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    let pending = mailbox.push(Box::new(2_u64), BackpressureStrategy::Block);
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    mailbox.close().await;
    let result = tokio::time::timeout(Duration::from_millis(200), pending).await;
    assert!(
        matches!(result, Ok(Err(MailboxError::Closed))),
        "pending producer must fail: {result:?}"
    );
    assert!(mailbox.is_closed().await);
    assert_eq!(mailbox.len().await, 0);
    assert!(mailbox.pop().await.is_none());
    assert!(matches!(
        mailbox
            .push(Box::new(3_u64), BackpressureStrategy::Error)
            .await,
        Err(MailboxError::Closed)
    ));
}

#[tokio::test]
async fn mpsc_close_rejects_waiting_producer() {
    close_pending(Arc::new(MpscMailbox::new(1, ActorPath::for_test("/mpsc")))).await;
}
#[tokio::test]
async fn spsc_close_rejects_waiting_producer() {
    close_pending(Arc::new(SpscMailbox::new(1, ActorPath::for_test("/spsc")))).await;
}
#[tokio::test]
async fn ringbuf_close_rejects_waiting_producer() {
    close_pending(Arc::new(SpscRingbufMailbox::new(
        1,
        ActorPath::for_test("/ring"),
    )))
    .await;
}

struct BlockingDrop {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
    drops: Arc<AtomicUsize>,
}
impl Drop for BlockingDrop {
    fn drop(&mut self) {
        self.entered.notify_one();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .expect("close cleanup release");
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

async fn concurrent_close_waits_for_shared_cleanup(mailbox: Arc<dyn Mailbox>) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, receive) = mpsc::channel();
    let drops = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(BlockingDrop {
                entered: entered.clone(),
                release: Arc::new(Mutex::new(receive)),
                drops: drops.clone(),
            }),
            BackpressureStrategy::Error,
        )
        .await
        .unwrap();
    let mut first = Box::pin(mailbox.close());
    assert!(futures::poll!(&mut first).is_pending());
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let mut second = Box::pin(mailbox.close());
    assert!(
        futures::poll!(&mut second).is_pending(),
        "a concurrent close must wait for the first drain's destructor"
    );
    let mut third = Box::pin(mailbox.close());
    assert!(futures::poll!(&mut third).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(first);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        futures::future::join(second, third).await;
        mailbox.close().await;
    })
    .await
    .expect("all waiters and repeated close must see shared completion");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(mailbox.len().await, 0);
    assert!(mailbox.is_closed().await);
}

#[tokio::test]
async fn concurrent_mpsc_close_waits_for_shared_cleanup() {
    concurrent_close_waits_for_shared_cleanup(Arc::new(MpscMailbox::new(
        1,
        ActorPath::for_test("/concurrent-close-mpsc"),
    )))
    .await;
}

#[tokio::test]
async fn concurrent_spsc_close_waits_for_shared_cleanup() {
    concurrent_close_waits_for_shared_cleanup(Arc::new(SpscMailbox::new(
        1,
        ActorPath::for_test("/concurrent-close-spsc"),
    )))
    .await;
}

#[tokio::test]
async fn concurrent_ringbuf_close_waits_for_shared_cleanup() {
    concurrent_close_waits_for_shared_cleanup(Arc::new(SpscRingbufMailbox::new(
        1,
        ActorPath::for_test("/concurrent-close-ring"),
    )))
    .await;
}

struct FailedCloseDrop {
    index: usize,
    drops: Arc<[AtomicUsize; 3]>,
    block: Option<BlockingDrop>,
}
impl Drop for FailedCloseDrop {
    fn drop(&mut self) {
        drop(self.block.take());
        self.drops[self.index].fetch_add(1, Ordering::SeqCst);
        if self.index < 2 {
            panic!("close payload {} disposal", self.index);
        }
    }
}

async fn concurrent_close_preserves_failure(mailbox: Arc<dyn Mailbox>) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, receive) = mpsc::channel();
    let drops: Arc<[AtomicUsize; 3]> = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
    let mut block = Some(BlockingDrop {
        entered: entered.clone(),
        release: Arc::new(Mutex::new(receive)),
        drops: Arc::new(AtomicUsize::new(0)),
    });
    for index in 0..3 {
        mailbox
            .push(
                Box::new(FailedCloseDrop {
                    index,
                    drops: drops.clone(),
                    block: block.take(),
                }),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
    }
    let mut first = Box::pin(AssertUnwindSafe(mailbox.close()).catch_unwind());
    assert!(futures::poll!(&mut first).is_pending());
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("cleanup must enter the blocked first destructor");
    let mut waiters: Vec<_> = (0..3)
        .map(|_| Box::pin(AssertUnwindSafe(mailbox.close()).catch_unwind()))
        .collect();
    for waiter in &mut waiters {
        assert!(futures::poll!(waiter.as_mut()).is_pending());
    }
    assert_eq!(mailbox.len().await, 0, "empty is not physically retired");
    assert_eq!(drops.each_ref().map(|n| n.load(Ordering::SeqCst)), [0; 3]);
    drop(first);
    drop(waiters.pop());
    release.send(()).unwrap();
    let results = tokio::time::timeout(Duration::from_secs(2), futures::future::join_all(waiters))
        .await
        .expect("surviving waiters must finish even when cleanup fails");
    let mut receipts = Vec::new();
    for result in results {
        receipts.push(
            *result
                .expect_err("every waiter must report cleanup failure")
                .downcast::<String>()
                .expect("close failure must have a string receipt"),
        );
    }
    let repeated = tokio::time::timeout(
        Duration::from_secs(2),
        AssertUnwindSafe(mailbox.close()).catch_unwind(),
    )
    .await
    .expect("failed completion must remain repeatable")
    .expect_err("repeated close must retain the original cleanup failures");
    receipts.push(*repeated.downcast::<String>().unwrap());
    assert_eq!(receipts.len(), 3);
    assert!(receipts.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(receipts[0].contains("close payload 0 disposal"));
    assert!(receipts[0].contains("close payload 1 disposal"));
    assert_eq!(drops.each_ref().map(|n| n.load(Ordering::SeqCst)), [1; 3]);
    assert_eq!(mailbox.len().await, 0);
    assert!(mailbox.pop().await.is_none());
    assert!(mailbox.is_closed().await);
    assert!(matches!(
        mailbox
            .push(Box::new(91_u64), BackpressureStrategy::Error)
            .await,
        Err(MailboxError::Closed)
    ));
}

#[tokio::test]
async fn concurrent_mpsc_close_preserves_failure_after_waiter_cancellation() {
    concurrent_close_preserves_failure(Arc::new(MpscMailbox::new(
        3,
        ActorPath::for_test("/failed-close-mpsc"),
    )))
    .await;
}

#[tokio::test]
async fn concurrent_spsc_close_preserves_failure_after_waiter_cancellation() {
    concurrent_close_preserves_failure(Arc::new(SpscMailbox::new(
        3,
        ActorPath::for_test("/failed-close-spsc"),
    )))
    .await;
}

#[tokio::test]
async fn concurrent_ringbuf_close_preserves_failure_after_waiter_cancellation() {
    concurrent_close_preserves_failure(Arc::new(SpscRingbufMailbox::new(
        3,
        ActorPath::for_test("/failed-close-ring"),
    )))
    .await;
}

#[tokio::test]
async fn priority_and_normal_messages_share_capacity() {
    let mailbox = MpscMailbox::new(1, ActorPath::for_test("/priority"));
    mailbox
        .push_with_priority(Box::new(1_u64), BackpressureStrategy::Error, true)
        .await
        .unwrap();
    assert!(matches!(
        mailbox
            .push(Box::new(2_u64), BackpressureStrategy::Error)
            .await,
        Err(MailboxError::Full { capacity: 1 })
    ));
    assert_eq!(mailbox.len().await, 1);
    assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 1);
}

#[tokio::test]
async fn spsc_raw_sender_is_counted_and_closed() {
    let mailbox = SpscMailbox::new(1, ActorPath::for_test("/raw"));
    let sender = mailbox.sender();
    sender.send(Box::new(1_u64)).await.unwrap();
    assert_eq!(mailbox.len().await, 1);
    assert!(!mailbox.is_empty().await);
    mailbox.close().await;
    assert!(sender.send(Box::new(2_u64)).await.is_err());
    assert_eq!(mailbox.len().await, 0);
}

#[tokio::test]
async fn mpsc_raw_sender_disconnects_on_close() {
    let mailbox = MpscMailbox::new(1, ActorPath::for_test("/raw"));
    let sender = mailbox.sender();
    mailbox.close().await;
    assert!(
        sender
            .try_send(parrot::thread::mailbox::MailboxItem::Plain(Box::new(1_u64)))
            .is_err()
    );
    assert_eq!(mailbox.len().await, 0);
}

#[tokio::test]
async fn ringbuf_block_has_no_hidden_retry_deadline() {
    let mailbox = SpscRingbufMailbox::new(1, ActorPath::for_test("/slow-consumer"));
    mailbox
        .push(Box::new(1_u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    let pending = mailbox.push(Box::new(2_u64), BackpressureStrategy::Block);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(700), &mut pending)
            .await
            .is_err(),
        "Block must keep waiting until space or closure"
    );
    assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 1);
    tokio::time::timeout(Duration::from_millis(200), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 2);
    assert_eq!(mailbox.len().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_priority_producers_cannot_overbook_capacity() {
    let mailbox = Arc::new(MpscMailbox::new(1, ActorPath::for_test("/race")));
    let barrier = Arc::new(tokio::sync::Barrier::new(64));
    let mut tasks = vec![];
    for n in 0..64 {
        let mailbox = mailbox.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            mailbox
                .push_with_priority(Box::new(n), BackpressureStrategy::Error, n % 2 == 0)
                .await
        }));
    }
    let accepted = futures::future::join_all(tasks)
        .await
        .into_iter()
        .filter(|r| matches!(r, Ok(Ok(_))))
        .count();
    assert_eq!(accepted, 1);
    assert_eq!(mailbox.len().await, 1);
}

#[tokio::test]
async fn drop_oldest_uses_arrival_order_across_priority_lanes() {
    for oldest_is_high in [false, true] {
        let mailbox = MpscMailbox::new(2, ActorPath::for_test("/oldest"));
        mailbox
            .push_with_priority(Box::new(1_u64), BackpressureStrategy::Error, oldest_is_high)
            .await
            .unwrap();
        mailbox
            .push_with_priority(
                Box::new(2_u64),
                BackpressureStrategy::Error,
                !oldest_is_high,
            )
            .await
            .unwrap();
        mailbox
            .push(Box::new(3_u64), BackpressureStrategy::DropOldest)
            .await
            .unwrap();
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 2);
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 3);
        assert!(mailbox.is_empty().await);
    }
}

async fn seal_without_user_destructors(mailbox: Arc<dyn Mailbox>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountDrop(Arc<AtomicUsize>);
    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    mailbox
        .push(
            Box::new(CountDrop(dropped.clone())),
            BackpressureStrategy::Block,
        )
        .await
        .unwrap();
    let waiting = mailbox.push(Box::new(2_u64), BackpressureStrategy::Block);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    mailbox.seal();
    assert!(matches!(
        tokio::time::timeout(Duration::from_millis(200), waiting).await,
        Ok(Err(MailboxError::Closed))
    ));
    assert!(mailbox.is_closed().await);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        0,
        "seal must not execute user destructors"
    );
    mailbox.close().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(mailbox.len().await, 0);
}

#[tokio::test]
async fn mpsc_seal_rejects_producers_without_running_destructors() {
    seal_without_user_destructors(Arc::new(MpscMailbox::new(1, ActorPath::for_test("/seal"))))
        .await;
}
#[tokio::test]
async fn spsc_seal_rejects_producers_without_running_destructors() {
    seal_without_user_destructors(Arc::new(SpscMailbox::new(1, ActorPath::for_test("/seal"))))
        .await;
}
#[tokio::test]
async fn ringbuf_seal_rejects_producers_without_running_destructors() {
    seal_without_user_destructors(Arc::new(SpscRingbufMailbox::new(
        1,
        ActorPath::for_test("/seal"),
    )))
    .await;
}

struct InspectMailboxOnDrop {
    mailbox: std::sync::Weak<dyn Mailbox>,
    inspected: Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for InspectMailboxOnDrop {
    fn drop(&mut self) {
        let mailbox = self.mailbox.upgrade().unwrap();
        let (finished, completion) = std::sync::mpsc::channel();
        // A bounded helper lets the old locking bug fail rather than leave a
        // permanently deadlocked test. The operation reads real mailbox state.
        std::thread::spawn(move || {
            futures::executor::block_on(mailbox.len());
            let _ = finished.send(());
        });
        self.inspected.store(
            completion.recv_timeout(Duration::from_millis(500)).is_ok(),
            std::sync::atomic::Ordering::SeqCst,
        );
    }
}
async fn disposal_can_inspect_mailbox(mailbox: Arc<dyn Mailbox>, strategy: BackpressureStrategy) {
    let inspected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = Box::new(InspectMailboxOnDrop {
        mailbox: Arc::downgrade(&mailbox),
        inspected: inspected.clone(),
    }) as parrot_api::types::BoxedMessage;
    let (first, second) = if matches!(strategy, BackpressureStrategy::DropOldest) {
        (probe, Box::new(2_u64) as parrot_api::types::BoxedMessage)
    } else {
        (Box::new(1_u64) as parrot_api::types::BoxedMessage, probe)
    };
    mailbox
        .push(first, BackpressureStrategy::Error)
        .await
        .unwrap();
    let outcome = mailbox.push(second, strategy.clone()).await;
    let reentered = inspected.load(std::sync::atomic::Ordering::SeqCst);
    mailbox.close().await;
    assert!(
        reentered,
        "user message Drop must be able to inspect the mailbox without a bookkeeping lock deadlock"
    );
    match strategy {
        BackpressureStrategy::Error => assert!(matches!(outcome, Err(MailboxError::Full { .. }))),
        BackpressureStrategy::DropNewest => {
            assert!(matches!(outcome, Err(MailboxError::Dropped { .. })))
        }
        BackpressureStrategy::DropOldest => outcome.unwrap(),
        BackpressureStrategy::Block => unreachable!("disposal test requires a nonblocking policy"),
    }
}
#[tokio::test]
async fn mpsc_eviction_drops_messages_outside_locks() {
    disposal_can_inspect_mailbox(
        Arc::new(MpscMailbox::new(1, ActorPath::for_test("/drop"))),
        BackpressureStrategy::DropOldest,
    )
    .await;
}
#[tokio::test]
async fn spsc_eviction_drops_messages_outside_locks() {
    disposal_can_inspect_mailbox(
        Arc::new(SpscMailbox::new(1, ActorPath::for_test("/drop"))),
        BackpressureStrategy::DropOldest,
    )
    .await;
}
#[tokio::test]
async fn ringbuf_eviction_drops_messages_outside_locks() {
    disposal_can_inspect_mailbox(
        Arc::new(SpscRingbufMailbox::new(1, ActorPath::for_test("/drop"))),
        BackpressureStrategy::DropOldest,
    )
    .await;
}
#[tokio::test]
async fn spsc_drop_newest_drops_messages_outside_locks() {
    disposal_can_inspect_mailbox(
        Arc::new(SpscMailbox::new(1, ActorPath::for_test("/drop"))),
        BackpressureStrategy::DropNewest,
    )
    .await;
}
#[tokio::test]
async fn mpsc_drop_newest_drops_messages_outside_locks() {
    disposal_can_inspect_mailbox(
        Arc::new(MpscMailbox::new(1, ActorPath::for_test("/drop"))),
        BackpressureStrategy::DropNewest,
    )
    .await;
}
#[tokio::test]
async fn ringbuf_drop_newest_drops_messages_outside_locks() {
    disposal_can_inspect_mailbox(
        Arc::new(SpscRingbufMailbox::new(1, ActorPath::for_test("/drop"))),
        BackpressureStrategy::DropNewest,
    )
    .await;
}

fn close_survives_runtime_shutdown(mailbox: Arc<dyn Mailbox>) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, receive) = mpsc::channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on({
        let mailbox = mailbox.clone();
        let entered = entered.clone();
        async move {
            mailbox
                .push(
                    Box::new(BlockingDrop {
                        entered: entered.clone(),
                        release: Arc::new(Mutex::new(receive)),
                        drops: Arc::new(AtomicUsize::new(0)),
                    }),
                    BackpressureStrategy::Error,
                )
                .await
                .unwrap();
            tokio::spawn(async move { mailbox.close().await });
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .expect("close must reach the detached destructor");
        }
    });
    // Abort the business runtime while the close destructor is still blocked.
    // The mailbox's completion owner must remain independent of this runtime.
    runtime.shutdown_timeout(Duration::from_millis(1));
    release.send(()).unwrap();

    let verifier = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    verifier.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), mailbox.close())
            .await
            .expect("a later close must not wait on an aborted runtime task");
        assert_eq!(mailbox.len().await, 0);
        assert!(mailbox.is_closed().await);
    });
}

#[test]
fn mpsc_close_completion_survives_runtime_shutdown() {
    close_survives_runtime_shutdown(Arc::new(MpscMailbox::new(
        1,
        ActorPath::for_test("/runtime-shutdown-mpsc"),
    )));
}

#[test]
fn spsc_close_completion_survives_runtime_shutdown() {
    close_survives_runtime_shutdown(Arc::new(SpscMailbox::new(
        1,
        ActorPath::for_test("/runtime-shutdown-spsc"),
    )));
}

#[test]
fn ringbuf_close_completion_survives_runtime_shutdown() {
    close_survives_runtime_shutdown(Arc::new(SpscRingbufMailbox::new(
        1,
        ActorPath::for_test("/runtime-shutdown-ring"),
    )));
}
#[tokio::test]
async fn spsc_rejection_drops_messages_outside_locks() {
    disposal_can_inspect_mailbox(
        Arc::new(SpscMailbox::new(1, ActorPath::for_test("/drop"))),
        BackpressureStrategy::Error,
    )
    .await;
}
