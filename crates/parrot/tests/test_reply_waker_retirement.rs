//! Returning a reply and retiring its last notification owner are independent.
use futures::Future;
use parrot::thread::envelope::AskEnvelope;
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
use tokio::sync::oneshot;

struct Value(Arc<AtomicUsize>);
impl Drop for Value {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("reply value destructor");
    }
}
struct Notification {
    drops: Arc<AtomicUsize>,
    wakes: Arc<AtomicUsize>,
}
impl Wake for Notification {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Notification {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("notification owner destructor");
    }
}

fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_REPLY_RETIREMENT_CHILD").as_deref() == Ok(name) {
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
    fn read(mut pipe: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut output = String::new();
            pipe.read_to_string(&mut output).unwrap();
            output
        })
    }
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_REPLY_RETIREMENT_CHILD", name)
            .env("RUST_BACKTRACE", "0")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = read(child.0.stdout.take().unwrap());
    let stderr = read(child.0.stderr.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let stdout = stdout.join().unwrap();
            let stderr = stderr.join().unwrap();
            assert!(
                status.success(),
                "reply retirement child failed: {status}\n{stdout}\n{stderr}"
            );
            assert!(
                stdout.contains("1 passed; 0 failed; 0 ignored;"),
                "child must execute one case: {stdout}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "reply retirement child timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn received(mode: u8) {
    let values = Arc::new(AtomicUsize::new(0));
    let notifications = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let (envelope, mut receiver) = AskEnvelope::new_inline(1u64);
    let waker = Waker::from(Arc::new(Notification {
        drops: notifications.clone(),
        wakes: wakes.clone(),
    }));
    assert!(
        Pin::new(&mut receiver)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    envelope.reply_sync(Ok(Box::new(Value(values.clone()))));
    let result = catch_unwind(AssertUnwindSafe(|| match mode {
        0 => {
            let quiet = futures::task::noop_waker();
            let _ = Pin::new(&mut receiver).poll(&mut Context::from_waker(&quiet));
        }
        1 => {
            let _ = receiver.try_recv();
        }
        _ => drop(receiver),
    }));
    if mode == 2 {
        assert!(
            result.is_ok(),
            "receiver cancellation must contain cleanup failure"
        );
    } else {
        let panic = result.expect_err("the original notification failure remains observable");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"notification owner destructor")
        );
    }
    assert_eq!(
        values.load(Ordering::SeqCst),
        1,
        "reply value must be disposed exactly once"
    );
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
}

#[test]
fn poll_reply_keeps_notification_panic_catchable() {
    isolated("poll_reply_keeps_notification_panic_catchable", || {
        received(0)
    });
}
#[test]
fn try_reply_keeps_notification_panic_catchable() {
    isolated("try_reply_keeps_notification_panic_catchable", || {
        received(1)
    });
}
#[test]
fn cancelled_reply_contains_notification_and_value_destructors() {
    isolated(
        "cancelled_reply_contains_notification_and_value_destructors",
        || received(2),
    );
}

fn raw_both_waiters(rejected: bool) {
    let values = Arc::new(AtomicUsize::new(0));
    let rx_drops = Arc::new(AtomicUsize::new(0));
    let tx_drops = Arc::new(AtomicUsize::new(0));
    let rx_wakes = Arc::new(AtomicUsize::new(0));
    let tx_wakes = Arc::new(AtomicUsize::new(0));
    let (mut sender, mut receiver) = oneshot::channel();
    let rx_waker = if rejected {
        futures::task::noop_waker()
    } else {
        Waker::from(Arc::new(Notification {
            drops: rx_drops.clone(),
            wakes: rx_wakes.clone(),
        }))
    };
    let tx_waker = Waker::from(Arc::new(Notification {
        drops: tx_drops.clone(),
        wakes: tx_wakes.clone(),
    }));
    assert!(
        Pin::new(&mut receiver)
            .poll(&mut Context::from_waker(&rx_waker))
            .is_pending()
    );
    assert!(
        sender
            .poll_closed(&mut Context::from_waker(&tx_waker))
            .is_pending()
    );
    drop(rx_waker);
    drop(tx_waker);
    let result = catch_unwind(AssertUnwindSafe(|| {
        if rejected {
            drop(receiver);
            let _ = sender.send(Value(values.clone()));
        } else {
            assert!(sender.send(Value(values.clone())).is_ok());
            let _ = receiver.try_recv();
        }
    }));
    let panic = result.expect_err("raw channel preserves its first waker disposal failure");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"notification owner destructor")
    );
    assert_eq!(values.load(Ordering::SeqCst), 1);
    assert_eq!(rx_drops.load(Ordering::SeqCst), usize::from(!rejected));
    assert_eq!(rx_wakes.load(Ordering::SeqCst), usize::from(!rejected));
    assert_eq!(tx_wakes.load(Ordering::SeqCst), usize::from(rejected));
    assert_eq!(
        tx_drops.load(Ordering::SeqCst),
        1,
        "sibling waker must not leak"
    );
}

#[test]
fn both_registered_waiters_are_disposed_before_received_value_unwinds() {
    isolated(
        "both_registered_waiters_are_disposed_before_received_value_unwinds",
        || raw_both_waiters(false),
    );
}
#[test]
fn rejected_send_keeps_value_disposal_independent_of_last_waiter() {
    isolated(
        "rejected_send_keeps_value_disposal_independent_of_last_waiter",
        || raw_both_waiters(true),
    );
}

#[test]
fn healthy_reply_transfers_the_original_allocation() {
    let (envelope, mut receiver) = AskEnvelope::new_inline(1u64);
    let waker = futures::task::noop_waker();
    assert!(
        Pin::new(&mut receiver)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let value = Box::new(String::from("original reply"));
    let pointer = value.as_ref() as *const String;
    envelope.reply_sync(Ok(value));
    let received = receiver
        .try_recv()
        .unwrap()
        .unwrap()
        .downcast::<String>()
        .unwrap();
    assert_eq!(received.as_ref() as *const String, pointer);
    assert_eq!(received.as_str(), "original reply");
    assert!(receiver.is_terminated());
}
