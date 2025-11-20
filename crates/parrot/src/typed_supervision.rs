//! Supervised dispatch keeps the static protocol and its original reply types.
use crate::error_receipt::ActorReceipt;
use crate::{children::ChildLifetime, supervision_policy::reply_error};
use parrot_api::types::{ActorResult, BoxedFuture};
use parrot_api::{errors::ActorError, typed::ParrotTypedDispatch, typed_channel::TypedEnvelope};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

pub(crate) enum Restart {
    Ready,
    Stopped,
    Declined,
}
impl Restart {
    pub(crate) fn constructed(ready: bool) -> Self {
        if ready { Self::Ready } else { Self::Stopped }
    }
}

pub(crate) struct DispatchCleanup<'a> {
    first: &'a mut Option<ActorReceipt>,
    cancelled: &'a mut bool,
    timeout: Duration,
    deadline: Option<tokio::time::Instant>,
}
impl<'a> DispatchCleanup<'a> {
    pub(crate) fn new(first: &'a mut Option<ActorReceipt>, cancelled: &'a mut bool) -> Self {
        Self {
            first,
            cancelled,
            timeout: Duration::ZERO,
            deadline: None,
        }
    }
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub(crate) fn with_deadline(mut self, deadline: Option<tokio::time::Instant>) -> Self {
        self.deadline = deadline;
        self
    }
    pub(crate) fn reborrow(&mut self) -> DispatchCleanup<'_> {
        DispatchCleanup::new(self.first, self.cancelled)
            .with_timeout(self.timeout)
            .with_deadline(self.deadline)
    }
    pub(crate) fn failed(&self) -> bool {
        self.first.is_some()
    }
    pub(crate) fn failure_error(&self) -> Option<ActorError> {
        self.first.clone().map(ActorReceipt::into_error)
    }
    pub(crate) fn record_stream_disposal(&mut self, message: String, cancelled: bool) {
        self.first
            .get_or_insert_with(|| ActorReceipt::from_error(ActorError::Panic(message)));
        *self.cancelled |= cancelled;
    }
}

fn dispose<T>(value: T, cleanup: &mut DispatchCleanup<'_>) -> ActorResult<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value))).map_err(|payload| {
        let message = crate::owned_payload::panic_message(payload);
        cleanup
            .first
            .get_or_insert_with(|| ActorReceipt::from_error(ActorError::Panic(message.clone())));
        ActorError::Panic(message)
    })
}

// The user future remains outside the poll unwind boundary. Its destructor is
// retired separately, including when the engine cancels this dispatch. Record
// that cleanup before releasing the actor borrow or publishing a reply.
struct DispatchFuture<'c, F: Future> {
    future: Option<F>,
    cleanup: DispatchCleanup<'c>,
    recording: Option<parrot_api::trace::SpanGuard>,
    retain_error: fn(&F::Output) -> bool,
    discard: fn(F::Output, &mut DispatchCleanup<'_>),
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}
impl<T, F: Future<Output = ActorResult<T>> + Unpin> Future for DispatchFuture<'_, F> {
    type Output = Result<ActorResult<T>, String>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let expired = self.deadline.as_mut().is_some_and(|timer| {
            tokio::time::Instant::now() >= timer.deadline() || timer.as_mut().poll(cx).is_ready()
        });
        let polled = if expired {
            Ok(Poll::Ready(Err(ActorError::Timeout)))
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Pin::new(self.future.as_mut().expect("live typed dispatch future")).poll(cx)
            }))
        };
        // A synchronous constructor or poll can occupy the executor past the
        // deadline. Retire its late output separately before reporting timeout.
        let timed_out = self
            .deadline
            .as_ref()
            .is_some_and(|timer| tokio::time::Instant::now() >= timer.deadline());
        let polled = if timed_out {
            match polled {
                Ok(Poll::Ready(output)) => (self.discard)(output, &mut self.cleanup),
                Err(payload) => {
                    let _ = crate::owned_payload::panic_message(payload);
                }
                Ok(Poll::Pending) => {}
            }
            Ok(Poll::Ready(Err(ActorError::Timeout)))
        } else {
            polled
        };
        let result = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => Ok(result),
            Err(payload) => Err(crate::owned_payload::panic_message(payload)),
        };
        let future = self.future.take();
        let disposal = dispose(future, &mut self.cleanup);
        let disposal_failed = disposal.is_err();
        if timed_out && disposal_failed {
            *self.cleanup.cancelled = true;
        }
        let result = match disposal {
            Err(ActorError::Panic(message)) if matches!(&result, Ok(output) if !(self.retain_error)(output)) =>
            {
                if let Ok(output) = result {
                    (self.discard)(output, &mut self.cleanup);
                }
                Err(message)
            }
            _ => result,
        };
        if let Some(recording) = self.recording.take() {
            recording.finish(if disposal_failed {
                parrot_api::trace::SpanStatus::Panicked
            } else {
                match &result {
                    Ok(result) => parrot_api::trace::SpanStatus::from_result(result),
                    Err(_) => parrot_api::trace::SpanStatus::Panicked,
                }
            });
        }
        Poll::Ready(result)
    }
}
impl<F: Future> Drop for DispatchFuture<'_, F> {
    fn drop(&mut self) {
        // Only a pending user future can fail here. A completed dispatch was
        // already disposed in poll and its error may have been explicitly
        // resumed by supervision; do not confuse that with failed cancellation.
        if self.future.is_some() {
            let future = self.future.take();
            let failed = dispose(future, &mut self.cleanup).is_err();
            if failed {
                *self.cleanup.cancelled = true;
            }
            if let Some(recording) = self.recording.take() {
                recording.finish(if failed {
                    parrot_api::trace::SpanStatus::Panicked
                } else {
                    parrot_api::trace::SpanStatus::Cancelled
                });
            }
        }
    }
}

// Initialization and stop hooks have the same independent poll/disposal
// ownership as dispatch. Construct lazily inside the protected boundary so a
// synchronous hook panic is also represented as an ordinary stage failure.
pub(crate) async fn lifecycle_stage<'a>(
    construct: impl FnOnce() -> BoxedFuture<'a, ActorResult<()>>,
    cleanup: DispatchCleanup<'_>,
) -> Result<ActorResult<()>, String> {
    guarded_stage(construct, cleanup).await
}

// Admit only when polled at the actual hook boundary. The child's scope owns
// construction, every poll and independent disposal, including cancellation.
pub(crate) struct LifecycleOutcome {
    pub(crate) result: Result<ActorResult<()>, String>,
    pub(crate) context: Option<parrot_api::trace::TraceContext>,
}

pub(crate) async fn recorded_lifecycle_stage<F: Future<Output = ActorResult<()>> + Unpin>(
    kind: parrot_api::trace::SpanKind,
    target: &str,
    construct: impl FnOnce() -> F,
    cleanup: DispatchCleanup<'_>,
) -> LifecycleOutcome {
    let child = parrot_api::trace::TraceContext::current().map(|parent| parent.child());
    let result = parrot_api::trace::scope(child.clone(), async {
        let recording = parrot_api::trace::SpanGuard::current(kind, target);
        recorded_stage(construct, cleanup, Some(recording)).await
    })
    .await;
    LifecycleOutcome {
        result,
        context: child,
    }
}

pub(crate) async fn guarded_stage<T, F: Future<Output = ActorResult<T>> + Unpin>(
    construct: impl FnOnce() -> F,
    cleanup: DispatchCleanup<'_>,
) -> Result<ActorResult<T>, String> {
    recorded_stage(construct, cleanup, None).await
}

// FutureExt::map disposes its delegate as soon as it becomes Ready, inside
// poll. Keep this delegate alive until DispatchFuture's independent disposal.
struct BatchResult<F>(F);
impl<F: Future + Unpin> Future for BatchResult<F> {
    type Output = ActorResult<F::Output>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map(Ok)
    }
}

pub(crate) fn guarded_batch_stage<'a, F>(
    construct: impl FnOnce() -> F + Send + 'a,
    cleanup: DispatchCleanup<'a>,
) -> BoxedFuture<'a, Result<ActorResult<Vec<ActorResult<parrot_api::types::BoxedMessage>>>, String>>
where
    F: Future<Output = Vec<ActorResult<parrot_api::types::BoxedMessage>>> + Unpin + Send + 'a,
{
    Box::pin(recorded_stage_with(
        || BatchResult(construct()),
        cleanup,
        None,
        |output| match output {
            Ok(outcomes) => outcomes.iter().any(Result::is_err),
            Err(_) => true,
        },
        |output, cleanup| match output {
            Ok(outcomes) => {
                // Each completed result is an independent user-owned value.
                // Dropping the Vec in one unwind boundary can double-panic.
                for outcome in outcomes {
                    let _ = dispose(outcome, cleanup);
                }
            }
            Err(error) => {
                let _ = dispose(error, cleanup);
            }
        },
    ))
}

pub(crate) async fn recorded_stream_stage<T, F: Future<Output = ActorResult<T>> + Unpin>(
    kind: parrot_api::trace::SpanKind,
    target: &str,
    construct: impl FnOnce() -> F,
    cleanup: DispatchCleanup<'_>,
) -> Result<ActorResult<T>, String> {
    let recording = parrot_api::trace::SpanGuard::current(kind, target);
    recorded_stage(construct, cleanup, Some(recording)).await
}

async fn recorded_stage<T, F: Future<Output = ActorResult<T>> + Unpin>(
    construct: impl FnOnce() -> F,
    cleanup: DispatchCleanup<'_>,
    recording: Option<parrot_api::trace::SpanGuard>,
) -> Result<ActorResult<T>, String> {
    recorded_stage_with(
        construct,
        cleanup,
        recording,
        Result::is_err,
        |output, cleanup| {
            let _ = dispose(output, cleanup);
        },
    )
    .await
}

async fn recorded_stage_with<T, F: Future<Output = ActorResult<T>> + Unpin>(
    construct: impl FnOnce() -> F,
    cleanup: DispatchCleanup<'_>,
    recording: Option<parrot_api::trace::SpanGuard>,
    retain_error: fn(&ActorResult<T>) -> bool,
    discard: fn(ActorResult<T>, &mut DispatchCleanup<'_>),
) -> Result<ActorResult<T>, String> {
    let relative =
        (!cleanup.timeout.is_zero()).then(|| tokio::time::Instant::now() + cleanup.timeout);
    let deadline = match (relative, cleanup.deadline) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
    .map(|deadline| Box::pin(tokio::time::sleep_until(deadline)));
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(construct)) {
        Ok(future) => {
            DispatchFuture {
                future: Some(future),
                cleanup,
                recording,
                retain_error,
                discard,
                deadline,
            }
            .await
        }
        Err(payload) => {
            if let Some(recording) = recording {
                recording.finish(parrot_api::trace::SpanStatus::Panicked);
            }
            Err(crate::owned_payload::panic_message(payload))
        }
    }
}

pub(crate) async fn dispatch<A: ParrotTypedDispatch>(
    actor: &mut A,
    envelope: TypedEnvelope<A>,
    lifetime: &Arc<ChildLifetime>,
    runtime: &tokio::runtime::Handle,
    timeout: Duration,
    context: Option<&parrot_api::typed_context::TypedContext>,
    cleanup: DispatchCleanup<'_>,
) -> Result<(), crate::error_receipt::ActorReceipt> {
    let TypedEnvelope {
        msg,
        reply,
        trace_context,
    } = envelope;
    parrot_api::trace::scope(
        trace_context.map(|parent| parent.child()),
        dispatch_stage(
            context.map_or(std::any::type_name::<A>(), |context| context.path()),
            || match context {
                Some(context) => actor.dispatch_with_context(msg, context),
                None => actor.dispatch(msg),
            },
            reply,
            lifetime,
            runtime,
            timeout,
            cleanup,
        ),
    )
    .await
}

pub(crate) async fn dispatch_stage<T, F: Future<Output = ActorResult<T>> + Unpin>(
    target: &str,
    construct: impl FnOnce() -> F,
    reply: Option<flume::Sender<ActorResult<T>>>,
    lifetime: &Arc<ChildLifetime>,
    runtime: &tokio::runtime::Handle,
    timeout: Duration,
    mut cleanup: DispatchCleanup<'_>,
) -> Result<(), crate::error_receipt::ActorReceipt> {
    let recording =
        parrot_api::trace::SpanGuard::current(parrot_api::trace::SpanKind::TypedHandler, target);
    let result = recorded_stage(construct, cleanup.reborrow(), Some(recording))
        .await
        .unwrap_or_else(|message| Err(ActorError::Panic(message)));
    let (reply_value, failure) = match result {
        Ok(value) => (Ok(value), None),
        Err(error) => {
            let error = crate::error_receipt::ActorReceipt::from_error(error);
            let resume = if *cleanup.cancelled {
                false
            } else {
                lifetime
                    .resume_message(
                        error.clone(),
                        tokio::time::Instant::now() + timeout,
                        false,
                        runtime,
                    )
                    .await
            };
            (
                Err(reply_error(error.clone())),
                if resume { None } else { Some(error) },
            )
        }
    };
    let disposed = if let Some(reply) = reply {
        // Successful delivery transfers ownership to the channel. A cancelled
        // caller returns the value to this generation; retire it explicitly.
        // The sender may itself own the last channel reference after a racing
        // receiver cancellation, so its Drop needs an independent boundary too.
        let sent = reply.try_send(reply_value);
        let sender_disposed = dispose(reply, &mut cleanup);
        let value_disposed = dispose(sent, &mut cleanup);
        sender_disposed.and(value_disposed)
    } else {
        // TELL still constructs the handler's result. The runtime owns that
        // discarded value, including its physical cleanup failure.
        dispose(reply_value, &mut cleanup)
    };
    // Preserve an original handler failure; physical cleanup separately vetoes
    // generation reconstruction through the retained first cleanup receipt.
    failure.map_or_else(
        || disposed.map_err(crate::error_receipt::ActorReceipt::from_error),
        Err,
    )
}

/// The owning runner decides when to retire or reconstruct. A resumed
/// escalation leaves the supplied init/receive/dispatch future pinned in place.
pub(crate) enum Step<T> {
    Ready(T),
    Stopped,
    Escalated(crate::error_receipt::ActorReceipt),
    Recovery(Arc<crate::children::recovery::Recovery>),
}

#[cfg(test)]
mod recording_cleanup_contracts {
    use super::*;
    use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus, TraceContext, TraceRecorder};

    #[derive(Clone, Copy)]
    enum Mode {
        Ready,
        Error,
        PollPanic,
        Pending,
    }
    struct Work(Mode);
    impl Future for Work {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            match self.0 {
                Mode::Ready => Poll::Ready(Ok(())),
                Mode::Error => Poll::Ready(Err(ActorError::InternalError(
                    "original business error".into(),
                ))),
                Mode::PollPanic => panic!("original poll panic"),
                Mode::Pending => Poll::Pending,
            }
        }
    }
    impl Drop for Work {
        fn drop(&mut self) {
            panic!("independent cleanup panic");
        }
    }

    async fn completed(mode: Mode) {
        let recorder = TraceRecorder::new(2).unwrap();
        let root = recorder.root();
        let mut first = None;
        let mut cancelled = false;
        let result = root
            .scope(async {
                let guard = SpanGuard::current(SpanKind::TypedHandler, "/cleanup");
                recorded_stage(
                    || Work(mode),
                    DispatchCleanup::new(&mut first, &mut cancelled),
                    Some(guard),
                )
                .await
            })
            .await;
        match mode {
            Mode::Ready => assert!(result.unwrap_err().contains("independent cleanup panic")),
            Mode::Error => assert!(
                matches!(result, Ok(Err(ActorError::InternalError(ref text))) if text == "original business error")
            ),
            Mode::PollPanic => assert!(result.unwrap_err().contains("original poll panic")),
            Mode::Pending => unreachable!(),
        }
        assert!(first.is_some());
        assert!(!cancelled);
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].status, SpanStatus::Panicked);
        assert_eq!(snapshot.counts.panicked, 1);
        assert_eq!(snapshot.counts.in_flight, 0);
    }
    #[tokio::test]
    async fn recording_ready_cleanup_panic_vetoes_success() {
        completed(Mode::Ready).await;
    }
    #[tokio::test]
    async fn recording_error_cleanup_panic_preserves_business_reply_and_records_panic() {
        completed(Mode::Error).await;
    }
    #[tokio::test]
    async fn recording_poll_and_cleanup_panic_keeps_original_reply() {
        completed(Mode::PollPanic).await;
    }
    #[tokio::test]
    async fn recording_pending_cleanup_panic_is_not_plain_cancellation() {
        let recorder = TraceRecorder::new(2).unwrap();
        let root = recorder.root();
        let mut first = None;
        let mut cancelled = false;
        let mut work = Box::pin(root.scope(async {
            let guard = SpanGuard::current(SpanKind::TypedHandler, "/cleanup-pending");
            recorded_stage(
                || Work(Mode::Pending),
                DispatchCleanup::new(&mut first, &mut cancelled),
                Some(guard),
            )
            .await
        }));
        assert!(futures::poll!(&mut work).is_pending());
        assert_eq!(recorder.snapshot().counts.in_flight, 1);
        drop(work);
        assert!(first.is_some());
        assert!(cancelled);
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.panicked, 1);
        assert_eq!(snapshot.counts.cancelled, 0);
        assert_eq!(snapshot.records.len(), 1);
    }
    #[tokio::test]
    async fn unrelated_lifecycle_stage_does_not_invent_a_handler_record() {
        let recorder = TraceRecorder::new(2).unwrap();
        let root = recorder.root();
        let mut first = None;
        let mut cancelled = false;
        root.scope(guarded_stage(
            || std::future::ready(Ok(())),
            DispatchCleanup::new(&mut first, &mut cancelled),
        ))
        .await
        .unwrap()
        .unwrap();
        assert_eq!(recorder.snapshot().counts.started, 0);
    }
    #[tokio::test]
    async fn lifecycle_cleanup_panics_keep_original_results_and_child_identity() {
        for mode in [Mode::Ready, Mode::Error, Mode::PollPanic, Mode::Pending] {
            let recorder = TraceRecorder::new(2).unwrap();
            let root = recorder.root();
            let mut first = None;
            let mut cancelled = false;
            let mut work = Box::pin(root.scope(recorded_lifecycle_stage(
                SpanKind::ActorInit,
                "/lifecycle-cleanup",
                || Work(mode),
                DispatchCleanup::new(&mut first, &mut cancelled),
            )));
            if matches!(mode, Mode::Pending) {
                assert!(futures::poll!(&mut work).is_pending());
                assert_eq!(recorder.snapshot().counts.in_flight, 1);
            } else {
                let result = work.as_mut().await.result;
                match mode {
                    Mode::Ready => {
                        assert!(result.unwrap_err().contains("independent cleanup panic"))
                    }
                    Mode::Error => assert!(
                        matches!(result, Ok(Err(ActorError::InternalError(ref text))) if text == "original business error")
                    ),
                    Mode::PollPanic => assert!(result.unwrap_err().contains("original poll panic")),
                    Mode::Pending => unreachable!(),
                }
            }
            drop(work);
            assert!(first.is_some());
            assert_eq!(cancelled, matches!(mode, Mode::Pending));
            recorder.close();
            let snapshot = recorder.drain();
            assert!(snapshot.terminal);
            assert_eq!(snapshot.records.len(), 1);
            let record = &snapshot.records[0];
            assert_eq!(record.kind, SpanKind::ActorInit);
            assert_eq!(record.status, SpanStatus::Panicked);
            assert_eq!(record.trace_id, root.trace_id());
            assert_eq!(record.parent_span_id, Some(root.span_id()));
            assert_ne!(record.span_id, root.span_id());
            assert!(TraceContext::current().is_none());
        }
    }
    #[tokio::test]
    async fn unpolled_lifecycle_does_not_admit_or_invoke_a_hook() {
        let recorder = TraceRecorder::new(2).unwrap();
        let root = recorder.root();
        let mut first = None;
        let mut cancelled = false;
        let mut constructed = false;
        let work = root.scope(recorded_lifecycle_stage(
            SpanKind::ActorBeforeStop,
            "/never-entered",
            || {
                constructed = true;
                std::future::ready(Ok(()))
            },
            DispatchCleanup::new(&mut first, &mut cancelled),
        ));
        drop(work);
        assert!(!constructed);
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 0);
        assert!(snapshot.records.is_empty());
    }
}
