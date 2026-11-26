//! Creation inputs and publication keep an owner after their host exits.
use futures::FutureExt;
use parrot::thread::{
    SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext,
};
use parrot_api::{
    Message,
    actor::{Actor, ActorState},
    address::ActorRef,
    context::ActorContext,
    engine::{AsyncEngineRuntime, AsyncFactoryRuntime, FactoryOptions, SpawnOptions},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    system::ActorSystem,
    trace::TraceContext,
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);
#[derive(Default)]
struct Stats {
    drops: AtomicUsize,
    constructed: AtomicUsize,
    initialized: AtomicUsize,
    entered: AtomicUsize,
    stalled: AtomicUsize,
    open: Mutex<bool>,
    changed: Condvar,
    traces: Mutex<Vec<Option<[u8; 16]>>>,
}
impl Stats {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Stats>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Capture(Arc<Stats>);
impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capture")
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.entered.fetch_add(1, Ordering::SeqCst);
        let (open, result) = self
            .0
            .changed
            .wait_timeout_while(self.0.open.lock().unwrap(), WAIT, |v| !*v)
            .unwrap();
        if !*open || result.timed_out() {
            self.0.stalled.fetch_add(1, Ordering::SeqCst);
        }
        drop(open);
        self.0
            .traces
            .lock()
            .unwrap()
            .push(TraceContext::current().map(|c| c.trace_id()));
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Native(Option<Capture>, Arc<Stats>);
impl Actor for Native {
    type Config = Box<dyn std::any::Any + Send>;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.1.initialized.fetch_add(1, Ordering::SeqCst);
        let _ = &self.0;
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async {
            Ok(Box::new(std::thread::current().name().unwrap_or("").to_owned()) as BoxedMessage)
        })
    }
}
struct Static(Capture);
struct Query;
impl Message for Query {
    type Result = usize;
}
impl ParrotMsgVariant<Static> for Query {
    fn inject(query: Self) -> Query {
        query
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
impl TypedReceive<Query> for Static {
    fn receive_typed(&mut self, _: Query) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}
impl ParrotTypedDispatch for Static {
    type Msg = Query;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.0.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, _: Query) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}
struct Erased(Capture);
impl Actor for Erased {
    type Config = Box<dyn std::any::Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.0.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
}
fn policy(s: &Arc<Stats>) -> OneForOneStrategy {
    let capture = Capture(s.clone());
    OneForOneStrategy {
        max_restarts: 1,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}
fn system(host: &tokio::runtime::Runtime, sharded: bool) -> Arc<ThreadActorSystem> {
    ThreadActorSystem::try_shared_with_handle(
        ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            default_scheduling_mode: if sharded {
                SchedulingMode::Sharded {
                    affinity_key: "creation-host".into(),
                }
            } else {
                SchedulingMode::DedicatedThread
            },
            shutdown_timeout: WAIT,
            ..Default::default()
        },
        host.handle().clone(),
    )
    .unwrap()
}
fn create<'a>(
    kind: usize,
    s: &'a Arc<ThreadActorSystem>,
    stats: &Arc<Stats>,
) -> BoxedFuture<'a, Result<(), String>> {
    let capture = Capture(stats.clone());
    let stats = stats.clone();
    macro_rules! erased {
        ($future:expr) => {
            Box::pin($future.map(|r| r.map(|_| ()).map_err(|e| e.to_string())))
        };
    }
    match kind {
        0 => erased!(s.spawn_at(
            Native(Some(capture), stats),
            "/occupied",
            None,
            Default::default()
        )),
        1 => erased!(s.spawn_supervised_with_config(
            move || {
                let _ = &capture;
                stats.constructed.fetch_add(1, Ordering::SeqCst);
                Native(None, stats.clone())
            },
            "/occupied",
            Default::default()
        )),
        2 => erased!(s.spawn_typed_with_strategy::<Static, Query>(
            Static(capture),
            "/occupied",
            SupervisorStrategyType::OneForOne(policy(&stats))
        )),
        3 => {
            let strategy = policy(&stats);
            erased!(s.spawn_typed_factory_with_strategy::<Static, Query, _>(
                move || {
                    let _ = &capture;
                    stats.constructed.fetch_add(1, Ordering::SeqCst);
                    Static(Capture(stats.clone()))
                },
                "/occupied",
                strategy
            ))
        }
        4 => erased!(AsyncEngineRuntime::spawn(
            s.as_ref(),
            Box::new(Erased(capture)),
            Box::new(Capture(stats)),
            SpawnOptions::at("")
        )),
        5 => erased!(AsyncFactoryRuntime::spawn_factory(
            s.as_ref(),
            Box::new(move || {
                let _ = &capture;
                stats.constructed.fetch_add(1, Ordering::SeqCst);
                Box::new(Erased(Capture(stats.clone())))
            }),
            FactoryOptions {
                spawn: SpawnOptions::at(""),
                ..Default::default()
            }
        )),
        6 => erased!(ActorSystem::spawn_root_typed(
            s.as_ref(),
            Native(Some(capture), stats.clone()),
            Box::new(Capture(stats))
        )),
        _ => unreachable!(),
    }
}
fn expected_error(kind: usize) -> String {
    match kind {
        0 | 1 => parrot::thread::error::SpawnError::ActorPathAlreadyExists("/occupied".into()).to_string(),
        2 | 3 => ActorError::InternalError("actor path already exists: /occupied".into()).to_string(),
        4 | 5 => parrot_api::system::SystemError::ActorCreationError("runtime path must contain 1..=4096 UTF-8 bytes and no control characters".into()).to_string(),
        6 => parrot_api::system::SystemError::ActorCreationError("Generic actor creation not supported directly. Use spawn_root_typed_thread on the ThreadActorSystem.".into()).to_string(),
        _ => unreachable!(),
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn shutdown(s: &ThreadActorSystem) {
    for _ in 0..2 {
        let error = tokio::time::timeout(WAIT, s.shutdown_internal())
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(&error, parrot::thread::error::SystemError::ShutdownError(text) if text == "1 shared cleanup failure(s): shared task unexpectedly cancelled"),
            "{error}"
        );
    }
    assert_eq!(s.actor_count(), 0);
    assert_eq!(
        s.scheduler_metrics().unwrap().status,
        parrot::thread::scheduler::shared::SchedulerStatus::Shutdown
    );
}
fn inputs(kind: usize, installed_before_exit: bool, action: usize) {
    let host = runtime();
    let s = system(&host, false);
    let occupied = host
        .block_on(s.spawn_at(
            Native(None, Arc::new(Stats::default())),
            "/occupied",
            None,
            Default::default(),
        ))
        .unwrap();
    let stats = Arc::new(Stats::default());
    let _release = Release(stats.clone());
    let cause = TraceContext::root();
    let host = if installed_before_exit {
        Some(host)
    } else {
        drop(host);
        None
    };
    let mut future = cause.in_scope(|| create(kind, &s, &stats));
    drop(host);
    let observer = runtime();
    let count = if matches!(kind, 2 | 3 | 4 | 6) { 2 } else { 1 };
    if action > 0 {
        observer.block_on(async {
            assert!(
                futures::poll!(&mut future).is_pending(),
                "rejection finished before input retirement"
            );
        });
    }
    if action == 2 {
        observer.block_on(until(|| stats.entered.load(Ordering::SeqCst) > 0));
        stats.release();
        assert_eq!(observer.block_on(future).unwrap_err(), expected_error(kind));
    } else {
        // Release before joining even on failure: a blocked destructor must not
        // prevent the watchdog from reporting the original caller violation.
        let returned = std::thread::scope(|scope| {
            let (done, receipt) = std::sync::mpsc::channel();
            let worker = scope.spawn(move || {
                drop(future);
                done.send(()).unwrap();
            });
            let returned = receipt.recv_timeout(Duration::from_millis(500)).is_ok();
            stats.release();
            worker.join().unwrap();
            returned
        });
        assert!(
            returned,
            "cancelling creation ran a blocked destructor on its caller"
        );
    }
    observer.block_on(async {
        until(|| stats.drops.load(Ordering::SeqCst) == count).await;
        assert_eq!(stats.constructed.load(Ordering::SeqCst), 0);
        assert_eq!(stats.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(
            stats.stalled.load(Ordering::SeqCst),
            0,
            "creation input blocked its caller until the gate watchdog"
        );
        assert_eq!(
            *stats.traces.lock().unwrap(),
            vec![Some(cause.trace_id()); count]
        );
        assert_eq!(s.actor_count(), 1);
        occupied.stop().await.unwrap();
        shutdown(&s).await;
    });
}
struct Fast(Native);
impl Actor for Fast {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.1.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async {
            Ok(Box::new(std::thread::current().name().unwrap_or("").to_owned()) as BoxedMessage)
        })
    }
}
fn publication(sharded: bool, with_config: bool, cancel: bool) {
    let host = runtime();
    let s = system(&host, sharded);
    drop(host);
    let stats = Arc::new(Stats::default());
    let _release = Release(stats.clone());
    let cause = TraceContext::root();
    let native = Native(Some(Capture(stats.clone())), stats.clone());
    let mut future: BoxedFuture<'_, Result<Box<dyn ActorRef>, parrot::thread::error::SpawnError>> =
        cause.in_scope(|| {
            if with_config {
                Box::pin(
                    s.spawn_typed_with_config(
                        native,
                        Box::new(Capture(stats.clone())),
                        ThreadActorConfig::default(),
                    )
                    .map(|r| r.map(|r| r.clone_boxed())),
                ) as BoxedFuture<'_, _>
            } else {
                Box::pin(
                    s.spawn_root_typed_thread(Fast(native), parrot_api::actor::EmptyConfig)
                        .map(|r| r.map(|r| r.clone_boxed())),
                ) as BoxedFuture<'_, _>
            }
        });
    runtime().block_on(async {
        if cancel {
            assert!(futures::poll!(&mut future).is_pending());
            until(|| stats.entered.load(Ordering::SeqCst) > 0).await;
            assert_eq!(s.actor_count(), 1);
            drop(future);
            stats.release();
            until(|| s.actor_count() == 0).await;
            assert_eq!(stats.initialized.load(Ordering::SeqCst), 0);
        } else {
            stats.release();
            let actor = tokio::time::timeout(WAIT, future).await.unwrap().unwrap();
            let reply = actor.send(Box::new(())).await.unwrap();
            let worker = reply.downcast::<String>().unwrap();
            assert!(
                worker.starts_with(if sharded {
                    "parrot-shard-"
                } else {
                    "parrot-dedicated-"
                }),
                "business execution moved: {worker}"
            );
            assert_eq!(stats.initialized.load(Ordering::SeqCst), 1);
            actor.stop().await.unwrap();
        }
        until(|| stats.drops.load(Ordering::SeqCst) == 1 + usize::from(with_config)).await;
        shutdown(&s).await;
    });
}
fn cancelled_static(factory: bool) {
    let host = runtime();
    let s = system(&host, false);
    drop(host);
    let stats = Arc::new(Stats::default());
    let _release = Release(stats.clone());
    let cause = TraceContext::root();
    let capture = Capture(stats.clone());
    runtime().block_on(async {
        let actor = if factory {
            let source = stats.clone();
            cause
                .in_scope(|| {
                    s.spawn_typed_factory::<Static, Query, _>(
                        move || {
                            let _ = &capture;
                            source.constructed.fetch_add(1, Ordering::SeqCst);
                            Static(Capture(source.clone()))
                        },
                        "/unpublished",
                    )
                })
                .await
                .unwrap()
        } else {
            cause
                .in_scope(|| s.spawn_typed::<Static, Query>(Static(capture), "/unpublished"))
                .await
                .unwrap()
        }
        .into_dyn();
        until(|| stats.entered.load(Ordering::SeqCst) > 0).await;
        assert_eq!(stats.drops.load(Ordering::SeqCst), 0);
        assert_eq!(s.actor_count(), 1);
        stats.release();
        until(|| s.actor_count() == 0).await;
        assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
        assert_eq!(stats.stalled.load(Ordering::SeqCst), 0);
        assert_eq!(stats.constructed.load(Ordering::SeqCst), 0);
        assert_eq!(stats.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(*stats.traces.lock().unwrap(), vec![Some(cause.trace_id())]);
        let first = actor.stop().await.unwrap_err().to_string();
        assert!(
            first.contains("typed actor /unpublished execution failed without a disposal receipt:")
                && first.contains("Cancelled"),
            "{first}"
        );
        assert_eq!(actor.stop().await.unwrap_err().to_string(), first);
        assert!(matches!(
            actor.retirement_status(),
            parrot_api::address::RetirementStatus::Retired(Ok(()))
        ));
        shutdown(&s).await;
    });
}
#[path = "support/isolated.rs"]
mod isolated_child;

fn isolated(name: &str, body: impl FnOnce()) {
    isolated_child::run(
        name,
        "PARROT_CREATION_HOST_CASE",
        Duration::from_secs(30),
        body,
    );
}
macro_rules! case {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || $body);
        }
    };
}
macro_rules! cases {
    ($kind:expr, $a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:ident) => {
        case!($a, inputs($kind, true, 0));
        case!($b, inputs($kind, false, 0));
        case!($c, inputs($kind, true, 1));
        case!($d, inputs($kind, false, 1));
        case!($e, inputs($kind, true, 2));
        case!($f, inputs($kind, false, 2));
    };
}
case!(typed_cancelled_execution, cancelled_static(false));
case!(typed_factory_cancelled_execution, cancelled_static(true));
cases!(
    0,
    instance_before_drop,
    instance_after_drop,
    instance_before_cancel,
    instance_after_cancel,
    instance_before_reject,
    instance_after_reject
);
cases!(
    1,
    factory_before_drop,
    factory_after_drop,
    factory_before_cancel,
    factory_after_cancel,
    factory_before_reject,
    factory_after_reject
);
cases!(
    2,
    typed_before_drop,
    typed_after_drop,
    typed_before_cancel,
    typed_after_cancel,
    typed_before_reject,
    typed_after_reject
);
cases!(
    3,
    typed_factory_before_drop,
    typed_factory_after_drop,
    typed_factory_before_cancel,
    typed_factory_after_cancel,
    typed_factory_before_reject,
    typed_factory_after_reject
);
cases!(
    4,
    runtime_before_drop,
    runtime_after_drop,
    runtime_before_cancel,
    runtime_after_cancel,
    runtime_before_reject,
    runtime_after_reject
);
cases!(
    5,
    runtime_factory_before_drop,
    runtime_factory_after_drop,
    runtime_factory_before_cancel,
    runtime_factory_after_cancel,
    runtime_factory_before_reject,
    runtime_factory_after_reject
);
cases!(
    6,
    generic_before_drop,
    generic_after_drop,
    generic_before_cancel,
    generic_after_cancel,
    generic_before_reject,
    generic_after_reject
);
case!(dedicated_fast_publication, publication(false, false, false));
case!(sharded_fast_publication, publication(true, false, false));
case!(
    dedicated_config_publication,
    publication(false, true, false)
);
case!(sharded_config_publication, publication(true, true, false));
case!(
    dedicated_config_cancellation,
    publication(false, true, true)
);
case!(sharded_config_cancellation, publication(true, true, true));
