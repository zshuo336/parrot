//! Final native mailbox ownership retires queued inputs and attachments independently.
#[path = "support/manager_ownership.rs"]
#[allow(dead_code)]
mod support;

use parrot::thread::{
    BackpressureStrategy,
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
        atomic::{AtomicUsize, Ordering},
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
            assert!(
                status.success(),
                "isolated mailbox disposal failed: {status}\n{}",
                std::fs::read_to_string(&log.0).unwrap()
            );
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
