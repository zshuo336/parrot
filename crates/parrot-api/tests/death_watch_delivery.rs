use futures::FutureExt;
use parrot_api::{
    context::ActorTerminated,
    death_watch::{DeathWatch, TerminationRecipient},
    errors::ActorError,
    types::{ActorResult, BoxedFuture},
    DeathReason,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone, Copy)]
enum Mode {
    Pending,
    Ready,
    PollPanic,
    ConstructPanic,
}

struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("payload destructor failed");
    }
}

struct Delivery {
    mode: Mode,
    drops: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl Future for Delivery {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        match self.mode {
            Mode::Pending => Poll::Pending,
            Mode::Ready => Poll::Ready(Ok(())),
            Mode::PollPanic => std::panic::panic_any(Payload(self.payloads.clone())),
            Mode::ConstructPanic => unreachable!(),
        }
    }
}
impl Drop for Delivery {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("delivery destructor failed");
    }
}

struct Recipient {
    mode: Mode,
    drops: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl TerminationRecipient for Recipient {
    fn deliver(&self, _: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        if matches!(self.mode, Mode::ConstructPanic) {
            std::panic::panic_any(Payload(self.payloads.clone()));
        }
        Box::pin(Delivery {
            mode: self.mode,
            drops: self.drops.clone(),
            payloads: self.payloads.clone(),
        })
    }
}

struct Healthy(Arc<AtomicUsize>);
impl TerminationRecipient for Healthy {
    fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(event.path, "target");
            assert!(matches!(event.reason, DeathReason::Normal));
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

fn event(target: &DeathWatch) -> ActorTerminated {
    ActorTerminated {
        generation: target.id(),
        path: "target".into(),
        reason: DeathReason::Normal,
    }
}

async fn complete(mode: Mode) {
    let target = DeathWatch::new(2);
    let hostile = DeathWatch::new(1);
    let healthy = DeathWatch::new(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(AtomicUsize::new(0));
    hostile
        .set_recipient(Arc::new(Recipient {
            mode,
            drops: drops.clone(),
            payloads: payloads.clone(),
        }))
        .unwrap();
    healthy
        .set_recipient(Arc::new(Healthy(delivered.clone())))
        .unwrap();
    target.watch(&hostile).unwrap();
    target.watch(&healthy).unwrap();
    let result = std::panic::AssertUnwindSafe(
        target.terminated_checked(event(&target), Duration::from_millis(20)),
    )
    .catch_unwind()
    .await;
    assert!(result.is_ok(), "recipient cleanup escaped owned delivery");
    let error = result
        .unwrap()
        .expect_err("failed retirement cannot report success");
    if matches!(mode, Mode::Pending) {
        assert!(
            matches!(error, ActorError::Timeout),
            "deadline stays a timeout"
        );
    } else {
        assert!(matches!(error, ActorError::Panic(_)));
    }
    assert_eq!(
        drops.load(Ordering::SeqCst),
        usize::from(!matches!(mode, Mode::ConstructPanic))
    );
    assert_eq!(
        payloads.load(Ordering::SeqCst),
        usize::from(matches!(mode, Mode::PollPanic | Mode::ConstructPanic))
    );
    assert_eq!(
        delivered.load(Ordering::SeqCst),
        1,
        "healthy peer must still receive the event"
    );
    assert_eq!(target.delivery_failures(), 1);
    assert_eq!(target.subscriptions(), (0, 0));
    assert_eq!(hostile.subscriptions(), (0, 0));
    assert_eq!(healthy.subscriptions(), (0, 0));
}

fn isolated(name: &str, body: impl Future<Output = ()>) {
    if std::env::var("PARROT_DELIVERY_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(body);
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
        Log(std::env::temp_dir().join(format!("parrot-delivery-{}.log", uuid::Uuid::new_v4())));
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log.0)
        .unwrap();
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_DELIVERY_CHILD", name)
            .env("RUST_BACKTRACE", "0")
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let output = std::fs::read_to_string(&log.0).unwrap();
            assert!(
                status.success(),
                "isolated delivery failed: {status}\n{output}"
            );
            assert!(
                output.contains("1 passed; 0 failed"),
                "contract did not execute: {output}"
            );
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "isolated delivery timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn timeout_contains_delivery_destructor_failure_and_preserves_healthy_peer() {
    isolated(
        "timeout_contains_delivery_destructor_failure_and_preserves_healthy_peer",
        complete(Mode::Pending),
    );
}

#[test]
fn successful_delivery_cannot_hide_its_future_disposal_failure() {
    isolated(
        "successful_delivery_cannot_hide_its_future_disposal_failure",
        complete(Mode::Ready),
    );
}

#[test]
fn constructing_delivery_contains_recursive_panic_payload_cleanup() {
    isolated(
        "constructing_delivery_contains_recursive_panic_payload_cleanup",
        complete(Mode::ConstructPanic),
    );
}

#[test]
fn poll_panic_and_delivery_destructor_failure_have_separate_boundaries() {
    isolated(
        "poll_panic_and_delivery_destructor_failure_have_separate_boundaries",
        complete(Mode::PollPanic),
    );
}

async fn cancel_pending(unwatch: bool) {
    let target = DeathWatch::new(1);
    let watcher = DeathWatch::new(1);
    let drops = Arc::new(AtomicUsize::new(0));
    watcher
        .set_recipient(Arc::new(Recipient {
            mode: Mode::Pending,
            drops: drops.clone(),
            payloads: Arc::new(AtomicUsize::new(0)),
        }))
        .unwrap();
    target.watch(&watcher).unwrap();
    let mut delivery = Box::pin(target.terminated_checked(event(&target), Duration::from_secs(60)));
    assert!(futures::poll!(&mut delivery).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    if unwatch {
        target.unwatch(&watcher);
    }
    let cancelled = if unwatch {
        let result = std::panic::AssertUnwindSafe(&mut delivery)
            .catch_unwind()
            .await;
        assert!(
            result.is_ok(),
            "unwatch cleanup escaped delivery destructor boundary"
        );
        assert!(matches!(result.unwrap(), Err(ActorError::Panic(_))));
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(delivery)))
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(delivery)))
    };
    assert!(
        cancelled.is_ok(),
        "cancellation escaped delivery destructor boundary"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(target.subscriptions(), (0, 0));
    assert_eq!(watcher.subscriptions(), (0, 0));
    assert_eq!(target.delivery_failures(), 1);
}

#[test]
fn cancelling_pending_delivery_contains_destructor_failure_and_unlinks() {
    isolated(
        "cancelling_pending_delivery_contains_destructor_failure_and_unlinks",
        cancel_pending(false),
    );
}

#[test]
fn unwatch_contains_pending_delivery_destructor_failure_and_unlinks() {
    isolated(
        "unwatch_contains_pending_delivery_destructor_failure_and_unlinks",
        cancel_pending(true),
    );
}

struct BusinessError {
    drops: Arc<AtomicUsize>,
    formats: Arc<AtomicUsize>,
}
impl std::fmt::Display for BusinessError {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.formats.fetch_add(1, Ordering::SeqCst);
        panic!("business error formatting must not run");
    }
}
impl std::fmt::Debug for BusinessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for BusinessError {}
impl Drop for BusinessError {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("business error disposal failed");
    }
}
struct ErrorRecipient {
    drops: Arc<AtomicUsize>,
    formats: Arc<AtomicUsize>,
}
impl TerminationRecipient for ErrorRecipient {
    fn deliver(&self, _: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async {
            Err(ActorError::Other(anyhow::Error::new(BusinessError {
                drops: self.drops.clone(),
                formats: self.formats.clone(),
            })))
        })
    }
}

struct FormattingSubscriber;
impl tracing::Subscriber for FormattingSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Visitor;
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let _ = format!("{value:?}");
            }
        }
        event.record(&mut Visitor);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn discarded_notification_errors_are_not_formatted_and_retire_independently() {
    isolated(
        "discarded_notification_errors_are_not_formatted_and_retire_independently",
        async {
            let dispatch = tracing::Dispatch::new(FormattingSubscriber);
            let _subscriber = tracing::dispatcher::set_default(&dispatch);
            let target = DeathWatch::new(2);
            let drops = Arc::new(AtomicUsize::new(0));
            let formats = Arc::new(AtomicUsize::new(0));
            let watchers: Vec<_> = (0..2)
                .map(|_| {
                    let watcher = DeathWatch::new(1);
                    watcher
                        .set_recipient(Arc::new(ErrorRecipient {
                            drops: drops.clone(),
                            formats: formats.clone(),
                        }))
                        .unwrap();
                    target.watch(&watcher).unwrap();
                    watcher
                })
                .collect();
            let result = std::panic::AssertUnwindSafe(
                target.terminated(event(&target), Duration::from_secs(1)),
            )
            .catch_unwind()
            .await;
            assert!(
                result.is_ok(),
                "discarding recipient errors escaped notification ownership"
            );
            assert_eq!(formats.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 2);
            assert_eq!(target.delivery_failures(), 2);
            assert!(watchers
                .iter()
                .all(|watcher| watcher.subscriptions() == (0, 0)));
        },
    );
}

#[test]
fn checked_notification_retains_the_original_business_error_for_its_caller() {
    isolated(
        "checked_notification_retains_the_original_business_error_for_its_caller",
        async {
            let target = DeathWatch::new(1);
            let watcher = DeathWatch::new(1);
            let drops = Arc::new(AtomicUsize::new(0));
            let formats = Arc::new(AtomicUsize::new(0));
            watcher
                .set_recipient(Arc::new(ErrorRecipient {
                    drops: drops.clone(),
                    formats: formats.clone(),
                }))
                .unwrap();
            target.watch(&watcher).unwrap();
            let result = target
                .terminated_checked(event(&target), Duration::from_secs(1))
                .await;
            let Err(ActorError::Other(error)) = result else {
                panic!("original concrete error was replaced")
            };
            let original = error
                .downcast_ref::<BusinessError>()
                .expect("business error cause lost");
            assert!(Arc::ptr_eq(&original.drops, &drops));
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert_eq!(formats.load(Ordering::SeqCst), 0);
            assert_eq!(target.delivery_failures(), 1);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(error))).is_err()
            );
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "returned error now belongs to caller"
            );
        },
    );
}

#[test]
fn cancelled_broadcast_owns_its_already_collected_error_and_pending_future() {
    isolated(
        "cancelled_broadcast_owns_its_already_collected_error_and_pending_future",
        async {
            let target = DeathWatch::new(2);
            let errors = DeathWatch::new(1);
            let pending = DeathWatch::new(1);
            let error_drops = Arc::new(AtomicUsize::new(0));
            let future_drops = Arc::new(AtomicUsize::new(0));
            errors
                .set_recipient(Arc::new(ErrorRecipient {
                    drops: error_drops.clone(),
                    formats: Arc::new(AtomicUsize::new(0)),
                }))
                .unwrap();
            pending
                .set_recipient(Arc::new(Recipient {
                    mode: Mode::Pending,
                    drops: future_drops.clone(),
                    payloads: Arc::new(AtomicUsize::new(0)),
                }))
                .unwrap();
            target.watch(&errors).unwrap();
            target.watch(&pending).unwrap();
            let mut delivery =
                Box::pin(target.terminated_checked(event(&target), Duration::from_secs(60)));
            assert!(futures::poll!(&mut delivery).is_pending());
            assert_eq!(
                target.delivery_failures(),
                1,
                "real first error has been collected"
            );
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(delivery)));
            assert!(
                result.is_ok(),
                "cancelled first error and pending future overlapped unwind"
            );
            assert_eq!(error_drops.load(Ordering::SeqCst), 1);
            assert_eq!(future_drops.load(Ordering::SeqCst), 1);
            assert_eq!(target.delivery_failures(), 2);
            assert_eq!(errors.subscriptions(), (0, 0));
            assert_eq!(pending.subscriptions(), (0, 0));
        },
    );
}

struct DroppingEndpoint(Arc<AtomicUsize>);
impl Drop for DroppingEndpoint {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("last recipient endpoint disposal failed");
    }
}
impl TerminationRecipient for DroppingEndpoint {
    fn deliver(&self, _: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(std::future::pending())
    }
}

#[test]
fn last_recipient_endpoint_retirement_is_owned_after_watcher_disappears() {
    isolated(
        "last_recipient_endpoint_retirement_is_owned_after_watcher_disappears",
        async {
            let target = DeathWatch::new(1);
            let watcher = DeathWatch::new(1);
            let drops = Arc::new(AtomicUsize::new(0));
            watcher
                .set_recipient(Arc::new(DroppingEndpoint(drops.clone())))
                .unwrap();
            target.watch(&watcher).unwrap();
            let mut delivery =
                Box::pin(target.terminated_checked(event(&target), Duration::from_secs(60)));
            assert!(futures::poll!(&mut delivery).is_pending());
            drop(watcher);
            assert_eq!(
                drops.load(Ordering::SeqCst),
                0,
                "pending operation owns endpoint"
            );
            let result = std::panic::AssertUnwindSafe(delivery).catch_unwind().await;
            assert!(
                matches!(result, Ok(Err(ActorError::Panic(_)))),
                "endpoint failure escaped or reported success"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(target.delivery_failures(), 1);
            assert_eq!(target.subscriptions(), (0, 0));
        },
    );
}
