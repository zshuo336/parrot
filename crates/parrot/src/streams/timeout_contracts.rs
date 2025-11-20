use super::*;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
    time::Duration,
};

#[derive(Default)]
struct Counts {
    polls: AtomicUsize,
    futures: AtomicUsize,
    outputs: AtomicUsize,
}
struct Output(Arc<Counts>, bool);
impl Drop for Output {
    fn drop(&mut self) {
        self.0.outputs.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "late stream output disposal");
    }
}
struct Probe {
    counts: Arc<Counts>,
    ready: bool,
    slow: bool,
    bad_future: bool,
    bad_output: bool,
}
impl Future for Probe {
    type Output = ActorResult<Output>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.counts.polls.fetch_add(1, Ordering::SeqCst);
        if self.slow {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.ready {
            Poll::Ready(Ok(Output(self.counts.clone(), self.bad_output)))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.futures.fetch_add(1, Ordering::SeqCst);
        assert!(!self.bad_future, "timed stream future disposal");
    }
}
#[tokio::test(start_paused = true)]
async fn expired_first_poll_never_enters_user_future() {
    let counts = Arc::new(Counts::default());
    let failures = InputFailures::new();
    let (mut first, mut cancelled) = (None, false);
    let timer = Box::pin(tokio::time::sleep(Duration::from_secs(1)));
    tokio::time::advance(Duration::from_secs(2)).await;
    let result = StreamFuture {
        recording: None,
        future: StreamInput::new(
            Box::pin(Probe {
                counts: counts.clone(),
                ready: true,
                slow: false,
                bad_future: false,
                bad_output: false,
            }) as BoxedFuture<'_, ActorResult<Output>>,
            &failures,
        ),
        supervise_panic: true,
        cleanup: Some(crate::typed_supervision::DispatchCleanup::new(
            &mut first,
            &mut cancelled,
        )),
        deadline: Some(timer),
    }
    .await;
    assert!(matches!(result, Err(ActorError::Timeout)));
    assert_eq!(counts.polls.load(Ordering::SeqCst), 0);
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 0);
    assert!(first.is_none());
    assert!(!cancelled);
}
async fn timed_probe(
    slow: bool,
    ready: bool,
    bad_future: bool,
    bad_output: bool,
    slow_constructor: bool,
) {
    let counts = Arc::new(Counts::default());
    let failures = InputFailures::new();
    let (mut first, mut cancelled) = (None, false);
    let result = actor_stage(
        &failures,
        Duration::from_millis(5),
        crate::typed_supervision::DispatchCleanup::new(&mut first, &mut cancelled),
        || {
            if slow_constructor {
                std::thread::sleep(Duration::from_millis(20));
            }
            Box::pin(Probe {
                counts: counts.clone(),
                ready,
                slow,
                bad_future,
                bad_output,
            })
        },
    )
    .await;
    assert!(matches!(result, Err(ActorError::Timeout)));
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.polls.load(Ordering::SeqCst),
        usize::from(!slow_constructor)
    );
    assert_eq!(
        counts.outputs.load(Ordering::SeqCst),
        usize::from(ready && !slow_constructor)
    );
    assert_eq!(first.is_some(), bad_future || bad_output);
    assert_eq!(cancelled, bad_future);
    assert_eq!(failures.take().is_some(), bad_future || bad_output);
}
#[tokio::test]
async fn pending_future_is_cancelled_before_timeout_returns() {
    timed_probe(false, false, false, false, false).await;
}
#[tokio::test]
async fn failed_timeout_disposal_reaches_both_owners() {
    timed_probe(false, false, true, false, false).await;
}
#[tokio::test]
async fn late_ready_output_is_retired_before_timeout() {
    timed_probe(true, true, false, false, false).await;
}
#[tokio::test]
async fn late_pending_poll_cannot_extend_budget() {
    timed_probe(true, false, false, false, false).await;
}
#[tokio::test]
async fn late_output_failure_reaches_both_owners() {
    timed_probe(true, true, false, true, false).await;
}
#[tokio::test]
async fn constructor_uses_same_budget_as_poll() {
    timed_probe(false, true, false, false, true).await;
}
#[tokio::test(start_paused = true)]
async fn zero_budget_keeps_pending_until_explicit_cancellation() {
    let counts = Arc::new(Counts::default());
    let failures = InputFailures::new();
    let (mut first, mut cancelled) = (None, false);
    let mut future = Box::pin(actor_stage(
        &failures,
        Duration::ZERO,
        crate::typed_supervision::DispatchCleanup::new(&mut first, &mut cancelled),
        || {
            Box::pin(Probe {
                counts: counts.clone(),
                ready: false,
                slow: false,
                bad_future: true,
                bad_output: false,
            })
        },
    ));
    assert!(futures::poll!(&mut future).is_pending());
    tokio::time::advance(Duration::from_secs(86400)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(counts.futures.load(Ordering::SeqCst), 0);
    drop(future);
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert!(first.is_some());
    assert!(cancelled);
    assert!(failures.take().is_some());
}
