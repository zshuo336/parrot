//! Receiver-side ask cancellation must retire replies that were already sent.

use parrot::thread::ReplyReceiver;
use parrot::thread::envelope::AskEnvelope;
use parrot::thread::reply::{ReplyChannel, ThreadReplyChannel};
use parrot::thread::{ThreadActorSystem, ThreadActorSystemConfig, ThreadContext};
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

struct ReplyActor {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    drops: Arc<AtomicUsize>,
    panic_on_drop: bool,
}

struct ReplyPayload {
    drops: Arc<AtomicUsize>,
    panic_on_drop: bool,
}

struct RecursivePanic;

impl Drop for RecursivePanic {
    fn drop(&mut self) {
        panic!("reply panic payload disposal");
    }
}

impl Drop for ReplyPayload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.panic_on_drop {
            std::panic::panic_any(RecursivePanic);
        }
    }
}

impl Actor for ReplyActor {
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
        if !message.is::<u64>() {
            return Box::pin(async {
                Err(ActorError::MessageHandlingError(
                    "unexpected message".into(),
                ))
            });
        }
        let value = *message
            .downcast_ref::<u64>()
            .expect("message type checked before handler");
        let entered = self.entered.clone();
        let release = self.release.clone();
        let drops = self.drops.clone();
        let panic_on_drop = self.panic_on_drop;
        Box::pin(async move {
            drop(message);
            entered.notify_one();
            release.notified().await;
            if value == 7 {
                Ok(Box::new(ReplyPayload {
                    drops,
                    panic_on_drop,
                }) as BoxedMessage)
            } else {
                Ok(Box::new(()) as BoxedMessage)
            }
        })
    }
}

async fn cancel_after_publication(panic_on_drop: bool) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        default_ask_timeout: Duration::from_secs(2),
        shutdown_timeout: Duration::from_secs(2),
        ..Default::default()
    });
    let actor = system
        .spawn_at(
            ReplyActor {
                entered: entered.clone(),
                release: release.clone(),
                drops: drops.clone(),
                panic_on_drop,
            },
            "/reply-cancellation",
            None,
            Default::default(),
        )
        .await
        .expect("actor starts");

    let mut request =
        Box::pin(actor.send_with_timeout(Box::new(7_u64), Some(Duration::from_secs(2))));
    assert!(futures::poll!(&mut request).is_pending());
    entered.notified().await;
    release.notify_one();

    // A second request is a publication barrier: its handler cannot begin
    // until the first result has been published to the first oneshot.
    let mut barrier =
        Box::pin(actor.send_with_timeout(Box::new(8_u64), Some(Duration::from_secs(2))));
    assert!(futures::poll!(&mut barrier).is_pending());
    entered.notified().await;
    release.notify_one();
    barrier.await.expect("publication barrier reply");

    // The first result is now in its channel while its receiver remains
    // unpolled. Dropping the ask must retire that result safely.
    let cancellation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(request)));
    assert!(
        cancellation.is_ok(),
        "receiver-side cancellation leaked a reply destructor panic"
    );

    tokio::time::timeout(Duration::from_secs(2), async {
        while drops.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("published reply was retired");
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    system.shutdown_internal().await.expect("system shuts down");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_retires_successful_reply() {
    cancel_after_publication(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_contains_recursive_reply_destructor_panic() {
    cancel_after_publication(true).await;
}

#[tokio::test]
async fn public_ask_envelope_constructor_uses_cancellation_safe_receiver() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (envelope, receiver) = AskEnvelope::new(Box::new(()));
    let mut receiver = Box::pin(receiver);
    assert!(futures::poll!(&mut receiver).is_pending());
    envelope.reply_sync(Ok(Box::new(ReplyPayload {
        drops: drops.clone(),
        panic_on_drop: true,
    }) as BoxedMessage));
    let cancellation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(receiver)));
    assert!(cancellation.is_ok());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn single_alloc_ask_uses_the_same_cancellation_safe_receiver() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (mut envelope, receiver) = parrot::thread::single_alloc::SingleAllocEnvelope::new_ask(());
    let sender = envelope.take_reply().expect("ask sender is present");
    let mut receiver = Box::pin(receiver);
    assert!(futures::poll!(&mut receiver).is_pending());
    sender
        .send(Ok(Box::new(ReplyPayload {
            drops: drops.clone(),
            panic_on_drop: true,
        }) as BoxedMessage))
        .expect("receiver remains open");
    let cancellation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(receiver)));
    assert!(cancellation.is_ok());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(envelope);
}

#[test]
fn concurrent_publication_and_cancellation_retire_the_reply_once() {
    for _ in 0..2048 {
        let drops = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let receiver = ReplyReceiver::new(receiver);
        let sending = {
            let gate = gate.clone();
            let drops = drops.clone();
            std::thread::spawn(move || {
                gate.wait();
                // A lost race leaves ownership with the sending helper.
                let _ = futures::executor::block_on(
                    Box::new(ThreadReplyChannel(sender)).send_reply(Ok(Box::new(ReplyPayload {
                        drops,
                        panic_on_drop: true,
                    }))),
                );
            })
        };
        gate.wait();
        let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(receiver)));
        sending
            .join()
            .expect("sender isolates failed delivery cleanup");
        assert!(
            cancelled.is_ok(),
            "cancelled receiver must isolate raced reply cleanup"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn blocking_receive_transfers_reply_ownership() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let receiver = ReplyReceiver::new(receiver);
    sender
        .send(Ok(Box::new(ReplyPayload {
            drops: drops.clone(),
            panic_on_drop: false,
        })))
        .unwrap();
    let value = receiver.blocking_recv().unwrap().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_blocking_receive_still_protects_published_reply() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let receiver = ReplyReceiver::new(receiver);
    sender
        .send(Ok(Box::new(ReplyPayload {
            drops: drops.clone(),
            panic_on_drop: true,
        })))
        .unwrap();
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = receiver.blocking_recv();
    }));
    let panic = rejected.expect_err("Tokio rejects blocking receive in an async context");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("Cannot block the current thread"),
        "unexpected panic: {message}"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn blocking_receive_is_allowed_on_tokio_blocking_workers() {
    tokio::task::spawn_blocking(|| {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sender.send(Ok(Box::new(81_u64) as BoxedMessage)).unwrap();
        let value = ReplyReceiver::new(receiver)
            .blocking_recv()
            .unwrap()
            .unwrap();
        assert_eq!(*value.downcast::<u64>().unwrap(), 81);
    })
    .await
    .expect("blocking worker succeeds");
}

#[test]
fn blocking_receive_is_allowed_inside_a_non_tokio_executor() {
    futures::executor::block_on(async {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sender.send(Ok(Box::new(82_u64) as BoxedMessage)).unwrap();
        let value = ReplyReceiver::new(receiver)
            .blocking_recv()
            .unwrap()
            .unwrap();
        assert_eq!(*value.downcast::<u64>().unwrap(), 82);
    });
}

#[test]
fn blocking_receive_waits_for_and_wakes_on_a_late_reply() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let (entered, started) = std::sync::mpsc::channel();
    let (finished, result) = std::sync::mpsc::channel();
    let waiting = std::thread::spawn(move || {
        entered.send(()).unwrap();
        let reply = ReplyReceiver::new(receiver)
            .blocking_recv()
            .unwrap()
            .unwrap();
        finished.send(*reply.downcast::<u64>().unwrap()).unwrap();
    });
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(10)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    sender.send(Ok(Box::new(83_u64) as BoxedMessage)).unwrap();
    assert_eq!(result.recv_timeout(Duration::from_secs(2)).unwrap(), 83);
    waiting
        .join()
        .expect("blocking receiver woke and completed");
}

#[tokio::test]
async fn receiver_state_matches_consumed_and_closed_oneshot_states() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut receiver = ReplyReceiver::new(receiver);
    assert!(!receiver.is_terminated());
    assert!(receiver.is_empty());
    sender.send(Ok(Box::new(93_u64) as BoxedMessage)).unwrap();
    assert!(!receiver.is_empty());
    let reply = (&mut receiver).await.unwrap().unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 93);
    assert!(receiver.is_terminated());
    assert!(receiver.is_empty());
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));

    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut receiver = ReplyReceiver::new(receiver);
    receiver.close();
    assert!(sender.is_closed());
    assert!(receiver.is_empty());
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    assert!(receiver.is_terminated());
}

#[test]
fn try_receive_transfers_the_original_reply_without_retiring_it() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut receiver = ReplyReceiver::new(receiver);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(!receiver.is_terminated());
    let payload = Box::new(ReplyPayload {
        drops: drops.clone(),
        panic_on_drop: false,
    });
    let original = std::ptr::from_ref(payload.as_ref());
    sender.send(Ok(payload as BoxedMessage)).unwrap();
    let value = receiver.try_recv().unwrap().unwrap();
    let value = value.downcast::<ReplyPayload>().unwrap();
    assert_eq!(std::ptr::from_ref(value.as_ref()), original);
    assert!(receiver.is_terminated());
    assert!(receiver.is_empty());
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    drop(receiver);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn close_preserves_a_published_reply_for_receive_or_safe_cancellation() {
    for cancel in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut receiver = ReplyReceiver::new(receiver);
        sender
            .send(Ok(Box::new(ReplyPayload {
                drops: drops.clone(),
                panic_on_drop: cancel,
            }) as BoxedMessage))
            .unwrap();
        receiver.close();
        receiver.close();
        assert!(!receiver.is_empty());
        assert!(!receiver.is_terminated());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        if cancel {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(receiver)));
            assert!(
                result.is_ok(),
                "close must retain safe cancellation ownership"
            );
        } else {
            let value = (&mut receiver).await.unwrap().unwrap();
            assert!(receiver.is_terminated());
            assert!(receiver.is_empty());
            drop(receiver);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(value);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn cancellation_contains_sender_close_notification_panic() {
    struct PanicWake;
    impl std::task::Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            std::panic::panic_any(RecursivePanic);
        }
    }
    let (mut sender, receiver) = tokio::sync::oneshot::channel();
    let receiver = ReplyReceiver::new(receiver);
    let waker = std::task::Waker::from(Arc::new(PanicWake));
    assert!(
        sender
            .poll_closed(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(receiver)));
    assert!(
        cancelled.is_ok(),
        "sender close notification must not unwind cancellation"
    );
    assert!(sender.is_closed());
}
