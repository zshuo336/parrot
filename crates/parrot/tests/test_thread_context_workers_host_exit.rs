//! Admitted generation workers must outlive the host of an independent actor.
use futures::Stream;
use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus},
    context::ActorContext,
    message::{CloneableMessage, Message},
    stream::StreamRegistryExt,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc};

const WAIT: Duration = Duration::from_secs(3);
type TraceEvent = (&'static str, Option<[u8; 16]>);

#[derive(Debug, Default)]
struct State {
    invoked: AtomicUsize,
    polled: AtomicUsize,
    completed: AtomicUsize,
    delivery_drops: AtomicUsize,
    target_drops: AtomicUsize,
    payloads: AtomicUsize,
    payload_drops: AtomicUsize,
    source_drops: AtomicUsize,
    started: AtomicUsize,
    items: AtomicUsize,
    finished: AtomicUsize,
    actor_drops: AtomicUsize,
    open: AtomicBool,
    fail_disposal: AtomicBool,
    release: Notify,
    traces: Mutex<Vec<TraceEvent>>,
}
impl State {
    fn note(&self, stage: &'static str) {
        self.traces
            .lock()
            .unwrap()
            .push((stage, TraceContext::current().map(|trace| trace.trace_id())));
    }
    fn release(&self) {
        self.open.store(true, Ordering::Release);
        self.release.notify_one();
    }
}

struct Payload(Arc<State>);
impl Payload {
    fn new(state: &Arc<State>) -> Self {
        state.payloads.fetch_add(1, Ordering::SeqCst);
        Self(state.clone())
    }
}
impl Clone for Payload {
    fn clone(&self) -> Self {
        self.0.note("payload-clone");
        Self::new(&self.0)
    }
}
impl Message for Payload {
    type Result = ();
}
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.note("payload-drop");
        self.0.payload_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct DeliveryDrop(Arc<State>);
impl Drop for DeliveryDrop {
    fn drop(&mut self) {
        self.0.note("delivery-drop");
        self.0.delivery_drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.0.fail_disposal.load(Ordering::SeqCst),
            "worker delivery disposal"
        );
    }
}
#[derive(Debug)]
struct Target(Arc<State>);
impl Drop for Target {
    fn drop(&mut self) {
        self.0.note("target-drop");
        self.0.target_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("context timers must deliver without asking")
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.note("delivery-invoke");
        self.0.invoked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let _message = message;
            let _drop = DeliveryDrop(self.0.clone());
            self.0.note("delivery-poll");
            self.0.polled.fetch_add(1, Ordering::SeqCst);
            if !self.0.open.load(Ordering::Acquire) {
                self.0.release.notified().await;
            }
            self.0.completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/context-worker-target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        panic!("the worker must retain its admitted target")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Source {
    state: Arc<State>,
    input: mpsc::UnboundedReceiver<u32>,
}
impl Stream for Source {
    type Item = u32;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<u32>> {
        self.state.note("source-poll");
        self.state.polled.fetch_add(1, Ordering::SeqCst);
        self.input.poll_recv(cx)
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.state.note("source-drop");
        self.state.source_drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.state.fail_disposal.load(Ordering::SeqCst),
            "worker source disposal"
        );
    }
}
#[derive(Clone, Copy, Debug)]
enum Route {
    Once,
    Periodic,
    Stream,
}
struct Register {
    route: Route,
    input: mpsc::UnboundedReceiver<u32>,
}
struct Probe(Arc<State>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.actor_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Register>() {
                let request = *message.downcast::<Register>().unwrap();
                match request.route {
                    Route::Once => {
                        context
                            .schedule_once(
                                Box::new(Target(self.0.clone())),
                                Box::new(Payload::new(&self.0)),
                                Duration::ZERO,
                            )
                            .await?;
                    }
                    Route::Periodic => {
                        context
                            .schedule_periodic(
                                Box::new(Target(self.0.clone())),
                                CloneableMessage::from_message(Payload::new(&self.0)),
                                Duration::ZERO,
                                Duration::from_secs(3600),
                            )
                            .await?;
                    }
                    Route::Stream => context.stream_registry().add_stream(Source {
                        state: self.0.clone(),
                        input: request.input,
                    })?,
                }
                return Ok(
                    Box::new(std::thread::current().name().unwrap_or("").to_owned())
                        as BoxedMessage,
                );
            }
            if message.is::<u32>() {
                self.0.note("stream-item");
                self.0.items.fetch_add(1, Ordering::SeqCst);
            }
            Ok(message)
        })
    }
    fn stream_started<'a>(
        &'a mut self,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.note("stream-started");
            self.0.started.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn stream_finished<'a>(
        &'a mut self,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.note("stream-finished");
            self.0.finished.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}
async fn until(mut ready: impl FnMut() -> bool) -> bool {
    tokio::time::timeout(WAIT, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok()
}
fn check(sharded: bool, registered_before_exit: bool, route: Route) {
    for traced in [false, true] {
        let host = runtime();
        let system = ThreadActorSystem::try_shared_with_handle(
            ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                default_scheduling_mode: if sharded {
                    SchedulingMode::Sharded {
                        affinity_key: "context-workers".into(),
                    }
                } else {
                    SchedulingMode::DedicatedThread
                },
                shutdown_timeout: WAIT,
                ..Default::default()
            },
            host.handle().clone(),
        )
        .unwrap();
        let state = Arc::new(State::default());
        let neighbour_state = Arc::new(State::default());
        let (actor, neighbour) = host.block_on(async {
            let actor = system
                .spawn_at(Probe(state.clone()), "/worker", None, Default::default())
                .await
                .unwrap();
            let neighbour = system
                .spawn_at(
                    Probe(neighbour_state.clone()),
                    "/neighbour",
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
            actor.send(Box::new(11u64)).await.unwrap();
            (actor, neighbour)
        });
        let driver = runtime();
        let cause = traced.then(TraceContext::root);
        let (send, input) = mpsc::unbounded_channel();
        let mut request = Some(Register { route, input });
        let register = |request| {
            let worker = driver
                .block_on(trace::scope(cause.clone(), async {
                    actor.send(Box::new(request)).await
                }))
                .unwrap();
            let worker = *worker.downcast::<String>().unwrap();
            assert!(worker.starts_with(if sharded {
                "parrot-shard-"
            } else {
                "parrot-dedicated-"
            }));
        };
        if registered_before_exit {
            register(request.take().unwrap());
            assert!(
                driver.block_on(until(|| state.polled.load(Ordering::SeqCst) > 0)),
                "live-host worker must be polled before host exit"
            );
            assert_eq!(state.source_drops.load(Ordering::SeqCst), 0);
            assert_eq!(state.target_drops.load(Ordering::SeqCst), 0);
        }
        drop(host);
        if !registered_before_exit {
            register(request.take().unwrap());
        }
        state.release();
        let submitted = !matches!(route, Route::Stream) || send.send(37).is_ok();
        drop(send);
        let progressed = driver.block_on(until(|| match route {
            Route::Stream => {
                state.items.load(Ordering::SeqCst) == 1
                    && state.finished.load(Ordering::SeqCst) == 1
            }
            _ => state.completed.load(Ordering::SeqCst) == 1,
        }));
        driver.block_on(trace::scope(Some(TraceContext::root()), async {
            assert_eq!(*neighbour.send(Box::new(19u64)).await.unwrap().downcast::<u64>().unwrap(), 19);
            actor.stop().await.unwrap();
            actor.stop().await.unwrap();
            assert!(matches!(actor.retirement_status(), RetirementStatus::Retired(Ok(()))));
            neighbour.stop().await.unwrap();
            assert_eq!(system.actor_count(), 0);
            for _ in 0..2 {
                let shutdown = system.shutdown_internal().await;
                assert!(matches!(shutdown, Err(parrot::thread::error::SystemError::ShutdownError(ref text))
                    if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{shutdown:?}");
            }
        }));
        assert_eq!(state.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(neighbour_state.actor_drops.load(Ordering::SeqCst), 1);
        assert!(
            progressed && submitted,
            "admitted context worker lost after host exit: {route:?}, before={registered_before_exit}, state={state:?}"
        );
        match route {
            Route::Stream => {
                assert_eq!(state.started.load(Ordering::SeqCst), 1);
                assert_eq!(state.items.load(Ordering::SeqCst), 1);
                assert_eq!(state.finished.load(Ordering::SeqCst), 1);
                assert_eq!(state.source_drops.load(Ordering::SeqCst), 1);
                assert_eq!(state.target_drops.load(Ordering::SeqCst), 0);
            }
            _ => {
                assert_eq!(state.invoked.load(Ordering::SeqCst), 1);
                assert_eq!(state.polled.load(Ordering::SeqCst), 1);
                assert_eq!(state.completed.load(Ordering::SeqCst), 1);
                assert_eq!(state.delivery_drops.load(Ordering::SeqCst), 1);
                assert_eq!(state.target_drops.load(Ordering::SeqCst), 1);
                let payloads = if matches!(route, Route::Periodic) {
                    2
                } else {
                    1
                };
                assert_eq!(state.payloads.load(Ordering::SeqCst), payloads);
                assert_eq!(state.payload_drops.load(Ordering::SeqCst), payloads);
            }
        }
        let events = state.traces.lock().unwrap();
        let expected = cause.map(|trace| trace.trace_id());
        assert!(!events.is_empty());
        assert!(
            events.iter().all(|(_, trace)| *trace == expected),
            "{events:?}"
        );
    }
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_CONTEXT_WORKER_CASE").as_deref() == Ok(name) {
        body();
        let path = std::env::var_os("PARROT_CONTEXT_WORKER_RECEIPT").unwrap();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut file, name.as_bytes()).unwrap();
        return;
    }
    let receipt =
        std::env::temp_dir().join(format!("parrot-context-worker-{}", uuid::Uuid::new_v4()));
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_CONTEXT_WORKER_CASE", name)
        .env("PARROT_CONTEXT_WORKER_RECEIPT", &receipt)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WAIT * 10;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            assert_eq!(
                std::fs::read_to_string(&receipt).unwrap(),
                name,
                "child did not execute the requested contract"
            );
            std::fs::remove_file(&receipt).unwrap();
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: watchdog expired");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident, $sharded:expr, $before:expr, $route:ident) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                check($sharded, $before, Route::$route)
            });
        }
    };
}
case!(dedicated_once_before_host_exit, false, true, Once);
case!(sharded_once_before_host_exit, true, true, Once);
case!(dedicated_once_after_host_exit, false, false, Once);
case!(sharded_once_after_host_exit, true, false, Once);
case!(dedicated_periodic_before_host_exit, false, true, Periodic);
case!(sharded_periodic_before_host_exit, true, true, Periodic);
case!(dedicated_periodic_after_host_exit, false, false, Periodic);
case!(sharded_periodic_after_host_exit, true, false, Periodic);
case!(dedicated_stream_before_host_exit, false, true, Stream);
case!(sharded_stream_before_host_exit, true, true, Stream);
case!(dedicated_stream_after_host_exit, false, false, Stream);
case!(sharded_stream_after_host_exit, true, false, Stream);

fn check_cancelled(sharded: bool, route: Route, failure: bool) {
    for traced in [false, true] {
        let host = runtime();
        let system = ThreadActorSystem::try_shared_with_handle(
            ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                default_scheduling_mode: if sharded {
                    SchedulingMode::Sharded {
                        affinity_key: "cancelled-workers".into(),
                    }
                } else {
                    SchedulingMode::DedicatedThread
                },
                shutdown_timeout: WAIT,
                ..Default::default()
            },
            host.handle().clone(),
        )
        .unwrap();
        let state = Arc::new(State::default());
        let neighbour_state = Arc::new(State::default());
        let actor = host
            .block_on(system.spawn_at(Probe(state.clone()), "/worker", None, Default::default()))
            .unwrap();
        let neighbour = host
            .block_on(system.spawn_at(
                Probe(neighbour_state.clone()),
                "/neighbour",
                None,
                Default::default(),
            ))
            .unwrap();
        drop(host);
        let cause = traced.then(TraceContext::root);
        let driver = runtime();
        let (send, input) = mpsc::unbounded_channel();
        driver.block_on(async {
            trace::scope(cause.clone(), async {
                actor.send(Box::new(Register { route, input })).await
            }).await.unwrap();
            assert!(until(|| state.polled.load(Ordering::SeqCst) > 0).await, "worker never entered Pending");
            assert_eq!(state.delivery_drops.load(Ordering::SeqCst), 0);
            assert_eq!(state.source_drops.load(Ordering::SeqCst), 0);
            state.fail_disposal.store(failure, Ordering::SeqCst);
            for _ in 0..2 {
                let result = trace::scope(Some(TraceContext::root()), actor.stop()).await;
                if failure {
                    let expected = if matches!(route, Route::Stream) { "worker source disposal" } else { "worker delivery disposal" };
                    assert!(matches!(&result, Err(parrot_api::errors::ActorError::Panic(message)) if message.contains(expected)), "{result:?}");
                    assert!(matches!(actor.retirement_status(), RetirementStatus::Retired(Err(_))));
                } else {
                    result.unwrap();
                    assert!(matches!(actor.retirement_status(), RetirementStatus::Retired(Ok(()))));
                }
            }
            assert_eq!(*neighbour.send(Box::new(23u64)).await.unwrap().downcast::<u64>().unwrap(), 23);
            neighbour.stop().await.unwrap();
            assert_eq!(system.actor_count(), 0);
            for _ in 0..2 {
                let result = system.shutdown_internal().await;
                assert!(matches!(result, Err(parrot::thread::error::SystemError::ShutdownError(ref message))
                    if message == "1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{result:?}");
            }
        });
        drop(send);
        assert_eq!(state.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(neighbour_state.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(state.completed.load(Ordering::SeqCst), 0);
        assert_eq!(state.items.load(Ordering::SeqCst), 0);
        assert_eq!(state.finished.load(Ordering::SeqCst), 0);
        if matches!(route, Route::Stream) {
            assert_eq!(state.source_drops.load(Ordering::SeqCst), 1);
            assert_eq!(state.started.load(Ordering::SeqCst), 1);
            assert_eq!(state.target_drops.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(state.invoked.load(Ordering::SeqCst), 1);
            assert_eq!(state.polled.load(Ordering::SeqCst), 1);
            assert_eq!(state.delivery_drops.load(Ordering::SeqCst), 1);
            assert_eq!(state.target_drops.load(Ordering::SeqCst), 1);
            let payloads = if matches!(route, Route::Periodic) {
                2
            } else {
                1
            };
            assert_eq!(state.payloads.load(Ordering::SeqCst), payloads);
            assert_eq!(state.payload_drops.load(Ordering::SeqCst), payloads);
            let events = state.traces.lock().unwrap();
            assert!(
                events
                    .iter()
                    .position(|(stage, _)| *stage == "delivery-drop")
                    .unwrap()
                    < events
                        .iter()
                        .position(|(stage, _)| *stage == "target-drop")
                        .unwrap()
            );
        }
        let events = state.traces.lock().unwrap();
        let expected = cause.map(|cause| cause.trace_id());
        assert!(!events.is_empty());
        assert!(
            events.iter().all(|(_, trace)| *trace == expected),
            "{events:?}"
        );
    }
}
macro_rules! cancelled {
    ($name:ident, $sharded:expr, $route:ident, $failure:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                check_cancelled($sharded, Route::$route, $failure)
            });
        }
    };
}
cancelled!(dedicated_once_cancelled, false, Once, false);
cancelled!(sharded_once_cancelled, true, Once, false);
cancelled!(dedicated_periodic_cancelled, false, Periodic, false);
cancelled!(sharded_periodic_cancelled, true, Periodic, false);
cancelled!(dedicated_stream_cancelled, false, Stream, false);
cancelled!(sharded_stream_cancelled, true, Stream, false);
cancelled!(dedicated_once_disposal_failure, false, Once, true);
cancelled!(sharded_once_disposal_failure, true, Once, true);
cancelled!(dedicated_periodic_disposal_failure, false, Periodic, true);
cancelled!(sharded_periodic_disposal_failure, true, Periodic, true);
cancelled!(dedicated_stream_disposal_failure, false, Stream, true);
cancelled!(sharded_stream_disposal_failure, true, Stream, true);
