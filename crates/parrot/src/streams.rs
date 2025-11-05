//! Generation-owned bounded stream workers and serial actor callbacks.
use crate::context_tasks::{ContextTasks, InputFailures, OwnedTask};
use futures::{FutureExt, Stream, StreamExt};
use parrot_api::{
    actor::Actor,
    errors::ActorError,
    stream::{StreamCallback, StreamContextView, StreamHandlerRegistration, StreamRegistry},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    any::TypeId,
    sync::{Arc, OnceLock},
};
use tokio::sync::oneshot;

type ErasedStream = Box<dyn Stream<Item = BoxedMessage> + Send>;
pub(crate) trait StreamRecipient: Send + Sync {
    fn deliver(&self, event: StreamEvent) -> BoxedFuture<'_, ActorResult<()>>;
}
pub(crate) struct StreamEvent {
    parent: Option<parrot_api::trace::TraceContext>,
    callback: StreamInput<StreamCallback>,
    handler: StreamInput<Option<StreamHandlerRegistration>>,
    completion: oneshot::Sender<StreamReply>,
}
struct StreamReply {
    handler: StreamInput<Option<StreamHandlerRegistration>>,
    result: StreamInput<ActorResult<()>>,
}

#[derive(Clone)]
pub(crate) enum StreamExecutor {
    Thread {
        handle: tokio::runtime::Handle,
        slots: Arc<tokio::sync::Semaphore>,
    },
    Actix(Option<actix::ArbiterHandle>),
}
impl StreamExecutor {
    fn submit(&self, task: OwnedTask) -> ActorResult<()> {
        match self {
            Self::Thread { handle, slots } => {
                let slots = slots.clone();
                handle.spawn(async move {
                    if let Ok(permit) = slots.clone().acquire_owned().await {
                        let _ = crate::thread::scheduler::shared::poll_isolated(
                            task,
                            permit,
                            slots,
                            Arc::new(tokio::sync::Notify::new()),
                        )
                        .await;
                    }
                });
                Ok(())
            }
            Self::Actix(Some(arbiter)) => {
                if arbiter.spawn(async move {
                    let _ = task.await;
                }) {
                    Ok(())
                } else {
                    Err(ActorError::Stopped)
                }
            }
            Self::Actix(None) => Err(ActorError::InternalError(
                "stream registry has no actor executor".into(),
            )),
        }
    }
}
#[derive(Clone)]
pub(crate) struct OwnedStreams {
    tasks: Arc<ContextTasks>,
    context_type: TypeId,
    executor: StreamExecutor,
    recipient: Arc<OnceLock<Arc<dyn StreamRecipient>>>,
}
impl std::fmt::Debug for OwnedStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedStreams")
            .field("bound", &self.recipient.get().is_some())
            .finish_non_exhaustive()
    }
}
impl OwnedStreams {
    pub(crate) fn new<C: 'static>(tasks: Arc<ContextTasks>, executor: StreamExecutor) -> Self {
        Self {
            tasks,
            context_type: TypeId::of::<C>(),
            executor,
            recipient: Arc::new(OnceLock::new()),
        }
    }
    pub(crate) fn set_tasks(&mut self, tasks: Arc<ContextTasks>) {
        assert!(
            self.recipient.get().is_none(),
            "stream task owner changed after binding"
        );
        self.tasks = tasks;
    }
    pub(crate) fn bind(&self, recipient: Arc<dyn StreamRecipient>) {
        assert!(
            self.recipient.set(recipient).is_ok(),
            "stream recipient installed twice"
        );
    }
    fn register_owned(
        &self,
        stream: StreamInput<ErasedStream>,
        handler: StreamInput<Option<StreamHandlerRegistration>>,
        failures: InputFailures,
    ) -> ActorResult<()> {
        let recipient = self
            .recipient
            .get()
            .ok_or_else(|| {
                ActorError::InternalError("stream registry is not bound to an actor".into())
            })?
            .clone();
        if handler
            .value()
            .as_ref()
            .is_some_and(|handler| !handler.accepts_context(self.context_type))
        {
            return Err(ActorError::MessageHandlingError(
                "stream handler context type mismatch".into(),
            ));
        }
        let task = self
            .tasks
            .user_task(run_owned(stream, handler, recipient, failures.clone()))?
            .with_input_failures(failures);
        self.executor.submit(task)
    }
}
impl StreamRegistry for OwnedStreams {
    fn add_stream_erased(&mut self, stream: ErasedStream) -> ActorResult<()> {
        let failures = InputFailures::new();
        let result = self.register_owned(
            StreamInput::new(stream, &failures),
            StreamInput::new(None, &failures),
            failures.clone(),
        );
        registration_result(result, &failures)
    }
    fn add_stream_with_handler_erased(
        &mut self,
        stream: ErasedStream,
        handler: BoxedMessage,
    ) -> ActorResult<()> {
        let failures = InputFailures::new();
        let stream = StreamInput::new(stream, &failures);
        let handler = StreamInput::new(handler, &failures);
        let result = (move || {
            // Downcast failure returns ownership; never implicitly discard an
            // arbitrary handler while another user input can also unwind.
            let handler = match handler.take().downcast::<StreamHandlerRegistration>() {
                Ok(handler) => StreamInput::new(Some(*handler), &stream.failures),
                Err(handler) => {
                    drop(StreamInput::new(handler, &stream.failures));
                    return Err(ActorError::MessageHandlingError(
                        "erased stream handlers require StreamHandlerRegistration".into(),
                    ));
                }
            };
            let failures = stream.failures.clone();
            self.register_owned(stream, handler, failures)
        })();
        registration_result(result, &failures)
    }
}
fn registration_result(result: ActorResult<()>, failures: &InputFailures) -> ActorResult<()> {
    match failures.take_unadmitted() {
        Some(message) => {
            drop(StreamInput::new(result, failures));
            Err(ActorError::Panic(message))
        }
        None => result,
    }
}

/// Every independent input owns its own unwind boundary, including while the
/// stream's handler travels through an actor mailbox and a completion channel.
struct StreamInput<T> {
    context: Option<parrot_api::trace::TraceContext>,
    value: Option<T>,
    failures: InputFailures,
}
impl<T> StreamInput<T> {
    fn new(value: T, failures: &InputFailures) -> Self {
        Self {
            context: parrot_api::trace::TraceContext::current(),
            value: Some(value),
            failures: failures.clone(),
        }
    }
    fn value(&self) -> &T {
        self.value.as_ref().expect("live stream input")
    }
    fn value_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("live stream input")
    }
    fn take(mut self) -> T {
        self.value.take().expect("live stream input")
    }
    fn dispose(&mut self) -> Option<String> {
        parrot_api::trace::in_scope(self.context.clone(), || {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.value.take())))
            {
                Ok(()) => None,
                Err(payload) => {
                    let message = crate::owned_payload::panic_message(payload);
                    self.failures.record(message.clone());
                    Some(message)
                }
            }
        })
    }
}
impl<T> Drop for StreamInput<T> {
    fn drop(&mut self) {
        self.dispose();
    }
}

// Keep a user future outside its polling unwind boundary. Cancellation and a
// completed future's Drop have their own boundary and the same cleanup receipt.
struct StreamFuture<'a, 'r, T> {
    recording: Option<&'r mut Option<parrot_api::trace::SpanGuard>>,
    future: StreamInput<BoxedFuture<'a, ActorResult<T>>>,
    supervise_panic: bool,
}
impl<T> std::future::Future for StreamFuture<'_, '_, T> {
    type Output = ActorResult<T>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future.value_mut().as_mut().poll(cx)
        })) {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result.map_err(crate::error_receipt::protect_actor),
            Err(payload) => {
                let message = crate::owned_payload::panic_message(payload);
                self.future.dispose();
                self.finish_recording(parrot_api::trace::SpanStatus::Panicked);
                if self.supervise_panic {
                    std::panic::resume_unwind(Box::new(message));
                }
                Err(ActorError::Panic(message))
            }
        };
        let disposal = self.future.dispose();
        if disposal.is_some() {
            self.finish_recording(parrot_api::trace::SpanStatus::Panicked);
        }
        match disposal {
            Some(message) if result.is_ok() => {
                drop(StreamInput::new(result, &self.future.failures));
                Poll::Ready(Err(ActorError::Panic(message)))
            }
            _ => Poll::Ready(result),
        }
    }
}
impl<T> StreamFuture<'_, '_, T> {
    fn finish_recording(&mut self, status: parrot_api::trace::SpanStatus) {
        if let Some(recording) = self.recording.as_mut().and_then(|slot| slot.take()) {
            recording.finish(status);
        }
    }
}
impl<T> Drop for StreamFuture<'_, '_, T> {
    fn drop(&mut self) {
        if self.future.value.is_some() {
            let failed = self.future.dispose().is_some();
            self.finish_recording(if failed || std::thread::panicking() {
                parrot_api::trace::SpanStatus::Panicked
            } else {
                parrot_api::trace::SpanStatus::Cancelled
            });
        }
    }
}
async fn recorded_actor_stage<'a, T>(
    failures: &InputFailures,
    recording: parrot_api::trace::SpanGuard,
    construct: impl FnOnce() -> BoxedFuture<'a, ActorResult<T>>,
    finish: impl FnOnce(T) -> ActorResult<()>,
) -> ActorResult<()> {
    let mut recording = Some(recording);
    let result = isolated_stage_recorded(failures, construct, true, Some(&mut recording))
        .await
        .and_then(finish);
    if let Some(recording) = recording {
        recording.finish_result(&result);
    }
    result
}
async fn stage<'a, T>(
    failures: &InputFailures,
    construct: impl FnOnce() -> BoxedFuture<'a, ActorResult<T>>,
) -> ActorResult<T> {
    isolated_stage(failures, construct, false).await
}
async fn actor_stage<'a, T>(
    failures: &InputFailures,
    construct: impl FnOnce() -> BoxedFuture<'a, ActorResult<T>>,
) -> ActorResult<T> {
    isolated_stage(failures, construct, true).await
}
async fn isolated_stage<'a, T>(
    failures: &InputFailures,
    construct: impl FnOnce() -> BoxedFuture<'a, ActorResult<T>>,
    supervise_panic: bool,
) -> ActorResult<T> {
    isolated_stage_recorded(failures, construct, supervise_panic, None).await
}
async fn isolated_stage_recorded<'a, T>(
    failures: &InputFailures,
    construct: impl FnOnce() -> BoxedFuture<'a, ActorResult<T>>,
    supervise_panic: bool,
    recording: Option<&mut Option<parrot_api::trace::SpanGuard>>,
) -> ActorResult<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(construct)) {
        Ok(future) => {
            StreamFuture {
                recording,
                future: StreamInput::new(future, failures),
                supervise_panic,
            }
            .await
        }
        Err(payload) => {
            if let Some(recording) = recording.and_then(|slot| slot.take()) {
                recording.finish(parrot_api::trace::SpanStatus::Panicked);
            }
            let message = crate::owned_payload::panic_message(payload);
            if supervise_panic {
                std::panic::resume_unwind(Box::new(message));
            }
            Err(ActorError::Panic(message))
        }
    }
}
async fn callback(
    recipient: &dyn StreamRecipient,
    event: StreamCallback,
    handler: StreamInput<Option<StreamHandlerRegistration>>,
) -> ActorResult<StreamInput<Option<StreamHandlerRegistration>>> {
    let failures = handler.failures.clone();
    let (completion, finished) = oneshot::channel();
    let event = StreamEvent {
        parent: parrot_api::trace::TraceContext::current(),
        callback: StreamInput::new(event, &failures),
        handler,
        completion,
    };
    stage(&failures, || recipient.deliver(event)).await?;
    let reply = finished.await.map_err(|_| ActorError::Stopped)?;
    reply
        .result
        .take()
        .map_err(crate::error_receipt::protect_actor)?;
    Ok(reply.handler)
}
#[cfg(test)]
fn run(
    stream: ErasedStream,
    handler: Option<StreamHandlerRegistration>,
    recipient: Arc<dyn StreamRecipient>,
) -> impl std::future::Future<Output = ActorResult<()>> {
    let failures = InputFailures::new();
    run_owned(
        StreamInput::new(stream, &failures),
        StreamInput::new(handler, &failures),
        recipient,
        failures,
    )
}
async fn run_owned(
    stream: StreamInput<ErasedStream>,
    handler: StreamInput<Option<StreamHandlerRegistration>>,
    recipient: Arc<dyn StreamRecipient>,
    failures: InputFailures,
) -> ActorResult<()> {
    // Construct guards before entering any user future or awaiting callbacks.
    let mut handler = callback(&*recipient, StreamCallback::Started, handler).await?;
    let mut stream = StreamInput::new(Box::into_pin(stream.take()), &failures);
    loop {
        let next = std::panic::AssertUnwindSafe(stream.value_mut().next())
            .catch_unwind()
            .await;
        match next {
            Ok(Some(item)) => {
                handler = callback(&*recipient, StreamCallback::Item(item), handler).await?;
            }
            Ok(None) => {
                callback(&*recipient, StreamCallback::Finished, handler).await?;
                return Ok(());
            }
            Err(payload) => {
                let message = crate::owned_payload::panic_message(payload);
                callback(
                    &*recipient,
                    StreamCallback::Error(ActorError::Panic(message.clone())),
                    handler,
                )
                .await?;
                return Err(ActorError::Panic(message));
            }
        }
    }
}

struct CallbackCleanup {
    recording: Option<parrot_api::trace::SpanGuard>,
    first: Option<String>,
    failures: InputFailures,
}
impl CallbackCleanup {
    fn finish(&mut self, result: &ActorResult<()>) {
        if let Some(recording) = self.recording.take() {
            recording.finish(if self.first.is_some() {
                parrot_api::trace::SpanStatus::Panicked
            } else {
                parrot_api::trace::SpanStatus::from_result(result)
            });
        }
    }
}
impl Drop for CallbackCleanup {
    fn drop(&mut self) {
        if let Some(recording) = self.recording.take() {
            recording.finish(if self.first.is_some() || std::thread::panicking() {
                parrot_api::trace::SpanStatus::Panicked
            } else {
                parrot_api::trace::SpanStatus::Cancelled
            });
        }
        if let Some(message) = self.first.take() {
            self.failures.record(message);
        }
    }
}

pub(crate) fn dispatch<A>(
    mut event: StreamEvent,
    actor: &mut A,
    ctx: &mut A::Context,
) -> impl std::future::Future<Output = ActorResult<BoxedMessage>>
where
    A: Actor,
    A::Context: parrot_api::context::ActorContext + Sized + 'static,
{
    let parent = event.parent.clone();
    parrot_api::trace::scope(parent.as_ref().map(|parent| parent.child()), async move {
        if event.completion.is_closed() {
            return Ok(Box::new(()) as BoxedMessage);
        }
        let failures = event.handler.failures.clone();
        // Each input retains the context at its own ownership boundary;
        // a handler can cross several callback identities before final disposal.
        let work = async {
            if let Some(handler) = event.handler.value_mut().as_mut() {
                let recording = parrot_api::trace::SpanGuard::current(
                    callback_kind(event.callback.value()),
                    parrot_api::context::ActorContext::path(ctx).path(),
                );
                let mut view = StreamContextView(ctx);
                let mut cleanup = CallbackCleanup {
                    recording: Some(recording),
                    first: None,
                    failures: failures.clone(),
                };
                let result = actor_stage(&failures, || {
                    handler.call_with_cleanup(event.callback.take(), &mut view, &mut cleanup.first)
                })
                .await;
                cleanup.finish(&result);
                result
            } else {
                default_callback(actor, ctx, event.callback, &failures, parent).await
            }
        };
        // Stop closes the source worker's receipt. Release suspended callbacks
        // before actor retirement waits for their serial execution lock.
        let result = tokio::select! {
            biased;
            _ = event.completion.closed() => return Ok(Box::new(()) as BoxedMessage),
            result = work => result,
        };
        let _ = event.completion.send(StreamReply {
            handler: event.handler,
            result: StreamInput::new(result, &failures),
        });
        Ok(Box::new(()) as BoxedMessage)
    })
}
fn callback_kind(event: &StreamCallback) -> parrot_api::trace::SpanKind {
    use parrot_api::trace::SpanKind;
    match event {
        StreamCallback::Started => SpanKind::StreamStarted,
        StreamCallback::Item(_) => SpanKind::StreamItem,
        StreamCallback::Finished => SpanKind::StreamFinished,
        StreamCallback::Error(_) => SpanKind::StreamError,
    }
}
async fn default_callback<A>(
    actor: &mut A,
    ctx: &mut A::Context,
    event: StreamInput<StreamCallback>,
    failures: &InputFailures,
    parent: Option<parrot_api::trace::TraceContext>,
) -> ActorResult<()>
where
    A: Actor,
    A::Context: parrot_api::context::ActorContext,
{
    let recording = parrot_api::trace::SpanGuard::current(
        callback_kind(event.value()),
        parrot_api::context::ActorContext::path(ctx).path(),
    );
    let result = match event.take() {
        StreamCallback::Started => {
            recorded_actor_stage(failures, recording, || actor.stream_started(ctx), Ok).await
        }
        StreamCallback::Item(item) => {
            recorded_actor_stage(
                failures,
                recording,
                || actor.handle_stream(item, ctx),
                |reply| {
                    let mut reply = StreamInput::new(reply, failures);
                    reply
                        .dispose()
                        .map_or(Ok(()), |message| Err(ActorError::Panic(message)))
                },
            )
            .await
        }
        StreamCallback::Finished => {
            recorded_actor_stage(failures, recording, || actor.stream_finished(ctx), Ok).await
        }
        StreamCallback::Error(error) => {
            return recorded_actor_stage(
                failures,
                recording,
                || actor.stream_error(error, ctx),
                Ok,
            )
            .await;
        }
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            parrot_api::trace::scope(parent.map(|parent| parent.child()), async {
                let recording = parrot_api::trace::SpanGuard::current(
                    parrot_api::trace::SpanKind::StreamError,
                    parrot_api::context::ActorContext::path(ctx).path(),
                );
                recorded_actor_stage(failures, recording, || actor.stream_error(error, ctx), Ok)
                    .await
            })
            .await
        }
    }
}

#[cfg(test)]
mod panic_contracts {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind, panic_any};
    use std::{
        pin::Pin,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("recursive stream secondary");
        }
    }
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic_any(Secondary);
        }
    }
    struct Source {
        drops: Arc<AtomicUsize>,
        payload_drops: Arc<AtomicUsize>,
        recursive: bool,
    }
    impl Stream for Source {
        type Item = BoxedMessage;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if self.recursive {
                panic_any(Payload(self.payload_drops.clone()));
            }
            panic!("stream poll failure");
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[derive(Clone, Copy)]
    enum Reply {
        Accept,
        Reject,
        Pending,
    }
    struct Recipient {
        observations: Mutex<Vec<(String, usize)>>,
        payload_drops: Arc<AtomicUsize>,
        reply: Reply,
    }
    impl StreamRecipient for Recipient {
        fn deliver(&self, event: StreamEvent) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                let error = matches!(event.callback.value(), StreamCallback::Error(_));
                let label = match event.callback.value() {
                    StreamCallback::Started => "started".to_owned(),
                    StreamCallback::Finished => "finished".to_owned(),
                    StreamCallback::Item(_) => "item".to_owned(),
                    StreamCallback::Error(e) => e.to_string(),
                };
                self.observations
                    .lock()
                    .unwrap()
                    .push((label, self.payload_drops.load(Ordering::SeqCst)));
                if error {
                    match self.reply {
                        Reply::Reject => return Err(ActorError::Stopped),
                        Reply::Pending => futures::future::pending::<()>().await,
                        Reply::Accept => {}
                    }
                }
                let _ = event.completion.send(StreamReply {
                    result: StreamInput::new(Ok(()), &event.handler.failures),
                    handler: event.handler,
                });
                Ok(())
            })
        }
    }
    fn no_unwind<T>(result: std::thread::Result<T>) -> T {
        match result {
            Ok(value) => value,
            Err(payload) => {
                std::mem::forget(payload);
                panic!("stream panic payload escaped its owner");
            }
        }
    }
    async fn terminal(recursive: bool, reply: Reply) {
        let drops = Arc::new(AtomicUsize::new(0));
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let recipient = Arc::new(Recipient {
            observations: Mutex::new(Vec::new()),
            payload_drops: payload_drops.clone(),
            reply,
        });
        let source = Source {
            drops: drops.clone(),
            payload_drops: payload_drops.clone(),
            recursive,
        };
        let result = no_unwind(
            AssertUnwindSafe(run(Box::new(source), None, recipient.clone()))
                .catch_unwind()
                .await,
        );
        match reply {
            Reply::Reject => assert!(matches!(result, Err(ActorError::Stopped))),
            Reply::Accept => assert!(matches!(result, Err(ActorError::Panic(_)))),
            Reply::Pending => unreachable!(),
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let observations = recipient.observations.lock().unwrap();
        assert_eq!(
            observations.len(),
            2,
            "no false successful completion or item"
        );
        assert_eq!(observations[0], ("started".into(), 0));
        if recursive {
            assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
            assert_eq!(
                observations[1].1, 1,
                "payload retired before callback waits"
            );
            assert!(
                observations[1]
                    .0
                    .contains("panic payload destructor panicked")
            );
        } else {
            assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
            assert!(observations[1].0.contains("stream poll failure"));
        }
    }
    #[tokio::test]
    async fn recursive_poll_error_reaches_callback_after_payload_retirement() {
        terminal(true, Reply::Accept).await;
    }
    #[tokio::test]
    async fn recursive_poll_error_cannot_unwind_rejected_callback() {
        terminal(true, Reply::Reject).await;
    }
    #[tokio::test]
    async fn recursive_poll_payload_not_retained_across_pending_error_callback() {
        let drops = Arc::new(AtomicUsize::new(0));
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let recipient = Arc::new(Recipient {
            observations: Mutex::new(Vec::new()),
            payload_drops: payload_drops.clone(),
            reply: Reply::Pending,
        });
        let source = Source {
            drops: drops.clone(),
            payload_drops: payload_drops.clone(),
            recursive: true,
        };
        let mut task = Box::pin(run(Box::new(source), None, recipient.clone()));
        assert!(futures::poll!(&mut task).is_pending());
        // Always dispose the future before asserting baseline observations so a
        // failing assertion cannot start a double panic during future cleanup.
        let before_cancel = payload_drops.load(Ordering::SeqCst);
        no_unwind(catch_unwind(AssertUnwindSafe(|| drop(task))));
        assert_eq!(before_cancel, 1);
        assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(recipient.observations.lock().unwrap().len(), 2);
    }
    #[tokio::test]
    async fn plain_poll_error_preserves_message_and_callback_order() {
        terminal(false, Reply::Accept).await;
    }
}

#[cfg(test)]
mod registration_input_contracts {
    use super::*;
    use std::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
        time::{Duration, Instant},
    };
    #[derive(Default)]
    struct Counts {
        sources: AtomicUsize,
        handlers: AtomicUsize,
        polls: AtomicUsize,
        callbacks: AtomicUsize,
    }
    struct Source(Arc<Counts>, bool);
    impl Stream for Source {
        type Item = BoxedMessage;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.polls.fetch_add(1, Ordering::SeqCst);
            Poll::Pending
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            assert!(!self.1, "stream source input drop");
        }
    }
    struct Handler(Arc<Counts>, bool);
    impl Drop for Handler {
        fn drop(&mut self) {
            self.0.handlers.fetch_add(1, Ordering::SeqCst);
            assert!(!self.1, "stream handler input drop");
        }
    }
    #[async_trait::async_trait]
    impl parrot_api::stream::StreamHandler<Source, ()> for Handler {
        async fn handle(&mut self, _: BoxedMessage, _: &mut ()) {
            self.0.callbacks.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Recipient(Arc<Counts>);
    impl StreamRecipient for Recipient {
        fn deliver(&self, _: StreamEvent) -> BoxedFuture<'_, ActorResult<()>> {
            self.0.callbacks.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }
    fn isolated(name: &str, body: impl FnOnce()) {
        const FLAG: &str = "PARROT_STREAM_INPUT_REGRESSION";
        if std::env::var(FLAG).as_deref() == Ok(name) {
            body();
            return;
        }
        let full = format!("streams::registration_input_contracts::{name}");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &full, "--nocapture"])
            .env(FLAG, name)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{name}: {status}");
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{name}: deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn check(case: &str, source_panics: bool, handler_panics: bool) {
        let counts = Arc::new(Counts::default());
        let tasks = ContextTasks::new(usize::from(case != "quota"));
        if case == "closed" {
            tasks.close();
        }
        let mut registry = if case == "context" {
            OwnedStreams::new::<u8>(tasks.clone(), StreamExecutor::Actix(None))
        } else {
            OwnedStreams::new::<()>(tasks.clone(), StreamExecutor::Actix(None))
        };
        if !matches!(case, "unbound" | "raw") {
            registry.bind(Arc::new(Recipient(counts.clone())));
        }
        let source: ErasedStream = Box::new(Source(counts.clone(), source_panics));
        let handler = Handler(counts.clone(), handler_panics);
        let handler: BoxedMessage = if case == "raw" {
            Box::new(handler)
        } else {
            Box::new(StreamHandlerRegistration::new::<Source, Handler, ()>(
                handler,
            ))
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.add_stream_with_handler_erased(source, handler)
        }));
        let result = result.unwrap_or_else(|payload| {
            panic!(
                "stream rejection unwound: {}",
                crate::owned_payload::panic_message(payload)
            );
        });
        let error = result.expect_err("invalid registration was accepted");
        assert_eq!(counts.sources.load(Ordering::SeqCst), 1);
        assert_eq!(counts.handlers.load(Ordering::SeqCst), 1);
        assert_eq!(counts.polls.load(Ordering::SeqCst), 0);
        assert_eq!(counts.callbacks.load(Ordering::SeqCst), 0);
        let joined = futures::executor::block_on(tasks.join());
        if source_panics || handler_panics {
            if case == "executor" {
                assert!(joined.unwrap_err().to_string().contains("input drop"));
            } else {
                assert!(error.to_string().contains("input drop"), "{error}");
                joined.unwrap(); // Rejected inputs never acquired a task ticket.
            }
        } else {
            joined.unwrap();
        }
    }
    macro_rules! regression {
        ($name:ident, $case:literal) => {
            #[test]
            fn $name() {
                isolated(stringify!($name), || check($case, true, true));
            }
        };
    }
    regression!(raw_handler_rejection_retires_both_panicking_inputs, "raw");
    regression!(unbound_rejection_retires_both_panicking_inputs, "unbound");
    regression!(context_mismatch_retires_both_panicking_inputs, "context");
    regression!(quota_rejection_retires_both_panicking_inputs, "quota");
    regression!(closed_generation_retires_both_panicking_inputs, "closed");
    regression!(executor_rejection_retires_both_panicking_inputs, "executor");
    #[test]
    fn single_source_disposal_failure_is_returned_without_unwind() {
        isolated(
            "single_source_disposal_failure_is_returned_without_unwind",
            || {
                check("unbound", true, false);
            },
        );
    }
    #[test]
    fn healthy_rejections_retire_each_input_without_poll_or_callback() {
        for case in ["raw", "unbound", "context", "quota", "closed", "executor"] {
            check(case, false, false);
        }
    }
}

#[cfg(test)]
mod accepted_input_contracts {
    use super::*;
    use std::{
        pin::Pin,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };
    struct Source(Arc<AtomicUsize>);
    impl Stream for Source {
        type Item = BoxedMessage;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("accepted source drop");
        }
    }
    struct Handler(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl parrot_api::stream::StreamHandler<Source, ()> for Handler {
        async fn handle(&mut self, _: BoxedMessage, _: &mut ()) {}
    }
    impl Drop for Handler {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("accepted handler drop");
        }
    }
    struct Recipient(Mutex<Option<StreamEvent>>);
    impl StreamRecipient for Recipient {
        fn deliver(&self, event: StreamEvent) -> BoxedFuture<'_, ActorResult<()>> {
            *self.0.lock().unwrap() = Some(event);
            Box::pin(async { Ok(()) })
        }
    }
    async fn abandoned(poll: bool, late_handler: bool) {
        let tasks = ContextTasks::new(1);
        let failures = InputFailures::new();
        let sources = Arc::new(AtomicUsize::new(0));
        let handlers = Arc::new(AtomicUsize::new(0));
        let recipient = Arc::new(Recipient(Mutex::new(None)));
        let source: ErasedStream = Box::new(Source(sources.clone()));
        let handler =
            StreamHandlerRegistration::new::<Source, Handler, ()>(Handler(handlers.clone()));
        let mut task = Box::pin(
            tasks
                .user_task(run_owned(
                    StreamInput::new(source, &failures),
                    StreamInput::new(Some(handler), &failures),
                    recipient.clone(),
                    failures.clone(),
                ))
                .unwrap()
                .with_input_failures(failures),
        );
        if poll {
            assert!(futures::poll!(&mut task).is_pending());
        }
        tasks.close();
        assert!(futures::poll!(&mut task).is_ready());
        drop(task);
        if late_handler {
            assert_eq!(handlers.load(Ordering::SeqCst), 0);
            assert!(
                tasks
                    .join()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("accepted source drop")
            );
        }
        drop(recipient.0.lock().unwrap().take());
        assert_eq!(sources.load(Ordering::SeqCst), 1);
        assert_eq!(handlers.load(Ordering::SeqCst), 1);
        let error = tasks.join().await.unwrap_err().to_string();
        assert!(error.contains("accepted source drop") || error.contains("accepted handler drop"));
    }
    #[tokio::test]
    async fn accepted_unpolled_stream_retires_independent_inputs() {
        abandoned(false, false).await;
    }
    #[tokio::test]
    async fn accepted_waiter_cancellation_retires_source_and_transferred_handler() {
        abandoned(true, true).await;
    }
    #[tokio::test]
    async fn successful_admission_cannot_steal_concurrent_worker_cleanup_error() {
        let tasks = ContextTasks::new(1);
        let failures = InputFailures::new();
        let task = tasks
            .user_task(async { Ok(()) })
            .unwrap()
            .with_input_failures(failures.clone());
        failures.record("concurrent worker cleanup".into());
        registration_result(Ok(()), &failures).unwrap();
        drop(task);
        assert!(
            tasks
                .join()
                .await
                .unwrap_err()
                .to_string()
                .contains("concurrent worker cleanup")
        );
    }
    #[tokio::test]
    async fn late_callback_cleanup_is_not_lost_after_worker_receipt() {
        let tasks = ContextTasks::new(1);
        let failures = InputFailures::new();
        let task = tasks
            .user_task(async { Ok(()) })
            .unwrap()
            .with_input_failures(failures.clone());
        drop(task);
        tasks.join().await.unwrap();
        failures.record("late callback destruction".into());
        assert!(
            tasks
                .join()
                .await
                .unwrap_err()
                .to_string()
                .contains("late callback destruction")
        );
    }
    #[tokio::test]
    async fn late_cleanup_preserves_first_generation_failure() {
        let tasks = ContextTasks::new(1);
        let failures = InputFailures::new();
        let mut task = Box::pin(
            tasks
                .user_task(async { panic!("primary worker failure") })
                .unwrap()
                .with_input_failures(failures.clone()),
        );
        assert!(futures::poll!(&mut task).is_ready());
        failures.record("later callback disposal".into());
        assert!(
            tasks
                .join()
                .await
                .unwrap_err()
                .to_string()
                .contains("primary worker failure")
        );
    }
    struct BadError(Arc<AtomicUsize>);
    impl std::fmt::Debug for BadError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error")
        }
    }
    impl std::fmt::Display for BadError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error")
        }
    }
    impl std::error::Error for BadError {}
    impl Drop for BadError {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("unread stream error");
        }
    }
    #[test]
    fn unread_callback_reply_disposes_error_and_handler_independently() {
        let failures = InputFailures::new();
        let handlers = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(AtomicUsize::new(0));
        let handler =
            StreamHandlerRegistration::new::<Source, Handler, ()>(Handler(handlers.clone()));
        let reply = StreamReply {
            handler: StreamInput::new(Some(handler), &failures),
            result: StreamInput::new(
                Err(ActorError::Other(anyhow::Error::new(BadError(
                    errors.clone(),
                )))),
                &failures,
            ),
        };
        drop(reply);
        assert_eq!(handlers.load(Ordering::SeqCst), 1);
        assert_eq!(errors.load(Ordering::SeqCst), 1);
        assert!(failures.take().is_some());
    }
    struct ProbeFuture {
        drops: Arc<AtomicUsize>,
        panic_poll: bool,
        ready: bool,
    }
    impl std::future::Future for ProbeFuture {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            assert!(!self.panic_poll, "primary callback poll");
            if self.ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }
    impl Drop for ProbeFuture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            panic!("callback future disposal");
        }
    }
    async fn future(panic_poll: bool, ready: bool) {
        let failures = InputFailures::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut future = Box::pin(stage(&failures, || {
            Box::pin(ProbeFuture {
                drops: drops.clone(),
                panic_poll,
                ready,
            })
        }));
        let polled = futures::poll!(&mut future);
        if panic_poll || ready {
            let Poll::Ready(Err(error)) = polled else {
                panic!("callback did not fail");
            };
            assert!(error.to_string().contains(if panic_poll {
                "primary callback poll"
            } else {
                "callback future disposal"
            }));
        } else {
            assert!(polled.is_pending());
        }
        drop(future);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            failures
                .take()
                .unwrap()
                .contains("callback future disposal")
        );
    }
    #[tokio::test]
    async fn callback_poll_and_drop_failures_are_independent() {
        future(true, false).await;
    }
    #[tokio::test]
    async fn successful_callback_with_failed_disposal_cannot_acknowledge_success() {
        future(false, true).await;
    }
    #[tokio::test]
    async fn cancelled_callback_preserves_future_disposal_failure() {
        future(false, false).await;
    }
}
