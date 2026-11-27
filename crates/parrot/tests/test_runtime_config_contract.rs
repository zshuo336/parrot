//! Runtime settings must change execution or fail before starting an engine.
use futures::FutureExt;
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    system::ParrotActorSystem,
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, SpawnOptions},
    errors::ActorError,
    runtime::{ActorRuntime, LoadBalancingStrategy, RuntimeConfig, TokioActorRuntime},
    supervisor::{
        BasicDecisionFn, DefaultStrategy, OneForAllStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    system::{ActorSystem, ActorSystemConfig, GuardianConfig, SystemError},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    collections::HashSet,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Plan {
    started: AtomicUsize,
    cancelled: AtomicUsize,
    dropped: AtomicUsize,
}
struct PendingWork(Arc<Plan>);
impl Drop for PendingWork {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}
struct Probe(Arc<Plan>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
enum Op {
    Identity,
    Hang,
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
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match *message.downcast::<Op>().unwrap() {
                Op::Identity => {
                    let before = std::thread::current().id();
                    tokio::task::yield_now().await;
                    Ok(Box::new((before, std::thread::current().id())) as BoxedMessage)
                }
                Op::Hang => {
                    let _work = PendingWork(self.0.clone());
                    self.0.started.fetch_add(1, Ordering::SeqCst);
                    std::future::pending().await
                }
            }
        })
    }
}

fn thread_settings() -> ThreadActorSystemConfig {
    ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    }
}
fn actix_settings() -> ActixSystemSettings {
    ActixSystemSettings {
        arbiter_count: 1,
        ..Default::default()
    }
}

async fn exercise_thread(system: &ThreadActorSystem) {
    let metrics = system.scheduler_metrics().unwrap();
    assert_eq!(metrics.pool_size, 2);
    assert_eq!(metrics.queue_capacity, 7);
    let plan = Arc::new(Plan::default());
    let actor = system
        .spawn(
            Box::new(Probe(plan.clone())),
            Box::new(()),
            SpawnOptions::at("/configured-thread"),
        )
        .await
        .unwrap();
    let reply = tokio::time::timeout(WAIT, actor.send(Box::new(Op::Identity)))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.downcast::<(ThreadId, ThreadId)>().is_ok());
    tokio::time::timeout(WAIT, AsyncEngineRuntime::shutdown(system))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn thread_generic_settings_reach_scheduler_on_every_startup_entry() {
    for entry in 0..4 {
        let mut config = ActorSystemConfig::default();
        config.runtime_config.worker_threads = Some(2);
        config.runtime_config.scheduler_config.task_queue_capacity = 7;
        if entry == 0 {
            let system = Arc::new(
                <ThreadActorSystem as ActorSystem>::start(config)
                    .await
                    .unwrap(),
            );
            system.set_self_weak(Arc::downgrade(&system));
            exercise_thread(&system).await;
        } else {
            let facade = match entry {
                1 => ParrotActorSystem::start(config),
                2 => ParrotActorSystem::with_thread(config, thread_settings()),
                _ => <ParrotActorSystem as ActorSystem>::start(config),
            }
            .await
            .unwrap();
            let runtime = facade.runtime("default").unwrap();
            exercise_thread(
                (runtime.as_ref() as &dyn Any)
                    .downcast_ref::<ThreadActorSystem>()
                    .unwrap(),
            )
            .await;
            facade.shutdown_runtime().await.unwrap();
        }
    }
}

#[actix::test]
async fn actix_worker_setting_controls_real_execution_threads() {
    let host = std::thread::current().id();
    for workers in [1, 2] {
        let facade = ParrotActorSystem::with_actix(
            ActorSystemConfig {
                runtime_config: RuntimeConfig {
                    worker_threads: Some(workers),
                    ..Default::default()
                },
                ..Default::default()
            },
            actix_settings(),
        )
        .await
        .unwrap();
        let engine = facade.runtime("default").unwrap();
        let plan = Arc::new(Plan::default());
        let mut owners = HashSet::new();
        for index in 0..6 {
            let actor = engine
                .spawn(
                    Box::new(Probe(plan.clone())),
                    Box::new(()),
                    SpawnOptions::at(format!("/worker-{index}")),
                )
                .await
                .unwrap();
            let mut previous = None;
            for _ in 0..2 {
                let reply = tokio::time::timeout(WAIT, actor.send(Box::new(Op::Identity)))
                    .await
                    .unwrap()
                    .unwrap();
                let (before, after) = *reply.downcast::<(ThreadId, ThreadId)>().unwrap();
                assert_ne!(before, host, "actor ran on the host instead of an arbiter");
                assert_eq!(before, after, "actor moved between threads across a yield");
                if let Some(previous) = previous {
                    assert_eq!(before, previous, "actor moved between messages");
                }
                previous = Some(before);
                owners.insert(before);
            }
        }
        tokio::time::timeout(WAIT, facade.shutdown_runtime())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(plan.dropped.load(Ordering::SeqCst), 6);
        assert_eq!(owners.len(), workers, "worker_threads was not applied");
        assert_eq!(
            (engine.as_ref() as &dyn Any)
                .downcast_ref::<ActixActorSystem>()
                .unwrap()
                .arbiter_size(),
            workers
        );
    }
}

async fn handler_deadline(actix: bool) {
    let mut config = ActorSystemConfig::default();
    config.guardian_config.supervision_strategy =
        SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure);
    config.runtime_config.scheduler_config.task_timeout = Duration::from_millis(100);
    let facade = if actix {
        ParrotActorSystem::with_actix(config, actix_settings()).await
    } else {
        ParrotActorSystem::with_thread(config, thread_settings()).await
    }
    .unwrap();
    let engine = facade.runtime("default").unwrap();
    let plan = Arc::new(Plan::default());
    let actor = engine
        .spawn(
            Box::new(Probe(plan.clone())),
            Box::new(()),
            SpawnOptions::at("/timed-handler"),
        )
        .await
        .unwrap();
    // Always finish engine cleanup before reporting a missing deadline.
    let result = tokio::time::timeout(WAIT, actor.send(Box::new(Op::Hang))).await;
    let cancelled_before_shutdown = plan.cancelled.load(Ordering::SeqCst);
    let resumed = tokio::time::timeout(WAIT, actor.send(Box::new(Op::Identity))).await;
    tokio::time::timeout(WAIT, facade.shutdown_runtime())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, Ok(Err(ActorError::Timeout))),
        "task_timeout did not cancel the actual handler: {result:?}"
    );
    assert_eq!(plan.started.load(Ordering::SeqCst), 1);
    assert!(
        matches!(resumed, Ok(Ok(_))),
        "Resume did not preserve the actor"
    );
    assert_eq!(
        cancelled_before_shutdown, 1,
        "handler was not actually disposed"
    );
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(plan.dropped.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn thread_generic_timeout_cancels_the_actual_handler() {
    handler_deadline(false).await;
}
#[actix::test]
async fn actix_generic_timeout_cancels_the_actual_handler() {
    handler_deadline(true).await;
}

struct Capture {
    dropped: Arc<AtomicUsize>,
    hostile: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        assert!(!self.hostile, "runtime config policy disposal witness");
    }
}
fn captured(dropped: &Arc<AtomicUsize>, hostile: bool) -> ActorSystemConfig {
    let capture = Capture {
        dropped: dropped.clone(),
        hostile,
    };
    ActorSystemConfig {
        guardian_config: GuardianConfig {
            max_restarts: 1,
            restart_window: Duration::from_secs(60),
            supervision_strategy: SupervisorStrategyType::OneForAll(OneForAllStrategy {
                max_restarts: 1,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(move |_| {
                    let _ = &capture;
                    SupervisionDecision::Stop
                }),
            }),
        },
        ..Default::default()
    }
}

fn invalid(config: &mut ActorSystemConfig, case: usize) -> &'static str {
    match case {
        0 => {
            config.runtime_config.worker_threads = Some(0);
            "worker_threads"
        }
        1 => {
            config.runtime_config.io_threads = Some(0);
            "io_threads"
        }
        2 => {
            config.runtime_config.io_threads = Some(4097);
            "io_threads"
        }
        3 => {
            config.runtime_config.worker_threads = Some(usize::MAX);
            "worker_threads"
        }
        4 => {
            config.runtime_config.scheduler_config.load_balancing = LoadBalancingStrategy::Random;
            "load_balancing"
        }
        5 => {
            config.runtime_config.scheduler_config.load_balancing =
                LoadBalancingStrategy::LeastLoaded;
            "load_balancing"
        }
        6 => {
            config.runtime_config.scheduler_config.task_queue_capacity = usize::MAX;
            "task_queue_capacity"
        }
        7 => {
            config.runtime_config.scheduler_config.task_timeout = Duration::MAX;
            "task_timeout"
        }
        8 => {
            config.timeouts.message_handling = Duration::from_secs(2);
            config.runtime_config.scheduler_config.task_timeout = Duration::from_secs(1);
            "task_timeout"
        }
        9 => {
            config.timeouts.actor_creation = Duration::MAX;
            "actor_creation"
        }
        10 => {
            config.timeouts.message_handling = Duration::MAX;
            "message_handling"
        }
        11 => {
            config.timeouts.system_shutdown = Duration::MAX;
            "system_shutdown"
        }
        _ => unreachable!(),
    }
}

async fn rejects_configuration(actix: bool, hostile: bool) {
    let entries = if actix { 1 } else { 4 };
    for entry in 0..entries {
        for case in 0..12 {
            if matches!(case, 4 | 5) {
                // Thread and Actix both route these policies through their
                // native scheduler; they are covered by positive placement
                // contracts below rather than the invalid-input matrix.
                continue;
            }
            let dropped = Arc::new(AtomicUsize::new(0));
            let mut config = captured(&dropped, hostile);
            let field = invalid(&mut config, case);
            let result = AssertUnwindSafe(async {
                if actix {
                    let result = ParrotActorSystem::with_actix(config, actix_settings()).await;
                    match result {
                        Err(error) => Err(error),
                        Ok(system) => {
                            let _ = system.shutdown_runtime().await;
                            Ok(())
                        }
                    }
                } else if entry == 0 {
                    match <ThreadActorSystem as ActorSystem>::start(config).await {
                        Err(error) => Err(error),
                        Ok(system) => {
                            let _ = system.shutdown().await;
                            Ok(())
                        }
                    }
                } else {
                    let future = match entry {
                        1 => ParrotActorSystem::start(config),
                        2 => ParrotActorSystem::with_thread(config, thread_settings()),
                        _ => <ParrotActorSystem as ActorSystem>::start(config),
                    };
                    match future.await {
                        Err(error) => Err(error),
                        Ok(system) => {
                            let _ = system.shutdown_runtime().await;
                            Ok(())
                        }
                    }
                }
            })
            .catch_unwind()
            .await;
            let error = result
                .unwrap_or_else(|_| {
                    panic!("startup unwound: actix={actix}, entry={entry}, case={case}")
                })
                .expect_err("invalid or unsupported configuration was accepted");
            assert_eq!(
                dropped.load(Ordering::SeqCst),
                1,
                "configuration cleanup was not joined"
            );
            if hostile {
                assert!(matches!(error, SystemError::ActorError(_)), "{error}");
                assert!(error.to_string().contains("disposal failed"), "{error}");
            } else {
                assert!(
                    matches!(error, SystemError::InitializationError(_)),
                    "{error}"
                );
                assert!(
                    error.to_string().contains(field),
                    "missing field {field}: {error}"
                );
            }
        }
    }
}

#[tokio::test]
async fn thread_startup_rejects_invalid_settings_and_joins_configuration_retirement() {
    rejects_configuration(false, false).await;
}
#[tokio::test]
async fn thread_startup_preserves_rejected_configuration_disposal_failure() {
    rejects_configuration(false, true).await;
}
#[actix::test]
async fn actix_startup_rejects_invalid_settings_and_joins_configuration_retirement() {
    rejects_configuration(true, false).await;
}
#[actix::test]
async fn actix_startup_preserves_rejected_configuration_disposal_failure() {
    rejects_configuration(true, true).await;
}

#[actix::test]
async fn actix_rejects_scheduler_capacity_without_substituting_mailbox_capacity() {
    let mut config = ActorSystemConfig::default();
    config.runtime_config.scheduler_config.task_queue_capacity = 7;
    let result = ParrotActorSystem::with_actix(config, actix_settings()).await;
    let error = match result {
        Err(error) => error,
        Ok(system) => {
            system.shutdown_runtime().await.unwrap();
            panic!("Actix silently accepted a scheduler queue capacity");
        }
    };
    assert!(matches!(error, SystemError::InitializationError(_)));
    assert!(error.to_string().contains("task_queue_capacity"));
}

#[tokio::test]
async fn tokio_runtime_applies_explicit_counts_and_supports_placement_policies() {
    let host = tokio::runtime::Handle::current().id();
    for config in [
        RuntimeConfig {
            worker_threads: Some(1),
            ..RuntimeConfig::default()
        },
        RuntimeConfig {
            io_threads: Some(1),
            ..RuntimeConfig::default()
        },
    ] {
        let separate_io = config.io_threads.is_some();
        let runtime = TokioActorRuntime::start(config)
            .await
            .expect("explicit Tokio thread settings must be applied");
        let identity = || async {
            let handle = tokio::runtime::Handle::current();
            (handle.id(), handle.metrics().num_workers())
        };
        let (actor, actor_workers) = runtime.spawn(identity()).await.unwrap();
        let (io, io_workers) = runtime.spawn_io(identity()).await.unwrap();
        assert_ne!(actor, host);
        assert_ne!(io, host);
        assert_eq!(actor == io, !separate_io);
        assert_eq!(io_workers, 1);
        if !separate_io {
            assert_eq!(actor_workers, 1);
        }
        runtime.shutdown().await.unwrap();
    }
    for strategy in [
        LoadBalancingStrategy::Random,
        LoadBalancingStrategy::LeastLoaded,
    ] {
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(2),
            scheduler_config: parrot_api::runtime::SchedulerConfig {
                load_balancing: strategy.clone(),
                ..Default::default()
            },
            ..RuntimeConfig::default()
        })
        .await
        .expect("Tokio placement policy must create an owned worker pool");
        let worker = || async {
            let handle = tokio::runtime::Handle::current();
            assert_eq!(handle.metrics().num_workers(), 1);
            let id = std::thread::current().id();
            tokio::task::yield_now().await;
            assert_eq!(std::thread::current().id(), id);
            (handle.id(), id)
        };
        if matches!(strategy, LoadBalancingStrategy::Random) {
            fastrand::seed(42);
            let mut owners = HashSet::new();
            for _ in 0..64 {
                owners.insert(runtime.spawn(worker()).await.unwrap());
            }
            assert_eq!(owners.len(), 2);
            assert!(owners.iter().all(|(runtime, _)| *runtime != host));
        } else {
            let (entered, observed) = tokio::sync::oneshot::channel();
            let (release, held) = tokio::sync::oneshot::channel();
            let busy = runtime.spawn(async move {
                entered.send(worker().await).unwrap();
                held.await.unwrap();
            });
            tokio::pin!(busy);
            assert!(futures::poll!(&mut busy).is_pending());
            let occupied = tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
            let available = runtime.spawn(worker()).await.unwrap();
            assert_ne!(occupied.0, host);
            assert_ne!(available.0, host);
            assert_ne!(occupied.1, available.1);
            assert_eq!(runtime.spawn(worker()).await.unwrap(), available);
            release.send(()).unwrap();
            busy.await.unwrap();
        }
        runtime.shutdown().await.unwrap();
    }
}
