//! Idle clocks follow independently executing actors after their host exits.
use futures::FutureExt;
use parrot::{
    actix::ActixActorSystem,
    thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    actor::{Actor, ActorState},
    address::{ActorRef, RetirementStatus},
    context::{ActorContext, LifecycleEvent},
    engine::AsyncEngineRuntime,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc, oneshot};

const WAIT: Duration = Duration::from_secs(5);
const INTERVAL: Duration = Duration::from_millis(60);
type Tick = (Instant, Option<[u8; 16]>);

#[derive(Default)]
struct StopGate {
    entered: Notify,
    changed: Notify,
    open: AtomicBool,
}
impl StopGate {
    fn release(&self) {
        self.open.store(true, Ordering::Release);
        self.changed.notify_one();
    }
    async fn wait(&self) {
        while !self.open.load(Ordering::Acquire) {
            self.changed.notified().await;
        }
    }
}
struct Release(Arc<StopGate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[derive(Default)]
struct State {
    drops: AtomicUsize,
    stops: AtomicUsize,
    completed: AtomicUsize,
}
enum Request {
    Configure(Option<Duration>),
    Read,
    Hold(oneshot::Sender<()>, Arc<StopGate>),
}
#[derive(Debug)]
struct Reply {
    timeout: Option<Duration>,
    trace: Option<[u8; 16]>,
    worker: String,
}
struct Probe {
    events: mpsc::UnboundedSender<Tick>,
    state: Arc<State>,
    stop_gate: Option<Arc<StopGate>>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.state.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Probe {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
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
            if matches!(
                message.downcast_ref::<LifecycleEvent>(),
                Some(LifecycleEvent::ReceiveTimeout)
            ) {
                self.events
                    .send((
                        Instant::now(),
                        TraceContext::current().map(|t| t.trace_id()),
                    ))
                    .unwrap();
                return Ok(Box::new(()) as BoxedMessage);
            }
            match *message.downcast::<Request>().unwrap() {
                Request::Configure(timeout) => context.set_receive_timeout(timeout),
                Request::Read => {}
                Request::Hold(entered, gate) => {
                    entered.send(()).unwrap();
                    gate.wait().await;
                    self.state.completed.fetch_add(1, Ordering::SeqCst);
                }
            }
            Ok(Box::new(Reply {
                timeout: context.receive_timeout(),
                trace: TraceContext::current().map(|t| t.trace_id()),
                worker: std::thread::current().name().unwrap_or("").to_owned(),
            }) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.state.stops.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.stop_gate {
                gate.entered.notify_one();
                gate.wait().await;
            }
            Ok(())
        })
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Dedicated,
    Sharded,
    Actix,
}
enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
impl Engine {
    async fn new(mode: Mode) -> Self {
        match mode {
            Mode::Dedicated | Mode::Sharded => {
                Self::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
                    shared_pool_size: 1,
                    shared_burst_workers_max: 0,
                    max_context_tasks: 0,
                    shutdown_timeout: WAIT,
                    default_scheduling_mode: match mode {
                        Mode::Dedicated => SchedulingMode::DedicatedThread,
                        _ => SchedulingMode::Sharded {
                            affinity_key: "idle-host-exit".into(),
                        },
                    },
                    ..Default::default()
                }))
            }
            Mode::Actix => Self::Actix(Arc::new(
                ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
                    arbiter_count: 1,
                    max_context_tasks: 0,
                    ..Default::default()
                })
                .await
                .unwrap(),
            )),
        }
    }
    fn runtime(&self) -> &dyn AsyncEngineRuntime {
        match self {
            Self::Thread(system) => system.as_ref(),
            Self::Actix(system) => system.as_ref(),
        }
    }
    async fn close(&self) {
        for _ in 0..2 {
            match self {
                Self::Thread(system) => {
                    let result = system.shutdown_internal().await;
                    assert!(
                        matches!(result,
                            Err(parrot::thread::error::SystemError::ShutdownError(ref text))
                            if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"),
                        "the departed borrowed shared executor retains its actual outcome: {result:?}"
                    );
                    assert_eq!(system.actor_count(), 0);
                }
                Self::Actix(system) => {
                    system.as_ref().clone().shutdown().await.unwrap();
                    assert_eq!(system.status().active_actors, 0);
                    assert_eq!(system.status().resources.thread_count, 0);
                }
            }
        }
    }
}
async fn reply(actor: &dyn ActorRef, request: Request, cause: &Option<TraceContext>) -> Reply {
    let request = trace::in_scope(cause.clone(), || actor.send(Box::new(request)));
    let result = tokio::time::timeout(WAIT, trace::scope(Some(TraceContext::root()), request))
        .await
        .expect("independent actor did not reply")
        .unwrap();
    let reply = *result.downcast::<Reply>().unwrap();
    assert_eq!(reply.trace, cause.as_ref().map(|t| t.trace_id()));
    reply
}
async fn tick(events: &mut mpsc::UnboundedReceiver<Tick>) -> Instant {
    let (at, trace) = tokio::time::timeout(WAIT, events.recv())
        .await
        .expect("idle clock did not deliver after host exit")
        .expect("actor disposed before its idle event");
    assert_eq!(trace, None, "maintenance must not inherit a business trace");
    at
}
async fn silent(events: &mut mpsc::UnboundedReceiver<Tick>) {
    assert!(
        tokio::time::timeout(INTERVAL * 2, events.recv())
            .await
            .is_err(),
        "disabled or busy clock emitted an idle event, or lost its actor"
    );
}
async fn retired(actor: &dyn ActorRef, state: &State, events: &mut mpsc::UnboundedReceiver<Tick>) {
    for _ in 0..2 {
        tokio::time::timeout(WAIT, actor.stop())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(matches!(
        actor.retirement_status(),
        RetirementStatus::Retired(Ok(()))
    ));
    assert_eq!(state.drops.load(Ordering::SeqCst), 1);
    assert_eq!(state.stops.load(Ordering::SeqCst), 1);
    while events.try_recv().is_ok() {}
    assert!(
        tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .is_none()
    );
    assert!(actor.send(Box::new(Request::Read)).await.is_err());
}

fn contract(mode: Mode, before: bool, cancel_waiter: bool) {
    for traced in [false, true] {
        let state = Arc::new(State::default());
        let gate = cancel_waiter.then(|| Arc::new(StopGate::default()));
        let _release = gate.as_ref().map(|g| Release(g.clone()));
        let cause = traced.then(TraceContext::root);
        let (events, mut receiver) = mpsc::unbounded_channel();
        let host_state = state.clone();
        let host_cause = cause.clone();
        let host_gate = gate.clone();
        let host = std::thread::spawn(move || {
            let host = actix::System::new();
            let (engine, actor, receiver) = host.block_on(async move {
                let engine = Engine::new(mode).await;
                let actor = engine
                    .runtime()
                    .spawn(
                        Box::new(Probe {
                            events,
                            state: host_state,
                            stop_gate: host_gate,
                        }),
                        Box::new(()),
                        Default::default(),
                    )
                    .await
                    .unwrap();
                let answer = reply(&*actor, Request::Read, &host_cause).await;
                assert_eq!(answer.timeout, None);
                match mode {
                    Mode::Dedicated => assert!(answer.worker.starts_with("parrot-dedicated-")),
                    Mode::Sharded => assert!(answer.worker.starts_with("parrot-shard-")),
                    Mode::Actix => {}
                }
                if before {
                    assert_eq!(
                        reply(&*actor, Request::Configure(Some(INTERVAL)), &host_cause)
                            .await
                            .timeout,
                        Some(INTERVAL)
                    );
                    tick(&mut receiver).await;
                }
                (engine, actor, receiver)
            });
            drop(host);
            (engine, actor, receiver)
        });
        // The old runtime and its actual OS thread are gone before later work.
        let (engine, actor, mut receiver) = host.join().unwrap();
        let host_exited = Instant::now();
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        observer.block_on(async {
            if !before {
                assert_eq!(
                    reply(&*actor, Request::Configure(Some(INTERVAL)), &cause)
                        .await
                        .timeout,
                    Some(INTERVAL)
                );
            } else {
                assert_eq!(
                    reply(&*actor, Request::Read, &cause).await.timeout,
                    Some(INTERVAL)
                );
            }
            while receiver.try_recv().is_ok() {}
            assert!(tick(&mut receiver).await >= host_exited);
            if let Some(gate) = gate {
                let mut stopped = actor.stop();
                assert!(stopped.as_mut().now_or_never().is_none());
                tokio::time::timeout(WAIT, gate.entered.notified())
                    .await
                    .unwrap();
                drop(stopped);
                while receiver.try_recv().is_ok() {}
                silent(&mut receiver).await;
                assert_eq!(state.drops.load(Ordering::SeqCst), 0);
                gate.release();
            } else {
                let (entered, entered_rx) = oneshot::channel();
                let gate = Arc::new(StopGate::default());
                let _release = Release(gate.clone());
                actor
                    .deliver(Box::new(Request::Hold(entered, gate.clone())))
                    .await
                    .unwrap();
                tokio::time::timeout(WAIT, entered_rx)
                    .await
                    .unwrap()
                    .unwrap();
                while receiver.try_recv().is_ok() {}
                silent(&mut receiver).await;
                let released = Instant::now();
                gate.release();
                let at = tick(&mut receiver).await;
                assert!(
                    at.duration_since(released) >= INTERVAL,
                    "idle period started before dispatch completed"
                );
                assert_eq!(state.completed.load(Ordering::SeqCst), 1);
                tick(&mut receiver).await;
                assert_eq!(
                    reply(&*actor, Request::Configure(None), &cause)
                        .await
                        .timeout,
                    None
                );
                while receiver.try_recv().is_ok() {}
                silent(&mut receiver).await;
                assert_eq!(
                    reply(&*actor, Request::Configure(Some(Duration::ZERO)), &cause)
                        .await
                        .timeout,
                    None
                );
                assert_eq!(
                    reply(&*actor, Request::Configure(Some(Duration::MAX)), &cause)
                        .await
                        .timeout,
                    None
                );
                assert_eq!(
                    reply(&*actor, Request::Configure(Some(INTERVAL)), &cause)
                        .await
                        .timeout,
                    Some(INTERVAL)
                );
                tick(&mut receiver).await;
            }
            retired(&*actor, &state, &mut receiver).await;
            tokio::time::timeout(WAIT, engine.close()).await.unwrap();
        });
    }
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_IDLE_HOST_CHILD").as_deref() == Ok(name) {
        body();
        let path = std::env::var_os("PARROT_IDLE_HOST_RECEIPT").unwrap();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut file, name.as_bytes()).unwrap();
        return;
    }
    let receipt = std::env::temp_dir().join(format!("parrot-idle-host-{}", uuid::Uuid::new_v4()));
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_IDLE_HOST_CHILD", name)
            .env("PARROT_IDLE_HOST_RECEIPT", &receipt)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + WAIT * 12;
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "isolated idle host contract failed: {name}: {status}"
            );
            assert_eq!(
                std::fs::read_to_string(&receipt).unwrap(),
                name,
                "requested child contract did not complete"
            );
            std::fs::remove_file(&receipt).unwrap();
            return;
        }
        assert!(
            Instant::now() < deadline,
            "host join or idle retirement blocked: {name}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident, $mode:ident, $before:expr, $cancel:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                contract(Mode::$mode, $before, $cancel)
            });
        }
    };
}
case!(
    dedicated_clock_started_before_host_exit,
    Dedicated,
    true,
    false
);
case!(sharded_clock_started_before_host_exit, Sharded, true, false);
case!(actix_clock_started_before_host_exit, Actix, true, false);
case!(
    dedicated_clock_started_after_host_exit,
    Dedicated,
    false,
    false
);
case!(sharded_clock_started_after_host_exit, Sharded, false, false);
case!(actix_clock_started_after_host_exit, Actix, false, false);
case!(
    dedicated_cancelled_stop_waiter_joins_idle_clock,
    Dedicated,
    false,
    true
);
case!(
    sharded_cancelled_stop_waiter_joins_idle_clock,
    Sharded,
    false,
    true
);
case!(
    actix_cancelled_stop_waiter_joins_idle_clock,
    Actix,
    false,
    true
);
