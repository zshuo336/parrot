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

pub(crate) struct DispatchCleanup<'a> {
    first: &'a mut Option<ActorReceipt>,
    cancelled: &'a mut bool,
}
impl<'a> DispatchCleanup<'a> {
    pub(crate) fn new(first: &'a mut Option<ActorReceipt>, cancelled: &'a mut bool) -> Self {
        Self { first, cancelled }
    }
    pub(crate) fn reborrow(&mut self) -> DispatchCleanup<'_> {
        DispatchCleanup::new(self.first, self.cancelled)
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
struct DispatchFuture<'c, F> {
    future: Option<F>,
    cleanup: DispatchCleanup<'c>,
    recording: Option<parrot_api::trace::SpanGuard>,
}
impl<T, F: Future<Output = ActorResult<T>> + Unpin> Future for DispatchFuture<'_, F> {
    type Output = Result<ActorResult<T>, String>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Pin::new(self.future.as_mut().expect("live typed dispatch future")).poll(cx)
        }));
        let result = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => Ok(result),
            Err(payload) => Err(crate::owned_payload::panic_message(payload)),
        };
        let future = self.future.take();
        let disposal = dispose(future, &mut self.cleanup);
        let disposal_failed = disposal.is_err();
        let result = match disposal {
            Err(ActorError::Panic(message)) if matches!(result, Ok(Ok(_))) => {
                let _ = dispose(result, &mut self.cleanup);
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
impl<F> Drop for DispatchFuture<'_, F> {
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

pub(crate) async fn guarded_stage<T, F: Future<Output = ActorResult<T>> + Unpin>(
    construct: impl FnOnce() -> F,
    cleanup: DispatchCleanup<'_>,
) -> Result<ActorResult<T>, String> {
    recorded_stage(construct, cleanup, None).await
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
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(construct)) {
        Ok(future) => {
            DispatchFuture {
                future: Some(future),
                cleanup,
                recording,
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
) -> Result<(), Arc<ActorError>> {
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
) -> Result<(), Arc<ActorError>> {
    let recording =
        parrot_api::trace::SpanGuard::current(parrot_api::trace::SpanKind::TypedHandler, target);
    let result = recorded_stage(construct, cleanup.reborrow(), Some(recording))
        .await
        .unwrap_or_else(|message| Err(ActorError::Panic(message)));
    let (reply_value, failure) = match result {
        Ok(value) => (Ok(value), None),
        Err(error) => {
            let error = Arc::new(error);
            let resume = lifetime
                .resume_message(
                    error.clone(),
                    tokio::time::Instant::now() + timeout,
                    false,
                    runtime,
                )
                .await;
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
    failure.map_or_else(|| disposed.map_err(Arc::new), Err)
}

/// The owning runner decides when to retire or reconstruct. A resumed
/// escalation leaves the supplied init/receive/dispatch future pinned in place.
pub(crate) enum Step<T> {
    Ready(T),
    Stopped,
    Escalated(Arc<ActorError>),
    Recovery(Arc<crate::children::recovery::Recovery>),
}

#[cfg(test)]
mod recording_cleanup_contracts {
    use super::*;
    use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus, TraceRecorder};

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
}
