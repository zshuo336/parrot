//! Real actors, explicit rendezvous, and externally observable management state.
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::host::ThreadPropsFactory;
use parrot_node::NodeComponentExecutor;
use parrot_remote::admin_v2::{
    AdminArtifactRef, AdminInstancePolicy, AdminReplyV2 as R, ComponentDeploy, ComponentExecutor,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Probe {
    init_started: Notify,
    init_release: Notify,
    initialized: AtomicUsize,
    dropped: AtomicUsize,
    second_started: Notify,
    factory_release: Notify,
    handler_started: Notify,
    handler_release: Notify,
    stop_started: Notify,
    stop_release: Notify,
    stopped: AtomicUsize,
    block_init: bool,
    block_factory: bool,
    fail_factory: bool,
    block_stop: bool,
    fail_stop: bool,
    reject_stop: Arc<AtomicBool>,
}

#[derive(Debug)]
struct RetryStopRef {
    actor: BoxedActorRef,
    reject: Arc<AtomicBool>,
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
            if self.reject.load(Ordering::SeqCst) {
                return Err(ActorError::InternalError(
                    "temporary cleanup transport rejection".into(),
                ));
            }
            self.actor.stop().await
        })
    }
    fn path(&self) -> String {
        self.actor.path()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.actor.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            actor: self.actor.clone_boxed(),
            reject: self.reject.clone(),
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn probes() -> &'static Mutex<HashMap<String, Arc<Probe>>> {
    static PROBES: OnceLock<Mutex<HashMap<String, Arc<Probe>>>> = OnceLock::new();
    PROBES.get_or_init(Default::default)
}
struct Block;
struct Observed {
    probe: Arc<Probe>,
    total: u64,
}
impl Actor for Observed {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.probe.init_started.notify_one();
            if self.probe.block_init {
                self.probe.init_release.notified().await;
            }
            self.probe.initialized.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Block>() {
                self.probe.handler_started.notify_one();
                self.probe.handler_release.notified().await;
            }
            if let Some(delta) = message.downcast_ref::<u64>() {
                self.total += delta;
            }
            Ok(Box::new(self.total) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.probe.stop_started.notify_one();
            if self.probe.block_stop {
                self.probe.stop_release.notified().await;
            }
            self.probe.stopped.fetch_add(1, Ordering::SeqCst);
            if self.probe.fail_stop {
                Err(ActorError::InternalError("injected stop failure".into()))
            } else {
                Ok(())
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
impl Drop for Observed {
    fn drop(&mut self) {
        self.probe.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let key = config.as_ref().unwrap()["probe"].as_str().unwrap();
        let probe = probes().lock().unwrap().get(key).unwrap().clone();
        if path.ends_with("-1") {
            if probe.block_factory {
                probe.second_started.notify_one();
                probe.factory_release.notified().await;
            }
            if probe.fail_factory {
                return Err(ActorError::InitializationError(
                    "second factory rejected".into(),
                ));
            }
        }
        let reject = probe.reject_stop.clone();
        engine
            .spawn_at(
                Observed { probe, total: 0 },
                &path,
                None,
                Default::default(),
            )
            .await
            .map(|actor| {
                Box::new(RetryStopRef {
                    actor: Box::new(actor),
                    reject,
                }) as BoxedActorRef
            })
            .map_err(|e| ActorError::InitializationError(e.to_string()))
    })
}

#[tokio::test]
async fn failed_contextual_pool_remains_owned_and_can_retry_failed_cleanup() {
    let setup = Setup::new(
        Probe {
            fail_factory: true,
            ..Default::default()
        },
        3,
    );
    setup.probe.reject_stop.store(true, Ordering::SeqCst);
    let reply = setup.executor.deploy(1, &setup.command).await;
    assert!(matches!(reply, R::Failed { .. }));
    let live = setup
        .engine
        .get_actor_ref(&format!("{}-0", setup.path))
        .unwrap();
    assert_eq!(
        *live
            .send(Box::new(7u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    let status = setup.executor.status(2, &setup.path).await;
    setup.probe.reject_stop.store(false, Ordering::SeqCst);
    let stopped = setup.executor.stop(3, &setup.path).await;
    let leaked = setup
        .engine
        .get_actor_ref(&format!("{}-0", setup.path))
        .is_some();
    // Also clean the baseline's orphan before asserting the management contract.
    setup.shutdown().await;
    assert!(
        matches!(status, R::Status { states, .. } if states.len() == 1 && states[0].state == "cleanup_failed"),
        "failed cleanup lost its management record"
    );
    assert!(matches!(stopped, R::Stopped { .. }), "{stopped:?}");
    assert!(!leaked, "successful retry left the original actor alive");
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "contract.node.contextual", spawn } }
struct Setup {
    engine: Arc<ThreadActorSystem>,
    executor: NodeComponentExecutor,
    command: ComponentDeploy,
    probe: Arc<Probe>,
    path: String,
}
impl Setup {
    fn new(probe: Probe, count: usize) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("admin-owned-{}", NEXT.fetch_add(1, Ordering::SeqCst));
        let probe = Arc::new(probe);
        probes().lock().unwrap().insert(name.clone(), probe.clone());
        let engine = ThreadActorSystem::shared(Default::default());
        let executor = NodeComponentExecutor::new(engine.clone());
        Self {
            engine,
            executor,
            command: ComponentDeploy {
                name: name.clone(),
                version: "1.2.3".into(),
                artifact: AdminArtifactRef::Props {
                    factory: "contract.node.contextual".into(),
                },
                instances: if count == 1 {
                    AdminInstancePolicy::Singleton
                } else {
                    AdminInstancePolicy::Pool { count }
                },
                config: Some(format!("probe = '{name}'").into_bytes()),
            },
            probe,
            path: format!("/user/{name}"),
        }
    }
    async fn deploy(&self) -> Vec<String> {
        match self.executor.deploy(1, &self.command).await {
            R::Deployed { instances, .. } => {
                for path in &instances {
                    self.ready(path).await;
                }
                instances
            }
            other => panic!("{other:?}"),
        }
    }
    async fn ready(&self, path: &str) {
        let actor = self.engine.get_actor_ref(path).unwrap();
        let reply = actor.send(Box::new(0u64)).await.unwrap();
        assert!(
            reply.is::<u64>(),
            "readiness must dispatch a real actor message"
        );
    }
    async fn shutdown(&self) {
        self.engine.shutdown_internal().await.unwrap();
    }
}
impl Drop for Setup {
    fn drop(&mut self) {
        probes().lock().unwrap().remove(&self.command.name);
    }
}
async fn wait(signal: &Notify) {
    tokio::time::timeout(Duration::from_secs(5), signal.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelled_deploy_is_owned_until_registered_and_manageable() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            ..Default::default()
        },
        3,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { reply = &mut deploy => panic!("unexpected early reply {reply:?}"), _ = wait(&setup.probe.second_started) => {} }
    assert!(setup
        .engine
        .get_actor_ref(&format!("{}-0", setup.path))
        .is_some());
    drop(deploy);
    setup.probe.factory_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop { if matches!(setup.executor.status(2, &setup.path).await, R::Status { states, .. } if states.len()==3 && states.iter().all(|state| state.state == "running")) { break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    for index in 0..3 {
        setup.ready(&format!("{}-{index}", setup.path)).await;
    }
    assert!(matches!(
        setup.executor.stop(3, &setup.path).await,
        R::Stopped { .. }
    ));
    assert_eq!(setup.probe.stopped.load(Ordering::SeqCst), 3);
    setup.shutdown().await;
}

#[tokio::test]
async fn cancelled_failed_deploy_rolls_back_partial_pool() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            fail_factory: true,
            ..Default::default()
        },
        3,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { reply = &mut deploy => panic!("unexpected early reply {reply:?}"), _ = wait(&setup.probe.second_started) => {} }
    setup.ready(&format!("{}-0", setup.path)).await;
    drop(deploy);
    setup.probe.factory_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while setup.probe.stopped.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
        while setup
            .engine
            .get_actor_ref(&format!("{}-0", setup.path))
            .is_some()
        {
            tokio::task::yield_now().await;
        }
        // Registry retirement precedes the management owner's error handling.
        // Wait for both public observations rather than relying on scheduling.
        while !matches!(
            setup.executor.status(2, &setup.path).await,
            R::Failed { .. }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        setup.executor.status(2, &setup.path).await,
        R::Failed { .. }
    ));
    assert!(setup
        .engine
        .get_actor_ref(&format!("{}-2", setup.path))
        .is_none());
    setup.shutdown().await;
}

#[tokio::test]
async fn cancelled_stop_finishes_and_updates_management_record() {
    let setup = Setup::new(
        Probe {
            block_stop: true,
            ..Default::default()
        },
        1,
    );
    setup.deploy().await;
    let mut stop = Box::pin(setup.executor.stop(2, &setup.path));
    tokio::select! { reply = &mut stop => panic!("unexpected early reply {reply:?}"), _ = wait(&setup.probe.stop_started) => {} }
    drop(stop);
    setup.probe.stop_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            setup.executor.status(3, &setup.path).await,
            R::Failed { .. }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(setup.engine.get_actor_ref(&setup.path).is_none());
    setup.shutdown().await;
}

#[tokio::test]
async fn stopping_during_initialization_retires_storage_without_a_normal_stop_hook() {
    let setup = Setup::new(
        Probe {
            block_init: true,
            ..Default::default()
        },
        1,
    );
    assert!(matches!(
        setup.executor.deploy(1, &setup.command).await,
        R::Deployed { .. }
    ));
    wait(&setup.probe.init_started).await;
    assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 0);
    let stopped =
        tokio::time::timeout(Duration::from_secs(5), setup.executor.stop(2, &setup.path)).await;
    let dropped_before_shutdown = setup.probe.dropped.load(Ordering::SeqCst);
    let registered_before_shutdown = setup.engine.actor_count();
    let status_before_shutdown = setup.executor.status(3, &setup.path).await;
    setup.probe.init_release.notify_one();
    setup.shutdown().await;
    assert!(matches!(stopped, Ok(R::Stopped { .. })), "{stopped:?}");
    assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(setup.probe.stopped.load(Ordering::SeqCst), 0);
    assert_eq!(setup.probe.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        dropped_before_shutdown, 1,
        "Stop must retire storage before returning"
    );
    assert_eq!(registered_before_shutdown, 0);
    assert!(matches!(status_before_shutdown, R::Failed { .. }));
    assert!(setup.engine.get_actor_ref(&setup.path).is_none());
    assert!(matches!(
        setup.executor.status(3, &setup.path).await,
        R::Failed { .. }
    ));
}

#[tokio::test]
async fn cancelled_partial_deploy_retires_an_initializing_member_without_a_stop_hook() {
    let setup = Setup::new(
        Probe {
            block_init: true,
            block_factory: true,
            fail_factory: true,
            ..Default::default()
        },
        3,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { reply = &mut deploy => panic!("unexpected early reply {reply:?}"), _ = wait(&setup.probe.second_started) => {} }
    wait(&setup.probe.init_started).await;
    assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 0);
    drop(deploy);
    setup.probe.factory_release.notify_one();
    let retired = tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            setup.executor.status(2, &setup.path).await,
            R::Failed { .. }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let dropped_before_shutdown = setup.probe.dropped.load(Ordering::SeqCst);
    let registered_before_shutdown = setup.engine.actor_count();
    setup.probe.init_release.notify_one();
    setup.shutdown().await;
    assert!(
        retired.is_ok(),
        "partial deployment retained an initializing member"
    );
    assert_eq!(setup.probe.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(setup.probe.stopped.load(Ordering::SeqCst), 0);
    assert_eq!(setup.probe.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        dropped_before_shutdown, 1,
        "rollback must retire storage before clearing ownership"
    );
    assert_eq!(registered_before_shutdown, 0);
    assert_eq!(setup.engine.actor_count(), 0);
    assert!(matches!(
        setup.executor.status(3, &setup.path).await,
        R::Failed { .. }
    ));
}

#[tokio::test]
async fn stop_failure_is_reported_and_remains_inspectable() {
    let setup = Setup::new(
        Probe {
            fail_stop: true,
            ..Default::default()
        },
        1,
    );
    setup.deploy().await;
    let stopped = setup.executor.stop(2, &setup.path).await;
    assert!(
        matches!(stopped, R::Failed { ref detail, .. } if detail.contains("injected stop failure")),
        "{stopped:?}"
    );
    assert!(
        matches!(setup.executor.status(3, &setup.path).await, R::Status { states, .. } if states[0].state == "cleanup_failed")
    );
    assert!(setup.engine.get_actor_ref(&setup.path).is_none());
    // Physical registry removal is not proof that the stop hook succeeded.
    // A completed native stop keeps its first receipt. Retrying a transport
    // rejection before native stop starts is a separate, recoverable case
    // covered by failed_contextual_pool_remains_owned_and_can_retry_failed_cleanup.
    let first_detail = match stopped {
        R::Failed { detail, .. } => detail,
        _ => unreachable!("checked first stop failure"),
    };
    for request_id in [4, 5] {
        let retry = setup.executor.stop(request_id, &setup.path).await;
        assert!(
            matches!(retry, R::Failed { ref detail, .. } if detail == &first_detail),
            "completed cleanup failure changed on retry: {retry:?}"
        );
        assert!(
            matches!(setup.executor.status(request_id + 10, &setup.path).await,
                R::Status { states, .. } if states.len() == 1 && states[0].state == "cleanup_failed")
        );
        assert!(setup.engine.get_actor_ref(&setup.path).is_none());
        assert_eq!(
            setup.probe.stopped.load(Ordering::SeqCst),
            1,
            "retry must not run an already completed stop hook again"
        );
    }
    setup.shutdown().await;
}

#[tokio::test]
async fn drain_budget_includes_the_current_handler_even_when_queue_is_empty() {
    let setup = Setup::new(Probe::default(), 1);
    setup.deploy().await;
    let actor = setup.engine.get_actor_ref(&setup.path).unwrap();
    let task = tokio::spawn(async move { actor.send(Box::new(Block)).await });
    wait(&setup.probe.handler_started).await;
    assert!(
        setup
            .engine
            .get_mailbox(&setup.path)
            .unwrap()
            .is_empty()
            .await
    );
    let reply = tokio::time::timeout(
        Duration::from_millis(500),
        setup.executor.drain(2, &setup.path, 40),
    )
    .await;
    // Release regardless of the assertion outcome to avoid a blocked test actor.
    setup.probe.handler_release.notify_one();
    task.await.unwrap().unwrap();
    assert!(
        matches!(
            reply,
            Ok(R::Drained {
                drained: 0,
                aborted: 1,
                ..
            })
        ),
        "{reply:?}"
    );
    assert!(
        setup
            .engine
            .get_actor_ref(&setup.path)
            .unwrap()
            .is_alive()
            .await
    );
    assert!(matches!(
        setup.executor.drain(3, &setup.path, 1000).await,
        R::Drained {
            drained: 1,
            aborted: 0,
            ..
        }
    ));
    setup.shutdown().await;
}

#[tokio::test]
async fn old_deployment_cannot_report_or_stop_a_replacement_at_the_same_path() {
    let setup = Setup::new(Probe::default(), 1);
    setup.deploy().await;
    setup
        .engine
        .get_actor_ref(&setup.path)
        .unwrap()
        .stop()
        .await
        .unwrap();
    let replacement = setup
        .engine
        .spawn_at(
            Observed {
                probe: Arc::default(),
                total: 42,
            },
            &setup.path,
            None,
            Default::default(),
        )
        .await
        .unwrap();
    assert!(
        matches!(setup.executor.status(2, &setup.path).await, R::Status { states, .. } if states[0].state == "unavailable")
    );
    assert!(matches!(
        setup.executor.stop(3, &setup.path).await,
        R::Stopped { .. }
    ));
    assert!(replacement.is_alive().await);
    assert_eq!(
        *replacement
            .send(Box::new(0u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    replacement.stop().await.unwrap();
    setup.shutdown().await;
}

#[tokio::test]
async fn stopping_one_pool_member_keeps_other_versions_and_identical_retry_keeps_state() {
    let setup = Setup::new(Probe::default(), 3);
    let paths = setup.deploy().await;
    let actor = setup.engine.get_actor_ref(&paths[1]).unwrap();
    actor.send(Box::new(7u64)).await.unwrap();
    assert_eq!(setup.deploy().await, paths);
    assert_eq!(
        *actor
            .send(Box::new(0u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    assert!(matches!(
        setup.executor.stop(2, &paths[0]).await,
        R::Stopped { .. }
    ));
    assert!(
        matches!(setup.executor.status(3, &setup.path).await, R::Status { states, .. } if states.len()==2 && states.iter().all(|s|s.version=="1.2.3"))
    );
    assert!(
        matches!(
            setup.executor.deploy(4, &setup.command).await,
            R::Failed { .. }
        ),
        "partial pool must not pretend to satisfy full retry"
    );
    setup.executor.stop(5, &setup.path).await;
    setup.shutdown().await;
}

#[tokio::test]
async fn malformed_direct_requests_are_rejected_before_spawning() {
    let setup = Setup::new(Probe::default(), 1);
    for change in 0..5 {
        let mut command = setup.command.clone();
        match change {
            0 => command.instances = AdminInstancePolicy::Pool { count: usize::MAX },
            1 => command.name = "../escape".into(),
            2 => command.version = "not-semver".into(),
            3 => command.config = Some(vec![0xff]),
            _ => command.config = Some(vec![b'x'; parrot_node::executor_v2::MAX_CONFIG_BYTES + 1]),
        }
        assert!(matches!(
            setup.executor.deploy(1, &command).await,
            R::Failed { .. }
        ));
        assert_eq!(setup.engine.actor_count(), 0);
    }
    setup.shutdown().await;
}

#[tokio::test]
async fn blocked_factory_does_not_block_an_unrelated_components_stop() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            ..Default::default()
        },
        2,
    );
    let mut neighbor = setup.command.clone();
    neighbor.name.push_str("-neighbor");
    neighbor.instances = AdminInstancePolicy::Singleton;
    assert!(matches!(
        setup.executor.deploy(1, &neighbor).await,
        R::Deployed { .. }
    ));
    let mut pending = Box::pin(setup.executor.deploy(2, &setup.command));
    tokio::select! { result = &mut pending => panic!("unexpected {result:?}"), _ = wait(&setup.probe.second_started) => {} }
    let stopped = tokio::time::timeout(
        Duration::from_millis(500),
        setup.executor.stop(3, &format!("/user/{}", neighbor.name)),
    )
    .await;
    setup.probe.factory_release.notify_one();
    assert!(matches!(pending.await, R::Deployed { .. }));
    assert!(
        matches!(stopped, Ok(R::Stopped { .. })),
        "unrelated component was blocked: {stopped:?}"
    );
    setup.executor.stop(4, &setup.path).await;
    setup.shutdown().await;
}

#[cfg(not(feature = "wasm"))]
#[tokio::test]
async fn node_without_wasm_feature_explicitly_rejects_wasm_artifacts() {
    let setup = Setup::new(Probe::default(), 1);
    let mut command = setup.command.clone();
    command.artifact = AdminArtifactRef::Wasm {
        digest: "0".repeat(64),
        uri: "file:///not-loaded.wasm".into(),
    };
    assert!(
        matches!(setup.executor.deploy(1, &command).await, R::Failed { code, .. } if code == parrot_remote::admin_v2::v2_err::DIALECT_MISMATCH)
    );
    assert_eq!(setup.engine.actor_count(), 0);
    setup.shutdown().await;
}

#[tokio::test]
async fn pending_pool_reserves_member_selectors_until_deploy_finishes() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            ..Default::default()
        },
        2,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { result = &mut deploy => panic!("unexpected {result:?}"), _ = wait(&setup.probe.second_started) => {} }
    let member = format!("{}-1", setup.path);
    // A zero-budget drain must find the reservation and wait on the deployment,
    // rather than falsely reporting that this member does not exist.
    assert!(matches!(setup.executor.drain(2, &member, 0).await,
        R::Failed { code, .. } if code == parrot_remote::admin_v2::v2_err::DRAIN_TIMEOUT));
    setup.probe.factory_release.notify_one();
    assert!(matches!(deploy.await, R::Deployed { .. }));
    assert!(matches!(
        setup.executor.stop(3, &member).await,
        R::Stopped { .. }
    ));
    assert!(setup
        .engine
        .get_actor_ref(&format!("{}-0", setup.path))
        .is_some());
    setup.executor.stop(4, &setup.path).await;
    setup.shutdown().await;
}

#[tokio::test]
async fn component_roots_cannot_alias_another_pools_member_even_while_pending() {
    let setup = Setup::new(
        Probe {
            block_factory: true,
            ..Default::default()
        },
        2,
    );
    let mut deploy = Box::pin(setup.executor.deploy(1, &setup.command));
    tokio::select! { result = &mut deploy => panic!("unexpected {result:?}"), _ = wait(&setup.probe.second_started) => {} }
    let mut alias = setup.command.clone();
    alias.name.push_str("-1");
    // Its own members have distinct actor paths, but its component selector
    // would shadow the original pool's second member.
    assert!(matches!(
        setup.executor.deploy(2, &alias).await,
        R::Failed { .. }
    ));
    setup.probe.factory_release.notify_one();
    assert!(matches!(deploy.await, R::Deployed { .. }));
    assert!(matches!(
        setup.executor.deploy(3, &alias).await,
        R::Failed { .. }
    ));
    setup.executor.stop(4, &setup.path).await;
    setup.probe.factory_release.notify_one();
    assert!(matches!(
        setup.executor.deploy(5, &alias).await,
        R::Deployed { .. }
    ));
    assert!(matches!(
        setup.executor.deploy(6, &setup.command).await,
        R::Failed { .. }
    ));
    setup
        .executor
        .stop(7, &format!("/user/{}", alias.name))
        .await;
    setup.shutdown().await;
}
