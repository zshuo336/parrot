// SPDX-License-Identifier: AGPL-3.0-only
//! Business actors and contract functions depend only on the public neutral API.
use futures::FutureExt;
use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, FactoryOptions, RuntimeActor, RuntimeFactory, SpawnOptions},
    errors::ActorError,
    supervisor::{DefaultSupervisorStrategyFactory, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "support/isolated.rs"]
mod isolated;

async fn bounded<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(8), f)
        .await
        .expect("contract deadline")
}
#[derive(Clone, Copy)]
enum Backend {
    Shared,
    Dedicated,
    Sharded,
    Actix,
}
#[derive(Clone, Copy)]
struct Entry {
    backend: Backend,
    facade: bool,
}
async fn engine(entry: Entry) -> Arc<dyn AsyncEngineRuntime> {
    let facade = if entry.facade {
        Some(
            parrot::system::ParrotActorSystem::new(Default::default())
                .await
                .unwrap(),
        )
    } else {
        None
    };
    if matches!(entry.backend, Backend::Actix) {
        let native = parrot::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap();
        if let Some(facade) = facade {
            facade
                .register_actix_system("selected".into(), native, true)
                .await
                .unwrap();
            Arc::new(facade)
        } else {
            Arc::new(native)
        }
    } else {
        let mode = match entry.backend {
            Backend::Dedicated => Some(SchedulingMode::DedicatedThread),
            Backend::Sharded => Some(SchedulingMode::Sharded {
                affinity_key: "neutral-factory".into(),
            }),
            Backend::Shared => None,
            Backend::Actix => unreachable!(),
        };
        let config = mode.map_or_else(ThreadActorSystemConfig::default, |mode| {
            ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                default_scheduling_mode: mode,
                ..Default::default()
            }
        });
        let native = ThreadActorSystem::shared(config);
        if let Some(facade) = facade {
            facade
                .register_thread_system("selected".into(), native, true)
                .await
                .unwrap();
            Arc::new(facade)
        } else {
            native
        }
    }
}
#[derive(Default)]
struct Counts {
    built: AtomicUsize,
    dropped: AtomicUsize,
    capture: AtomicUsize,
}
struct Probe {
    generation: usize,
    value: u64,
    counts: Arc<Counts>,
    stop: Option<StopGate>,
}
type StopGate = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Debug)]
struct Report {
    generation: usize,
    value: u64,
    parent: Option<String>,
    path: String,
    thread: String,
}
impl Actor for Probe {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if m.is::<bool>() {
                return Err(ActorError::InternalError("generation failed".into()));
            }
            Ok(Box::new(Report {
                generation: self.generation,
                value: self.value,
                parent: ctx.parent().map(|p| p.path()),
                path: ctx.get_self_ref().path(),
                thread: std::thread::current().name().unwrap_or_default().to_owned(),
            }) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if let Some((entered, release)) = self.stop.take() {
                let _ = entered.send(());
                let _ = release.await;
            }
            Ok(())
        })
    }
}
fn actor(c: &Arc<Counts>, generation: usize) -> RuntimeActor {
    Box::new(Probe {
        generation,
        value: 42,
        counts: c.clone(),
        stop: None,
    })
}
struct Capture {
    counts: Arc<Counts>,
    hostile: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.counts.capture.fetch_add(1, Ordering::SeqCst);
        assert!(!self.hostile, "factory capture disposal failed");
    }
}
fn factory(c: &Arc<Counts>, hostile: bool) -> RuntimeFactory {
    let capture = Capture {
        counts: c.clone(),
        hostile,
    };
    Box::new(move || {
        let capture = &capture;
        actor(
            &capture.counts,
            capture.counts.built.fetch_add(1, Ordering::SeqCst) + 1,
        )
    })
}
fn options(path: &str, max: u32) -> FactoryOptions {
    FactoryOptions {
        spawn: SpawnOptions::at(path),
        max_restarts: max,
        within: Duration::from_secs(60),
    }
}
async fn report(entry: Entry, reference: &BoxedActorRef, generation: usize) -> Report {
    bounded(async {
        loop {
            if let Ok(answer) = reference.send(Box::new(())).await {
                let report = *answer.downcast::<Report>().unwrap();
                let prefix = match entry.backend {
                    Backend::Dedicated => Some("parrot-dedicated-"),
                    Backend::Sharded => Some("parrot-shard-"),
                    _ => None,
                };
                if let Some(prefix) = prefix {
                    assert!(
                        report.thread.starts_with(prefix),
                        "expected {prefix} worker, got {}",
                        report.thread
                    );
                }
                if report.generation == generation {
                    return report;
                }
                assert!(
                    report.generation < generation,
                    "unexpected extra reconstruction"
                );
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}
async fn drained(runtime: &dyn AsyncEngineRuntime) {
    bounded(async {
        while runtime.status().active_actors != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
}
async fn root_restart(entry: Entry) {
    let runtime = engine(entry).await;
    let counts = Arc::new(Counts::default());
    let reference = bounded(
        runtime
            .factory_runtime()
            .unwrap()
            .spawn_factory(factory(&counts, false), options("/neutral/factory", 1)),
    )
    .await
    .unwrap();
    let identity = reference.death_watch().unwrap().id();
    let first = report(entry, &reference, 1).await;
    assert_eq!(first.value, 42);
    assert!(first.parent.is_none());
    assert_eq!(first.path, "/neutral/factory");
    assert!(reference.send(Box::new(true)).await.is_err());
    let second = report(entry, &reference, 2).await;
    assert_eq!(second.value, 42);
    let lookup = runtime.get_actor("/neutral/factory").await.unwrap();
    assert_eq!(lookup.death_watch().unwrap().id(), identity);
    assert_eq!(report(entry, &lookup, 2).await.path, first.path);
    assert!(reference.send(Box::new(true)).await.is_err());
    drained(runtime.as_ref()).await;
    assert!(!reference.is_alive().await);
    assert!(runtime.get_actor("/neutral/factory").await.is_none());
    assert_eq!(counts.built.load(Ordering::SeqCst), 2);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
    assert_eq!(counts.capture.load(Ordering::SeqCst), 1);
    bounded(runtime.shutdown()).await.unwrap();
}
async fn duplicate(entry: Entry) {
    let runtime = engine(entry).await;
    let counts = Arc::new(Counts::default());
    let reference = runtime
        .factory_runtime()
        .unwrap()
        .spawn_factory(factory(&counts, false), options("/shared", 0))
        .await
        .unwrap();
    assert!(
        runtime
            .spawn(actor(&counts, 9), Box::new(()), SpawnOptions::at("/shared"))
            .await
            .is_err()
    );
    assert!(
        runtime
            .factory_runtime()
            .unwrap()
            .spawn_factory(factory(&counts, false), options("/shared", 0))
            .await
            .is_err()
    );
    assert_eq!(report(entry, &reference, 1).await.value, 42);
    assert_eq!(
        counts.built.load(Ordering::SeqCst),
        1,
        "duplicate constructor must never run"
    );
    reference.stop().await.unwrap();
    let instance = runtime
        .spawn(
            actor(&counts, 10),
            Box::new(()),
            SpawnOptions::at("/shared"),
        )
        .await
        .unwrap();
    assert!(
        runtime
            .factory_runtime()
            .unwrap()
            .spawn_factory(factory(&counts, false), options("/shared", 0))
            .await
            .is_err()
    );
    assert_eq!(report(entry, &instance, 10).await.value, 42);
    instance.stop().await.unwrap();
    bounded(runtime.shutdown()).await.unwrap();
    assert_eq!(counts.capture.load(Ordering::SeqCst), 3);
}
async fn rejected_inputs(entry: Entry) {
    let runtime = engine(entry).await;
    let factories = runtime.factory_runtime().expect("live factory capability");
    for mode in 0..4 {
        let counts = Arc::new(Counts::default());
        if mode == 3 {
            runtime.shutdown().await.unwrap();
        }
        let mut opt = options("/input", 0);
        if mode == 1 {
            opt.spawn.path = Some(String::new());
        }
        if mode == 2 {
            opt.within = Duration::ZERO;
        }
        let future = factories.spawn_factory(factory(&counts, true), opt);
        if mode == 0 {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok()
            );
        } else {
            assert!(matches!(
                std::panic::AssertUnwindSafe(future).catch_unwind().await,
                Ok(Err(_))
            ));
        }
        bounded(async {
            while counts.capture.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(counts.built.load(Ordering::SeqCst), 0);
    }
}
struct Create {
    factory: bool,
    counts: Arc<Counts>,
    stop: Option<StopGate>,
}
struct BadInputs {
    counts: Arc<Counts>,
    unpolled: bool,
    factory: bool,
}
struct Parent;
impl Actor for Parent {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if m.is::<BadInputs>() {
                let request = *m.downcast::<BadInputs>().unwrap();
                let capture = || Capture {
                    counts: request.counts.clone(),
                    hostile: true,
                };
                let policy_capture = capture();
                let policy =
                    SupervisorStrategyType::OneForOne(parrot_api::supervisor::OneForOneStrategy {
                        max_restarts: 1,
                        within: Duration::from_secs(60),
                        decider: parrot_api::supervisor::BasicDecisionFn::new(move |_| {
                            let _ = &policy_capture;
                            parrot_api::supervisor::SupervisionDecision::Stop
                        }),
                    });
                let future = if request.factory {
                    ctx.spawner()
                        .spawn_factory_erased(Box::new(capture()), Some(policy))
                } else {
                    ctx.spawner().spawn_with_strategy(
                        Box::new(capture()),
                        Box::new(capture()),
                        policy,
                    )
                };
                let isolated = if request.unpolled {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok()
                } else {
                    matches!(
                        std::panic::AssertUnwindSafe(future).catch_unwind().await,
                        Ok(Err(_))
                    )
                };
                return Ok(Box::new(isolated) as BoxedMessage);
            }
            if m.is::<()>() {
                return Ok(Box::new(ctx.children().unwrap().len()) as BoxedMessage);
            }
            let request = *m.downcast::<Create>().unwrap();
            let spawner = ctx.async_spawner().expect("production child capability");
            let child = if request.factory {
                let stop = Mutex::new(request.stop);
                let counts = request.counts;
                let policy = SupervisorStrategyType::OneForOne(
                    DefaultSupervisorStrategyFactory::one_for_one(1, Duration::from_secs(60)),
                );
                spawner
                    .spawn_factory(
                        Box::new(move || {
                            Box::new(Probe {
                                generation: counts.built.fetch_add(1, Ordering::SeqCst) + 1,
                                value: 42,
                                counts: counts.clone(),
                                stop: stop.lock().unwrap().take(),
                            })
                        }),
                        Some(policy),
                    )
                    .await?
            } else {
                spawner
                    .spawn(
                        Box::new(Probe {
                            generation: 1,
                            value: 42,
                            counts: request.counts,
                            stop: request.stop,
                        }),
                        Box::new(()),
                        None,
                    )
                    .await?
            };
            Ok(Box::new(child) as BoxedMessage)
        })
    }
}
async fn child_scope(entry: Entry, factory_child: bool) {
    let runtime = engine(entry).await;
    let parent = runtime
        .spawn(
            Box::new(Parent),
            Box::new(()),
            SpawnOptions::at("/neutral/parent"),
        )
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    let answer = bounded(parent.send(Box::new(Create {
        factory: factory_child,
        counts: counts.clone(),
        stop: Some((entered, held)),
    })))
    .await
    .unwrap();
    let child = *answer.downcast::<BoxedActorRef>().unwrap();
    let info = report(entry, &child, 1).await;
    assert_eq!(info.parent.as_deref(), Some("/neutral/parent"));
    assert!(info.path.starts_with("/neutral/parent/"));
    assert!(runtime.get_actor(&info.path).await.is_some());
    assert_eq!(
        *parent
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        1
    );
    let stopping = parent.stop();
    tokio::pin!(stopping);
    tokio::select! { result = &mut stopping => panic!("parent stopped before child cleanup: {result:?}"),
    result = bounded(observed) => result.unwrap() }
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    bounded(stopping).await.unwrap();
    drained(runtime.as_ref()).await;
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    assert!(!child.is_alive().await);
    bounded(runtime.shutdown()).await.unwrap();
}
async fn child_restart(entry: Entry) {
    let runtime = engine(entry).await;
    let parent = runtime
        .spawn(Box::new(Parent), Box::new(()), SpawnOptions::at("/parent"))
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let child = *parent
        .send(Box::new(Create {
            factory: true,
            counts: counts.clone(),
            stop: None,
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    let original = report(entry, &child, 1).await;
    let identity = child.death_watch().unwrap().id();
    assert!(child.send(Box::new(true)).await.is_err());
    let replacement = report(entry, &child, 2).await;
    assert_eq!(replacement.parent, original.parent);
    assert_eq!(replacement.path, original.path);
    assert_eq!(
        runtime
            .get_actor(&original.path)
            .await
            .unwrap()
            .death_watch()
            .unwrap()
            .id(),
        identity
    );
    assert!(child.send(Box::new(true)).await.is_err());
    bounded(async {
        while runtime.get_actor(&original.path).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(parent.is_alive().await);
    assert_eq!(counts.built.load(Ordering::SeqCst), 2);
    bounded(runtime.shutdown()).await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
}
macro_rules! pair {
    ($thread:ident, $actix:ident, $body:ident $(, $arg:expr)*) => {
        pair!(@entries false; $thread, $actix, $body $(, $arg)*);
    };
    (@entries $facade:expr; $thread:ident, $actix:ident, $body:ident $(, $arg:expr)*) => {
        #[tokio::test] async fn $thread() { $body(Entry { backend: Backend::Shared, facade: $facade } $(, $arg)*).await; }
        #[actix::test] async fn $actix() { $body(Entry { backend: Backend::Actix, facade: $facade } $(, $arg)*).await; }
    }
}
pair!(
    thread_root_restart_budget,
    actix_root_restart_budget,
    root_restart
);
pair!(thread_shared_namespace, actix_shared_namespace, duplicate);
pair!(
    thread_factory_input_retirement,
    actix_factory_input_retirement,
    rejected_inputs
);
pair!(
    thread_child_instance_scope,
    actix_child_instance_scope,
    child_scope,
    false
);
pair!(
    thread_child_factory_scope,
    actix_child_factory_scope,
    child_scope,
    true
);
pair!(thread_child_restart, actix_child_restart, child_restart);

struct ConstructionGate {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    capture: Capture,
}
async fn cancelled_construction(entry: Entry) {
    let runtime = engine(entry).await;
    let counts = Arc::new(Counts::default());
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let gate = ConstructionGate {
        entered: Mutex::new(Some(entered)),
        release: Mutex::new(held),
        capture: Capture {
            counts: counts.clone(),
            hostile: false,
        },
    };
    let constructor: RuntimeFactory = Box::new(move || {
        let gate = &gate;
        gate.entered
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(())
            .unwrap();
        gate.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        actor(
            &gate.capture.counts,
            gate.capture.counts.built.fetch_add(1, Ordering::SeqCst) + 1,
        )
    });
    let mut future = runtime
        .factory_runtime()
        .unwrap()
        .spawn_factory(constructor, options("/pending", 0));
    tokio::select! { result = future.as_mut() => panic!("construction completed before release: {result:?}"),
    result = bounded(observed) => result.unwrap() }
    let pending = runtime
        .get_actor("/pending")
        .await
        .expect("pending path is reserved");
    assert!(
        runtime
            .factory_runtime()
            .unwrap()
            .spawn_factory(factory(&counts, false), options("/pending", 0))
            .await
            .is_err()
    );
    drop(future);
    // Cancellation must retain the reservation until the running constructor and
    // late output retire, so another actor cannot acquire the same live identity.
    assert!(
        runtime
            .spawn(
                actor(&counts, 9),
                Box::new(()),
                SpawnOptions::at("/pending")
            )
            .await
            .is_err()
    );
    release.send(()).unwrap();
    drained(runtime.as_ref()).await;
    assert!(!pending.is_alive().await);
    assert_eq!(counts.built.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
    assert_eq!(counts.capture.load(Ordering::SeqCst), 2);
    runtime.shutdown().await.unwrap();
}
struct RetiringCapture {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    count: Arc<AtomicUsize>,
}
impl Drop for RetiringCapture {
    fn drop(&mut self) {
        self.entered.take().unwrap().send(()).unwrap();
        self.release
            .get_mut()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}
async fn cancelled_rejection(entry: Entry) {
    let runtime = engine(entry).await;
    let count = Arc::new(AtomicUsize::new(0));
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let capture = RetiringCapture {
        entered: Some(entered),
        release: Mutex::new(held),
        count: count.clone(),
    };
    let factory: RuntimeFactory = Box::new(move || {
        let _ = &capture;
        panic!("invalid path constructor executed");
    });
    let mut future = runtime
        .factory_runtime()
        .unwrap()
        .spawn_factory(factory, options("", 0));
    tokio::select! { result = future.as_mut() => panic!("rejection acknowledged before disposal: {result:?}"),
    result = bounded(observed) => result.unwrap() }
    drop(future);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    bounded(async {
        while count.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    runtime.shutdown().await.unwrap();
}
pair!(
    thread_cancelled_construction,
    actix_cancelled_construction,
    cancelled_construction
);
pair!(
    thread_cancelled_rejection,
    actix_cancelled_rejection,
    cancelled_rejection
);

async fn malformed_child_inputs(entry: Entry) {
    let runtime = engine(entry).await;
    let parent = runtime
        .spawn(Box::new(Parent), Box::new(()), SpawnOptions::at("/parent"))
        .await
        .unwrap();
    for factory in [false, true] {
        for unpolled in [false, true] {
            let counts = Arc::new(Counts::default());
            let result = parent
                .send(Box::new(BadInputs {
                    counts: counts.clone(),
                    factory,
                    unpolled,
                }))
                .await
                .unwrap();
            assert!(
                *result.downcast::<bool>().unwrap(),
                "destructor panic escaped managed spawner"
            );
            let expected = if factory { 2 } else { 3 };
            bounded(async {
                while counts.capture.load(Ordering::SeqCst) != expected {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert_eq!(runtime.status().active_actors, 1);
        }
    }
    runtime.shutdown().await.unwrap();
}
pair!(
    thread_malformed_child_inputs,
    actix_malformed_child_inputs,
    malformed_child_inputs
);

mod facade {
    use super::*;

    fn host_exit(name: &str, backend: Backend) {
        isolated::run(
            name,
            "PARROT_NEUTRAL_FACTORY_HOST_EXIT",
            Duration::from_secs(30),
            || {
                let entry = Entry {
                    backend,
                    facade: true,
                };
                let host = actix::System::new();
                let path = "/neutral/surviving-parent";
                let (system, parent) = host.block_on(async {
                    let system = engine(entry).await;
                    let parent = system
                        .spawn(Box::new(Parent), Box::new(()), SpawnOptions::at(path))
                        .await
                        .unwrap();
                    assert_eq!(
                        *parent
                            .send(Box::new(()))
                            .await
                            .unwrap()
                            .downcast::<usize>()
                            .unwrap(),
                        0
                    );
                    (system, parent)
                });
                drop(host);
                let observer = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                observer.block_on(bounded(async {
                let counts = Arc::new(Counts::default());
                let child = *parent.send(Box::new(Create { factory: true, counts: counts.clone(), stop: None }))
                    .await.unwrap().downcast::<BoxedActorRef>().unwrap();
                let first = report(entry, &child, 1).await;
                assert_eq!(first.parent.as_deref(), Some(path));
                let lookup = system.get_actor(&first.path).await.unwrap();
                assert_eq!(child.death_watch().unwrap().id(), lookup.death_watch().unwrap().id());
                assert!(child.send(Box::new(true)).await.is_err());
                let next = report(entry, &lookup, 2).await;
                assert_eq!(next.value, 42);
                assert_eq!((next.parent, next.path), (first.parent, first.path));
                assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
                parent.stop().await.unwrap();
                assert_eq!(counts.built.load(Ordering::SeqCst), 2);
                assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
                for member in [&parent, &child, &lookup] {
                    assert!(matches!(member.retirement_status(), parrot_api::address::RetirementStatus::Retired(Ok(()))));
                    assert!(system.get_actor(&member.path()).await.is_none());
                    assert!(member.send(Box::new(())).await.is_err());
                }
                let stopped = system.shutdown().await;
                if matches!(backend, Backend::Actix) {
                    stopped.unwrap();
                } else {
                    let error = stopped.unwrap_err().to_string();
                    assert!(error.contains("engine \"selected\": Failed during shutdown: 1 shared cleanup failure(s): shared task unexpectedly cancelled"), "{error}");
                    assert_eq!(system.shutdown().await.unwrap_err().to_string(), error);
                }
            }));
            },
        );
    }

    #[test]
    fn dedicated_child_restart_after_host_exit() {
        host_exit(
            "facade::dedicated_child_restart_after_host_exit",
            Backend::Dedicated,
        );
    }
    #[test]
    fn sharded_child_restart_after_host_exit() {
        host_exit(
            "facade::sharded_child_restart_after_host_exit",
            Backend::Sharded,
        );
    }
    #[test]
    fn actix_child_restart_after_host_exit() {
        host_exit(
            "facade::actix_child_restart_after_host_exit",
            Backend::Actix,
        );
    }

    pair!(@entries true; thread_root_restart_budget, actix_root_restart_budget, root_restart);
    pair!(@entries true; thread_shared_namespace, actix_shared_namespace, duplicate);
    pair!(@entries true; thread_factory_input_retirement, actix_factory_input_retirement, rejected_inputs);
    pair!(@entries true; thread_child_instance_scope, actix_child_instance_scope, child_scope, false);
    pair!(@entries true; thread_child_factory_scope, actix_child_factory_scope, child_scope, true);
    pair!(@entries true; thread_child_restart, actix_child_restart, child_restart);
    pair!(@entries true; thread_cancelled_construction, actix_cancelled_construction, cancelled_construction);
    pair!(@entries true; thread_cancelled_rejection, actix_cancelled_rejection, cancelled_rejection);
    pair!(@entries true; thread_malformed_child_inputs, actix_malformed_child_inputs, malformed_child_inputs);

    macro_rules! thread_contracts {
        ($module:ident, $backend:expr) => {
            mod $module {
                use super::*;
                const ENTRY: Entry = Entry {
                    backend: $backend,
                    facade: true,
                };

                #[tokio::test]
                async fn root_restart_budget() {
                    root_restart(ENTRY).await;
                }
                #[tokio::test]
                async fn shared_namespace() {
                    duplicate(ENTRY).await;
                }
                #[tokio::test]
                async fn factory_input_retirement() {
                    rejected_inputs(ENTRY).await;
                }
                #[tokio::test]
                async fn child_instance_scope() {
                    child_scope(ENTRY, false).await;
                }
                #[tokio::test]
                async fn child_factory_scope() {
                    child_scope(ENTRY, true).await;
                }
                #[tokio::test]
                async fn child_reconstruction() {
                    child_restart(ENTRY).await;
                }
                #[tokio::test]
                async fn cancelled_factory_construction() {
                    cancelled_construction(ENTRY).await;
                }
                #[tokio::test]
                async fn cancelled_factory_rejection() {
                    cancelled_rejection(ENTRY).await;
                }
                #[tokio::test]
                async fn malformed_inputs() {
                    malformed_child_inputs(ENTRY).await;
                }
            }
        };
    }
    thread_contracts!(dedicated, Backend::Dedicated);
    thread_contracts!(sharded, Backend::Sharded);
}
