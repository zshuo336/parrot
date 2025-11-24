//! Final native mailbox ownership retires queued inputs and attachments independently.
#[path = "support/manager_ownership.rs"]
#[allow(dead_code)]
mod support;

use parrot::thread::{
    BackpressureStrategy, MailboxError,
    envelope::AskEnvelope,
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
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct RecursivePayload(Arc<AtomicUsize>);
impl Drop for RecursivePayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("secondary payload disposal");
    }
}

struct DiagnosticSubscriber {
    hostile: Arc<AtomicBool>,
    events: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl tracing::Subscriber for DiagnosticSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target().starts_with("parrot::thread::mailbox")
            && *metadata.level() == tracing::Level::ERROR
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        if self.hostile.load(Ordering::SeqCst) {
            std::panic::panic_any(RecursivePayload(self.payloads.clone()));
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

struct Input {
    drops: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl Input {
    fn touch(&self) {}
}
impl Drop for Input {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(RecursivePayload(self.payloads.clone()));
    }
}

fn mailbox(kind: usize) -> Arc<dyn Mailbox> {
    let path = ActorPath::for_test("/final-owner");
    match kind {
        0 => Arc::new(MpscMailbox::new(4, path)),
        1 => Arc::new(SpscMailbox::new(4, path)),
        _ => Arc::new(SpscRingbufMailbox::new(4, path)),
    }
}

fn input(drops: &Arc<AtomicUsize>, payloads: &Arc<AtomicUsize>) -> BoxedMessage {
    Box::new(Input {
        drops: drops.clone(),
        payloads: payloads.clone(),
    })
}

fn retire_caught_panic(payload: Box<dyn std::any::Any + Send>) -> String {
    let mut text = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
        })
        .unwrap_or_else(|| "non-string panic payload".to_owned());
    // Failure diagnostics must not drop a hostile payload while asserting.
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        text.push_str("; payload disposal failed");
        std::mem::forget(secondary);
    }
    text
}

fn fill(mailbox: &dyn Mailbox, drops: &Arc<AtomicUsize>, payloads: &Arc<AtomicUsize>) {
    // Leave no runtime alive at final release: a detached task alone cannot
    // establish actual disposal of these values.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            mailbox
                .push(input(drops, payloads), BackpressureStrategy::Error)
                .await
                .unwrap();
            mailbox
                .push_ask(
                    AskEnvelope::with_boxed(input(drops, payloads)).0,
                    BackpressureStrategy::Error,
                )
                .await
                .unwrap();
            mailbox
                .push_with_priority(input(drops, payloads), BackpressureStrategy::Error, true)
                .await
                .unwrap();
            mailbox
                .push_item(
                    MailboxItem::Ask(AskEnvelope::with_boxed(input(drops, payloads)).0),
                    BackpressureStrategy::Error,
                )
                .await
                .unwrap();
            assert_eq!(mailbox.len().await, 4);
        });
    assert_eq!(drops.load(Ordering::SeqCst), 0);
}

fn exercise(kind: usize, unwinding: bool, attachments: bool) {
    let owner = mailbox(kind);
    let weak = Arc::downgrade(&owner);
    let drops = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    fill(owner.as_ref(), &drops, &payloads);
    let processor = Arc::new(support::Counts::default());
    if attachments {
        let payload = payloads.clone();
        owner.set_processor(Arc::new(support::Probe {
            counts: processor.clone(),
            on_drop: Some(Box::new(move || {
                std::panic::panic_any(RecursivePayload(payload.clone()))
            })),
        }));
        let capture = Input {
            drops: drops.clone(),
            payloads: payloads.clone(),
        };
        owner.set_wake_hook(Arc::new(move || capture.touch()));
    }
    let result = catch_unwind(AssertUnwindSafe(move || {
        let _owner = owner;
        if unwinding {
            panic!("original caller failure");
        }
    }));
    let escaped = result.err().map(retire_caught_panic);
    if unwinding {
        assert_eq!(escaped.as_deref(), Some("original caller failure"));
    } else {
        assert!(
            escaped.is_none(),
            "native mailbox disposal escaped: {escaped:?}"
        );
    }
    assert!(
        weak.upgrade().is_none(),
        "no final mailbox owner may remain"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 4 + usize::from(attachments));
    assert_eq!(
        payloads.load(Ordering::SeqCst),
        4 + 2 * usize::from(attachments)
    );
    assert_eq!(
        processor.drops.load(Ordering::SeqCst),
        usize::from(attachments)
    );
    assert_eq!(
        processor.stops.load(Ordering::SeqCst),
        0,
        "Drop is not an actor stop hook"
    );
}

fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_MAILBOX_DROP_CHILD").as_deref() == Ok(name) {
        body();
        println!("MAILBOX_OWNER_DROP_OK {name}");
        return;
    }
    struct Log(std::path::PathBuf);
    impl Drop for Log {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
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
    let log =
        Log(std::env::temp_dir().join(format!("parrot-mailbox-drop-{}.log", uuid::Uuid::new_v4())));
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log.0)
        .unwrap();
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_MAILBOX_DROP_CHILD", name)
            .env("RUST_BACKTRACE", "0")
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let output = std::fs::read_to_string(&log.0).unwrap();
            assert!(
                status.success(),
                "isolated mailbox disposal failed: {status}\n{output}"
            );
            assert_eq!(
                output
                    .lines()
                    .filter(|line| *line == format!("MAILBOX_OWNER_DROP_OK {name}"))
                    .count(),
                1,
                "child must execute the actual contract exactly once"
            );
            assert!(output.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
            if name.contains("_diagnostic_") {
                for marker in [
                    "MAILBOX_DIAGNOSTIC_CONTROL_OK",
                    "MAILBOX_DIAGNOSTIC_HOSTILE_OK",
                ] {
                    assert_eq!(output.lines().filter(|line| *line == marker).count(), 1);
                }
            }
            return;
        }
        assert!(
            Instant::now() < deadline,
            "isolated mailbox disposal timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

macro_rules! contract {
    ($name:ident, $kind:expr, $unwinding:expr, $attachments:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                exercise($kind, $unwinding, $attachments)
            });
        }
    };
}
contract!(
    mpsc_final_owner_retires_queued_messages_without_close,
    0,
    false,
    false
);
contract!(
    spsc_final_owner_retires_queued_messages_without_close,
    1,
    false,
    false
);
contract!(
    ring_final_owner_retires_queued_messages_without_close,
    2,
    false,
    false
);
contract!(
    mpsc_final_owner_preserves_the_callers_unwind,
    0,
    true,
    false
);
contract!(
    spsc_final_owner_preserves_the_callers_unwind,
    1,
    true,
    false
);
contract!(
    ring_final_owner_preserves_the_callers_unwind,
    2,
    true,
    false
);
contract!(
    mpsc_final_owner_retires_all_attachments_and_messages,
    0,
    false,
    true
);
contract!(
    spsc_final_owner_retires_all_attachments_and_messages,
    1,
    false,
    true
);
contract!(
    ring_final_owner_retires_all_attachments_and_messages,
    2,
    false,
    true
);

#[test]
fn mpsc_final_owner_disconnects_a_retained_raw_sender_without_retaining_messages() {
    isolated(
        "mpsc_final_owner_disconnects_a_retained_raw_sender_without_retaining_messages",
        || {
            let mailbox = MpscMailbox::new(4, ActorPath::for_test("/retained-sender"));
            let sender = mailbox.sender();
            let drops = Arc::new(AtomicUsize::new(0));
            struct Counted(Arc<AtomicUsize>);
            impl Drop for Counted {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    for priority in [false, false, true, false] {
                        mailbox
                            .push_with_priority(
                                Box::new(Counted(drops.clone())),
                                BackpressureStrategy::Error,
                                priority,
                            )
                            .await
                            .unwrap();
                    }
                    assert_eq!(mailbox.len().await, 4);
                });
            let escaped = catch_unwind(AssertUnwindSafe(|| drop(mailbox)))
                .err()
                .map(retire_caught_panic);
            assert!(escaped.is_none(), "mailbox disposal escaped: {escaped:?}");
            assert_eq!(drops.load(Ordering::SeqCst), 4);
            assert!(sender.is_disconnected());
            assert!(
                sender
                    .try_send(MailboxItem::Plain(Box::new(17u64)))
                    .is_err()
            );
            drop(sender);
            assert_eq!(drops.load(Ordering::SeqCst), 4);
        },
    );
}

fn reject_inputs(kind: usize) {
    let drops = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let owner = mailbox(kind);
            for value in 11_u64..15 {
                owner
                    .push(Box::new(value), BackpressureStrategy::Error)
                    .await
                    .unwrap();
            }
            for strategy in [
                BackpressureStrategy::Error,
                BackpressureStrategy::DropNewest,
            ] {
                let result = owner.push(input(&drops, &payloads), strategy.clone()).await;
                assert!(
                    match strategy {
                        BackpressureStrategy::Error =>
                            matches!(result, Err(MailboxError::Full { capacity: 4 })),
                        BackpressureStrategy::DropNewest =>
                            matches!(result, Err(MailboxError::Dropped { capacity: 4 })),
                        _ => unreachable!(),
                    },
                    "rejection category changed: {result:?}"
                );
            }
            owner.seal();
            let result = owner
                .push(input(&drops, &payloads), BackpressureStrategy::Block)
                .await;
            assert!(matches!(result, Err(MailboxError::Closed)), "{result:?}");
            assert_eq!(drops.load(Ordering::SeqCst), 3);
            assert_eq!(payloads.load(Ordering::SeqCst), 3);
            for value in 11_u64..15 {
                assert_eq!(
                    *owner.pop().await.unwrap().downcast::<u64>().unwrap(),
                    value
                );
            }
            assert!(owner.pop().await.is_none());

            // Successful replacement reports the actual eviction failure and
            // leaves the new message admitted, independently of diagnostics.
            let owner = mailbox(kind);
            owner
                .push(input(&drops, &payloads), BackpressureStrategy::Error)
                .await
                .unwrap();
            for value in 21_u64..24 {
                owner
                    .push(Box::new(value), BackpressureStrategy::Error)
                    .await
                    .unwrap();
            }
            let result = owner
                .push(Box::new(24_u64), BackpressureStrategy::DropOldest)
                .await;
            assert!(
                matches!(result, Err(MailboxError::PushError(ref error))
                if error.contains("replacement admitted; evicted mailbox input disposal failed")
                && error.contains("panic payload destructor panicked")),
                "{result:?}"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 4);
            assert_eq!(payloads.load(Ordering::SeqCst), 4);
            assert_eq!(owner.len().await, 4);
            for value in 21_u64..25 {
                assert_eq!(
                    *owner.pop().await.unwrap().downcast::<u64>().unwrap(),
                    value
                );
            }
        });
}

fn diagnostic_contract(kind: usize, unwind: bool, rejection: bool) {
    // Suppress nested-panic backtraces only in this owned subprocess.
    std::panic::set_hook(Box::new(|_| {}));
    let hostile = Arc::new(AtomicBool::new(false));
    let events = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(DiagnosticSubscriber {
        hostile: hostile.clone(),
        events: events.clone(),
        payloads: payloads.clone(),
    })
    .unwrap();
    for panics in [false, true] {
        hostile.store(panics, Ordering::SeqCst);
        events.store(0, Ordering::SeqCst);
        payloads.store(0, Ordering::SeqCst);
        if rejection {
            reject_inputs(kind);
        } else {
            exercise(kind, unwind, true);
        }
        let expected = if rejection { 3 } else { 6 };
        assert_eq!(events.load(Ordering::SeqCst), expected);
        assert_eq!(
            payloads.load(Ordering::SeqCst),
            expected * usize::from(panics)
        );
        if !panics {
            println!("MAILBOX_DIAGNOSTIC_CONTROL_OK");
        }
    }
    println!("MAILBOX_DIAGNOSTIC_HOSTILE_OK");
}

macro_rules! diagnostic_case {
    ($name:ident, $kind:expr, $unwind:expr, $rejection:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                diagnostic_contract($kind, $unwind, $rejection)
            });
        }
    };
}
diagnostic_case!(mpsc_diagnostic_final_owner, 0, false, false);
diagnostic_case!(spsc_diagnostic_final_owner, 1, false, false);
diagnostic_case!(ring_diagnostic_final_owner, 2, false, false);
diagnostic_case!(mpsc_diagnostic_original_unwind, 0, true, false);
diagnostic_case!(spsc_diagnostic_original_unwind, 1, true, false);
diagnostic_case!(ring_diagnostic_original_unwind, 2, true, false);
diagnostic_case!(mpsc_diagnostic_rejected_input, 0, false, true);
diagnostic_case!(spsc_diagnostic_rejected_input, 1, false, true);
diagnostic_case!(ring_diagnostic_rejected_input, 2, false, true);
