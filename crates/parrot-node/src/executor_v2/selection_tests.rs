use super::*;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture};

fn spawn_counter(
    engine: Arc<ThreadActorSystem>,
    path: String,
    _: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        engine
            .spawn_at(
                crate::CounterActor::default(),
                &path,
                None,
                Default::default(),
            )
            .await
            .map(|actor| Box::new(actor) as BoxedActorRef)
            .map_err(|error| ActorError::InitializationError(error.to_string()))
    })
}
parrot_api::message::inventory::submit! {
    parrot_app::host::ThreadPropsFactory {
        name: "contract.node.selection.counter",
        spawn: spawn_counter,
    }
}

async fn stale_selection_does_not_retire_replacement(drain: bool, selector: &str) {
    let engine = ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::new(engine.clone());
    let command = ComponentDeploy {
        name: "selection-cohort".into(),
        version: "1.0.0".into(),
        artifact: AdminArtifactRef::Props {
            factory: "contract.node.selection.counter".into(),
        },
        instances: AdminInstancePolicy::Pool { count: 2 },
        config: None,
    };
    assert!(matches!(
        executor.deploy(1, &command).await,
        R::Deployed { .. }
    ));
    // Hold the production component lock for an earlier Stop's retirement.
    // Explicit polling registers the redeploy and stale operation in FIFO order;
    // no scheduler timing or sleeps establish this race.
    let held = executor
        .operation_locks(vec![command.name.clone()])
        .remove(0)
        .lock_owned()
        .await;
    let old = executor.owner.records.lock().unwrap()[&command.name]
        .instances
        .clone();
    let mut redeploy = Box::pin(executor.deploy_owned(2, command.clone()));
    assert!(futures::poll!(&mut redeploy).is_pending());
    let mut stale = Box::pin(async {
        if drain {
            executor
                .drain_owned(
                    3,
                    selector.into(),
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await
        } else {
            executor.stop_owned(3, selector.into()).await
        }
    });
    assert!(futures::poll!(&mut stale).is_pending());
    // Complete retirement under that lock using the same production helpers as
    // Stop. The queued deployment is intentionally byte-for-byte identical.
    for instance in old.iter().rev() {
        stop_instance(instance).await.unwrap();
        executor.retire_record(&command.name, instance, None);
    }
    drop(held);
    let (deployed, stale_reply) = tokio::time::timeout(Duration::from_secs(10), async {
        futures::join!(redeploy, stale)
    })
    .await
    .unwrap();
    assert!(matches!(deployed, R::Deployed { .. }), "{deployed:?}");
    let status = executor.status(4, "/user/selection-cohort").await;
    let mut totals = Vec::new();
    for path in ["/user/selection-cohort-0", "/user/selection-cohort-1"] {
        if let Some(actor) = engine.get_actor_ref(path) {
            actor.send(Box::new(crate::NInc(7))).await.unwrap();
            let reply = actor.send(Box::new(crate::NGetTotal)).await.unwrap();
            totals.push(reply.downcast_ref::<crate::NTotal>().unwrap().0);
        }
    }
    // Clean even the faulty baseline before checking the public observations.
    executor.stop(5, "/user").await;
    engine.shutdown_internal().await.unwrap();
    assert!(
        matches!(stale_reply, R::Failed { req_id: 3, code, .. } if code == v2_err::COMPONENT_NOT_FOUND),
        "stale request retargeted a new deployment: {stale_reply:?}"
    );
    assert!(
        matches!(status, R::Status { states, .. } if states.len() == 2 && states.iter().all(|s|s.state=="running"))
    );
    assert_eq!(
        totals,
        vec![7, 7],
        "replacement actors must still serve independently"
    );
}

#[tokio::test]
async fn stale_stop_component_cannot_retire_same_name_redeployment() {
    stale_selection_does_not_retire_replacement(false, "/user/selection-cohort").await;
}
#[tokio::test]
async fn stale_stop_member_cannot_retire_same_name_redeployment() {
    stale_selection_does_not_retire_replacement(false, "/user/selection-cohort-1").await;
}
#[tokio::test]
async fn stale_stop_all_cannot_retire_same_name_redeployment() {
    stale_selection_does_not_retire_replacement(false, "/user").await;
}
#[tokio::test]
async fn stale_drain_component_cannot_retire_same_name_redeployment() {
    stale_selection_does_not_retire_replacement(true, "/user/selection-cohort").await;
}
#[tokio::test]
async fn stale_drain_member_cannot_retire_same_name_redeployment() {
    stale_selection_does_not_retire_replacement(true, "/user/selection-cohort-1").await;
}
#[tokio::test]
async fn stale_drain_all_cannot_retire_same_name_redeployment() {
    stale_selection_does_not_retire_replacement(true, "/user").await;
}

struct FactoryGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail_factory: bool,
    reject_cleanup: std::sync::atomic::AtomicBool,
}
fn gates() -> &'static Mutex<BTreeMap<String, Arc<FactoryGate>>> {
    static GATES: OnceLock<Mutex<BTreeMap<String, Arc<FactoryGate>>>> = OnceLock::new();
    GATES.get_or_init(Default::default)
}
#[derive(Debug)]
struct RejectOnceStop {
    actor: BoxedActorRef,
    reject: Arc<std::sync::atomic::AtomicBool>,
}
impl ActorRef for RejectOnceStop {
    fn send<'a>(
        &'a self,
        message: parrot_api::types::BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        self.actor.send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: parrot_api::types::BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        self.actor.send_with_timeout(message, timeout)
    }
    fn deliver<'a>(
        &'a self,
        message: parrot_api::types::BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.deliver(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.reject.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(ActorError::InternalError("first cleanup rejected".into()));
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
fn spawn_gated(
    engine: Arc<ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let key = config.as_ref().unwrap()["gate"].as_str().unwrap();
        let gate = gates().lock().unwrap()[key].clone();
        if path.ends_with("-1") {
            gate.entered.notify_one();
            gate.release.notified().await;
            if gate.fail_factory {
                return Err(ActorError::InitializationError(
                    "second instance rejected".into(),
                ));
            }
        }
        let actor = spawn_counter(engine, path, None).await?;
        Ok(Box::new(RejectOnceStop {
            actor,
            reject: Arc::new(std::sync::atomic::AtomicBool::new(
                gate.reject_cleanup
                    .load(std::sync::atomic::Ordering::SeqCst),
            )),
        }) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! {
    parrot_app::host::ThreadPropsFactory {
        name: "contract.node.selection.gated",
        spawn: spawn_gated,
    }
}
async fn pending_selection_survives_publication(drain: bool, fail: bool) {
    let key = format!("pending-{drain}-{fail}");
    let gate = Arc::new(FactoryGate {
        entered: Default::default(),
        release: Default::default(),
        fail_factory: fail,
        reject_cleanup: std::sync::atomic::AtomicBool::new(fail),
    });
    gates().lock().unwrap().insert(key.clone(), gate.clone());
    let engine = ThreadActorSystem::shared(Default::default());
    let executor = NodeComponentExecutor::new(engine.clone());
    let command = ComponentDeploy {
        name: "pending-cohort".into(),
        version: "1.0.0".into(),
        artifact: AdminArtifactRef::Props {
            factory: "contract.node.selection.gated".into(),
        },
        instances: AdminInstancePolicy::Pool { count: 2 },
        config: Some(format!("gate = '{key}'").into_bytes()),
    };
    let mut deployment = Box::pin(executor.deploy(1, &command));
    tokio::select! {
        result = &mut deployment => panic!("early deploy result: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(5), gate.entered.notified()) => result.unwrap(),
    }
    assert!(engine.get_actor_ref("/user/pending-cohort-0").is_some());
    let mut waiting = Box::pin(async {
        if drain {
            executor
                .drain_owned(
                    2,
                    "/user/pending-cohort".into(),
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await
        } else {
            executor.stop_owned(2, "/user/pending-cohort".into()).await
        }
    });
    assert!(futures::poll!(&mut waiting).is_pending());
    gate.release.notify_one();
    let (deployed, retired) = tokio::time::timeout(Duration::from_secs(10), async {
        futures::join!(deployment, waiting)
    })
    .await
    .unwrap();
    let status = executor.status(3, "/user/pending-cohort").await;
    let count = engine.actor_count();
    engine.shutdown_internal().await.unwrap();
    gates().lock().unwrap().remove(&key);
    if fail {
        assert!(matches!(deployed, R::Failed { .. }), "{deployed:?}");
    } else {
        assert!(matches!(deployed, R::Deployed { .. }), "{deployed:?}");
    }
    if drain {
        assert!(
            matches!(retired, R::Drained { req_id: 2, drained, aborted: 0 } if drained == if fail { 1 } else { 2 }),
            "{retired:?}"
        );
    } else {
        assert!(matches!(retired, R::Stopped { req_id: 2 }), "{retired:?}");
    }
    assert!(
        matches!(status, R::Failed { code, .. } if code == v2_err::COMPONENT_NOT_FOUND),
        "{status:?}"
    );
    assert_eq!(count, 0);
}
#[tokio::test]
async fn pending_stop_follows_the_same_reservation_through_commit() {
    pending_selection_survives_publication(false, false).await;
}
#[tokio::test]
async fn pending_drain_follows_the_same_reservation_through_commit() {
    pending_selection_survives_publication(true, false).await;
}
#[tokio::test]
async fn pending_stop_follows_the_same_reservation_through_failed_cleanup() {
    pending_selection_survives_publication(false, true).await;
}
#[tokio::test]
async fn pending_drain_follows_the_same_reservation_through_failed_cleanup() {
    pending_selection_survives_publication(true, true).await;
}
