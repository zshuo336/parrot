//! Diagnostic subscribers must not unwind a scheduler's failure cleanup.
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Self(self.0.clone()));
    }
}
struct Subscriber {
    events: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl tracing::Subscriber for Subscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::ERROR
            && metadata.target().starts_with("parrot::thread::scheduler")
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Payload(self.payloads.clone()));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

pub(super) fn dispose(work: impl FnOnce(), during_unwind: bool) {
    if !during_unwind {
        work();
        return;
    }
    struct OnDrop<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for OnDrop<F> {
        fn drop(&mut self) {
            self.0.take().unwrap()();
        }
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _cleanup = OnDrop(Some(work));
        panic!("original unrelated unwind");
    }));
    let payload = result.expect_err("original unwind was lost");
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"original unrelated unwind")
    );
}

pub(super) async fn isolated(name: &str, exercise: impl FnOnce()) {
    isolated_async(name, async { exercise() }, 1).await;
}

pub(super) async fn isolated_async(
    name: &str,
    exercise: impl std::future::Future<Output = ()>,
    expected_events: usize,
) {
    use futures::FutureExt;
    const CHILD: &str = "PARROT_NOTIFICATION_LOGGING_CHILD";
    if std::env::var_os(CHILD).is_some() {
        // Nested panics force symbolized backtraces even with RUST_BACKTRACE=0.
        // This isolated child asserts receipts and payloads, not std's printer.
        std::panic::set_hook(Box::new(|_| {}));
        let events = Arc::new(AtomicUsize::new(0));
        let payloads = Arc::new(AtomicUsize::new(0));
        tracing::subscriber::set_global_default(Subscriber {
            events: events.clone(),
            payloads: payloads.clone(),
        })
        .unwrap();
        // Keep the subscriber's throwing payload out of assertion unwinding.
        if let Err(payload) = AssertUnwindSafe(exercise).catch_unwind().await {
            let message = crate::owned_payload::panic_message(payload);
            panic!("notification logging escaped cleanup: {message}");
        }
        assert_eq!(
            events.load(Ordering::SeqCst),
            expected_events,
            "diagnostic missing or recursively retried"
        );
        assert_eq!(
            payloads.load(Ordering::SeqCst),
            expected_events,
            "logging payload was not safely retired"
        );
        return;
    }
    let module = if name.starts_with("retirement_") {
        "thread::scheduler::retirement_submission_contracts"
    } else if name.starts_with("control_") {
        "thread::scheduler::control_panic_contracts"
    } else if name.starts_with("observer_") {
        "thread::scheduler::cancellation_contracts"
    } else if name.starts_with("shared_") {
        "thread::scheduler::shared::worker::stage_contracts"
    } else if name.starts_with("dedicated_") {
        "thread::scheduler::dedicated_thread::stage_ownership_contracts"
    } else {
        "thread::scheduler::sharded::cancellation_contracts"
    };
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::{name}"), "--nocapture"])
            .env(CHILD, "1")
            .env("RUST_BACKTRACE", "0")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"),
        "notification child failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
