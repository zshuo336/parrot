use parrot_api::{
    address::{stop_for_cleanup, stop_for_owned_cleanup, ActorRef, RetirementStatus},
    errors::ActorError,
    trace::TraceContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

type Stop = dyn Fn() -> BoxedFuture<'static, ActorResult<()>> + Send + Sync;
#[derive(Clone)]
struct Reference {
    stop: Arc<Stop>,
    status: Arc<dyn Fn() -> RetirementStatus + Send + Sync>,
}
impl fmt::Debug for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RetirementTestRef")
    }
}
impl ActorRef for Reference {
    fn send(&self, _: BoxedMessage) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        panic!("unexpected send")
    }
    fn send_with_timeout(
        &self,
        _: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        panic!("unexpected send")
    }
    fn deliver(&self, _: BoxedMessage) -> BoxedFuture<'_, ActorResult<()>> {
        panic!("unexpected delivery")
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        (self.stop)()
    }
    fn retirement_status(&self) -> RetirementStatus {
        (self.status)()
    }
    fn path(&self) -> String {
        panic!("cleanup must never resolve or diagnose through path")
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        panic!("liveness is not a retirement receipt")
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn reference(
    stopped: fn() -> ActorResult<()>,
    status: impl Fn() -> RetirementStatus + Send + Sync + 'static,
) -> Reference {
    Reference {
        stop: Arc::new(move || Box::pin(async move { stopped() })),
        status: Arc::new(status),
    }
}
fn success() -> ActorResult<()> {
    Ok(())
}
fn missing() -> ActorResult<()> {
    Err(ActorError::ActorNotFound("old identity".into()))
}
fn timeout() -> ActorResult<()> {
    Err(ActorError::TimeoutDetail("historical timeout".into()))
}

#[tokio::test]
async fn pending_retirement_cannot_confirm_success_or_missing_identity() {
    for stopped in [success, missing, timeout] {
        let actor = reference(stopped, || RetirementStatus::Pending);
        assert!(stop_for_cleanup(&actor).await.is_err());
    }
}
#[tokio::test]
async fn unsupported_retirement_preserves_legacy_stop_fallback() {
    for stopped in [success, missing] {
        let actor = reference(stopped, || RetirementStatus::Unsupported);
        stop_for_cleanup(&actor).await.unwrap();
    }
    let actor = reference(timeout, || RetirementStatus::Unsupported);
    assert!(matches!(
        stop_for_cleanup(&actor).await,
        Err(ActorError::TimeoutDetail(_))
    ));
}

#[tokio::test]
async fn owned_cleanup_requires_confirmation_when_retirement_is_unsupported() {
    let actor = reference(success, || RetirementStatus::Unsupported);
    stop_for_owned_cleanup(&actor).await.unwrap();
    let actor = reference(missing, || RetirementStatus::Unsupported);
    assert!(matches!(
        stop_for_owned_cleanup(&actor).await,
        Err(ActorError::ActorNotFound(_))
    ));
    let actor = reference(timeout, || RetirementStatus::Unsupported);
    assert!(matches!(
        stop_for_owned_cleanup(&actor).await,
        Err(ActorError::TimeoutDetail(_))
    ));
}

#[tokio::test]
async fn owned_cleanup_uses_actual_receipt_and_rejects_pending_or_actual_failure() {
    for stopped in [success, missing, timeout] {
        let actor = reference(stopped, || RetirementStatus::Retired(Ok(())));
        stop_for_owned_cleanup(&actor).await.unwrap();
        let actor = reference(stopped, || RetirementStatus::Pending);
        assert!(stop_for_owned_cleanup(&actor).await.is_err());
        let actor = reference(stopped, || RetirementStatus::Retired(missing()));
        assert!(matches!(
            stop_for_owned_cleanup(&actor).await,
            Err(ActorError::ActorNotFound(_))
        ));
    }
}
#[tokio::test]
async fn actual_success_confirms_cleanup_without_rewriting_public_timeout() {
    let actor = reference(timeout, || RetirementStatus::Retired(Ok(())));
    stop_for_cleanup(&actor).await.unwrap();
    assert!(matches!(
        actor.stop().await,
        Err(ActorError::TimeoutDetail(_))
    ));
}
#[tokio::test]
async fn actual_failure_including_actor_not_found_is_never_normalized_to_success() {
    for stopped in [success, missing, timeout] {
        let actor = reference(stopped, || RetirementStatus::Retired(missing()));
        assert!(matches!(
            stop_for_cleanup(&actor).await,
            Err(ActorError::ActorNotFound(_))
        ));
    }
}
#[tokio::test]
async fn observation_and_stop_construction_panics_do_not_confirm_cleanup() {
    let observed = reference(success, || panic!("retirement observation panic"));
    assert!(matches!(
        stop_for_cleanup(&observed).await,
        Err(ActorError::Panic(_))
    ));
    let actor = Reference {
        stop: Arc::new(|| panic!("stop construction panic")),
        status: Arc::new(|| panic!("must not inspect after a callback failure")),
    };
    assert!(matches!(
        stop_for_cleanup(&actor).await,
        Err(ActorError::Panic(_))
    ));
}

struct StopFuture {
    pending: bool,
    poll_panic: bool,
    drop_panic: bool,
    dropped: Arc<AtomicUsize>,
}
impl Future for StopFuture {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.poll_panic, "stop poll panic");
        if self.pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
impl Drop for StopFuture {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        assert!(!self.drop_panic, "stop future disposal panic");
    }
}
#[tokio::test]
async fn poll_and_future_disposal_panics_have_independent_unwind_boundaries() {
    for (poll_panic, drop_panic) in [(true, false), (false, true), (true, true)] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(AtomicUsize::new(0));
        let count = dropped.clone();
        let observations = observed.clone();
        let actor = Reference {
            stop: Arc::new(move || {
                Box::pin(StopFuture {
                    pending: false,
                    poll_panic,
                    drop_panic,
                    dropped: count.clone(),
                })
            }),
            status: Arc::new(move || {
                observations.fetch_add(1, Ordering::SeqCst);
                RetirementStatus::Retired(Ok(()))
            }),
        };
        let error = stop_for_cleanup(&actor).await.unwrap_err();
        let ActorError::Panic(message) = error else {
            panic!("callback failure lost its classification: {error}")
        };
        assert_eq!(
            message,
            if poll_panic {
                "stop poll panic"
            } else {
                "stop future disposal panic"
            }
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(observed.load(Ordering::SeqCst), 0);
    }
}
#[derive(Debug)]
struct ErrorDrop(Arc<AtomicUsize>);
impl fmt::Display for ErrorDrop {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("cleanup helper must not format discarded errors")
    }
}
impl std::error::Error for ErrorDrop {}
impl Drop for ErrorDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("stop error disposal panic");
    }
}
#[tokio::test]
async fn discarded_stop_error_disposal_cannot_publish_success_or_replace_actual_failure() {
    for actual_failure in [false, true] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let count = dropped.clone();
        let actor = Reference {
            stop: Arc::new(move || {
                let count = count.clone();
                Box::pin(
                    async move { Err(ActorError::Other(anyhow::Error::new(ErrorDrop(count)))) },
                )
            }),
            status: Arc::new(move || {
                RetirementStatus::Retired(if actual_failure {
                    Err(ActorError::InternalError("actual cleanup failure".into()))
                } else {
                    Ok(())
                })
            }),
        };
        let error = stop_for_cleanup(&actor).await.unwrap_err();
        if actual_failure {
            assert!(
                matches!(error, ActorError::InternalError(ref text) if text == "actual cleanup failure")
            );
        } else {
            assert!(matches!(error, ActorError::Panic(_)));
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn unpolled_and_cancelled_cleanup_do_not_invoke_observation_or_lose_future_disposal() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let count = dropped.clone();
    let actor = Reference {
        stop: Arc::new(move || {
            Box::pin(StopFuture {
                pending: true,
                poll_panic: false,
                drop_panic: true,
                dropped: count.clone(),
            })
        }),
        status: Arc::new(|| panic!("cancelled stop has no outcome to inspect")),
    };
    drop(stop_for_cleanup(&actor));
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    let mut cleanup = stop_for_cleanup(&actor);
    std::future::poll_fn(|cx| {
        assert!(cleanup.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(cleanup);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn cleanup_context_is_fixed_at_construction_including_explicit_none() {
    type Events = Arc<Mutex<Vec<Option<TraceContext>>>>;
    struct ContextFuture(Events);
    impl Future for ContextFuture {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.lock().unwrap().push(TraceContext::current());
            Poll::Ready(Ok(()))
        }
    }
    impl Drop for ContextFuture {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(TraceContext::current());
        }
    }
    for traced in [false, true] {
        let events: Events = Arc::default();
        let construction = events.clone();
        let observation = events.clone();
        let actor = Reference {
            stop: Arc::new(move || {
                construction.lock().unwrap().push(TraceContext::current());
                Box::pin(ContextFuture(construction.clone()))
            }),
            status: Arc::new(move || {
                observation.lock().unwrap().push(TraceContext::current());
                RetirementStatus::Retired(Ok(()))
            }),
        };
        let parent = traced.then(TraceContext::root);
        let cleanup = parrot_api::trace::scope(
            parent.clone(),
            std::future::poll_fn(|_| Poll::Ready(stop_for_cleanup(&actor))),
        )
        .await;
        TraceContext::root().scope(cleanup).await.unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert!(
            events.iter().all(|context| context == &parent),
            "{events:?}"
        );
    }
}
