#[path = "support/manager_ownership.rs"]
mod support;

use parrot::thread::{
    MailboxError, ProcessorInterface,
    config::BackpressureStrategy,
    envelope::AskEnvelope,
    mailbox::{
        Mailbox, MailboxItem, mpsc::MpscMailbox, spsc::SpscMailbox,
        spsc_ringbuf::SpscRingbufMailbox,
    },
    reply::ReplyReceiver,
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc as channel,
    },
    time::Duration,
};
use support::{Counts, Probe, isolated, panics};

struct Capture {
    drops: Arc<AtomicUsize>,
    action: Option<Box<dyn FnOnce() + Send + Sync>>,
}
impl Capture {
    fn touch(&self) {}
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(action) = self.action.take() {
            action();
        }
    }
}

fn callback_can_replace_itself_and_fire_the_replacement(mailbox: Arc<dyn Mailbox>) {
    let old_calls = Arc::new(AtomicUsize::new(0));
    let new_calls = Arc::new(AtomicUsize::new(0));
    let owner = Arc::downgrade(&mailbox);
    let first = old_calls.clone();
    let next = new_calls.clone();
    mailbox.set_wake_hook(Arc::new(move || {
        first.fetch_add(1, Ordering::SeqCst);
        let mailbox = owner.upgrade().unwrap();
        let calls = next.clone();
        mailbox.set_wake_hook(Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
        }));
        mailbox.fire_wake_hook();
    }));
    mailbox.fire_wake_hook();
    assert_eq!(old_calls.load(Ordering::SeqCst), 1);
    assert_eq!(new_calls.load(Ordering::SeqCst), 1);
    mailbox.fire_wake_hook();
    assert_eq!(old_calls.load(Ordering::SeqCst), 1);
    assert_eq!(new_calls.load(Ordering::SeqCst), 2);
}

fn callback_panic_preserves_payload_and_keeps_the_hook_slot_usable(mailbox: Arc<dyn Mailbox>) {
    mailbox.set_wake_hook(Arc::new(|| panic!("callback panic payload")));
    let result = catch_unwind(AssertUnwindSafe(|| mailbox.fire_wake_hook()));
    let payload = result.expect_err("callback panic must propagate");
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"callback panic payload")
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    mailbox.set_wake_hook(Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    mailbox.fire_wake_hook();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

fn replaced_hook_drop_can_observe_and_replace_the_published_hook(mailbox: Arc<dyn Mailbox>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let replacement_calls = Arc::new(AtomicUsize::new(0));
    let nested_calls = Arc::new(AtomicUsize::new(0));
    let owner = Arc::downgrade(&mailbox);
    let nested = nested_calls.clone();
    let capture = Capture {
        drops: drops.clone(),
        action: Some(Box::new(move || {
            let mailbox = owner.upgrade().unwrap();
            mailbox.fire_wake_hook();
            mailbox.set_wake_hook(Arc::new(move || {
                nested.fetch_add(1, Ordering::SeqCst);
            }));
        })),
    };
    mailbox.set_wake_hook(Arc::new(move || capture.touch()));
    let replacement = replacement_calls.clone();
    mailbox.set_wake_hook(Arc::new(move || {
        replacement.fetch_add(1, Ordering::SeqCst);
    }));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_calls.load(Ordering::SeqCst), 1);
    mailbox.fire_wake_hook();
    assert_eq!(replacement_calls.load(Ordering::SeqCst), 1);
    assert_eq!(nested_calls.load(Ordering::SeqCst), 1);
}

fn replaced_hook_drop_panic_does_not_poison_or_undo_publication(mailbox: Arc<dyn Mailbox>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: drops.clone(),
        action: Some(Box::new(|| panic!("old hook disposal"))),
    };
    mailbox.set_wake_hook(Arc::new(move || capture.touch()));
    let calls = Arc::new(AtomicUsize::new(0));
    let replacement = calls.clone();
    assert!(panics(|| {
        mailbox.set_wake_hook(Arc::new(move || {
            replacement.fetch_add(1, Ordering::SeqCst);
        }));
    }));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    mailbox.fire_wake_hook();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    mailbox.set_wake_hook(Arc::new(|| {}));
}

fn replaced_processor_drop_can_observe_and_replace_the_published_processor(
    mailbox: Arc<dyn Mailbox>,
) {
    let old_counts = Arc::new(Counts::default());
    let replacement_counts = Arc::new(Counts::default());
    let nested_counts = Arc::new(Counts::default());
    let replacement: Arc<dyn ProcessorInterface> = Probe::plain(&replacement_counts);
    let nested: Arc<dyn ProcessorInterface> = Probe::plain(&nested_counts);
    let expected = Arc::downgrade(&replacement);
    let expected_nested = Arc::downgrade(&nested);
    let owner = Arc::downgrade(&mailbox);
    mailbox.set_processor(Arc::new(Probe {
        counts: old_counts.clone(),
        on_drop: Some(Box::new(move || {
            let mailbox = owner.upgrade().unwrap();
            assert!(mailbox.has_processor());
            assert!(Arc::ptr_eq(
                &mailbox.get_processor().unwrap(),
                &expected.upgrade().unwrap()
            ));
            mailbox.set_processor(nested);
        })),
    }));
    mailbox.set_processor(replacement);
    assert_eq!(old_counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_counts.drops.load(Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(
        &mailbox.get_processor().unwrap(),
        &expected_nested.upgrade().unwrap()
    ));
    assert_eq!(nested_counts.drops.load(Ordering::SeqCst), 0);
    drop(mailbox);
    assert_eq!(nested_counts.drops.load(Ordering::SeqCst), 1);
    for counts in [&old_counts, &replacement_counts, &nested_counts] {
        assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
    }
}

fn replaced_processor_panic_keeps_slot_usable_and_external_owner_alive(mailbox: Arc<dyn Mailbox>) {
    let old_counts = Arc::new(Counts::default());
    mailbox.set_processor(Arc::new(Probe {
        counts: old_counts.clone(),
        on_drop: Some(Box::new(|| panic!("old processor disposal"))),
    }));
    let counts = Arc::new(Counts::default());
    let replacement: Arc<dyn ProcessorInterface> = Probe::plain(&counts);
    assert!(panics(|| mailbox.set_processor(replacement.clone())));
    assert_eq!(old_counts.drops.load(Ordering::SeqCst), 1);
    assert!(mailbox.has_processor());
    assert!(Arc::ptr_eq(&mailbox.get_processor().unwrap(), &replacement));
    mailbox.set_processor(Probe::plain(&old_counts));
    assert_eq!(Arc::strong_count(&replacement), 1);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    drop(replacement);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
    drop(mailbox);
    assert_eq!(old_counts.drops.load(Ordering::SeqCst), 2);
    assert_eq!(old_counts.stops.load(Ordering::SeqCst), 0);
}

fn in_flight_hook_snapshot_does_not_block_replacement(mailbox: Arc<dyn Mailbox>) {
    let (entered, wait_entered) = channel::sync_channel(1);
    let (release, wait_release) = channel::sync_channel(1);
    let wait_release = Mutex::new(wait_release);
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: drops.clone(),
        action: None,
    };
    mailbox.set_wake_hook(Arc::new(move || {
        capture.touch();
        entered.send(()).unwrap();
        wait_release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }));
    let firing = mailbox.clone();
    let fire = std::thread::spawn(move || firing.fire_wake_hook());
    let entered = wait_entered.recv_timeout(Duration::from_secs(2));
    let (published, wait_published) = channel::sync_channel(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let setting = mailbox.clone();
    let setter = std::thread::spawn(move || {
        setting.set_wake_hook(Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        setting.fire_wake_hook();
        published.send(()).unwrap();
    });
    let published = wait_published.recv_timeout(Duration::from_millis(500));
    let drops_before_release = drops.load(Ordering::SeqCst);
    let calls_before_release = calls.load(Ordering::SeqCst);
    release.send(()).unwrap();
    fire.join().unwrap();
    setter.join().unwrap();
    assert!(entered.is_ok(), "old hook must actually be running");
    assert!(
        published.is_ok(),
        "replacement must complete before old hook returns"
    );
    assert_eq!(
        drops_before_release, 0,
        "in-flight snapshot must retain its capture"
    );
    assert_eq!(calls_before_release, 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    mailbox.fire_wake_hook();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

fn push_wakes_after_commit_and_full_rejection_does_not_wake(mailbox: Arc<dyn Mailbox>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    mailbox.set_wake_hook(Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    runtime.block_on(async {
        mailbox
            .push(Box::new(1u64), BackpressureStrategy::Error)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            mailbox
                .push(Box::new(2u64), BackpressureStrategy::Error)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 1);
    });
    mailbox.set_wake_hook(Arc::new(|| panic!("wake after commit")));
    assert!(panics(|| {
        runtime
            .block_on(mailbox.push(Box::new(3u64), BackpressureStrategy::Error))
            .unwrap();
    }));
    runtime.block_on(async {
        assert_eq!(
            mailbox.len().await,
            1,
            "wake failure cannot undo committed delivery"
        );
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 3);
    });
    mailbox.set_wake_hook(Arc::new(|| {}));
    runtime.block_on(async {
        mailbox
            .push(Box::new(4u64), BackpressureStrategy::Error)
            .await
            .unwrap();
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 4);
        mailbox.close().await;
    });
}

async fn enqueue_entry(
    mailbox: &dyn Mailbox,
    entry: usize,
    value: u64,
    strategy: BackpressureStrategy,
) -> (Result<(), MailboxError>, Option<ReplyReceiver>) {
    let result = match entry {
        0 => mailbox.push(Box::new(value), strategy).await,
        2 => {
            mailbox
                .push_item(MailboxItem::Plain(Box::new(value)), strategy)
                .await
        }
        4 => mailbox
            .push_with_priority(Box::new(value), strategy, true)
            .await
            .map(|_| ()),
        1 | 3 => {
            let (envelope, reply) = AskEnvelope::new_inline(value);
            let result = if entry == 1 {
                mailbox.push_ask(envelope, strategy).await
            } else {
                mailbox
                    .push_item(MailboxItem::Ask(envelope), strategy)
                    .await
            };
            return (result, Some(reply));
        }
        _ => unreachable!(),
    };
    (result, None)
}

fn full_dropped_and_closed_rejections_never_fire_success_hooks(mailbox: Arc<dyn Mailbox>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    mailbox.set_wake_hook(Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    runtime.block_on(async {
        mailbox
            .push(Box::new(1u64), BackpressureStrategy::Error)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for entry in 0..5 {
            for strategy in [
                BackpressureStrategy::Error,
                BackpressureStrategy::DropNewest,
            ] {
                let (result, reply) =
                    enqueue_entry(mailbox.as_ref(), entry, 2, strategy.clone()).await;
                match strategy {
                    BackpressureStrategy::Error => {
                        assert!(matches!(result, Err(MailboxError::Full { capacity: 1 })))
                    }
                    BackpressureStrategy::DropNewest => {
                        assert!(matches!(result, Err(MailboxError::Dropped { capacity: 1 })))
                    }
                    _ => unreachable!(),
                }
                if let Some(reply) = reply {
                    assert!(reply.await.is_err());
                }
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "rejected entry {entry} fired a hook"
                );
                assert_eq!(mailbox.len().await, 1);
            }
        }
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 1);
        assert!(mailbox.is_empty().await);
        mailbox
            .push(Box::new(3u64), BackpressureStrategy::Error)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 3);
        mailbox.close().await;
        for entry in 0..5 {
            for strategy in [
                BackpressureStrategy::Error,
                BackpressureStrategy::DropNewest,
                BackpressureStrategy::DropOldest,
                BackpressureStrategy::Block,
            ] {
                let (result, reply) = enqueue_entry(mailbox.as_ref(), entry, 4, strategy).await;
                assert!(matches!(result, Err(MailboxError::Closed)));
                if let Some(reply) = reply {
                    assert!(reply.await.is_err());
                }
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    2,
                    "closed entry {entry} fired a hook"
                );
                assert!(mailbox.is_empty().await);
            }
        }
    });
}

fn zero_capacity_constructor_has_exactly_one_usable_slot(mailbox: Arc<dyn Mailbox>) {
    assert_eq!(mailbox.capacity(), 1);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            mailbox
                .push(Box::new(11u64), BackpressureStrategy::Error)
                .await
                .unwrap();
            assert_eq!(mailbox.len().await, 1);
            assert!(matches!(
                mailbox
                    .push(Box::new(12u64), BackpressureStrategy::Error)
                    .await,
                Err(MailboxError::Full { capacity: 1 })
            ));
            assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 11);
            mailbox
                .push(Box::new(13u64), BackpressureStrategy::Error)
                .await
                .unwrap();
            assert_eq!(*mailbox.pop().await.unwrap().downcast::<u64>().unwrap(), 13);
            assert!(mailbox.is_empty().await);
            mailbox.close().await;
        });
}

fn self_replacing_callback_and_capture_panics_cannot_abort_the_process(mailbox: Arc<dyn Mailbox>) {
    for callback_panics in [true, false] {
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            drops: drops.clone(),
            action: Some(Box::new(|| panic!("snapshot capture disposal"))),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let replacement = calls.clone();
        let owner = Arc::downgrade(&mailbox);
        mailbox.set_wake_hook(Arc::new(move || {
            capture.touch();
            let calls = replacement.clone();
            owner.upgrade().unwrap().set_wake_hook(Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
            }));
            if callback_panics {
                panic!("self replacing callback failure");
            }
        }));
        let payload = catch_unwind(AssertUnwindSafe(|| mailbox.fire_wake_hook()))
            .expect_err("callback or disposal failure must remain observable");
        let message = payload.downcast_ref::<String>().unwrap();
        assert_eq!(
            message.contains("self replacing callback failure"),
            callback_panics,
            "{message}"
        );
        assert!(message.contains("snapshot capture disposal"), "{message}");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        mailbox.fire_wake_hook();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct RecursivePayload(Arc<AtomicUsize>);
    impl Drop for RecursivePayload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("secondary hook panic payload");
        }
    }
    let payload_drops = Arc::new(AtomicUsize::new(0));
    let disposal_payload = payload_drops.clone();
    let callback_payload = payload_drops.clone();
    let capture_drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: capture_drops.clone(),
        action: Some(Box::new(move || {
            std::panic::panic_any(RecursivePayload(disposal_payload))
        })),
    };
    let owner = Arc::downgrade(&mailbox);
    mailbox.set_wake_hook(Arc::new(move || {
        capture.touch();
        owner.upgrade().unwrap().set_wake_hook(Arc::new(|| {}));
        std::panic::panic_any(RecursivePayload(callback_payload.clone()));
    }));
    let payload = catch_unwind(AssertUnwindSafe(|| mailbox.fire_wake_hook()))
        .expect_err("recursive payloads must not erase either failure");
    let message = payload.downcast_ref::<String>().unwrap();
    assert!(message.contains("mailbox wake hook failed:"), "{message}");
    assert!(message.contains("disposal failed:"), "{message}");
    assert_eq!(
        message.matches("non-string panic payload").count(),
        2,
        "{message}"
    );
    assert_eq!(
        message.matches("panic payload destructor panicked").count(),
        2,
        "{message}"
    );
    assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
    assert_eq!(payload_drops.load(Ordering::SeqCst), 2);
    mailbox.fire_wake_hook();
}

macro_rules! mailbox_contracts {
    ($module:ident, $mailbox:ty) => {
        mod $module {
            use super::*;
            fn mailbox() -> Arc<dyn Mailbox> {
                Arc::new(<$mailbox>::new(
                    1,
                    parrot_api::address::ActorPath::placeholder("/attachments"),
                ))
            }
            macro_rules! contract {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        isolated(
                            concat!(stringify!($module), "::", stringify!($name)),
                            || {
                                super::$name(mailbox());
                            },
                        );
                    }
                };
            }
            contract!(callback_can_replace_itself_and_fire_the_replacement);
            contract!(callback_panic_preserves_payload_and_keeps_the_hook_slot_usable);
            contract!(replaced_hook_drop_can_observe_and_replace_the_published_hook);
            contract!(replaced_hook_drop_panic_does_not_poison_or_undo_publication);
            contract!(replaced_processor_drop_can_observe_and_replace_the_published_processor);
            contract!(replaced_processor_panic_keeps_slot_usable_and_external_owner_alive);
            contract!(in_flight_hook_snapshot_does_not_block_replacement);
            contract!(push_wakes_after_commit_and_full_rejection_does_not_wake);
            contract!(full_dropped_and_closed_rejections_never_fire_success_hooks);
            contract!(self_replacing_callback_and_capture_panics_cannot_abort_the_process);
            #[test]
            fn zero_capacity_constructor_has_exactly_one_usable_slot() {
                isolated(
                    concat!(
                        stringify!($module),
                        "::zero_capacity_constructor_has_exactly_one_usable_slot"
                    ),
                    || {
                        super::zero_capacity_constructor_has_exactly_one_usable_slot(Arc::new(
                            <$mailbox>::new(
                                0,
                                parrot_api::address::ActorPath::placeholder("/attachments-zero"),
                            ),
                        ));
                    },
                );
            }
        }
    };
}
mailbox_contracts!(mpsc, MpscMailbox);
mailbox_contracts!(spsc, SpscMailbox);
mailbox_contracts!(ringbuf, SpscRingbufMailbox);
