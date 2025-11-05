//! Real dynamic mailbox contracts: creation, every poll, and owned cleanup.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig, EngineContextHandle},
    address::ActorRef,
    message::MessageOptions,
    trace::TraceContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    collections::HashSet,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(5);
type Events = Arc<Mutex<Vec<(usize, &'static str, Option<TraceContext>)>>>;
struct Request {
    id: usize,
    finished: tokio::sync::oneshot::Sender<()>,
}
struct Guard {
    request: Option<Request>,
    events: Events,
}
impl Guard {
    fn record(&self, stage: &'static str) {
        self.events.lock().unwrap().push((
            self.request.as_ref().unwrap().id,
            stage,
            TraceContext::current(),
        ));
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.record("drop");
        let _ = self.request.take().unwrap().finished.send(());
    }
}
fn begin(events: &Events, message: BoxedMessage) -> Guard {
    let guard = Guard {
        request: Some(*message.downcast::<Request>().unwrap()),
        events: events.clone(),
    };
    guard.record("construct");
    guard
}
fn asynchronous(
    events: &Events,
    message: BoxedMessage,
) -> BoxedFuture<'static, ActorResult<BoxedMessage>> {
    let (message, action) = match message.downcast::<FaultRequest>() {
        Ok(fault) => (Box::new(fault.request) as BoxedMessage, fault.action),
        Err(message) => (message, FaultAction::Normal),
    };
    let guard = begin(events, message);
    assert!(
        !matches!(action, FaultAction::ConstructPanic),
        "construction panic"
    );
    Box::pin(async move {
        guard.record("poll");
        tokio::task::yield_now().await;
        guard.record("resume");
        match action {
            FaultAction::Error => {
                return Err(parrot_api::errors::ActorError::InternalError(
                    "handler error".into(),
                ));
            }
            FaultAction::PollPanic => panic!("handler poll panic"),
            FaultAction::Wait(entered) => {
                let _ = entered.send(());
                std::future::pending::<()>().await;
            }
            _ => {}
        }
        Ok(Box::new(guard.request.as_ref().unwrap().id) as BoxedMessage)
    })
}
struct ThreadProbe(Events);
impl Actor for ThreadProbe {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        asynchronous(&self.0, message)
    }
}
struct ActixProbe<const ASYNC: bool>(Events);
impl<const ASYNC: bool> Actor for ActixProbe<ASYNC> {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        ASYNC
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        asynchronous(&self.0, message)
    }
}
impl<const ASYNC: bool> ActixEngineExt for ActixProbe<ASYNC> {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
        _: EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        let guard = begin(&self.0, message);
        guard.record("sync");
        Some(Ok(
            Box::new(guard.request.as_ref().unwrap().id) as BoxedMessage
        ))
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(LIMIT, future)
        .await
        .expect("contract timed out")
}
fn invoke(
    reference: &dyn ActorRef,
    entry: usize,
    request: Request,
) -> BoxedFuture<'_, ActorResult<()>> {
    let id = request.id;
    let message = Box::new(request) as BoxedMessage;
    if entry >= 4 {
        return if entry == 4 {
            reference.deliver(message)
        } else {
            reference.deliver_with_options(message, MessageOptions::default())
        };
    }
    let future = match entry {
        0 => reference.send(message),
        1 => reference.send_with_timeout(message, Some(LIMIT)),
        2 => reference.send_with_timeout(message, None),
        3 => reference.send_with_options(message, MessageOptions::default()),
        _ => unreachable!(),
    };
    Box::pin(async move {
        let reply = future.await?;
        assert_eq!(
            *reply.downcast::<usize>().unwrap(),
            id,
            "reply identity changed"
        );
        Ok(())
    })
}
async fn entries(
    reference: &dyn ActorRef,
    recorder: &parrot_api::trace::TraceRecorder,
) -> Vec<Option<TraceContext>> {
    let roots = [recorder.root(), recorder.root()];
    let poller = TraceContext::root();
    let mut parents = Vec::new();
    for entry in 0..6 {
        for traced in [true, false] {
            let id = parents.len();
            let (finished, receipt) = tokio::sync::oneshot::channel();
            let parent = traced.then(|| roots[entry % 2].clone());
            let request = Request { id, finished };
            let future = match &parent {
                Some(root) => root.in_scope(|| invoke(reference, entry, request)),
                None => invoke(reference, entry, request),
            };
            bounded(poller.scope(future)).await.unwrap();
            // TELL only returned admission: independently wait for owned handler cleanup.
            bounded(receipt).await.unwrap();
            assert!(
                TraceContext::current().is_none(),
                "poll scope escaped into caller"
            );
            parents.push(parent);
        }
    }
    parents
}
fn check(events: &Events, parents: &[Option<TraceContext>], asynchronous: bool) {
    let events = events.lock().unwrap();
    let stages: &[&str] = if asynchronous {
        &["construct", "poll", "resume", "drop"]
    } else {
        &["construct", "sync", "drop"]
    };
    assert_eq!(events.len(), parents.len() * stages.len());
    let mut children = HashSet::new();
    for (id, parent) in parents.iter().enumerate() {
        let actual: Vec<_> = events.iter().filter(|event| event.0 == id).collect();
        assert_eq!(
            actual.iter().map(|event| event.1).collect::<Vec<_>>(),
            stages
        );
        let context = &actual[0].2;
        match parent {
            Some(parent) => {
                let context = context
                    .as_ref()
                    .expect("dynamic dispatch lost causal context");
                assert_eq!(context.trace_id(), parent.trace_id());
                assert_eq!(context.parent_span_id(), Some(parent.span_id()));
                assert_ne!(context.span_id(), parent.span_id());
                assert!(
                    children.insert(context.span_id()),
                    "separate invocations reused span"
                );
            }
            None => assert!(
                context.is_none(),
                "untraced message inherited polling context"
            ),
        }
        assert!(
            actual.iter().all(|event| &event.2 == context),
            "construction, polling and cleanup contexts differ"
        );
    }
}
fn thread_entries(mode: parrot::thread::SchedulingMode) {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let events = Events::default();
            let system = parrot::thread::ThreadActorSystem::shared(Default::default());
            let actor = bounded(system.spawn_at(
                ThreadProbe(events.clone()),
                "/trace",
                None,
                parrot::thread::ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
            let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
            let parents = entries(&actor, &recorder).await;
            bounded(actor.stop()).await.unwrap();
            bounded(system.shutdown_internal()).await.unwrap();
            check(&events, &parents, true);
            recording_receipt(
                &recorder,
                &events,
                parrot_api::trace::SpanKind::DynamicHandler,
                &[],
            );
        });
}
#[test]
fn thread_shared_dynamic_public_entries_preserve_call_time_context() {
    thread_entries(parrot::thread::SchedulingMode::SharedPool {
        max_messages_per_run: 10,
    });
}
#[test]
fn thread_dedicated_dynamic_public_entries_preserve_call_time_context() {
    thread_entries(parrot::thread::SchedulingMode::DedicatedThread);
}
#[test]
fn thread_sharded_dynamic_public_entries_preserve_call_time_context() {
    thread_entries(parrot::thread::SchedulingMode::Sharded {
        affinity_key: "trace".into(),
    });
}

fn actix_entries<const ASYNC: bool>() {
    actix::System::new().block_on(async {
        let events = Events::default();
        let system = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let actor =
            bounded(system.spawn_root_typed(ActixProbe::<ASYNC>(events.clone()), EmptyConfig))
                .await
                .unwrap();
        let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
        let parents = entries(actor.as_ref(), &recorder).await;
        bounded(actor.stop()).await.unwrap();
        bounded(system.as_ref().clone().shutdown()).await.unwrap();
        check(&events, &parents, ASYNC);
        recording_receipt(
            &recorder,
            &events,
            parrot_api::trace::SpanKind::DynamicHandler,
            &[],
        );
    });
}
#[test]
fn actix_async_dynamic_public_entries_preserve_call_time_context() {
    actix_entries::<true>();
}
#[test]
fn actix_sync_dynamic_public_entries_preserve_call_time_context() {
    actix_entries::<false>();
}

struct HelperProbe(Events);
impl Actor for HelperProbe {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let id = *message.downcast::<u64>().unwrap() as usize;
        let (finished, _) = tokio::sync::oneshot::channel();
        asynchronous(&self.0, Box::new(Request { id, finished }))
    }
}
fn helper_call(
    reference: &parrot::thread::ThreadActorRef<HelperProbe>,
    entry: usize,
    id: usize,
) -> BoxedFuture<'_, ActorResult<()>> {
    use parrot::thread::BackpressureStrategy::Block;
    let payload = Box::new(id as u64) as BoxedMessage;
    let ask: BoxedFuture<'_, ActorResult<BoxedMessage>> = match entry {
        0 => Box::pin(reference.ask(payload)),
        1 => Box::pin(reference.ask_with_timeout(payload, LIMIT)),
        2 => Box::pin(reference.ask_unbounded(payload, Block)),
        3 => Box::pin(reference.ask_with_strategy_and_timeout(payload, Block, LIMIT)),
        4 => Box::pin(reference.ask_inline(id as u64, LIMIT)),
        5 => return Box::pin(reference.send_msg(payload)),
        6 => return Box::pin(reference.send_with_strategy(payload, Block)),
        7 => return Box::pin(reference.send_with_priority(payload, Block, false)),
        8 => return Box::pin(reference.send_with_priority(payload, Block, true)),
        _ => unreachable!(),
    };
    Box::pin(async move {
        assert_eq!(*ask.await?.downcast::<usize>().unwrap(), id);
        Ok(())
    })
}
#[test]
fn thread_native_helpers_inline_and_priority_capture_before_first_poll() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let events = Events::default();
            let system = parrot::thread::ThreadActorSystem::shared(Default::default());
            let actor =
                bounded(system.spawn_root_typed_thread(HelperProbe(events.clone()), EmptyConfig))
                    .await
                    .unwrap();
            let parent = TraceContext::root();
            let poller = TraceContext::root();
            let mut parents = Vec::new();
            for entry in 0..9 {
                let id = parents.len();
                let future = parent.in_scope(|| helper_call(&actor, entry, id));
                bounded(poller.scope(future)).await.unwrap();
                parents.push(Some(parent.clone()));
                if entry >= 5 {
                    // Ordered normal-lane ASK follows TELL, including priority TELL.
                    let id = parents.len();
                    let reply = bounded(actor.send(Box::new(id as u64))).await.unwrap();
                    assert_eq!(*reply.downcast::<usize>().unwrap(), id);
                    parents.push(None);
                }
            }
            bounded(actor.stop()).await.unwrap();
            bounded(system.shutdown_internal()).await.unwrap();
            check(&events, &parents, true);
        });
}

#[derive(Clone, Debug)]
struct Group {
    ids: Vec<u64>,
    contexts: Vec<(&'static str, Option<TraceContext>)>,
}
type Groups = Arc<Mutex<Vec<Group>>>;
struct BatchGuard {
    groups: Groups,
    index: usize,
}
impl BatchGuard {
    fn record(&self, stage: &'static str) {
        self.groups.lock().unwrap()[self.index]
            .contexts
            .push((stage, TraceContext::current()));
    }
}
impl Drop for BatchGuard {
    fn drop(&mut self) {
        self.record("drop");
    }
}
struct Gate;
struct BatchProbe {
    groups: Groups,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl Actor for BatchProbe {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        message.is::<u64>()
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            assert!(
                message.is::<Gate>(),
                "eligible message bypassed the batch handler"
            );
            self.entered.notify_one();
            self.release.notified().await;
            Ok(Box::new(()) as BoxedMessage)
        })
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        let ids = messages
            .iter()
            .map(|message| *message.downcast_ref::<u64>().unwrap())
            .collect();
        let mut groups = self.groups.lock().unwrap();
        let index = groups.len();
        groups.push(Group {
            ids,
            contexts: vec![("construct", TraceContext::current())],
        });
        drop(groups);
        let guard = BatchGuard {
            groups: self.groups.clone(),
            index,
        };
        Box::pin(async move {
            guard.record("poll");
            tokio::task::yield_now().await;
            guard.record("resume");
            messages.into_iter().map(Ok).collect()
        })
    }
}
fn poll_pending<T>(future: &mut BoxedFuture<'_, T>) {
    assert!(
        future
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
}
fn batch_contexts(mode: parrot::thread::SchedulingMode) {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let groups = Groups::default();
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let system = parrot::thread::ThreadActorSystem::shared(Default::default());
            let actor = bounded(system.spawn_at(
                BatchProbe {
                    groups: groups.clone(),
                    entered: entered.clone(),
                    release: release.clone(),
                },
                "/batch-trace",
                None,
                parrot::thread::ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    message_batch_size: Some(4),
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
            let mut gate = actor.send(Box::new(Gate));
            poll_pending(&mut gate);
            bounded(entered.notified()).await;
            let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
            let a = recorder.root();
            let b = recorder.root();
            let poller = TraceContext::root();
            let parents = [
                Some(a.clone()),
                Some(a.clone()),
                Some(b.clone()),
                None,
                None,
                Some(b.clone()),
                Some(b),
                Some(a),
            ];
            let mut calls = Vec::new();
            for (id, parent) in parents.iter().enumerate() {
                if matches!(id, 1 | 3 | 5) {
                    let call = match parent {
                        Some(parent) => parent.in_scope(|| actor.deliver(Box::new(id as u64))),
                        None => actor.deliver(Box::new(id as u64)),
                    };
                    bounded(poller.scope(call)).await.unwrap();
                } else {
                    let mut call = match parent {
                        Some(parent) => parent.in_scope(|| actor.send(Box::new(id as u64))),
                        None => actor.send(Box::new(id as u64)),
                    };
                    poller.in_scope(|| poll_pending(&mut call));
                    calls.push((id, call));
                }
            }
            release.notify_one();
            bounded(gate).await.unwrap();
            for (id, call) in calls {
                assert_eq!(
                    *bounded(call).await.unwrap().downcast::<u64>().unwrap(),
                    id as u64
                );
            }
            bounded(actor.stop()).await.unwrap();
            bounded(system.shutdown_internal()).await.unwrap();
            let groups = groups.lock().unwrap();
            assert_eq!(
                groups
                    .iter()
                    .flat_map(|group| group.ids.iter().copied())
                    .collect::<Vec<_>>(),
                (0..8).collect::<Vec<_>>()
            );
            assert!(
                groups.iter().any(|group| group.ids.len() > 1),
                "compatible parents should still coalesce"
            );
            recorder.close();
            let snapshot = recorder.drain();
            assert!(snapshot.terminal);
            assert_eq!(snapshot.counts.overwritten, 0);
            assert_eq!(
                snapshot.records.len(),
                groups
                    .iter()
                    .filter(|group| group.contexts[0].1.is_some())
                    .count()
            );
            let mut spans = HashSet::new();
            for group in groups.iter() {
                assert!(group.ids.len() <= 4);
                let parent = &parents[group.ids[0] as usize];
                assert!(
                    group.ids.iter().all(|id| &parents[*id as usize] == parent),
                    "batch merged different causal parents"
                );
                assert_eq!(
                    group
                        .contexts
                        .iter()
                        .map(|(stage, _)| *stage)
                        .collect::<Vec<_>>(),
                    ["construct", "poll", "resume", "drop"]
                );
                let context = &group.contexts[0].1;
                match parent {
                    Some(parent) => {
                        let context = context.as_ref().expect("batch lost causal context");
                        assert_eq!(context.trace_id(), parent.trace_id());
                        assert_eq!(context.parent_span_id(), Some(parent.span_id()));
                        assert_ne!(context.span_id(), parent.span_id());
                        assert!(spans.insert(context.span_id()));
                        let record = snapshot
                            .records
                            .iter()
                            .find(|record| record.span_id == context.span_id())
                            .expect("actual batch missing record");
                        assert_eq!(record.kind, parrot_api::trace::SpanKind::BusinessBatch);
                        assert_eq!(record.status, parrot_api::trace::SpanStatus::Ok);
                        assert_eq!(record.parent_span_id, Some(parent.span_id()));
                        assert_eq!(record.target, "/batch-trace");
                    }
                    None => assert!(context.is_none()),
                }
                assert!(
                    group
                        .contexts
                        .iter()
                        .all(|(_, observed)| observed == context)
                );
            }
        });
}
#[test]
fn shared_batches_split_causal_parents_and_preserve_eligible_dispatch() {
    batch_contexts(parrot::thread::SchedulingMode::SharedPool {
        max_messages_per_run: 100,
    });
}
#[test]
fn dedicated_batches_split_causal_parents_and_preserve_eligible_dispatch() {
    batch_contexts(parrot::thread::SchedulingMode::DedicatedThread);
}
#[test]
fn sharded_batches_split_causal_parents_and_preserve_eligible_dispatch() {
    batch_contexts(parrot::thread::SchedulingMode::Sharded {
        affinity_key: "batch-trace".into(),
    });
}

struct RawRequest(Request);
#[derive(Debug)]
struct Normalize;
impl parrot::thread::mailbox::admission::MessageAdmission for Normalize {
    fn prepare(
        &self,
        message: BoxedMessage,
    ) -> Result<BoxedMessage, parrot::thread::error::MailboxError> {
        let raw = message.downcast::<RawRequest>().map_err(|_| {
            parrot::thread::error::MailboxError::PushError(
                "admission saw framework envelope instead of user payload".into(),
            )
        })?;
        Ok(Box::new(raw.0))
    }
}
#[test]
fn traced_tell_admission_normalizes_payload_without_losing_parent() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let events = Events::default();
            let system = parrot::thread::ThreadActorSystem::shared(Default::default());
            let actor = bounded(system.spawn_at(
                ThreadProbe(events.clone()),
                "/trace-admission",
                None,
                parrot::thread::ThreadActorConfig {
                    message_admission: Some(Arc::new(Normalize)),
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
            let parent = TraceContext::root();
            let poller = TraceContext::root();
            let (finished, done) = tokio::sync::oneshot::channel();
            let call =
                parent.in_scope(|| actor.send(Box::new(RawRequest(Request { id: 0, finished }))));
            let ask = bounded(poller.scope(call)).await;
            let ask_completed = bounded(done).await;
            let (finished, done) = tokio::sync::oneshot::channel();
            let call = parent
                .in_scope(|| actor.deliver(Box::new(RawRequest(Request { id: 1, finished }))));
            let tell = bounded(poller.scope(call)).await;
            let tell_completed = bounded(done).await;
            bounded(actor.stop()).await.unwrap();
            bounded(system.shutdown_internal()).await.unwrap();
            assert_eq!(*ask.unwrap().downcast::<usize>().unwrap(), 0);
            ask_completed.unwrap();
            tell.expect("tracing must preserve the admission payload contract");
            tell_completed.unwrap();
            check(&events, &[Some(parent.clone()), Some(parent)], true);
        });
}

enum FaultAction {
    Normal,
    Error,
    PollPanic,
    ConstructPanic,
    Wait(tokio::sync::oneshot::Sender<()>),
}
struct FaultRequest {
    request: Request,
    action: FaultAction,
}
async fn fault_calls(
    actor: &dyn ActorRef,
    drain_timeout: bool,
    recorder: &parrot_api::trace::TraceRecorder,
) -> Vec<Option<TraceContext>> {
    let parent = recorder.root();
    let poller = TraceContext::root();
    let mut parents = Vec::new();
    for action in [
        FaultAction::Error,
        FaultAction::PollPanic,
        FaultAction::ConstructPanic,
    ] {
        let id = parents.len();
        let (finished, done) = tokio::sync::oneshot::channel();
        let request = FaultRequest {
            request: Request { id, finished },
            action,
        };
        let future = parent.in_scope(|| actor.send(Box::new(request)));
        assert!(bounded(poller.scope(future)).await.is_err());
        bounded(done).await.unwrap();
        parents.push(Some(parent.clone()));
        let id = parents.len();
        let (finished, done) = tokio::sync::oneshot::channel();
        let future = actor.send(Box::new(Request { id, finished }));
        let reply = bounded(poller.scope(future)).await.unwrap();
        assert_eq!(*reply.downcast::<usize>().unwrap(), id);
        bounded(done).await.unwrap();
        parents.push(None);
    }
    let id = parents.len();
    let (finished, done) = tokio::sync::oneshot::channel();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let mut future = parent.in_scope(|| {
        actor.send(Box::new(FaultRequest {
            request: Request { id, finished },
            action: FaultAction::Wait(entered),
        }))
    });
    poller.in_scope(|| poll_pending(&mut future));
    bounded(waiting).await.unwrap();
    let stopped = bounded(actor.stop()).await;
    if drain_timeout {
        assert!(
            matches!(
                stopped,
                Err(parrot_api::errors::ActorError::TimeoutDetail(_))
            ),
            "pending dynamic Thread drain must report its configured deadline: {stopped:?}"
        );
    } else {
        stopped.unwrap();
    }
    assert!(bounded(future).await.is_err());
    bounded(done).await.unwrap();
    parents.push(Some(parent));
    assert!(TraceContext::current().is_none());
    parents
}
fn check_faults(events: &Events, parents: &[Option<TraceContext>]) {
    let events = events.lock().unwrap();
    let mut spans = HashSet::new();
    assert_eq!(events.len(), 26);
    for (id, parent) in parents.iter().enumerate() {
        let actual: Vec<_> = events.iter().filter(|event| event.0 == id).collect();
        let stages: &[&str] = if id == 4 {
            &["construct", "drop"]
        } else {
            &["construct", "poll", "resume", "drop"]
        };
        assert_eq!(
            actual.iter().map(|event| event.1).collect::<Vec<_>>(),
            stages
        );
        let observed = &actual[0].2;
        match parent {
            Some(parent) => {
                let observed = observed.as_ref().expect("fault dispatch lost trace");
                assert_eq!(observed.trace_id(), parent.trace_id());
                assert_eq!(observed.parent_span_id(), Some(parent.span_id()));
                assert_ne!(observed.span_id(), parent.span_id());
                assert!(spans.insert(observed.span_id()));
            }
            None => assert!(observed.is_none()),
        }
        assert!(actual.iter().all(|event| &event.2 == observed));
    }
}
#[test]
fn thread_dynamic_error_panic_cancel_dispose_inside_context_then_isolate_next_message() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let events = Events::default();
            let system = parrot::thread::ThreadActorSystem::shared(
                parrot::thread::ThreadActorSystemConfig {
                    shutdown_timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            );
            let actor = bounded(system.spawn_at(
                ThreadProbe(events.clone()),
                "/trace-fault",
                None,
                parrot::thread::ThreadActorConfig {
                    supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
            let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
            let parents = fault_calls(&actor, true, &recorder).await;
            bounded(async {
                while system
                    .actor_paths()
                    .iter()
                    .any(|path| path == "/trace-fault")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            bounded(system.shutdown_internal()).await.unwrap();
            check_faults(&events, &parents);
            use parrot_api::trace::SpanStatus::*;
            recording_receipt(
                &recorder,
                &events,
                parrot_api::trace::SpanKind::DynamicHandler,
                &[(0, Error), (2, Panicked), (4, Panicked), (6, Cancelled)],
            );
        });
}
#[test]
fn actix_dynamic_error_panic_cancel_dispose_inside_context_then_isolate_next_message() {
    actix::System::new().block_on(async {
        let events = Events::default();
        let system = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let strategy = parrot_api::supervisor::SupervisorStrategyType::Default(
            parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
        );
        let actor = bounded(system.spawn_root_typed_with_strategy(
            ActixProbe::<true>(events.clone()),
            EmptyConfig,
            strategy,
        ))
        .await
        .unwrap();
        let recorder = parrot_api::trace::TraceRecorder::new(64).unwrap();
        let parents = fault_calls(actor.as_ref(), false, &recorder).await;
        bounded(system.as_ref().clone().shutdown()).await.unwrap();
        check_faults(&events, &parents);
        use parrot_api::trace::SpanStatus::*;
        recording_receipt(
            &recorder,
            &events,
            parrot_api::trace::SpanKind::DynamicHandler,
            &[(0, Error), (2, Panicked), (4, Panicked), (6, Cancelled)],
        );
    });
}

fn raw_envelopes(batch_size: Option<usize>) {
    use parrot::thread::{
        BackpressureStrategy,
        actor::ThreadActor,
        envelope::AskEnvelope,
        mailbox::{Mailbox, mpsc::MpscMailbox},
        processor::{ActorProcessor, ProcessorInterface},
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let events = Events::default();
            let processor = Arc::new(ActorProcessor::new(
                ThreadActor::new_for_test(HelperProbe(events.clone())),
                parrot::thread::ThreadContext::new_for_test("/raw-trace"),
                "/raw-trace".into(),
                parrot::thread::ThreadActorConfig {
                    message_batch_size: batch_size,
                    ..Default::default()
                },
            ));
            bounded(processor.clone().initialize_and_start_erased())
                .await
                .unwrap();
            let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
                32,
                parrot_api::address::ActorPath::for_test("/raw-trace"),
            ));
            let parent = TraceContext::root();
            let poller = TraceContext::root();
            let mut parents = Vec::new();
            for constructor in 0..4 {
                for legacy in [false, true] {
                    for traced in [true, false] {
                        let id = parents.len();
                        let make = || match constructor {
                            0 => AskEnvelope::new_inline(id as u64),
                            1 => AskEnvelope::new_typed(id as u64),
                            2 => AskEnvelope::with_boxed(Box::new(id as u64)),
                            3 => AskEnvelope::with_boxed_options(
                                Box::new(id as u64),
                                MessageOptions::default(),
                            ),
                            _ => unreachable!(),
                        };
                        let (envelope, reply) = if traced {
                            parent.in_scope(make)
                        } else {
                            make()
                        };
                        if legacy {
                            bounded(mailbox.push(Box::new(envelope), BackpressureStrategy::Block))
                                .await
                                .unwrap();
                        } else {
                            bounded(mailbox.push_ask(envelope, BackpressureStrategy::Block))
                                .await
                                .unwrap();
                        }
                        assert_eq!(
                            bounded(
                                poller
                                    .scope(processor.process_batch_of_messages(&mailbox, 1, false))
                            )
                            .await
                            .unwrap(),
                            (1, 0)
                        );
                        assert_eq!(
                            *bounded(reply)
                                .await
                                .unwrap()
                                .unwrap()
                                .downcast::<usize>()
                                .unwrap(),
                            id
                        );
                        parents.push(traced.then(|| parent.clone()));
                    }
                }
            }
            bounded(processor.clone().stop_erased()).await.unwrap();
            bounded(processor.clone().retire_erased()).await.unwrap();
            check(&events, &parents, true);
        });
}
#[test]
fn raw_by_value_legacy_inline_dispatch_clears_ambient_poller() {
    raw_envelopes(None);
}
#[test]
fn raw_batch_barriers_keep_envelope_context_and_clear_ambient_poller() {
    raw_envelopes(Some(4));
}

impl parrot_api::message::Message for Request {
    type Result = usize;
}
#[test]
fn native_actix_envelope_constructors_retain_creation_context() {
    actix::System::new().block_on(async {
        let events = Events::default();
        let system = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let actor =
            bounded(system.spawn_root_typed(ActixProbe::<true>(events.clone()), EmptyConfig))
                .await
                .unwrap();
        let native = actor
            .as_any()
            .downcast_ref::<parrot::actix::reference::ActixActorRef<ActixProbe<true>>>()
            .unwrap();
        let parent = TraceContext::root();
        let poller = TraceContext::root();
        let mut parents = Vec::new();
        for constructor in 0..4 {
            for traced in [true, false] {
                let id = parents.len();
                let (finished, done) = tokio::sync::oneshot::channel();
                let request = Request { id, finished };
                let make = || {
                    use parrot::actix::message::ActixMessageWrapper;
                    use parrot_api::message::MessageEnvelope;
                    match constructor {
                        0 => ActixMessageWrapper {
                            envelope: MessageEnvelope::new(request, None, None),
                        },
                        1 => ActixMessageWrapper {
                            envelope: MessageEnvelope::from_boxed(
                                Box::new(request),
                                None,
                                MessageOptions::default(),
                            ),
                        },
                        2 => ActixMessageWrapper {
                            envelope: parrot::actix::message::create_envelope(request),
                        },
                        3 => native.create_envelope(
                            Box::new(request),
                            None,
                            MessageOptions::default(),
                            "Request",
                        ),
                        _ => unreachable!(),
                    }
                };
                let wrapper = if traced {
                    parent.in_scope(make)
                } else {
                    make()
                };
                let reply = bounded(poller.scope(native.get_addr().send(wrapper)))
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(*reply.downcast::<usize>().unwrap(), id);
                bounded(done).await.unwrap();
                parents.push(traced.then(|| parent.clone()));
            }
        }
        bounded(actor.stop()).await.unwrap();
        bounded(system.as_ref().clone().shutdown()).await.unwrap();
        check(&events, &parents, true);
    });
}

type ChainReply = Vec<Option<TraceContext>>;
struct ChainRequest;
impl parrot_api::message::Message for ChainRequest {
    type Result = ChainReply;
}
fn chain(
    next: &Option<parrot_api::types::BoxedActorRef>,
) -> BoxedFuture<'_, ActorResult<ChainReply>> {
    let constructed = TraceContext::current();
    Box::pin(async move {
        tokio::task::yield_now().await;
        let mut contexts = vec![constructed, TraceContext::current()];
        if let Some(next) = next {
            contexts.extend(
                *next
                    .send(Box::new(ChainRequest))
                    .await?
                    .downcast::<ChainReply>()
                    .unwrap(),
            );
        }
        Ok(contexts)
    })
}
struct ChainTyped(Option<parrot_api::types::BoxedActorRef>);
impl parrot_api::typed::ParrotTypedDispatch for ChainTyped {
    type Msg = ChainRequest;
    type Reply = ChainReply;
    fn dispatch(&mut self, _: ChainRequest) -> BoxedFuture<'_, ActorResult<ChainReply>> {
        chain(&self.0)
    }
}
impl parrot_api::typed::ParrotMsgVariant<ChainTyped> for ChainRequest {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: ChainReply) -> ActorResult<ChainReply> {
        Ok(reply)
    }
}
struct ChainThread(Option<parrot_api::types::BoxedActorRef>);
impl Actor for ChainThread {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(message.is::<ChainRequest>());
        let future = chain(&self.0);
        Box::pin(async move { Ok(Box::new(future.await?) as BoxedMessage) })
    }
}
struct ChainActix(Option<parrot_api::types::BoxedActorRef>);
impl Actor for ChainActix {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(message.is::<ChainRequest>());
        let future = chain(&self.0);
        Box::pin(async move { Ok(Box::new(future.await?) as BoxedMessage) })
    }
}
impl ActixEngineExt for ChainActix {}
async fn chain_calls(front: &dyn ActorRef) -> Vec<(Option<TraceContext>, ChainReply)> {
    let a = TraceContext::root();
    let b = TraceContext::root();
    let poller = TraceContext::root();
    let mut replies = Vec::new();
    for parent in [Some(a.clone()), Some(b), None, Some(a)] {
        let future = match &parent {
            Some(parent) => parent.in_scope(|| front.send(Box::new(ChainRequest))),
            None => front.send(Box::new(ChainRequest)),
        };
        let reply = bounded(poller.scope(future)).await.unwrap();
        replies.push((parent, *reply.downcast::<ChainReply>().unwrap()));
    }
    replies
}
fn check_chain(replies: Vec<(Option<TraceContext>, ChainReply)>) {
    let mut spans = HashSet::new();
    for (parent, replies) in replies {
        assert_eq!(replies.len(), 6);
        let mut parent = parent;
        for pair in replies.as_chunks::<2>().0 {
            assert_eq!(pair[0], pair[1], "handler construction and poll disagree");
            match &parent {
                Some(parent) => {
                    let context = pair[0]
                        .as_ref()
                        .expect("mixed dynamic/typed hop lost trace");
                    assert_eq!(context.trace_id(), parent.trace_id());
                    assert_eq!(context.parent_span_id(), Some(parent.span_id()));
                    assert_ne!(context.span_id(), parent.span_id());
                    assert!(spans.insert(context.span_id()));
                }
                None => assert!(pair[0].is_none()),
            }
            parent = pair[0].clone();
        }
    }
}
#[test]
fn thread_dynamic_to_typed_to_dynamic_keeps_each_actual_parent() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let system = parrot::thread::ThreadActorSystem::shared(Default::default());
            let leaf = bounded(system.spawn_root_typed_thread(ChainThread(None), EmptyConfig))
                .await
                .unwrap();
            let middle = bounded(system.spawn_typed::<ChainTyped, ChainRequest>(
                ChainTyped(Some(leaf.clone_boxed())),
                "/middle",
            ))
            .await
            .unwrap();
            let front = bounded(system.spawn_root_typed_thread(
                ChainThread(Some(middle.clone().into_dyn())),
                EmptyConfig,
            ))
            .await
            .unwrap();
            let replies = chain_calls(&front).await;
            bounded(front.stop()).await.unwrap();
            bounded(middle.stop()).await.unwrap();
            bounded(leaf.stop()).await.unwrap();
            bounded(system.shutdown_internal()).await.unwrap();
            check_chain(replies);
        });
}
#[test]
fn actix_dynamic_to_typed_to_dynamic_keeps_each_actual_parent() {
    actix::System::new().block_on(async {
        let system = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let leaf = bounded(system.spawn_root_typed(ChainActix(None), EmptyConfig))
            .await
            .unwrap();
        let middle = bounded(system.spawn_typed::<ChainTyped, ChainRequest>(
            ChainTyped(Some(leaf.clone_boxed())),
            "/middle",
        ))
        .await
        .unwrap();
        let front = bounded(
            system.spawn_root_typed(ChainActix(Some(middle.clone().into_dyn())), EmptyConfig),
        )
        .await
        .unwrap();
        let replies = chain_calls(front.as_ref()).await;
        bounded(front.stop()).await.unwrap();
        bounded(middle.stop()).await.unwrap();
        bounded(leaf.stop()).await.unwrap();
        bounded(system.as_ref().clone().shutdown()).await.unwrap();
        check_chain(replies);
    });
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

struct SyncOutcomeProbe(Events);
impl Actor for SyncOutcomeProbe {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("synchronous probe must use the engine callback")
    }
}
impl ActixEngineExt for SyncOutcomeProbe {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
        _: EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        let request = *message.downcast::<FaultRequest>().unwrap();
        let guard = begin(&self.0, Box::new(request.request));
        guard.record("sync");
        match request.action {
            FaultAction::Error => Some(Err(parrot_api::errors::ActorError::InternalError(
                "sync business error".into(),
            ))),
            FaultAction::PollPanic => panic!("sync callback panic"),
            FaultAction::Normal => None,
            _ => unreachable!(),
        }
    }
}
#[test]
fn actix_synchronous_recording_classifies_error_panic_and_unhandled_callback() {
    actix::System::new().block_on(async {
        let events=Events::default();
        let system=Arc::new(parrot::actix::ActixActorSystem::with_arbiter_count(1).await.unwrap());
        let strategy=parrot_api::supervisor::SupervisorStrategyType::Default(parrot_api::supervisor::DefaultStrategy::ResumeOnFailure);
        let actor=bounded(system.spawn_root_typed_with_strategy(SyncOutcomeProbe(events.clone()),EmptyConfig,strategy)).await.unwrap();
        let recorder=parrot_api::trace::TraceRecorder::new(8).unwrap(); let root=recorder.root();
        for (id,action) in [(0,FaultAction::Error),(1,FaultAction::PollPanic),(2,FaultAction::Normal)] {
            let (finished,done)=tokio::sync::oneshot::channel();
            let future=root.in_scope(|| actor.send(Box::new(FaultRequest {request:Request {id,finished},action})));
            let error=bounded(future).await.unwrap_err();
            match id {
                0=>assert!(matches!(error,parrot_api::errors::ActorError::InternalError(ref s) if s == "sync business error")),
                1=>assert!(matches!(error,parrot_api::errors::ActorError::Panic(_))),
                2=>assert!(matches!(error,parrot_api::errors::ActorError::MessageHandlingError(ref s) if s.contains("returned None"))),
                _=>unreachable!(),
            }
            bounded(done).await.unwrap();
        }
        bounded(actor.stop()).await.unwrap();
        bounded(system.as_ref().clone().shutdown()).await.unwrap();
        use parrot_api::trace::{SpanKind,SpanStatus};
        recording_receipt(&recorder,&events,SpanKind::DynamicHandler,&[(0,SpanStatus::Error),(1,SpanStatus::Panicked),(2,SpanStatus::Error)]);
    });
}
