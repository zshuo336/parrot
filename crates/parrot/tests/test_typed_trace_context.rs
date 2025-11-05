//! Causal identities must survive actual mailboxes, not just a scoped async block.
use parrot_api::{
    address::ActorRefExt,
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedDispatch},
    message::{BackoffStrategy, Message, MessageOptions, RetryPolicy},
    trace::{self, TraceContext},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::{TypedActorRef, TypedEnvelope, consume_one, typed_channel},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::{
    future::Future,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Debug)]
struct Observation {
    context: Option<TraceContext>,
    nested: Option<Box<Observation>>,
}
enum Action {
    Observe,
    Nested,
    Error,
    Panic,
    ConstructPanic,
    ErrorOnce(Arc<AtomicBool>),
    Wait(tokio::sync::oneshot::Sender<()>),
}
struct Request {
    id: usize,
    action: Action,
}
impl Request {
    fn observe(id: usize) -> Self {
        Self {
            id,
            action: Action::Observe,
        }
    }
}
impl Message for Request {
    type Result = Observation;
}
#[derive(Clone)]
struct ReplayRequest {
    id: usize,
    fail: Arc<AtomicBool>,
}
impl Message for ReplayRequest {
    type Result = Observation;
    fn message_options(&self) -> Option<MessageOptions> {
        Some(MessageOptions {
            retry_policy: Some(RetryPolicy {
                max_attempts: 1,
                retry_interval: Duration::ZERO,
                backoff_strategy: BackoffStrategy::Fixed,
            }),
            ..Default::default()
        })
    }
}
type Events = Arc<Mutex<Vec<(usize, &'static str, Option<TraceContext>)>>>;
fn record(events: &Events, id: usize, stage: &'static str) {
    events
        .lock()
        .unwrap()
        .push((id, stage, TraceContext::current()));
}
struct PollOwner {
    events: Events,
    id: usize,
}
impl Drop for PollOwner {
    fn drop(&mut self) {
        record(&self.events, self.id, "drop");
    }
}
struct Probe {
    events: Events,
    next: Option<BoxedActorRef>,
}
impl Probe {
    fn handle(
        &mut self,
        request: Request,
    ) -> impl Future<Output = ActorResult<Observation>> + Send + '_ {
        record(&self.events, request.id, "construct");
        assert!(
            !matches!(request.action, Action::ConstructPanic),
            "trace construction panic"
        );
        let owner = PollOwner {
            events: self.events.clone(),
            id: request.id,
        };
        async move {
            let _owner = owner;
            record(&self.events, request.id, "poll");
            tokio::task::yield_now().await;
            record(&self.events, request.id, "resume");
            let nested = match request.action {
                Action::Nested => {
                    let reply = self
                        .next
                        .as_ref()
                        .unwrap()
                        .send(Box::new(Request::observe(request.id)))
                        .await?;
                    Some(reply.downcast::<Observation>().unwrap())
                }
                Action::Error => return Err(ActorError::InternalError("trace error".into())),
                Action::Panic => panic!("trace poll panic"),
                Action::ErrorOnce(fail) if fail.swap(false, Ordering::SeqCst) => {
                    return Err(ActorError::InternalError("retry trace request".into()));
                }
                Action::Wait(entered) => {
                    let _ = entered.send(());
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                _ => None,
            };
            Ok(Observation {
                context: TraceContext::current(),
                nested,
            })
        }
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Request;
    type Reply = Observation;
    fn dispatch(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<Observation>> {
        Box::pin(self.handle(request))
    }
}
impl ParrotMsgVariant<Probe> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Observation) -> ActorResult<Observation> {
        Ok(reply)
    }
}
impl ParrotMsgVariant<Probe> for ReplayRequest {
    fn inject(message: Self) -> Request {
        Request {
            id: message.id,
            action: Action::ErrorOnce(message.fail),
        }
    }
    fn extract(reply: Observation) -> ActorResult<Observation> {
        Ok(reply)
    }
}
struct LocalProbe {
    probe: Probe,
    state: Rc<()>,
}
impl LocalTypedDispatch for LocalProbe {
    type Msg = Request;
    type Reply = Observation;
    fn dispatch<'a>(
        &'a mut self,
        request: Request,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<Observation>> {
        let held = self.state.clone();
        let future = self.probe.handle(request);
        Box::pin(async move {
            let _held = held;
            future.await
        })
    }
}
impl ParrotMsgVariant<LocalProtocol<LocalProbe>> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Observation) -> ActorResult<Observation> {
        Ok(reply)
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("trace contract deadline")
}
fn child_of(observed: &Observation, parent: &TraceContext) -> TraceContext {
    let child = observed
        .context
        .as_ref()
        .expect("mailbox lost trace context");
    assert_eq!(child.trace_id(), parent.trace_id());
    assert_eq!(child.parent_span_id(), Some(parent.span_id()));
    assert_ne!(child.span_id(), parent.span_id());
    child.clone()
}
fn stages(events: &Events, id: usize, expected: Option<&TraceContext>, names: &[&str]) {
    let rows = events.lock().unwrap();
    let selected: Vec<_> = rows.iter().filter(|r| r.0 == id).collect();
    assert_eq!(selected.iter().map(|r| r.1).collect::<Vec<_>>(), names);
    assert!(
        selected.iter().all(|r| r.2.as_ref() == expected),
        "context changed across handler lifecycle: {selected:?}"
    );
}
enum Engine {
    Thread(Arc<parrot::thread::ThreadActorSystem>),
    Actix(Arc<parrot::actix::ActixActorSystem>),
}
impl Engine {
    async fn new(actix: bool) -> Self {
        if actix {
            Self::Actix(Arc::new(
                parrot::actix::ActixActorSystem::with_arbiter_count(1)
                    .await
                    .unwrap(),
            ))
        } else {
            Self::Thread(parrot::thread::ThreadActorSystem::shared(Default::default()))
        }
    }
    async fn spawn(
        &self,
        events: Events,
        next: Option<BoxedActorRef>,
        path: &str,
    ) -> TypedActorRef<Probe, Request> {
        let probe = Probe { events, next };
        let strategy = parrot_api::supervisor::SupervisorStrategyType::Default(
            parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
        );
        match self {
            Self::Thread(s) => bounded(s.spawn_typed_with_strategy(probe, path, strategy))
                .await
                .unwrap(),
            Self::Actix(s) => bounded(s.spawn_typed_with_strategy(probe, path, strategy))
                .await
                .unwrap(),
        }
    }
    async fn close(self) {
        match self {
            Self::Thread(s) => bounded(s.shutdown_internal()).await.unwrap(),
            Self::Actix(s) => bounded(s.as_ref().clone().shutdown()).await.unwrap(),
        }
    }
}
async fn delivery(actix: bool) {
    let engine = Engine::new(actix).await;
    let events = Events::default();
    let actor = engine.spawn(events.clone(), None, "/trace").await;
    let dynamic = actor.clone().into_dyn();
    let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
    let root = recorder.root();
    let other = TraceContext::root();
    // Capture before polling: a future moved to another context keeps its parent.
    let request = root.in_scope(|| actor.ask(Request::observe(0)));
    let observed = bounded(other.scope(request)).await.unwrap();
    let child = child_of(&observed, &root);
    stages(
        &events,
        0,
        Some(&child),
        &["construct", "poll", "resume", "drop"],
    );
    let request = root.in_scope(|| actor.ask_unbounded(Request::observe(1)));
    child_of(&bounded(request).await.unwrap(), &root);
    let request =
        root.in_scope(|| actor.ask_with_timeout(Request::observe(2), Duration::from_secs(5)));
    child_of(&bounded(request).await.unwrap(), &root);
    let request = root.in_scope(|| dynamic.send(Box::new(Request::observe(3))));
    child_of(
        &bounded(other.scope(request))
            .await
            .unwrap()
            .downcast::<Observation>()
            .unwrap(),
        &root,
    );
    let request = root.in_scope(|| {
        dynamic.send_with_timeout(Box::new(Request::observe(4)), Some(Duration::from_secs(5)))
    });
    child_of(
        &bounded(request)
            .await
            .unwrap()
            .downcast::<Observation>()
            .unwrap(),
        &root,
    );
    let tell = root.in_scope(|| actor.tell(Request::observe(5)));
    bounded(other.scope(tell)).await.unwrap();
    let tell = root.in_scope(|| dynamic.deliver(Box::new(Request::observe(6))));
    bounded(tell).await.unwrap();
    // An untraced operation created outside a scope must not adopt its poller's root.
    let untraced = actor.ask(Request::observe(7));
    assert!(
        bounded(other.scope(untraced))
            .await
            .unwrap()
            .context
            .is_none()
    );
    for id in [5, 6] {
        let context = events
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.0 == id)
            .unwrap()
            .2
            .clone();
        let child = child_of(
            &Observation {
                context,
                nested: None,
            },
            &root,
        );
        stages(
            &events,
            id,
            Some(&child),
            &["construct", "poll", "resume", "drop"],
        );
    }
    stages(&events, 7, None, &["construct", "poll", "resume", "drop"]);
    assert!(TraceContext::current().is_none());
    engine.close().await;
    recording_receipt(
        &recorder,
        &events,
        parrot_api::trace::SpanKind::TypedHandler,
        &[],
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_all_typed_and_erased_delivery_entries_preserve_call_time_parent() {
    delivery(false).await;
}
#[actix::test]
async fn actix_all_typed_and_erased_delivery_entries_preserve_call_time_parent() {
    delivery(true).await;
}

async fn adapters(actix: bool) {
    let engine = Engine::new(actix).await;
    let events = Events::default();
    let actor = engine.spawn(events.clone(), None, "/adapters").await;
    let dynamic = actor.clone().into_dyn();
    let root = TraceContext::root();
    let poller = TraceContext::root();
    let request = root.in_scope(|| {
        dynamic.send_with_options(Box::new(Request::observe(10)), MessageOptions::default())
    });
    bounded(poller.scope(request)).await.unwrap();
    let request = root.in_scope(|| {
        dynamic.deliver_with_options(Box::new(Request::observe(11)), MessageOptions::default())
    });
    bounded(poller.scope(request)).await.unwrap();
    let request = root.in_scope(|| dynamic.ask(Request::observe(12)));
    bounded(poller.scope(request)).await.unwrap();
    let request = root.in_scope(|| dynamic.tell(Request::observe(13)));
    bounded(poller.scope(request)).await.unwrap();
    let retry_ref = actor.ref_for::<ReplayRequest>().into_dyn();
    let fail = Arc::new(AtomicBool::new(true));
    let request = root.in_scope(|| {
        retry_ref.ask_retrying(
            ReplayRequest {
                id: 14,
                fail: fail.clone(),
            },
            |_| true,
        )
    });
    bounded(poller.scope(request)).await.unwrap();
    assert!(!fail.load(Ordering::SeqCst));
    let untraced = dynamic.ask(Request::observe(15));
    let observed = bounded(poller.scope(untraced)).await.unwrap();
    assert!(
        observed.context.is_none(),
        "untraced adapter adopted poller"
    );
    {
        let rows = events.lock().unwrap();
        let contexts: Vec<_> = rows
            .iter()
            .filter(|row| (10..=14).contains(&row.0) && row.1 == "construct")
            .collect();
        assert_eq!(
            contexts.len(),
            6,
            "five adapters include two real retry deliveries"
        );
        assert_eq!(contexts.iter().filter(|row| row.0 == 14).count(), 2);
        let mut spans = std::collections::HashSet::new();
        for row in contexts {
            let context = row.2.as_ref().expect("adapter lost context");
            assert_eq!(
                context.trace_id(),
                root.trace_id(),
                "adapter {} replaced parent",
                row.0
            );
            assert_eq!(context.parent_span_id(), Some(root.span_id()));
            assert!(spans.insert(context.span_id()), "retry reused handler span");
        }
    }
    assert!(TraceContext::current().is_none());
    engine.close().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_public_adapters_and_retry_attempts_preserve_call_time_context() {
    adapters(false).await;
}
#[actix::test]
async fn actix_public_adapters_and_retry_attempts_preserve_call_time_context() {
    adapters(true).await;
}

async fn nesting(actix: bool) {
    let engine = Engine::new(actix).await;
    let second = engine.spawn(Events::default(), None, "/second").await;
    let first = engine
        .spawn(Events::default(), Some(second.into_dyn()), "/first")
        .await;
    let roots: Vec<_> = (0..32).map(|_| TraceContext::root()).collect();
    let calls = roots.iter().enumerate().map(|(id, root)| {
        root.in_scope(|| {
            first.ask(Request {
                id,
                action: Action::Nested,
            })
        })
    });
    let replies = bounded(futures::future::join_all(calls)).await;
    let mut spans = std::collections::HashSet::new();
    for (root, reply) in roots.iter().zip(replies) {
        let reply = reply.unwrap();
        let parent = child_of(&reply, root);
        let nested = child_of(reply.nested.as_ref().unwrap(), &parent);
        assert!(spans.insert(parent.span_id()));
        assert!(spans.insert(nested.span_id()));
    }
    engine.close().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_nested_requests_keep_distinct_roots_and_explicit_parents() {
    nesting(false).await;
}
#[actix::test]
async fn actix_nested_requests_keep_distinct_roots_and_explicit_parents() {
    nesting(true).await;
}

async fn faults(actix: bool) {
    let engine = Engine::new(actix).await;
    let events = Events::default();
    let actor = engine.spawn(events.clone(), None, "/faults").await;
    let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
    let root = recorder.root();
    for (id, action) in [
        (0, Action::Error),
        (1, Action::Panic),
        (2, Action::ConstructPanic),
    ] {
        let request = root.in_scope(|| actor.ask(Request { id, action }));
        assert!(bounded(request).await.is_err());
        let context = events
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.0 == id)
            .unwrap()
            .2
            .clone();
        let child = child_of(
            &Observation {
                context,
                nested: None,
            },
            &root,
        );
        stages(
            &events,
            id,
            Some(&child),
            if id == 2 {
                &["construct"]
            } else {
                &["construct", "poll", "resume", "drop"]
            },
        );
        assert!(
            bounded(actor.ask(Request::observe(100 + id)))
                .await
                .unwrap()
                .context
                .is_none()
        );
    }
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let request = root.in_scope(|| {
        actor.ask(Request {
            id: 3,
            action: Action::Wait(entered),
        })
    });
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    bounded(waiting).await.unwrap();
    bounded(actor.stop()).await.unwrap();
    assert!(bounded(request).await.is_err());
    let context = events
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.0 == 3)
        .unwrap()
        .2
        .clone();
    let child = child_of(
        &Observation {
            context,
            nested: None,
        },
        &root,
    );
    stages(
        &events,
        3,
        Some(&child),
        &["construct", "poll", "resume", "drop"],
    );
    assert!(TraceContext::current().is_none());
    engine.close().await;
    use parrot_api::trace::SpanStatus::*;
    recording_receipt(
        &recorder,
        &events,
        parrot_api::trace::SpanKind::TypedHandler,
        &[(0, Error), (1, Panicked), (2, Panicked), (3, Cancelled)],
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_error_panic_and_stop_keep_cleanup_context_without_leaking() {
    faults(false).await;
}
#[actix::test]
async fn actix_error_panic_and_stop_keep_cleanup_context_without_leaking() {
    faults(true).await;
}

#[actix::test]
async fn local_non_send_dispatch_and_cleanup_retain_context_on_arbiter() {
    let system = Arc::new(
        parrot::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap(),
    );
    let events = Events::default();
    let copy = events.clone();
    let actor = system
        .spawn_local_typed::<LocalProbe, Request, _>(
            move || LocalProbe {
                probe: Probe {
                    events: copy.clone(),
                    next: None,
                },
                state: Rc::new(()),
            },
            "/local-trace",
        )
        .await
        .unwrap();
    let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
    let root = recorder.root();
    let request = root.in_scope(|| actor.ask(Request::observe(0)));
    let child = child_of(&bounded(request).await.unwrap(), &root);
    stages(
        &events,
        0,
        Some(&child),
        &["construct", "poll", "resume", "drop"],
    );
    assert!(
        bounded(actor.ask(Request::observe(1)))
            .await
            .unwrap()
            .context
            .is_none()
    );
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let request = root.in_scope(|| {
        actor.ask(Request {
            id: 2,
            action: Action::Wait(entered),
        })
    });
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    bounded(waiting).await.unwrap();
    bounded(actor.stop()).await.unwrap();
    assert!(bounded(request).await.is_err());
    let context = events
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.0 == 2)
        .unwrap()
        .2
        .clone();
    let child = child_of(
        &Observation {
            context,
            nested: None,
        },
        &root,
    );
    stages(
        &events,
        2,
        Some(&child),
        &["construct", "poll", "resume", "drop"],
    );
    bounded(system.as_ref().clone().shutdown()).await.unwrap();
    recording_receipt(
        &recorder,
        &events,
        parrot_api::trace::SpanKind::TypedHandler,
        &[(2, parrot_api::trace::SpanStatus::Cancelled)],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_consumer_clears_ambient_context_and_scopes_future_construction() {
    let (sender, receiver) = typed_channel::<Probe>(2);
    let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
    let root = recorder.root();
    let (reply, response) = parrot_api::typed_channel::reply_channel::<Probe>();
    let envelope = root.in_scope(|| TypedEnvelope::new(Request::observe(0), Some(reply)));
    sender
        .try_send(envelope)
        .unwrap_or_else(|_| panic!("mailbox capacity"));
    let (reply, untraced) = parrot_api::typed_channel::reply_channel::<Probe>();
    sender
        .try_send(TypedEnvelope::new(Request::observe(1), Some(reply)))
        .unwrap_or_else(|_| panic!("mailbox capacity"));
    let events = Events::default();
    let copy = events.clone();
    let ambient = TraceContext::root();
    let worker = tokio::spawn(ambient.scope(async move {
        let mut actor = Probe {
            events: copy,
            next: None,
        };
        for _ in 0..2 {
            consume_one(&mut actor, receiver.recv_async().await.unwrap()).await;
        }
    }));
    bounded(worker).await.unwrap();
    let child = child_of(&response.try_recv().unwrap().unwrap(), &root);
    stages(
        &events,
        0,
        Some(&child),
        &["construct", "poll", "resume", "drop"],
    );
    assert!(untraced.try_recv().unwrap().unwrap().context.is_none());
    stages(&events, 1, None, &["construct", "poll", "resume", "drop"]);
    recording_receipt(
        &recorder,
        &events,
        parrot_api::trace::SpanKind::TypedHandler,
        &[],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicitly_captured_task_moves_context_and_ordinary_spawn_does_not_inherit() {
    let root = TraceContext::root();
    let caller = std::thread::current().id();
    let captured = root.in_scope(|| {
        trace::capture(async {
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            (std::thread::current().id(), TraceContext::current())
        })
    });
    let (worker, context) = std::thread::spawn(move || futures::executor::block_on(captured))
        .join()
        .unwrap();
    assert_ne!(worker, caller);
    assert_eq!(context, Some(root.clone()));
    let unrelated = root.in_scope(|| tokio::spawn(async { TraceContext::current() }));
    assert!(bounded(unrelated).await.unwrap().is_none());
    let unwound = std::panic::catch_unwind(|| root.in_scope(|| panic!("scope panic")));
    assert!(unwound.is_err());
    assert!(TraceContext::current().is_none());
    let mut cancelled = Box::pin(root.scope(std::future::pending::<()>()));
    assert!(futures::poll!(&mut cancelled).is_pending());
    assert!(TraceContext::current().is_none());
    drop(cancelled);
    assert!(TraceContext::current().is_none());
}

fn recording_receipt(
    recorder: &parrot_api::trace::TraceRecorder,
    events: &Events,
    kind: parrot_api::trace::SpanKind,
    expected: &[(usize, parrot_api::trace::SpanStatus)],
) {
    recorder.close();
    let snapshot = recorder.drain();
    assert!(
        snapshot.terminal,
        "recording owners outlived completed engine cleanup: {snapshot:?}"
    );
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(snapshot.counts.rejected_after_close, 0);
    assert_eq!(snapshot.counts.targets_truncated, 0);
    assert!(!snapshot.counts.counters_overflowed);
    let events = events.lock().unwrap();
    let observations: Vec<_> = events
        .iter()
        .filter(|event| event.1 == "construct" && event.2.is_some())
        .collect();
    assert_eq!(
        snapshot.records.len(),
        observations.len(),
        "each actual traced dispatch needs exactly one record"
    );
    assert_eq!(snapshot.counts.started as usize, observations.len());
    assert_eq!(snapshot.counts.completed as usize, observations.len());
    assert_eq!(snapshot.counts.drained as usize, observations.len());
    assert_eq!(snapshot.counts.in_flight, 0);
    let mut spans = std::collections::HashSet::new();
    for event in observations {
        let context = event.2.as_ref().unwrap();
        let record = snapshot
            .records
            .iter()
            .find(|record| record.span_id == context.span_id())
            .expect("handler observation has no record");
        assert!(spans.insert(record.span_id));
        assert_eq!(record.trace_id, context.trace_id());
        assert_eq!(record.parent_span_id, context.parent_span_id());
        assert_eq!(record.kind, kind);
        assert!(!record.target.is_empty());
        assert!(record.started + record.duration <= snapshot.elapsed);
        let expected = expected
            .iter()
            .find(|(id, _)| *id == event.0)
            .map_or(parrot_api::trace::SpanStatus::Ok, |(_, status)| *status);
        assert_eq!(
            record.status, expected,
            "wrong status for observed operation {}",
            event.0
        );
    }
}

#[tokio::test]
async fn standalone_consumer_records_errors_and_propagated_panics() {
    use futures::FutureExt;
    let recorder = parrot_api::trace::TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let events = Events::default();
    let mut actor = Probe {
        events: events.clone(),
        next: None,
    };
    for (id, action) in [
        (0, Action::Error),
        (1, Action::Panic),
        (2, Action::ConstructPanic),
    ] {
        let envelope = root.in_scope(|| TypedEnvelope::new(Request { id, action }, None));
        let result = std::panic::AssertUnwindSafe(consume_one(&mut actor, envelope))
            .catch_unwind()
            .await;
        assert_eq!(
            result.is_err(),
            id != 0,
            "raw helper must still propagate panic"
        );
    }
    use parrot_api::trace::SpanStatus::*;
    recording_receipt(
        &recorder,
        &events,
        parrot_api::trace::SpanKind::TypedHandler,
        &[(0, Error), (1, Panicked), (2, Panicked)],
    );
}

struct RawCleanupProbe;
struct RawCleanupFuture;
impl Future for RawCleanupFuture {
    type Output = ActorResult<()>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Ready(Ok(()))
    }
}
impl Drop for RawCleanupFuture {
    fn drop(&mut self) {
        panic!("raw completed future cleanup panic");
    }
}
impl ParrotTypedDispatch for RawCleanupProbe {
    type Msg = u8;
    type Reply = ();
    fn dispatch(&mut self, _: u8) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(RawCleanupFuture)
    }
}
#[tokio::test]
async fn standalone_consumer_records_completed_future_cleanup_panic() {
    use futures::FutureExt;
    let recorder = parrot_api::trace::TraceRecorder::new(2).unwrap();
    let root = recorder.root();
    let envelope = root.in_scope(|| TypedEnvelope::new(0, None));
    let result = std::panic::AssertUnwindSafe(consume_one(&mut RawCleanupProbe, envelope))
        .catch_unwind()
        .await;
    assert!(
        result.is_err(),
        "standalone helper must propagate cleanup panic"
    );
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(
        snapshot.records[0].status,
        parrot_api::trace::SpanStatus::Panicked
    );
    assert_eq!(snapshot.counts.panicked, 1);
    assert_eq!(snapshot.counts.cancelled, 0);
}
