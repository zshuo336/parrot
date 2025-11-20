//! Message disposal and reply receiver notification are independent user actions.
use futures::{Future, executor::block_on};
use parrot::thread::{
    BackpressureStrategy,
    envelope::AskEnvelope,
    mailbox::{Mailbox, mpsc::MpscMailbox, spsc::SpscMailbox, spsc_ringbuf::SpscRingbufMailbox},
};
use parrot_api::address::ActorPath;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Wake, Waker},
    time::{Duration, Instant},
};

struct Value(Arc<AtomicUsize>, bool);
impl Drop for Value {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            panic!("message destructor");
        }
    }
}
struct ReceiverWake(Arc<AtomicUsize>);
impl Wake for ReceiverWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("reply receiver notification");
    }
}

fn exercise(kind: usize, close: bool) {
    let drops = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let (envelope, mut receiver) = AskEnvelope::with_boxed(Box::new(Value(drops.clone(), true)));
    let waker = Waker::from(Arc::new(ReceiverWake(wakes.clone())));
    assert!(
        Pin::new(&mut receiver)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let path = ActorPath::for_test("/ask-disposal");
    let mailbox: Arc<dyn Mailbox> = match kind {
        0 => Arc::new(MpscMailbox::new(2, path)),
        1 => Arc::new(SpscMailbox::new(2, path)),
        _ => Arc::new(SpscRingbufMailbox::new(2, path)),
    };
    block_on(async {
        mailbox
            .push_ask(envelope, BackpressureStrategy::Error)
            .await
            .unwrap();
        mailbox
            .push(
                Box::new(Value(drops.clone(), false)),
                BackpressureStrategy::Error,
            )
            .await
            .unwrap();
    });
    let result = catch_unwind(AssertUnwindSafe(|| {
        if close {
            block_on(mailbox.close());
        }
    }));
    if close {
        assert!(
            result.is_err(),
            "explicit close must report the payload cleanup failure"
        );
    } else {
        assert!(result.is_ok());
    }
    assert!(
        catch_unwind(AssertUnwindSafe(|| drop(mailbox))).is_ok(),
        "final mailbox disposal must not unwind"
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "all queued values must actually be disposed"
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(
        Pin::new(&mut receiver)
            .poll(&mut Context::from_waker(&waker))
            .is_ready()
    );
}

fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_ASK_DISPOSAL_CHILD").as_deref() == Ok(name) {
        body();
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
            .env("PARROT_ASK_DISPOSAL_CHILD", name)
            .env("RUST_BACKTRACE", "0")
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let output = std::fs::read_to_string(log.path()).unwrap();
            assert!(
                status.success(),
                "isolated ASK disposal failed: {status}\n{output}"
            );
            assert!(
                output.contains("1 passed; 0 failed; 0 ignored;"),
                "child must run exactly one case: {output}"
            );
            return;
        }
        assert!(Instant::now() < deadline, "isolated ASK disposal timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

macro_rules! mailbox_contract {
    ($name:ident, $kind:expr, $close:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || exercise($kind, $close));
        }
    };
}
mailbox_contract!(
    mpsc_drop_isolates_reply_notification_from_payload_disposal,
    0,
    false
);
mailbox_contract!(
    spsc_drop_isolates_reply_notification_from_payload_disposal,
    1,
    false
);
mailbox_contract!(
    ring_drop_isolates_reply_notification_from_payload_disposal,
    2,
    false
);
mailbox_contract!(
    mpsc_close_isolates_reply_notification_from_payload_disposal,
    0,
    true
);
mailbox_contract!(
    spsc_close_isolates_reply_notification_from_payload_disposal,
    1,
    true
);
mailbox_contract!(
    ring_close_isolates_reply_notification_from_payload_disposal,
    2,
    true
);

#[test]
fn unqueued_envelope_keeps_payload_failure_catchable() {
    isolated("unqueued_envelope_keeps_payload_failure_catchable", || {
        let drops = Arc::new(AtomicUsize::new(0));
        let wakes = Arc::new(AtomicUsize::new(0));
        let (envelope, mut receiver) =
            AskEnvelope::with_boxed(Box::new(Value(drops.clone(), true)));
        let waker = Waker::from(Arc::new(ReceiverWake(wakes.clone())));
        assert!(
            Pin::new(&mut receiver)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let error = catch_unwind(AssertUnwindSafe(|| drop(envelope)))
            .expect_err("payload failure remains observable");
        assert_eq!(error.downcast_ref::<&str>(), Some(&"message destructor"));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert!(
            Pin::new(&mut receiver)
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        );
    });
}

#[test]
fn unpolled_reply_future_isolates_sender_notification() {
    isolated("unpolled_reply_future_isolates_sender_notification", || {
        let drops = Arc::new(AtomicUsize::new(0));
        let wakes = Arc::new(AtomicUsize::new(0));
        let (envelope, mut receiver) =
            AskEnvelope::with_boxed(Box::new(Value(drops.clone(), true)));
        let waker = Waker::from(Arc::new(ReceiverWake(wakes.clone())));
        assert!(
            Pin::new(&mut receiver)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let future = envelope.reply_success(Box::new(Value(drops.clone(), false)));
        assert!(
            catch_unwind(AssertUnwindSafe(|| drop(future))).is_ok(),
            "cancelling an unpolled reply must not unwind"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert!(
            Pin::new(&mut receiver)
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        );
    });
}
