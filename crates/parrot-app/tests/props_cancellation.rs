//! Public host contracts: cancellation must not orphan a partial pool or a late result.
#![cfg(feature = "host")]

use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::ParrotSpawner,
    host::{ThreadPropsFactory, ThreadPropsSpawner},
};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    task::Poll,
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Probe {
    constructed: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
    entered: Notify,
    release: Notify,
    returned: Notify,
    retired: Notify,
    block_second: bool,
    fail_third: bool,
    block_stop: bool,
    panic_clone: bool,
    reject_stop: Arc<AtomicBool>,
    stop_attempts: Arc<AtomicUsize>,
    per_path_attempts: Arc<Mutex<HashMap<String, usize>>>,
    reject_only_first: bool,
    panic_path: bool,
    stop_entered: Notify,
    stop_release: Notify,
}

fn probes() -> &'static Mutex<HashMap<String, Arc<Probe>>> {
    static PROBES: OnceLock<Mutex<HashMap<String, Arc<Probe>>>> = OnceLock::new();
    PROBES.get_or_init(Default::default)
}

struct Observed {
    path: String,
    probe: Arc<Probe>,
}
impl Actor for Observed {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.probe.block_stop && self.path.ends_with("-1") {
                self.probe.stop_entered.notify_one();
                self.probe.stop_release.notified().await;
            }
            self.probe.stopped.lock().unwrap().push(self.path.clone());
            Ok(())
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
impl Drop for Observed {
    fn drop(&mut self) {
        self.probe.retired.notify_one();
    }
}

#[derive(Debug)]
struct RetryStopRef {
    actor: BoxedActorRef,
    reject: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
    per_path_attempts: Arc<Mutex<HashMap<String, usize>>>,
    declared_path: String,
    reject_only_first: bool,
    panic_path: bool,
}
impl ActorRef for RetryStopRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send_with_timeout(message, timeout)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.deliver(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            *self
                .per_path_attempts
                .lock()
                .unwrap()
                .entry(self.declared_path.clone())
                .or_default() += 1;
            if self.reject.load(Ordering::SeqCst)
                && (!self.reject_only_first || self.declared_path.ends_with("-0"))
            {
                return Err(ActorError::InternalError(
                    "temporary cleanup transport rejection".into(),
                ));
            }
            self.actor.stop().await
        })
    }
    fn path(&self) -> String {
        assert!(!self.panic_path, "injected diagnostic path panic");
        self.actor.path()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.actor.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            actor: self.actor.clone_boxed(),
            reject: self.reject.clone(),
            attempts: self.attempts.clone(),
            per_path_attempts: self.per_path_attempts.clone(),
            declared_path: self.declared_path.clone(),
            reject_only_first: self.reject_only_first,
            panic_path: self.panic_path,
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Debug)]
struct PanicClone(BoxedActorRef);
impl ActorRef for PanicClone {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send_with_timeout(message, timeout)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.deliver(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.stop()
    }
    fn path(&self) -> String {
        self.0.path()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.0.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        panic!("deliberate factory reference clone panic")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let name = config.as_ref().unwrap()["probe"].as_str().unwrap();
        let probe = probes().lock().unwrap()[name].clone();
        if path.ends_with("-2") && probe.fail_third {
            return Err(ActorError::InternalError(
                "deliberate third factory failure".into(),
            ));
        }
        let actor = engine
            .spawn_at(
                Observed {
                    path: path.clone(),
                    probe: probe.clone(),
                },
                &path,
                None,
                Default::default(),
            )
            .await
            .map_err(|e| ActorError::InternalError(e.to_string()))?;
        // Prove actual initialization and business progress before exposing a gate.
        actor.send(Box::new(())).await?;
        probe.constructed.lock().unwrap().push(path.clone());
        if path.ends_with("-1") && probe.block_second {
            probe.entered.notify_one();
            probe.release.notified().await;
        }
        probe.returned.notify_one();
        let actor: BoxedActorRef = Box::new(RetryStopRef {
            actor: Box::new(actor),
            reject: probe.reject_stop.clone(),
            attempts: probe.stop_attempts.clone(),
            per_path_attempts: probe.per_path_attempts.clone(),
            declared_path: path.clone(),
            reject_only_first: probe.reject_only_first,
            panic_path: probe.panic_path,
        });
        if probe.panic_clone && path.ends_with("-1") {
            return Ok(Box::new(PanicClone(actor)) as BoxedActorRef);
        }
        Ok(actor)
    })
}
parrot_api::message::inventory::submit! {
    ThreadPropsFactory { name: "test.cancellation", spawn }
}

struct Setup {
    name: String,
    engine: Arc<ThreadActorSystem>,
    probe: Arc<Probe>,
    paths: Vec<String>,
    config: toml::Table,
}
impl Setup {
    fn new(name: &str, probe: Probe, count: usize) -> Self {
        let probe = Arc::new(probe);
        assert!(probes()
            .lock()
            .unwrap()
            .insert(name.into(), probe.clone())
            .is_none());
        let paths = (0..count).map(|i| format!("/user/{name}-{i}")).collect();
        let config = toml::Table::from_iter([("probe".into(), name.into())]);
        Self {
            name: name.into(),
            engine: ThreadActorSystem::shared(Default::default()),
            probe,
            paths,
            config,
        }
    }
    async fn retired(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let notified = self.probe.retired.notified();
                if self
                    .paths
                    .iter()
                    .all(|path| self.engine.get_actor_ref(path).is_none())
                {
                    break;
                }
                // Registry removal can follow actor Drop; yield after its notification.
                tokio::select! { _ = notified => {}, _ = tokio::task::yield_now() => {} }
            }
        })
        .await
        .expect("cancelled spawn left registered actors");
        assert_eq!(self.probe.stopped.lock().unwrap().len(), count);
    }
}
impl Drop for Setup {
    fn drop(&mut self) {
        self.probe.release.notify_one();
        self.probe.stop_release.notify_one();
        probes().lock().unwrap().remove(&self.name);
    }
}
async fn wait(notification: &Notify) {
    tokio::time::timeout(Duration::from_secs(3), notification.notified())
        .await
        .unwrap();
}
async fn poll_once<F: Future + ?Sized>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn cancellation_owns_partial_pool_and_late_factory_output_without_starting_more_instances() {
    let setup = Setup::new(
        "pending-pool",
        Probe {
            block_second: true,
            ..Default::default()
        },
        3,
    );
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let mut future =
        Box::pin(spawner.spawn_props("test.cancellation", &setup.paths, Some(&setup.config)));
    // Polling must allow the second factory to start, without ever awaiting completion.
    tokio::select! {
        result = &mut future => panic!("unexpected spawn completion: {}", result.is_ok()),
        _ = wait(&setup.probe.entered) => {},
    }
    drop(future);
    assert_eq!(setup.probe.constructed.lock().unwrap().len(), 2);
    setup.probe.release.notify_one();
    setup.retired(2).await;
    assert_eq!(setup.probe.constructed.lock().unwrap().len(), 2);
    assert_eq!(
        *setup.probe.stopped.lock().unwrap(),
        vec![setup.paths[1].clone(), setup.paths[0].clone()]
    );
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancellation_before_receiving_success_retires_the_unclaimed_result() {
    let setup = Setup::new("unclaimed", Probe::default(), 1);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let mut future =
        Box::pin(spawner.spawn_props("test.cancellation", &setup.paths, Some(&setup.config)));
    poll_once(future.as_mut()).await;
    // A current-thread runtime runs the worker through its send before this waiter resumes.
    wait(&setup.probe.returned).await;
    drop(future);
    setup.retired(1).await;
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelling_failed_pool_rollback_keeps_cleanup_owned_and_preserves_an_unrelated_actor() {
    let setup = Setup::new(
        "rollback",
        Probe {
            fail_third: true,
            block_stop: true,
            ..Default::default()
        },
        3,
    );
    let survivor = Setup::new("survivor", Probe::default(), 1);
    let unrelated = spawn(
        setup.engine.clone(),
        survivor.paths[0].clone(),
        Some(survivor.config.clone()),
    )
    .await
    .unwrap();
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let mut future =
        Box::pin(spawner.spawn_props("test.cancellation", &setup.paths, Some(&setup.config)));
    tokio::select! {
        result = &mut future => panic!("rollback returned before stop: {}", result.is_ok()),
        _ = wait(&setup.probe.stop_entered) => {},
    }
    drop(future);
    setup.probe.stop_release.notify_one();
    setup.retired(2).await;
    unrelated.send(Box::new(())).await.unwrap();
    unrelated.stop().await.unwrap();
    setup.engine.shutdown_internal().await.unwrap();
    survivor.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn unpolled_spawn_has_no_effect_and_claimed_success_stays_usable() {
    let setup = Setup::new("claimed", Probe::default(), 2);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    drop(spawner.spawn_props("test.cancellation", &setup.paths, Some(&setup.config)));
    assert!(setup.probe.constructed.lock().unwrap().is_empty());
    let actors = spawner
        .spawn_props("test.cancellation", &setup.paths, Some(&setup.config))
        .await
        .unwrap();
    // Let the ownership acknowledgement be consumed by its worker.
    tokio::task::yield_now().await;
    assert!(setup.probe.stopped.lock().unwrap().is_empty());
    for actor in &actors {
        assert_eq!(
            *actor
                .send(Box::new(42_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
    }
    for actor in actors.into_iter().rev() {
        actor.stop().await.unwrap();
    }
    setup.retired(2).await;
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn factory_reference_clone_panic_reclaims_the_actual_pool_before_returning_error() {
    let setup = Setup::new(
        "clone-panic",
        Probe {
            panic_clone: true,
            ..Default::default()
        },
        2,
    );
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let error = spawner
        .spawn_props("test.cancellation", &setup.paths, Some(&setup.config))
        .await
        .unwrap_err();
    assert_eq!(setup.probe.stopped.lock().unwrap().len(), 2);
    assert!(setup
        .paths
        .iter()
        .all(|path| setup.engine.get_actor_ref(path).is_none()));
    setup.retired(2).await;
    assert!(
        error.to_string().contains("reference clone panicked"),
        "{error}"
    );
    assert_eq!(
        *setup.probe.stopped.lock().unwrap(),
        vec![setup.paths[1].clone(), setup.paths[0].clone()]
    );
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelled_assembly_reclaims_its_unregistered_partial_component() {
    use parrot_app::{
        assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
        manifest::{AppManifest, ComponentSpec},
        planner::{plan, LocalTopology},
    };
    struct NoGateway;
    #[async_trait::async_trait]
    impl GatewayFactory for NoGateway {
        async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
            Ok(())
        }
        async fn start_gateway(
            &self,
            _: &ComponentSpec,
            _: &[String],
            _: &str,
        ) -> Result<Vec<BoxedActorRef>, DeployError> {
            panic!("unexpected gateway")
        }
        async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
            panic!("unexpected gateway")
        }
    }
    let setup = Setup::new(
        "assembly",
        Probe {
            block_second: true,
            ..Default::default()
        },
        3,
    );
    let manifest = AppManifest::from_toml("name = 'cancelled-app'\nversion = '1.0.0'\n[[components]]\nname = 'assembly'\nengine = 'parrot'\nartifact = { Props = { factory = 'test.cancellation' } }\ninstances = { Pool = 3 }\n[components.config]\nprobe = 'assembly'").unwrap();
    let plan = plan(&manifest, &LocalTopology).unwrap();
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let config = Default::default();
    let mut context = AssemblingContext::new();
    let mut assembling = Box::pin(context.assemble(&plan, &deployer, &manifest, &config));
    tokio::select! {
        result = &mut assembling => panic!("assembly unexpectedly finished: {result:?}"),
        _ = wait(&setup.probe.entered) => {},
    }
    drop(assembling);
    assert!(context.component_ref("assembly").is_none());
    context.teardown(&deployer).await.unwrap();
    setup.probe.release.notify_one();
    setup.retired(2).await;
    setup.engine.shutdown_internal().await.unwrap();
}

#[test]
fn spawning_uses_the_configured_engine_runtime_without_an_ambient_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let setup = runtime.block_on(async { Setup::new("explicit-runtime", Probe::default(), 1) });
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    assert!(tokio::runtime::Handle::try_current().is_err());
    let actors = futures::executor::block_on(spawner.spawn_props(
        "test.cancellation",
        &setup.paths,
        Some(&setup.config),
    ))
    .expect("the engine already owns a valid running runtime");
    runtime.block_on(async {
        for actor in actors {
            actor.send(Box::new(())).await.unwrap();
            actor.stop().await.unwrap();
        }
        setup.retired(1).await;
        setup.engine.shutdown_internal().await.unwrap();
    });
}

#[test]
fn dropping_the_callers_runtime_cannot_abandon_the_engines_partial_pool() {
    let engine_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let setup = engine_runtime.block_on(async {
        Setup::new(
            "transient-runtime",
            Probe {
                block_second: true,
                ..Default::default()
            },
            3,
        )
    });
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let caller_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    caller_runtime.block_on(async {
        let mut future =
            Box::pin(spawner.spawn_props("test.cancellation", &setup.paths, Some(&setup.config)));
        tokio::select! {
            result = &mut future => panic!("unexpected completion: {result:?}"),
            _ = wait(&setup.probe.entered) => {},
        }
        drop(future);
    });
    // The real engine remains alive; the caller's executor is only a waiter.
    drop(caller_runtime);
    setup.probe.release.notify_one();
    engine_runtime.block_on(async {
        setup.retired(2).await;
        assert_eq!(setup.probe.constructed.lock().unwrap().len(), 2);
        setup.engine.shutdown_internal().await.unwrap();
    });
}

async fn pending_count(spawner: &ThreadPropsSpawner, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while spawner.pending_cleanup().len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pending cleanup did not reach expected state");
}

#[tokio::test]
async fn failed_rollback_keeps_original_references_and_clone_can_retry() {
    let setup = Setup::new(
        "failed-rollback-owned",
        Probe {
            fail_third: true,
            ..Default::default()
        },
        3,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let error = spawner
        .spawn_props("test.cancellation", &setup.paths, Some(&setup.config))
        .await
        .err()
        .unwrap();
    assert_eq!(error.retained_actors().len(), 2);
    assert_eq!(spawner.pending_cleanup().len(), 2);
    for actor in error.retained_actors() {
        assert!(actor.send(Box::new(())).await.is_ok());
    }
    let later_paths: Vec<_> = (0..3)
        .map(|i| format!("/user/failed-rollback-later-{i}"))
        .collect();
    assert!(spawner
        .spawn_props("test.cancellation", &later_paths, Some(&setup.config))
        .await
        .is_err());
    assert!(spawner.retry_pending_cleanup().await.is_err());
    assert_eq!(
        spawner.pending_cleanup().len(),
        4,
        "retry duplicated the journal"
    );
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    spawner.clone().retry_pending_cleanup().await.unwrap();
    assert!(spawner.pending_cleanup().is_empty());
    setup.retired(4).await;
    assert!(later_paths
        .iter()
        .all(|path| setup.engine.get_actor_ref(path).is_none()));
    assert_eq!(
        *setup.probe.stopped.lock().unwrap(),
        [
            later_paths[1].clone(),
            later_paths[0].clone(),
            setup.paths[1].clone(),
            setup.paths[0].clone()
        ],
        "cleanup retry must reverse admission order within and across batches"
    );
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelled_spawn_retains_failed_cleanup_for_explicit_retry() {
    let setup = Setup::new(
        "cancel-failed-cleanup",
        Probe {
            block_second: true,
            ..Default::default()
        },
        2,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let mut spawn =
        Box::pin(spawner.spawn_props("test.cancellation", &setup.paths, Some(&setup.config)));
    poll_once(spawn.as_mut()).await;
    wait(&setup.probe.entered).await;
    drop(spawn);
    setup.probe.release.notify_one();
    pending_count(&spawner, 2).await;
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    spawner.retry_pending_cleanup().await.unwrap();
    setup.retired(2).await;
    assert!(spawner.pending_cleanup().is_empty());
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelled_retry_preserves_inflight_stop_and_unattempted_entries() {
    let setup = Setup::new(
        "cancel-cleanup-retry",
        Probe {
            fail_third: true,
            block_stop: true,
            ..Default::default()
        },
        3,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    assert!(spawner
        .spawn_props("test.cancellation", &setup.paths, Some(&setup.config))
        .await
        .is_err());
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    let member = spawner
        .pending_cleanup()
        .into_iter()
        .find(|a| a.path.ends_with("-1"))
        .unwrap();
    let mut stop = Box::pin(member.stop());
    poll_once(stop.as_mut()).await;
    wait(&setup.probe.stop_entered).await;
    let mut joined = Box::pin(member.stop());
    poll_once(joined.as_mut()).await;
    drop(stop);
    setup.probe.stop_release.notify_one();
    joined.await.unwrap();
    assert_eq!(
        setup.probe.stop_attempts.load(Ordering::SeqCst),
        3,
        "concurrent retry invoked the original stop twice"
    );
    pending_count(&spawner, 1).await;
    assert!(setup.engine.get_actor_ref(&setup.paths[0]).is_some());
    spawner.retry_pending_cleanup().await.unwrap();
    setup.retired(2).await;
    assert_eq!(setup.probe.stop_attempts.load(Ordering::SeqCst), 4);
    assert!(spawner.pending_cleanup().is_empty());
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn clone_panic_failure_retains_cleanup_without_cloning_user_references() {
    let setup = Setup::new(
        "clone-failed-cleanup",
        Probe {
            panic_clone: true,
            ..Default::default()
        },
        2,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let error = spawner
        .spawn_props("test.cancellation", &setup.paths, Some(&setup.config))
        .await
        .err()
        .unwrap();
    let copy = error.clone();
    assert_eq!(copy.retained_actors().len(), 2);
    assert!(copy.to_string().contains("clone panicked"));
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    for actor in copy.retained_actors() {
        actor.stop().await.unwrap();
    }
    assert!(spawner.pending_cleanup().is_empty());
    setup.retired(2).await;
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn pending_cleanup_capacity_is_reserved_before_starting_another_factory() {
    let setup = Setup::new(
        "cleanup-reservation",
        Probe {
            fail_third: true,
            ..Default::default()
        },
        4096,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    assert!(spawner
        .spawn_props("test.cancellation", &setup.paths, Some(&setup.config))
        .await
        .is_err());
    let error = spawner
        .spawn_props(
            "test.cancellation",
            &["/user/no-capacity".into()],
            Some(&setup.config),
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("ownership limit"));
    assert_eq!(setup.probe.constructed.lock().unwrap().len(), 2);
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    spawner.retry_pending_cleanup().await.unwrap();
    let actors = spawner
        .spawn_props("test.cancellation", &setup.paths[..1], Some(&setup.config))
        .await
        .unwrap();
    actors[0].stop().await.unwrap();
    setup.retired(3).await;
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn retrying_old_cleanup_does_not_stop_an_external_replacement() {
    let setup = Setup::new(
        "cleanup-replacement",
        Probe {
            fail_third: true,
            ..Default::default()
        },
        3,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    assert!(spawner
        .spawn_props("test.cancellation", &setup.paths, Some(&setup.config))
        .await
        .is_err());
    let old = setup.engine.get_actor_ref(&setup.paths[0]).unwrap();
    old.stop().await.unwrap();
    let replacement = spawn(
        setup.engine.clone(),
        setup.paths[0].clone(),
        Some(setup.config.clone()),
    )
    .await
    .unwrap();
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    spawner.retry_pending_cleanup().await.unwrap();
    assert!(replacement.send(Box::new(())).await.is_ok());
    replacement.stop().await.unwrap();
    setup.retired(3).await;
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn failed_component_is_registered_for_application_teardown_retry() {
    use parrot_app::{
        assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
        manifest::{AppManifest, ComponentSpec},
        planner::{plan, LocalTopology},
    };
    struct NoGateway;
    #[async_trait::async_trait]
    impl GatewayFactory for NoGateway {
        async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
            Ok(())
        }
        async fn start_gateway(
            &self,
            _: &ComponentSpec,
            _: &[String],
            _: &str,
        ) -> Result<Vec<BoxedActorRef>, DeployError> {
            panic!("local-only test invoked gateway")
        }
        async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
            panic!("local-only test stopped gateway")
        }
    }
    let setup = Setup::new(
        "application-cleanup-retry",
        Probe {
            fail_third: true,
            ..Default::default()
        },
        3,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let manifest = AppManifest::from_toml(&format!(
        r#"
name = "cleanup-app"
version = "1.0.0"
[[components]]
name = "{0}"
engine = "parrot"
instances = {{ Pool = 3 }}
artifact = {{ Props = {{ factory = "test.cancellation" }} }}
[components.config]
probe = "{0}"
"#,
        setup.name
    ))
    .unwrap();
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let error = context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.retained_actors().len(),
        2,
        "aggregation discarded structured ownership"
    );
    assert_eq!(context.started(), std::slice::from_ref(&setup.name));
    assert_eq!(context.component_refs(&setup.name).unwrap().len(), 2);
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    context.teardown(&deployer).await.unwrap();
    assert!(context.started().is_empty());
    assert!(spawner.pending_cleanup().is_empty());
    setup.retired(2).await;
    setup.engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn retained_error_owns_cleanup_after_the_spawner_is_dropped() {
    let setup = Setup::new(
        "cleanup-error-owner",
        Probe {
            fail_third: true,
            ..Default::default()
        },
        3,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let error = spawner
        .spawn_props("test.cancellation", &setup.paths, Some(&setup.config))
        .await
        .err()
        .unwrap();
    assert_eq!(error.retained_actors().len(), 2);
    drop(spawner);
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    for actor in error.retained_actors() {
        actor.stop().await.unwrap();
    }
    setup.retired(2).await;
    setup.engine.shutdown_internal().await.unwrap();
}

async fn application_teardown_progress_contract(mode: u8) {
    use parrot_app::{
        assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
        manifest::{AppManifest, ComponentSpec},
        planner::{plan, LocalTopology},
    };
    struct NoGateway;
    #[async_trait::async_trait]
    impl GatewayFactory for NoGateway {
        async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
            Ok(())
        }
        async fn start_gateway(
            &self,
            _: &ComponentSpec,
            _: &[String],
            _: &str,
        ) -> Result<Vec<BoxedActorRef>, DeployError> {
            panic!("local-only test invoked gateway")
        }
        async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
            panic!("local-only test stopped gateway")
        }
    }
    let setup = Setup::new(
        &format!("teardown-progress-{mode}"),
        Probe {
            reject_only_first: true,
            panic_path: mode == 1,
            block_stop: mode == 2,
            ..Default::default()
        },
        3,
    );
    let spawner = ThreadPropsSpawner::new(setup.engine.clone());
    let manifest = AppManifest::from_toml(&format!(
        r#"
name = "teardown-progress"
version = "1.0.0"
[[components]]
name = "{0}"
engine = "parrot"
instances = {{ Pool = 3 }}
artifact = {{ Props = {{ factory = "test.cancellation" }} }}
[components.config]
probe = "{0}"
"#,
        setup.name
    ))
    .unwrap();
    let plan = plan(&manifest, &LocalTopology).unwrap();
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    context
        .assemble(&plan, &deployer, &manifest, &Default::default())
        .await
        .unwrap();
    let (first_error, remaining_after_first) = if mode == 2 {
        let mut cleanup = Box::pin(context.teardown(&deployer));
        tokio::select! {
            result = &mut cleanup => panic!("cleanup did not wait for actual second actor: {result:?}"),
            _ = wait(&setup.probe.stop_entered) => {},
        }
        assert!(setup.engine.get_actor_ref(&setup.paths[0]).is_none());
        drop(cleanup);
        setup.probe.stop_release.notify_one();
        (None, None)
    } else {
        setup.probe.reject_stop.store(true, Ordering::SeqCst);
        let error = context.teardown(&deployer).await.unwrap_err();
        let remaining = setup.engine.actor_count();
        // The failed first instance must still serve real business messages.
        setup
            .engine
            .get_actor_ref(&setup.paths[0])
            .unwrap()
            .send(Box::new(7u64))
            .await
            .unwrap();
        setup.probe.reject_stop.store(false, Ordering::SeqCst);
        (Some(error.to_string()), Some(remaining))
    };
    context.teardown(&deployer).await.unwrap();
    assert!(context.started().is_empty());
    setup.retired(3).await;
    let attempts = setup.probe.per_path_attempts.lock().unwrap().clone();
    // Reusing the same context and paths creates new actors which must also stop.
    context
        .assemble(&plan, &deployer, &manifest, &Default::default())
        .await
        .unwrap();
    if mode == 2 {
        setup.probe.stop_release.notify_one();
    }
    context.teardown(&deployer).await.unwrap();
    setup.retired(6).await;
    let reused_attempts = setup.probe.per_path_attempts.lock().unwrap().clone();
    setup.engine.shutdown_internal().await.unwrap();
    if let Some(remaining) = remaining_after_first {
        assert_eq!(
            remaining, 1,
            "diagnostic failure skipped later pool members"
        );
        let error = first_error.unwrap();
        assert!(
            error.contains(&setup.paths[0])
                && error.contains("temporary cleanup transport rejection"),
            "original failure lost: {error}"
        );
    }
    let expected = if mode == 2 { [1, 2, 1] } else { [2, 1, 1] };
    for (index, expected) in expected.into_iter().enumerate() {
        assert_eq!(
            attempts[&setup.paths[index]], expected,
            "confirmed retirement was invoked again"
        );
        assert_eq!(
            reused_attempts[&setup.paths[index]],
            expected + 1,
            "new assembly reused an old retirement marker"
        );
    }
}
#[tokio::test]
async fn application_teardown_partial_success_is_not_retried() {
    application_teardown_progress_contract(0).await;
}
#[tokio::test]
async fn application_teardown_diagnostic_panic_cannot_skip_siblings() {
    application_teardown_progress_contract(1).await;
}
#[tokio::test]
async fn application_teardown_cancelled_wait_retains_confirmed_progress() {
    application_teardown_progress_contract(2).await;
}
