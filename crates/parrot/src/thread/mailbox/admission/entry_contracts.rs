use super::*;
use crate::thread::reply::ReplyReceiver;
use parrot_api::types::ActorResult;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug)]
enum Entry {
    Plain,
    ItemPlain,
    Ask,
    ItemAsk,
    LegacyAsk,
    Priority,
    PriorityAsk,
    InlineAsk,
    ItemInlineAsk,
    LegacyInlineAsk,
}
const ENTRIES: [Entry; 10] = [
    Entry::Plain,
    Entry::ItemPlain,
    Entry::Ask,
    Entry::ItemAsk,
    Entry::LegacyAsk,
    Entry::Priority,
    Entry::PriorityAsk,
    Entry::InlineAsk,
    Entry::ItemInlineAsk,
    Entry::LegacyInlineAsk,
];

fn mailbox(kind: usize) -> Arc<dyn Mailbox> {
    let path = ActorPath::for_test("/admission-entries");
    match kind {
        0 => Arc::new(super::super::mpsc::MpscMailbox::new(1, path)),
        1 => Arc::new(super::super::spsc::SpscMailbox::new(1, path)),
        2 => Arc::new(super::super::spsc_ringbuf::SpscRingbufMailbox::new(1, path)),
        _ => unreachable!(),
    }
}

async fn enqueue(
    mailbox: &dyn Mailbox,
    entry: Entry,
    message: BoxedMessage,
) -> (Result<(), MailboxError>, Option<ReplyReceiver>) {
    let strategy = BackpressureStrategy::Error;
    let result = match entry {
        Entry::Plain => mailbox.push(message, strategy).await,
        Entry::ItemPlain => {
            mailbox
                .push_item(MailboxItem::Plain(message), strategy)
                .await
        }
        Entry::Priority => mailbox
            .push_with_priority(message, strategy, true)
            .await
            .map(|high| assert_eq!(high, mailbox.supports_high_priority())),
        _ => {
            let (envelope, reply) = if matches!(
                entry,
                Entry::InlineAsk | Entry::ItemInlineAsk | Entry::LegacyInlineAsk
            ) {
                AskEnvelope::new_inline(*message.downcast::<u64>().unwrap())
            } else {
                AskEnvelope::with_boxed(message)
            };
            let result = match entry {
                Entry::Ask | Entry::InlineAsk => mailbox.push_ask(envelope, strategy).await,
                Entry::ItemAsk | Entry::ItemInlineAsk => {
                    mailbox
                        .push_item(MailboxItem::Ask(envelope), strategy)
                        .await
                }
                Entry::LegacyAsk | Entry::LegacyInlineAsk => {
                    mailbox.push(Box::new(envelope), strategy).await
                }
                Entry::PriorityAsk => mailbox
                    .push_with_priority(Box::new(envelope), strategy, true)
                    .await
                    .map(|high| assert_eq!(high, mailbox.supports_high_priority())),
                _ => unreachable!(),
            };
            return (result, Some(reply));
        }
    };
    (result, None)
}

#[derive(Debug, Default)]
struct Counts {
    calls: AtomicUsize,
    held: AtomicUsize,
    retired: AtomicUsize,
}
#[derive(Debug)]
struct Prepared {
    value: u64,
    counts: Arc<Counts>,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        assert_eq!(self.counts.held.fetch_sub(1, Ordering::SeqCst), 1);
        self.counts.retired.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Debug)]
struct Gate {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
#[derive(Debug)]
struct Policy {
    counts: Arc<Counts>,
    gate: Mutex<Option<Gate>>,
}
impl MessageAdmission for Policy {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
        self.counts.calls.fetch_add(1, Ordering::SeqCst);
        let value = *message
            .downcast::<u64>()
            .expect("only original business input reaches prepare");
        self.counts
            .held
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| MailboxError::Full { capacity: 1 })?;
        let prepared = Prepared {
            value,
            counts: self.counts.clone(),
        };
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.send(()).unwrap();
            gate.release
                .recv_timeout(Duration::from_secs(10))
                .expect("prepare gate not released");
        }
        Ok(Box::new(prepared))
    }
}

type UnpackedItem = (
    BoxedMessage,
    Option<oneshot::Sender<ActorResult<BoxedMessage>>>,
);
fn take_item(item: MailboxItem) -> UnpackedItem {
    match item {
        MailboxItem::Plain(message) => match message.downcast::<AskEnvelope>() {
            Ok(envelope) => take_item(MailboxItem::Ask(*envelope)),
            Err(message) => (message, None),
        },
        MailboxItem::Ask(mut envelope) => {
            let reply = envelope.take_reply();
            (envelope.payload.into_boxed(), reply)
        }
    }
}

#[tokio::test]
async fn every_enqueue_entry_prepares_once_and_retains_the_original_reply() {
    for kind in 0..3 {
        for entry in ENTRIES {
            let counts = Arc::new(Counts::default());
            let policy = Arc::new(Policy {
                counts: counts.clone(),
                gate: Mutex::new(None),
            });
            let mailbox = AdmissionMailbox::new(mailbox(kind), policy);
            let (result, reply) = enqueue(&mailbox, entry, Box::new(73u64)).await;
            result.unwrap_or_else(|error| panic!("{kind}/{entry:?}: {error}"));
            assert_eq!(counts.calls.load(Ordering::SeqCst), 1, "{kind}/{entry:?}");
            assert_eq!(counts.held.load(Ordering::SeqCst), 1);
            assert_eq!(mailbox.len().await, 1);
            let (prepared, sender) = take_item(mailbox.pop_item().await.unwrap());
            assert_eq!(prepared.downcast_ref::<Prepared>().unwrap().value, 73);
            assert_eq!(
                counts.held.load(Ordering::SeqCst),
                1,
                "dequeue is not retirement"
            );
            assert_eq!(sender.is_some(), reply.is_some());
            if let Some(sender) = sender {
                sender.send(Ok(Box::new(91u64))).unwrap();
            }
            if let Some(reply) = reply {
                assert_eq!(
                    *reply.await.unwrap().unwrap().downcast::<u64>().unwrap(),
                    91
                );
            }
            drop(prepared);
            assert_eq!(counts.held.load(Ordering::SeqCst), 0);
            assert_eq!(counts.retired.load(Ordering::SeqCst), 1);
            mailbox.close().await;
        }
    }
}

#[tokio::test]
async fn controls_watch_and_unwatch_skip_prepare_at_every_boxed_entry() {
    for kind in 0..3 {
        let counts = Arc::new(Counts::default());
        let mailbox = AdmissionMailbox::new(
            mailbox(kind),
            Arc::new(Policy {
                counts: counts.clone(),
                gate: Mutex::new(None),
            }),
        );
        for entry in ENTRIES.into_iter().take(7) {
            for control in 0..3 {
                let message: BoxedMessage = match control {
                    0 => Box::new(ControlMessage::HealthCheck),
                    1 => Box::new(crate::thread::system::WatchRequest {
                        watcher_path: "/watcher".into(),
                    }),
                    2 => Box::new(crate::thread::system::UnwatchRequest {
                        watcher_path: "/watcher".into(),
                    }),
                    _ => unreachable!(),
                };
                let (result, reply) = enqueue(&mailbox, entry, message).await;
                result.unwrap_or_else(|error| panic!("{kind}/{entry:?}/{control}: {error}"));
                let (message, sender) = take_item(mailbox.pop_item().await.unwrap());
                match control {
                    0 => assert!(message.is::<ControlMessage>()),
                    1 => assert_eq!(
                        message
                            .downcast_ref::<crate::thread::system::WatchRequest>()
                            .unwrap()
                            .watcher_path,
                        "/watcher"
                    ),
                    2 => assert_eq!(
                        message
                            .downcast_ref::<crate::thread::system::UnwatchRequest>()
                            .unwrap()
                            .watcher_path,
                        "/watcher"
                    ),
                    _ => unreachable!(),
                }
                assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
                drop((message, sender));
                if let Some(reply) = reply {
                    assert!(reply.await.is_err());
                }
            }
        }
        mailbox.close().await;
    }
}

#[tokio::test]
async fn close_during_prepare_rejects_the_transformed_value_and_restores_its_permit() {
    for kind in 0..3 {
        for entry in ENTRIES {
            let counts = Arc::new(Counts::default());
            let (entered, wait_entered) = mpsc::sync_channel(1);
            let (release, wait_release) = mpsc::sync_channel(1);
            let policy = Arc::new(Policy {
                counts: counts.clone(),
                gate: Mutex::new(Some(Gate {
                    entered,
                    release: wait_release,
                })),
            });
            let wrapped = Arc::new(AdmissionMailbox::new(mailbox(kind), policy.clone()));
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            wrapped.set_wake_hook(Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }));
            let producer = wrapped.clone();
            let worker = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(enqueue(producer.as_ref(), entry, Box::new(73u64)))
            });
            let entered = wait_entered.recv_timeout(Duration::from_secs(5));
            let held_before_close = counts.held.load(Ordering::SeqCst);
            let closed = tokio::time::timeout(Duration::from_secs(3), wrapped.close()).await;
            // Always release and join the owned producer before reporting failure.
            let released = release.send(());
            let outcome = worker.join();
            assert!(
                entered.is_ok(),
                "{kind}/{entry:?}: prepare did not reach its gate"
            );
            assert_eq!(held_before_close, 1);
            assert!(closed.is_ok(), "close waited on user prepare");
            released.unwrap();
            let (result, reply) = outcome.unwrap();
            assert!(
                matches!(result, Err(MailboxError::Closed)),
                "{kind}/{entry:?}: {result:?}"
            );
            if let Some(reply) = reply {
                assert!(reply.await.is_err());
            }
            assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
            assert_eq!(counts.held.load(Ordering::SeqCst), 0);
            assert_eq!(counts.retired.load(Ordering::SeqCst), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(wrapped.is_empty().await);
            let (result, reply) = enqueue(wrapped.as_ref(), entry, Box::new(74u64)).await;
            assert!(matches!(result, Err(MailboxError::Closed)));
            if let Some(reply) = reply {
                assert!(reply.await.is_err());
            }
            assert_eq!(
                counts.calls.load(Ordering::SeqCst),
                1,
                "closed entry invoked prepare"
            );
            let replacement = AdmissionMailbox::new(mailbox(kind), policy);
            let (result, reply) = enqueue(&replacement, entry, Box::new(75u64)).await;
            result.unwrap();
            let (prepared, sender) = take_item(replacement.pop_item().await.unwrap());
            assert_eq!(prepared.downcast_ref::<Prepared>().unwrap().value, 75);
            drop((prepared, sender));
            if let Some(reply) = reply {
                assert!(reply.await.is_err());
            }
            assert_eq!(counts.calls.load(Ordering::SeqCst), 2);
            assert_eq!(counts.held.load(Ordering::SeqCst), 0);
            assert_eq!(counts.retired.load(Ordering::SeqCst), 2);
            replacement.close().await;
        }
    }
}
