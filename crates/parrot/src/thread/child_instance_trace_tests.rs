use crate::thread::{
    config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::{
        ChildSpawnReceipt, ErasedSpawnBox, ErasedSpawnable, ThreadActorSystem, TypedSpawnPayload,
    },
};
use parrot_api::{
    actor::{Actor, ActorConfig, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorSpawner, ActorSpawnerExt},
    trace::{SpanKind, SpanStatus, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    trace: [u8; 16],
    span: [u8; 8],
}
fn identity() -> Option<Identity> {
    TraceContext::current().map(|context| Identity {
        trace: context.trace_id(),
        span: context.span_id(),
    })
}
#[derive(Default)]
struct State {
    phases: Mutex<Vec<(&'static str, Option<Identity>)>>,
    entered: tokio::sync::Notify,
    disposed: AtomicUsize,
    config_disposed: AtomicUsize,
    creation_parent: Mutex<Option<Identity>>,
    config_parent: Mutex<Option<Option<Identity>>>,
    bridge_phases: Mutex<Vec<(&'static str, Option<Identity>)>>,
}
impl State {
    fn note(&self, phase: &'static str) {
        self.phases.lock().unwrap().push((phase, identity()));
    }
}
struct InitFutureOwner(Arc<State>);
impl Drop for InitFutureOwner {
    fn drop(&mut self) {
        self.0.note("init-future-drop");
    }
}
struct DeferredConfig(Arc<State>);
impl ActorConfig for DeferredConfig {}
impl Drop for DeferredConfig {
    fn drop(&mut self) {
        *self.0.config_parent.lock().unwrap() = Some(identity());
        self.0.config_disposed.fetch_add(1, Ordering::SeqCst);
    }
}
trait ChildConfig: ActorConfig {
    const CUSTOM_BRIDGE: bool;
    fn make(state: &Arc<State>) -> Self;
}
impl ChildConfig for EmptyConfig {
    const CUSTOM_BRIDGE: bool = false;
    fn make(_: &Arc<State>) -> Self {
        Self
    }
}
impl ChildConfig for DeferredConfig {
    const CUSTOM_BRIDGE: bool = true;
    fn make(state: &Arc<State>) -> Self {
        Self(state.clone())
    }
}
struct Parent<C> {
    state: Arc<State>,
    sink: BoxedActorRef,
    traced: bool,
    hold: bool,
    config: PhantomData<fn() -> C>,
}
impl<C: ChildConfig> Actor for Parent<C> {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            assert!(message.is::<()>());
            let parent = TraceContext::current().expect("actual traced spawning handler");
            let ambient = TraceContext::root();
            let child = InitProbe::<C> {
                state: self.state.clone(),
                sink: self.sink.clone_boxed(),
                hold: self.hold,
                config: PhantomData,
            };
            let creation = parrot_api::trace::in_scope(self.traced.then(|| parent.clone()), || {
                *self.state.creation_parent.lock().unwrap() = identity();
                if C::CUSTOM_BRIDGE {
                    let bridge = ObservedBridge {
                        payload: TypedSpawnPayload::new(child),
                        state: self.state.clone(),
                    };
                    ActorSpawner::spawn(
                        ctx,
                        Box::new(ErasedSpawnBox(Box::new(bridge))),
                        Box::new(C::make(&self.state)),
                    )
                } else {
                    ctx.spawn_typed(child, C::make(&self.state))
                }
            });
            let reference = ambient
                .scope(async {
                    let result = creation.await;
                    assert_eq!(
                        identity(),
                        Some(Identity {
                            trace: ambient.trace_id(),
                            span: ambient.span_id()
                        }),
                        "child creation must restore polling context"
                    );
                    result
                })
                .await?;
            assert_eq!(
                identity(),
                Some(Identity {
                    trace: parent.trace_id(),
                    span: parent.span_id()
                }),
                "child creation must restore the parent handler context"
            );
            Ok(Box::new(reference) as BoxedMessage)
        })
    }
}
struct Receipt {
    value: u64,
    identity: Option<Identity>,
}
struct Sink;
impl Actor for Sink {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = *message
                .downcast::<u64>()
                .expect("sink receives a real numeric request");
            Ok(Box::new(Receipt {
                value: value + 1,
                identity: identity(),
            }) as BoxedMessage)
        })
    }
}
struct InitProbe<C> {
    state: Arc<State>,
    sink: BoxedActorRef,
    hold: bool,
    config: PhantomData<fn() -> C>,
}
// Exercise the internal erased bridge as well as the typed convenience
// path. Both delegate publication to the real engine; no mock actor is used.
struct ObservedBridge<C> {
    payload: TypedSpawnPayload<InitProbe<C>>,
    state: Arc<State>,
}
#[async_trait::async_trait]
impl<C: ChildConfig> ErasedSpawnable for ObservedBridge<C> {
    fn accepts_child_config(&self, config: &BoxedMessage) -> bool {
        self.state
            .bridge_phases
            .lock()
            .unwrap()
            .push(("validation", identity()));
        self.payload.accepts_child_config(config)
    }
    fn spawn_child_on(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        engine: ThreadActorConfig,
        parent: BoxedActorRef,
        lifetime: ChildSpawnReceipt,
    ) -> ActorResult<BoxedActorRef> {
        self.state
            .bridge_phases
            .lock()
            .unwrap()
            .push(("publication", identity()));
        Box::new(self.payload).spawn_child_on(system, engine, parent, lifetime)
    }
    async fn spawn_on(
        self: Box<Self>,
        _: Arc<ThreadActorSystem>,
        _: BoxedMessage,
        _: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        panic!("child creation must use the real child admission path");
    }
}
impl<C> Drop for InitProbe<C> {
    fn drop(&mut self) {
        self.state.disposed.fetch_add(1, Ordering::SeqCst);
    }
}
impl<C: ActorConfig> Actor for InitProbe<C> {
    type Config = C;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        assert_eq!(
            self.state.config_disposed.load(Ordering::SeqCst),
            usize::from(std::mem::needs_drop::<C>()),
            "child configuration must retire before initialization starts"
        );
        self.state.note("init-call");
        Box::pin(async move {
            let _owner = InitFutureOwner(self.state.clone());
            self.state.note("init-poll");
            tokio::task::yield_now().await;
            self.state.note("init-resumed");
            // A stop issued by the test now cancels the suspended init body,
            // rather than racing its first cooperative yield.
            self.state.entered.notify_one();
            if self.hold {
                std::future::pending::<()>().await;
            }
            let reply = self
                .sink
                .send(Box::new(7u64))
                .await?
                .downcast::<Receipt>()
                .expect("real init request receipt");
            assert_eq!(reply.value, 8);
            let caller = identity();
            match (caller, reply.identity) {
                (Some(parent), Some(child)) => {
                    assert_eq!(child.trace, parent.trace);
                    assert_ne!(child.span, parent.span);
                }
                (None, None) => {}
                mismatch => panic!("initialization business trace mismatch: {mismatch:?}"),
            }
            self.state.note("init-complete");
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Ok(Box::new(Receipt {
                value: 19,
                identity: identity(),
            }) as BoxedMessage)
        })
    }
}
fn modes() -> Vec<SchedulingMode> {
    vec![
        SchedulingMode::SharedPool {
            max_messages_per_run: 64,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "initialization".into(),
        },
    ]
}
async fn case<C: ChildConfig>(mode: SchedulingMode, traced: bool, hold: bool) {
    let mode_name = format!("{mode:?}");
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            max_dedicated_threads: 4,
            default_scheduling_mode: mode,
            ..Default::default()
        });
        let sink = system
            .spawn_at(Sink, "/child-init/sink", None, Default::default())
            .await
            .unwrap();
        let state = Arc::new(State::default());
        let parent = system
            .spawn_at(
                Parent::<C> {
                    state: state.clone(),
                    sink: Box::new(sink),
                    traced,
                    hold,
                    config: PhantomData,
                },
                "/child-init/parent",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let recorder = TraceRecorder::new(8).unwrap();
        let origin = recorder.root();
        let call = origin.in_scope(|| parent.send(Box::new(())));
        let response = call.await.unwrap();
        let child = *response
            .downcast::<BoxedActorRef>()
            .expect("actual child creation receipt");
        state.entered.notified().await;
        assert_eq!(
            state.config_disposed.load(Ordering::SeqCst),
            usize::from(std::mem::needs_drop::<C>())
        );
        if !hold {
            let receipt = child
                .send(Box::new(1u64))
                .await
                .unwrap()
                .downcast::<Receipt>()
                .unwrap();
            assert_eq!(receipt.value, 19);
            assert!(
                receipt.identity.is_none(),
                "child initialization parent must not leak into later messages"
            );
        }
        child
            .stop()
            .await
            .expect("child stop must join initialization disposal");
        parent
            .stop()
            .await
            .expect("parent retirement must join its owned child");
        system.shutdown_internal().await.unwrap();
        assert_eq!(system.actor_count(), 0);
        assert_eq!(state.disposed.load(Ordering::SeqCst), 1);
        assert!(TraceContext::current().is_none());
        let phases = state.phases.lock().unwrap().clone();
        let names: Vec<_> = phases.iter().map(|row| row.0).collect();
        if hold {
            assert_eq!(
                names,
                ["init-call", "init-poll", "init-resumed", "init-future-drop"]
            );
        } else {
            assert_eq!(
                names,
                [
                    "init-call",
                    "init-poll",
                    "init-resumed",
                    "init-complete",
                    "init-future-drop"
                ]
            );
        }
        let expected = *state.creation_parent.lock().unwrap();
        assert_eq!(expected.is_some(), traced);
        assert_eq!(
            *state.config_parent.lock().unwrap(),
            std::mem::needs_drop::<C>().then_some(expected),
            "child configuration disposal must preserve the creation parent"
        );
        let bridges = state.bridge_phases.lock().unwrap().clone();
        let expected_bridges = if C::CUSTOM_BRIDGE {
            vec![("validation", expected), ("publication", expected)]
        } else {
            Vec::new()
        };
        assert_eq!(
            bridges, expected_bridges,
            "child validation and publication must preserve the creation parent"
        );
        assert!(
            phases.iter().all(|(_, actual)| *actual == expected),
            "child initialization must retain its creation parent through disposal: {phases:?}"
        );
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.counts.overwritten, 0);
        assert_eq!(snapshot.retained, 0);
        assert!(!snapshot.counts.counters_overflowed);
        let records = 1 + u64::from(traced && !hold);
        assert_eq!(snapshot.counts.started, records);
        assert_eq!(snapshot.counts.completed, records);
        assert_eq!(snapshot.counts.ok, records);
        assert_eq!(snapshot.counts.drained, records);
        assert_eq!(snapshot.records.len() as u64, records);
        assert!(
            snapshot
                .records
                .iter()
                .all(|record| record.kind == SpanKind::DynamicHandler
                    && record.status == SpanStatus::Ok
                    && record.trace_id == origin.trace_id())
        );
        let parent_record = snapshot
            .records
            .iter()
            .find(|r| r.target == "/child-init/parent")
            .unwrap();
        assert_eq!(parent_record.parent_span_id, Some(origin.span_id()));
        if traced {
            assert_eq!(
                expected.unwrap(),
                Identity {
                    trace: parent_record.trace_id,
                    span: parent_record.span_id
                }
            );
        }
        let sink_record = snapshot
            .records
            .iter()
            .find(|r| r.target == "/child-init/sink");
        if traced && !hold {
            assert_eq!(
                sink_record.unwrap().parent_span_id,
                Some(parent_record.span_id),
                "real child init request must descend directly from the spawning parent handler"
            );
        } else {
            assert!(sink_record.is_none());
        }
        eprintln!(
            "PARROT_CHILD_INIT_CASE_OK mode={mode_name} config={} traced={traced} held={hold}",
            std::any::type_name::<C>()
        );
    })
    .await
    .expect("child initialization independent liveness watchdog");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_initialization_keeps_spawning_handler_parent_across_schedulers() {
    for mode in modes() {
        case::<EmptyConfig>(mode.clone(), true, false).await;
        case::<DeferredConfig>(mode, true, false).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untraced_child_creation_does_not_inherit_parent_or_polling_context() {
    for mode in modes() {
        case::<EmptyConfig>(mode.clone(), false, false).await;
        case::<DeferredConfig>(mode, false, false).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_child_initialization_disposes_inside_creation_context() {
    for mode in modes() {
        case::<EmptyConfig>(mode.clone(), true, true).await;
        case::<DeferredConfig>(mode, true, true).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_untraced_child_initialization_disposes_without_ambient_context() {
    for mode in modes() {
        case::<EmptyConfig>(mode.clone(), false, true).await;
        case::<DeferredConfig>(mode, false, true).await;
    }
}
