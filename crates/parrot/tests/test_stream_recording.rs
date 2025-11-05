//! Actual stream invocations must produce independent terminal records.
use futures::{FutureExt, Stream};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::ActorContext,
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalStreamContext, LocalTypedDispatch},
    message::Message,
    stream::{StreamHandler, StreamRegistryExt},
    trace::{self, SpanStatus, TraceContext, TraceRecorder},
    typed::ParrotMsgVariant,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug)]
struct Event {
    stage: &'static str,
    phase: &'static str,
    context: Option<TraceContext>,
}
type Events = flume::Sender<Event>;
fn record(events: &Events, stage: &'static str, phase: &'static str) {
    let _ = events.send(Event {
        stage,
        phase,
        context: TraceContext::current(),
    });
}
struct Callback {
    events: Events,
    stage: &'static str,
    error: bool,
    pending: bool,
    polled: bool,
}
impl Future for Callback {
    type Output = ActorResult<()>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.polled {
            record(&self.events, self.stage, "poll");
            self.polled = true;
        }
        if self.pending {
            Poll::Pending
        } else if self.error {
            Poll::Ready(Err(ActorError::MessageHandlingError(
                "callback failure".into(),
            )))
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
impl Drop for Callback {
    fn drop(&mut self) {
        record(&self.events, self.stage, "drop");
    }
}
fn callback(events: &Events, stage: &'static str) -> Callback {
    record(events, stage, "construct");
    Callback {
        events: events.clone(),
        stage,
        error: false,
        pending: false,
        polled: false,
    }
}
struct Source {
    events: Events,
    next: u32,
    fail: bool,
}
impl Stream for Source {
    type Item = u32;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<u32>> {
        record(&self.events, "source", "poll");
        if self.next == 2 {
            assert!(!self.fail, "trace source failed");
            return Poll::Ready(None);
        }
        self.next += 1;
        Poll::Ready(Some(self.next))
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        record(&self.events, "source", "drop");
    }
}
struct Custom(Events, bool);
impl Drop for Custom {
    fn drop(&mut self) {
        record(&self.0, "handler", "drop");
    }
}
#[async_trait::async_trait]
impl<C: ActorContext + ?Sized> StreamHandler<Source, C> for Custom {
    async fn started(&mut self, _: &mut C) {
        let mut future = callback(&self.0, "start");
        future.pending = self.1;
        future.await.unwrap();
    }
    async fn handle(&mut self, _: u32, _: &mut C) {
        callback(&self.0, "item").await.unwrap();
    }
    async fn finished(&mut self, _: &mut C) {
        callback(&self.0, "finish").await.unwrap();
    }
    async fn handle_error(&mut self, _: ActorError, _: &mut C) {
        callback(&self.0, "error").await.unwrap();
    }
}
struct Register {
    traced: bool,
    custom: bool,
    fail: bool,
    callback_error: bool,
    pending: bool,
}
struct ThreadProbe(Events, bool, bool);
struct ActixProbe(Events, bool, bool);
impl ActixEngineExt for ActixProbe {}
macro_rules! actor {
    ($name:ident, $context:ty) => {
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                if msg.is::<Register>() {
                    let request = msg.downcast::<Register>().unwrap();
                    self.1 = request.callback_error;
                    self.2 = request.pending;
                    let parent = request.traced.then(TraceContext::current).flatten();
                    let result = trace::in_scope(parent.clone(), || {
                        let source = Source {
                            events: self.0.clone(),
                            next: 0,
                            fail: request.fail,
                        };
                        if request.custom {
                            ctx.stream_registry()
                                .add_stream_with_handler::<_, _, Self::Context>(
                                    source,
                                    Custom(self.0.clone(), request.pending),
                                )
                        } else {
                            ctx.stream_registry().add_stream(source)
                        }
                    });
                    Box::pin(async move {
                        result?;
                        Ok(Box::new(parent) as BoxedMessage)
                    })
                } else {
                    let mut future = callback(&self.0, "item");
                    future.error = self.1 && *msg.downcast::<u32>().unwrap() == 2;
                    Box::pin(future.map(|r| r.map(|()| Box::new(()) as BoxedMessage)))
                }
            }
            fn stream_started<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                let mut future = callback(&self.0, "start");
                future.pending = self.2;
                Box::pin(future)
            }
            fn stream_finished<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(callback(&self.0, "finish"))
            }
            fn stream_error<'a>(
                &'a mut self,
                _: ActorError,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(callback(&self.0, "error"))
            }
        }
    };
}
actor!(ThreadProbe, parrot::thread::ThreadContext<Self>);
actor!(
    ActixProbe,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("stream trace deadline")
}
async fn collect(receiver: &flume::Receiver<Event>) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let event = bounded(receiver.recv_async()).await.unwrap();
        let done = event.stage == "source" && event.phase == "drop";
        events.push(event);
        if done {
            return events;
        }
    }
}
fn check(
    events: &[Event],
    parent: Option<TraceContext>,
    stages: &[&str],
    polls: usize,
    custom: bool,
) {
    let source: Vec<_> = events.iter().filter(|e| e.stage == "source").collect();
    let mut expected_source = vec!["poll"; polls];
    expected_source.push("drop");
    assert_eq!(
        source.iter().map(|e| e.phase).collect::<Vec<_>>(),
        expected_source
    );
    for e in source {
        assert_eq!(e.context, parent, "source {}", e.phase);
    }
    let callbacks: Vec<_> = events
        .iter()
        .filter(|e| e.stage != "source" && e.stage != "handler")
        .collect();
    assert_eq!(callbacks.len(), stages.len() * 3);
    let handlers: Vec<_> = events.iter().filter(|e| e.stage == "handler").collect();
    assert_eq!(handlers.len(), usize::from(custom));
    for handler in handlers {
        assert_eq!(handler.phase, "drop");
        assert_eq!(
            handler.context, parent,
            "custom handler cleanup lost registration identity"
        );
    }
    let mut identities = Vec::new();
    for (group, stage) in callbacks.as_chunks::<3>().0.iter().zip(stages) {
        assert!(group.iter().all(|e| e.stage == *stage));
        assert_eq!(
            group.iter().map(|e| e.phase).collect::<Vec<_>>(),
            ["construct", "poll", "drop"]
        );
        let context = group[0].context.clone();
        for e in group {
            assert_eq!(
                e.context, context,
                "callback identity changed during {}",
                e.phase
            );
        }
        if let Some(parent) = &parent {
            let child = context.expect("stream callback lost its registration parent");
            assert_eq!(child.trace_id(), parent.trace_id());
            assert_eq!(child.parent_span_id(), Some(parent.span_id()));
            assert_ne!(child.span_id(), parent.span_id());
            assert!(
                !identities.contains(&child.span_id()),
                "callbacks reused an identity"
            );
            identities.push(child.span_id());
        } else {
            assert!(
                context.is_none(),
                "untraced callback inherited executor identity"
            );
        }
    }
}
fn dynamic(use_actix: bool, custom: bool, fail: bool) {
    dynamic_full(use_actix, custom, fail, false, false);
}
fn dynamic_full(use_actix: bool, custom: bool, fail: bool, callback_error: bool, pending: bool) {
    actix::System::new().block_on(async {
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let system = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let mut results = Vec::new();
        for traced in [true, false] {
            let (sender, receiver) = flume::unbounded();
            let actor: BoxedActorRef = if use_actix {
                bounded(system.spawn_root_typed(ActixProbe(sender, false, false), EmptyConfig))
                    .await
                    .unwrap()
                    .clone_boxed()
            } else {
                bounded(
                    thread.spawn_root_typed_thread(ThreadProbe(sender, false, false), EmptyConfig),
                )
                .await
                .unwrap()
                .clone_boxed()
            };
            let recorder = TraceRecorder::new(128).unwrap();
            let root = recorder.root();
            let future = root.in_scope(|| {
                actor.send(Box::new(Register {
                    traced,
                    custom,
                    fail,
                    callback_error,
                    pending,
                }))
            });
            let parent = *bounded(future)
                .await
                .unwrap()
                .downcast::<Option<TraceContext>>()
                .unwrap();
            let mut events = if pending {
                vec![
                    bounded(receiver.recv_async()).await.unwrap(),
                    bounded(receiver.recv_async()).await.unwrap(),
                ]
            } else {
                collect(&receiver).await
            };
            bounded(actor.stop()).await.unwrap();
            events.extend(receiver.try_iter());
            results.push((parent, events, recorder, actor.path().to_owned()));
        }
        bounded(thread.shutdown_internal()).await.unwrap();
        bounded(system.as_ref().clone().shutdown()).await.unwrap();
        for (parent, events, recorder, target) in results {
            check_records(
                &events,
                parent.as_ref(),
                &recorder,
                &target,
                callback_error,
                pending,
                true,
            );
            let stages = if pending {
                vec!["start"]
            } else if callback_error {
                vec!["start", "item", "item", "error", "finish"]
            } else {
                vec![
                    "start",
                    "item",
                    "item",
                    if fail { "error" } else { "finish" },
                ]
            };
            check(
                &events,
                parent,
                &stages,
                if pending { 0 } else { 3 },
                custom,
            );
        }
    });
}
#[test]
fn thread_default_stream_callbacks_keep_parent() {
    dynamic(false, false, false);
}
#[test]
fn actix_default_stream_callbacks_keep_parent() {
    dynamic(true, false, false);
}
#[test]
fn thread_custom_stream_callbacks_keep_parent() {
    dynamic(false, true, false);
}
#[test]
fn actix_custom_stream_callbacks_keep_parent() {
    dynamic(true, true, false);
}
#[test]
fn thread_default_stream_error_keeps_parent() {
    dynamic(false, false, true);
}
#[test]
fn actix_default_stream_error_keeps_parent() {
    dynamic(true, false, true);
}
#[test]
fn thread_custom_stream_error_keeps_parent() {
    dynamic(false, true, true);
}
#[test]
fn actix_custom_stream_error_keeps_parent() {
    dynamic(true, true, true);
}

struct LocalSource {
    source: Source,
    _local: Rc<()>,
}
impl Stream for LocalSource {
    type Item = Rc<u32>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.source)
            .poll_next(cx)
            .map(|item| item.map(Rc::new))
    }
}
struct LocalProbe {
    events: Events,
    parent: Option<TraceContext>,
    fail: bool,
    callback_error: bool,
    pending: bool,
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl LocalTypedDispatch for LocalProbe {
    type Msg = Ping;
    type Reply = ();
    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        trace::in_scope(self.parent.clone(), || {
            streams
                .add_stream(LocalSource {
                    source: Source {
                        events: self.events.clone(),
                        next: 0,
                        fail: self.fail,
                    },
                    _local: Rc::new(()),
                })
                .unwrap()
        });
        Box::pin(async { Ok(()) })
    }
    fn dispatch<'a>(
        &'a mut self,
        _: Ping,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_started<'a>(
        &'a mut self,
        _: u64,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let mut future = callback(&self.events, "start");
        future.pending = self.pending;
        Box::pin(future)
    }
    fn local_stream_item<'a>(
        &'a mut self,
        _: u64,
        item: Box<dyn std::any::Any>,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let mut future = callback(&self.events, "item");
        future.error = self.callback_error && **item.downcast::<Rc<u32>>().unwrap() == 2;
        Box::pin(future)
    }
    fn local_stream_finished<'a>(
        &'a mut self,
        _: u64,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(callback(&self.events, "finish"))
    }
    fn local_stream_error<'a>(
        &'a mut self,
        _: u64,
        _: ActorError,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(callback(&self.events, "error"))
    }
}
impl ParrotMsgVariant<LocalProtocol<LocalProbe>> for Ping {
    fn inject(message: Self) -> Ping {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
fn local(fail: bool) {
    local_full(fail, false, false);
}
fn local_full(fail: bool, callback_error: bool, pending: bool) {
    actix::System::new().block_on(async {
        let mut results = Vec::new();
        for traced in [true, false] {
            let system = Arc::new(
                parrot::actix::ActixActorSystem::with_arbiter_count(1)
                    .await
                    .unwrap(),
            );
            let (events, receiver) = flume::unbounded();
            let recorder = TraceRecorder::new(128).unwrap();
            let parent = traced.then(|| recorder.root());
            let registration = parent.clone();
            let actor = bounded(system.spawn_local_typed::<LocalProbe, Ping, _>(
                move || LocalProbe {
                    events: events.clone(),
                    parent: registration.clone(),
                    fail,
                    callback_error,
                    pending,
                },
                "/local/trace",
            ))
            .await
            .unwrap();
            let mut events = if pending {
                vec![
                    bounded(receiver.recv_async()).await.unwrap(),
                    bounded(receiver.recv_async()).await.unwrap(),
                ]
            } else {
                collect(&receiver).await
            };
            if fail || callback_error {
                // An actor may finish and deregister before system shutdown starts.
                // Its own durable stop receipt must retain the failure in that order.
                bounded(async {
                    while system.status().active_actors != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                for _ in 0..2 {
                    let error = bounded(actor.stop()).await.unwrap_err();
                    if callback_error {
                        assert!(
                            matches!(error, ActorError::MessageHandlingError(ref reason)
                            if reason == "callback failure"),
                            "{error:?}"
                        );
                    } else {
                        assert!(
                            matches!(error, ActorError::Panic(ref reason)
                            if reason.contains("trace source failed")),
                            "{error:?}"
                        );
                    }
                }
            } else {
                bounded(actor.stop()).await.unwrap();
            }
            bounded(system.as_ref().clone().shutdown()).await.unwrap();
            assert_eq!(system.status().active_actors, 0);
            assert_eq!(system.status().resources.thread_count, 0);
            events.extend(receiver.try_iter());
            results.push((parent, events, recorder, actor.path().to_owned()));
        }
        for (parent, events, recorder, target) in results {
            check_records(
                &events,
                parent.as_ref(),
                &recorder,
                &target,
                callback_error,
                pending,
                false,
            );
            let stages = if pending {
                vec!["start"]
            } else {
                vec![
                    "start",
                    "item",
                    "item",
                    if fail || callback_error {
                        "error"
                    } else {
                        "finish"
                    },
                ]
            };
            check(
                &events,
                parent,
                &stages,
                if pending {
                    0
                } else if callback_error {
                    2
                } else {
                    3
                },
                false,
            );
        }
    });
}
#[test]
fn local_stream_source_and_callbacks_keep_parent() {
    local(false);
}
#[test]
fn local_stream_source_error_keeps_parent() {
    local(true);
}

#[test]
fn thread_error_hook_has_distinct_sibling_identity() {
    dynamic_full(false, false, false, true, false);
}
#[test]
fn actix_error_hook_has_distinct_sibling_identity() {
    dynamic_full(true, false, false, true, false);
}
#[test]
fn local_error_hook_has_distinct_sibling_identity() {
    local_full(false, true, false);
}
#[test]
fn thread_cancelled_callback_cleanup_keeps_identity() {
    dynamic_full(false, false, false, false, true);
}
#[test]
fn actix_cancelled_callback_cleanup_keeps_identity() {
    dynamic_full(true, false, false, false, true);
}
#[test]
fn thread_cancelled_custom_callback_cleanup_keeps_identity() {
    dynamic_full(false, true, false, false, true);
}
#[test]
fn actix_cancelled_custom_callback_cleanup_keeps_identity() {
    dynamic_full(true, true, false, false, true);
}
#[test]
fn local_cancelled_callback_cleanup_keeps_identity() {
    local_full(false, false, true);
}

fn check_records(
    events: &[Event],
    parent: Option<&TraceContext>,
    recorder: &TraceRecorder,
    target: &str,
    callback_error: bool,
    pending: bool,
    dynamic: bool,
) {
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.in_flight, 0);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(snapshot.counts.completed as usize, snapshot.records.len());
    let callbacks: Vec<_> = events
        .iter()
        .filter(|e| e.phase == "construct" && e.stage != "source" && e.stage != "handler")
        .collect();
    let records: Vec<_> = snapshot
        .records
        .iter()
        .filter(|r| format!("{:?}", r.kind).starts_with("Stream"))
        .collect();
    assert_eq!(
        records.len(),
        if parent.is_some() { callbacks.len() } else { 0 },
        "actual callback records missing: {snapshot:?}"
    );
    let lifetimes: Vec<_> = snapshot
        .records
        .iter()
        .filter(|r| r.kind == parrot_api::trace::SpanKind::ManagedTask)
        .collect();
    assert_eq!(lifetimes.len(), usize::from(parent.is_some()));
    if let Some(lifetime) = lifetimes.first() {
        let parent = parent.unwrap();
        assert_eq!(lifetime.parent_span_id, Some(parent.span_id()));
        assert_eq!(lifetime.trace_id, parent.trace_id());
        assert_ne!(lifetime.span_id, parent.span_id());
        assert_eq!(
            lifetime.target,
            if dynamic {
                "context.task"
            } else {
                "context.stream.local"
            }
        );
        let source_failed = !callback_error && callbacks.iter().any(|event| event.stage == "error");
        assert_eq!(
            lifetime.status,
            if pending {
                SpanStatus::Cancelled
            } else if source_failed {
                SpanStatus::Panicked
            } else if !dynamic && callback_error {
                SpanStatus::Error
            } else {
                SpanStatus::Ok
            }
        );
        assert!(
            records
                .iter()
                .all(|callback| callback.span_id != lifetime.span_id)
        );
    }
    assert_eq!(
        snapshot.records.len(),
        records.len() + usize::from(dynamic) + lifetimes.len(),
        "only actual callbacks, registration handler and admitted lifetime are recorded"
    );
    let Some(parent) = parent else {
        return;
    };
    for (index, event) in callbacks.iter().enumerate() {
        let context = event.context.as_ref().unwrap();
        let matches: Vec<_> = records
            .iter()
            .filter(|r| r.span_id == context.span_id() && r.trace_id == context.trace_id())
            .collect();
        assert_eq!(matches.len(), 1, "each callback must finish exactly once");
        let record = matches[0];
        let kind = match event.stage {
            "start" => "StreamStarted",
            "item" => "StreamItem",
            "finish" => "StreamFinished",
            "error" => "StreamError",
            _ => unreachable!(),
        };
        assert_eq!(format!("{:?}", record.kind), kind);
        assert_eq!(record.parent_span_id, Some(parent.span_id()));
        assert_eq!(record.target, target);
        let expected = if pending {
            SpanStatus::Cancelled
        } else if callback_error && index == 2 {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        };
        assert_eq!(record.status, expected, "{event:?}");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultMode {
    ReplyDrop,
    Construct,
    Poll,
    ReadyDrop,
    ErrorDrop,
    CancelDrop,
}
struct FaultFuture {
    events: Events,
    mode: FaultMode,
    polled: bool,
}
fn fault_callback(events: &Events, mode: FaultMode) -> FaultFuture {
    record(events, "fault", "construct");
    assert_ne!(mode, FaultMode::Construct, "stream constructor panic");
    FaultFuture {
        events: events.clone(),
        mode,
        polled: false,
    }
}
impl Future for FaultFuture {
    type Output = ActorResult<()>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.polled {
            record(&self.events, "fault", "poll");
            self.polled = true;
        }
        match self.mode {
            FaultMode::Poll => panic!("stream poll panic"),
            FaultMode::CancelDrop => Poll::Pending,
            FaultMode::ErrorDrop => Poll::Ready(Err(ActorError::InternalError(
                "primary callback error".into(),
            ))),
            _ => Poll::Ready(Ok(())),
        }
    }
}
impl Drop for FaultFuture {
    fn drop(&mut self) {
        record(&self.events, "fault", "drop");
        assert!(
            !matches!(
                self.mode,
                FaultMode::ReadyDrop | FaultMode::ErrorDrop | FaultMode::CancelDrop
            ),
            "stream future cleanup panic"
        );
    }
}
struct FaultReply(Events);
impl Drop for FaultReply {
    fn drop(&mut self) {
        record(&self.0, "fault", "drop");
        panic!("discarded stream reply cleanup panic");
    }
}
struct FaultCustom(Events, FaultMode);
#[async_trait::async_trait]
impl<C: ActorContext + ?Sized> StreamHandler<futures::stream::Pending<u32>, C> for FaultCustom {
    async fn started(&mut self, _: &mut C) {
        fault_callback(&self.0, self.1).await.unwrap();
    }
    async fn handle(&mut self, _: u32, _: &mut C) {}
}
struct FaultThread(Events, FaultMode);
struct FaultActix(Events, FaultMode);
impl ActixEngineExt for FaultActix {}
macro_rules! fault_actor {
    ($actor:ident,$ctx:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $ctx;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                if message.is::<u32>() {
                    record(&self.0, "fault", "construct");
                    let events = self.0.clone();
                    return Box::pin(async move {
                        record(&events, "fault", "poll");
                        Ok(Box::new(FaultReply(events)) as BoxedMessage)
                    });
                }
                let custom = *message.downcast::<bool>().unwrap();
                let parent = TraceContext::current();
                let result = if custom {
                    ctx.stream_registry()
                        .add_stream_with_handler::<_, _, Self::Context>(
                            futures::stream::pending::<u32>(),
                            FaultCustom(self.0.clone(), self.1),
                        )
                } else if self.1 == FaultMode::ReplyDrop {
                    ctx.stream_registry()
                        .add_stream(futures::stream::iter([1_u32]))
                } else {
                    ctx.stream_registry()
                        .add_stream(futures::stream::pending::<u32>())
                };
                Box::pin(async move {
                    result?;
                    Ok(Box::new(parent) as BoxedMessage)
                })
            }
            fn stream_started<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                if self.1 == FaultMode::ReplyDrop {
                    Box::pin(async { Ok(()) })
                } else {
                    Box::pin(fault_callback(&self.0, self.1))
                }
            }
        }
    };
}
fault_actor!(FaultThread, parrot::thread::ThreadContext<Self>);
fault_actor!(
    FaultActix,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
async fn wait_fault(receiver: &flume::Receiver<Event>, mode: FaultMode) -> Vec<Event> {
    let wanted = match mode {
        FaultMode::Construct => "construct",
        FaultMode::CancelDrop => "poll",
        _ => "drop",
    };
    let mut events = Vec::new();
    loop {
        let event = bounded(receiver.recv_async()).await.unwrap();
        let done = event.phase == wanted;
        events.push(event);
        if done {
            return events;
        }
    }
}
fn check_fault(
    recorder: &TraceRecorder,
    parent: &TraceContext,
    events: &[Event],
    target: &str,
    mode: FaultMode,
) {
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal, "{snapshot:?}");
    assert_eq!(snapshot.counts.in_flight, 0);
    assert_eq!(snapshot.counts.overwritten, 0);
    let expected = if mode == FaultMode::Construct {
        vec!["construct"]
    } else {
        vec!["construct", "poll", "drop"]
    };
    assert_eq!(events.iter().map(|e| e.phase).collect::<Vec<_>>(), expected);
    let identity = events[0].context.as_ref().unwrap();
    assert!(events.iter().all(|e| e.context.as_ref() == Some(identity)));
    let records: Vec<_> = snapshot
        .records
        .iter()
        .filter(|r| {
            format!("{:?}", r.kind)
                == if mode == FaultMode::ReplyDrop {
                    "StreamItem"
                } else {
                    "StreamStarted"
                }
        })
        .collect();
    assert_eq!(records.len(), 1, "{snapshot:?}");
    let span = records[0];
    assert_eq!(span.trace_id, identity.trace_id());
    assert_eq!(span.span_id, identity.span_id());
    assert_eq!(span.parent_span_id, Some(parent.span_id()));
    assert_eq!(span.target, target);
    assert_eq!(span.status, SpanStatus::Panicked, "{mode:?}");
}
fn fault_dynamic(use_actix: bool, custom: bool, mode: FaultMode) {
    actix::System::new().block_on(async {
        let recorder = TraceRecorder::new(32).unwrap();
        let (sender, receiver) = flume::unbounded();
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let system = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let actor: BoxedActorRef = if use_actix {
            bounded(system.spawn_root_typed(FaultActix(sender, mode), EmptyConfig))
                .await
                .unwrap()
                .clone_boxed()
        } else {
            bounded(thread.spawn_root_typed_thread(FaultThread(sender, mode), EmptyConfig))
                .await
                .unwrap()
                .clone_boxed()
        };
        let future = recorder.root().in_scope(|| actor.send(Box::new(custom)));
        let parent = *bounded(future)
            .await
            .unwrap()
            .downcast::<Option<TraceContext>>()
            .unwrap();
        let mut events = wait_fault(&receiver, mode).await;
        let stopped = bounded(actor.stop()).await;
        if let Err(error) = stopped {
            assert!(matches!(error, ActorError::Panic(_)), "{error:?}");
        }
        let _ = bounded(thread.shutdown_internal()).await;
        let _ = bounded(system.as_ref().clone().shutdown()).await;
        events.extend(receiver.try_iter());
        check_fault(&recorder, &parent.unwrap(), &events, &actor.path(), mode);
    });
}
struct FaultLocal {
    events: Events,
    mode: FaultMode,
    parent: TraceContext,
}
impl LocalTypedDispatch for FaultLocal {
    type Msg = Ping;
    type Reply = ();
    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.parent.in_scope(|| {
            streams
                .add_stream(futures::stream::pending::<Rc<u32>>())
                .unwrap()
        });
        Box::pin(async { Ok(()) })
    }
    fn dispatch<'a>(
        &'a mut self,
        _: Ping,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_started<'a>(
        &'a mut self,
        _: u64,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(fault_callback(&self.events, self.mode))
    }
}
impl ParrotMsgVariant<LocalProtocol<FaultLocal>> for Ping {
    fn inject(message: Self) -> Ping {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
fn fault_local(mode: FaultMode) {
    actix::System::new().block_on(async {
        let recorder = TraceRecorder::new(32).unwrap();
        let parent = recorder.root();
        let registration = parent.clone();
        let (sender, receiver) = flume::unbounded();
        let system = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let actor = bounded(system.spawn_local_typed::<FaultLocal, Ping, _>(
            move || FaultLocal {
                events: sender.clone(),
                mode,
                parent: registration.clone(),
            },
            "/local/fault-recording",
        ))
        .await
        .unwrap();
        let mut events = wait_fault(&receiver, mode).await;
        let stopped = bounded(actor.stop()).await;
        if let Err(error) = stopped {
            assert!(matches!(error, ActorError::Panic(_)), "{error:?}");
        }
        let _ = bounded(system.as_ref().clone().shutdown()).await;
        events.extend(receiver.try_iter());
        let snapshot = recorder.snapshot();
        let lifetime: Vec<_> = snapshot
            .records
            .iter()
            .filter(|r| r.kind == parrot_api::trace::SpanKind::ManagedTask)
            .collect();
        assert_eq!(lifetime.len(), 1, "{snapshot:?}");
        assert_eq!(lifetime[0].status, SpanStatus::Panicked, "{mode:?}");
        assert_eq!(lifetime[0].parent_span_id, Some(parent.span_id()));
        assert_eq!(lifetime[0].target, "context.stream.local");
        check_fault(&recorder, &parent, &events, actor.path(), mode);
    });
}
macro_rules! dynamic_fault_cases {
    ($module:ident,$actix:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn constructor_panic_recorded() {
                fault_dynamic($actix, false, FaultMode::Construct);
            }
            #[test]
            fn poll_panic_recorded() {
                fault_dynamic($actix, false, FaultMode::Poll);
            }
            #[test]
            fn successful_future_drop_panic_recorded() {
                fault_dynamic($actix, false, FaultMode::ReadyDrop);
            }
            #[test]
            fn failed_future_drop_panic_recorded() {
                fault_dynamic($actix, false, FaultMode::ErrorDrop);
            }
            #[test]
            fn cancelled_future_drop_panic_recorded() {
                fault_dynamic($actix, false, FaultMode::CancelDrop);
            }
            #[test]
            fn custom_successful_future_drop_panic_recorded() {
                fault_dynamic($actix, true, FaultMode::ReadyDrop);
            }
            #[test]
            fn custom_cancelled_future_drop_panic_recorded() {
                fault_dynamic($actix, true, FaultMode::CancelDrop);
            }
        }
    };
}
dynamic_fault_cases!(thread_faults, false);
dynamic_fault_cases!(actix_faults, true);
mod local_faults {
    use super::*;
    #[test]
    fn constructor_panic_recorded() {
        fault_local(FaultMode::Construct);
    }
    #[test]
    fn poll_panic_recorded() {
        fault_local(FaultMode::Poll);
    }
    #[test]
    fn successful_future_drop_panic_recorded() {
        fault_local(FaultMode::ReadyDrop);
    }
    #[test]
    fn failed_future_drop_panic_recorded() {
        fault_local(FaultMode::ErrorDrop);
    }
    #[test]
    fn cancelled_future_drop_panic_recorded() {
        fault_local(FaultMode::CancelDrop);
    }
}

#[test]
fn thread_discarded_stream_reply_panic_recorded() {
    fault_dynamic(false, false, FaultMode::ReplyDrop);
}
#[test]
fn actix_discarded_stream_reply_panic_recorded() {
    fault_dynamic(true, false, FaultMode::ReplyDrop);
}
