use parrot::thread::{
    config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorConfig, ActorState, EmptyConfig},
    address::ActorRef,
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
    stops: AtomicUsize,
    config_disposed: AtomicUsize,
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
        self.0.config_disposed.fetch_add(1, Ordering::SeqCst);
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
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.state.stops.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
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
async fn case<C: ActorConfig>(
    config: impl FnOnce(&Arc<State>) -> C,
    mode: SchedulingMode,
    traced: bool,
    hold: bool,
) {
    let mode_name = format!("{mode:?}");
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1, shared_burst_workers_max: 0, max_dedicated_threads: 2,
            ..Default::default()
        });
        let sink = system.spawn_at(Sink, "/init-trace/sink", None, Default::default()).await.unwrap();
        let state = Arc::new(State::default());
        let recorder = TraceRecorder::new(8).unwrap();
        let origin = recorder.root();
        let expected = traced.then_some(Identity { trace: origin.trace_id(), span: origin.span_id() });
        let ambient = TraceContext::root();
        let actor = InitProbe::<C> {
            state: state.clone(), sink: Box::new(sink), hold, config: PhantomData,
        };
        // Build under A (or explicitly None), then poll under B. The operation
        // must retain its construction-time parent across both owner paths.
        let creation = parrot_api::trace::in_scope(traced.then(|| origin.clone()), || {
            system.spawn_typed_with_config(actor, config(&state), ThreadActorConfig {
                scheduling_mode: Some(mode), ..Default::default()
            })
        });
        let reference = ambient.scope(async {
            let reference = creation.await;
            assert_eq!(identity(), Some(Identity {
                trace: ambient.trace_id(), span: ambient.span_id(),
            }), "returning from creation must restore the polling task context");
            reference
        }).await.unwrap();
        assert!(TraceContext::current().is_none());
        state.entered.notified().await;
        assert_eq!(state.config_disposed.load(Ordering::SeqCst),
            usize::from(std::mem::needs_drop::<C>()),
            "configuration retirement must precede user initialization");
        if !hold {
            let receipt = reference.send(Box::new(1u64)).await.unwrap()
                .downcast::<Receipt>().unwrap();
            assert_eq!(receipt.value, 19);
            assert!(receipt.identity.is_none(), "initialization parent must not leak into later messages");
        }
        reference.stop().await.expect("instance stop must join initialization disposal");
        assert_eq!(state.disposed.load(Ordering::SeqCst), 1);
        assert_eq!(state.stops.load(Ordering::SeqCst), usize::from(!hold),
            "only a fully initialized actor admits the normal Stop hook");
        let phases = state.phases.lock().unwrap().clone();
        system.shutdown_internal().await.unwrap();
        assert_eq!(system.actor_count(), 0);
        assert!(TraceContext::current().is_none());
        let names: Vec<_> = phases.iter().map(|row| row.0).collect();
        if hold {
            assert_eq!(names, ["init-call", "init-poll", "init-resumed", "init-future-drop"]);
        } else {
            assert_eq!(names, ["init-call", "init-poll", "init-resumed", "init-complete", "init-future-drop"]);
        }
        assert!(phases.iter().all(|(_, actual)| *actual == expected),
            "initialization and its future disposal must retain construction-time parent: {phases:?}");
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.counts.overwritten, 0);
        assert_eq!(snapshot.retained, 0);
        assert!(!snapshot.counts.counters_overflowed);
        let records = u64::from(traced && !hold);
        assert_eq!(snapshot.counts.started, records);
        assert_eq!(snapshot.counts.completed, records);
        assert_eq!(snapshot.counts.ok, records);
        assert_eq!(snapshot.counts.drained, records);
        if traced && !hold {
            assert_eq!(snapshot.records.len(), 1,
                "real init business call must preserve the recording context across publication");
            let record = &snapshot.records[0];
            assert_eq!(record.kind, SpanKind::DynamicHandler);
            assert_eq!(record.status, SpanStatus::Ok);
            assert_eq!(record.target, "/init-trace/sink");
            assert_eq!(record.trace_id, origin.trace_id());
            assert_eq!(record.parent_span_id, Some(origin.span_id()));
        } else {
            assert!(snapshot.records.is_empty());
        }
        eprintln!("PARROT_INIT_TRACE_CASE_OK mode={mode_name} config={} traced={traced} held={hold}",
            std::any::type_name::<C>());
    }).await.expect("initialization trace independent liveness watchdog");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_initialization_keeps_creation_parent_across_schedulers_and_owners() {
    for mode in modes() {
        case(|_| EmptyConfig, mode.clone(), true, false).await;
        case(|state| DeferredConfig(state.clone()), mode, true, false).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untraced_initialization_does_not_inherit_polling_context() {
    for mode in modes() {
        case(|_| EmptyConfig, mode.clone(), false, false).await;
        case(|state| DeferredConfig(state.clone()), mode, false, false).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_initialization_disposes_its_future_inside_creation_context() {
    for mode in modes() {
        case(|_| EmptyConfig, mode.clone(), true, true).await;
        case(|state| DeferredConfig(state.clone()), mode, true, true).await;
    }
}
