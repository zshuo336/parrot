//! Trace identity is checked at source and actual callback ownership boundaries.
use futures::{FutureExt, Stream};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::ActorContext,
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalStreamContext, LocalTypedDispatch},
    message::Message,
    stream::{StreamHandler, StreamRegistryExt},
    trace::{self, TraceContext},
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
            let root = TraceContext::root();
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
            results.push((parent, events));
        }
        bounded(thread.shutdown_internal()).await.unwrap();
        bounded(system.as_ref().clone().shutdown()).await.unwrap();
        for (parent, events) in results {
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
            let parent = traced.then(TraceContext::root);
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
            results.push((parent, events));
        }
        for (parent, events) in results {
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
