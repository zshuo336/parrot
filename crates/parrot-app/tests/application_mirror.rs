#![cfg(feature = "host")]
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::{ApplicationLookup, GatewayRoutes, ThreadPropsFactory, ThreadPropsSpawner},
    manifest::{AppManifest, ComponentSpec, InstancePolicy},
    mirror::MirrorSpec,
    planner::{plan, LocalTopology},
};
use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{sync::Arc, time::Duration};
#[derive(serde::Serialize, serde::Deserialize)]
struct Work(u64);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:application.mirror#v1", type_id: std::any::TypeId::of::<Work>(),
        encode: |message| {
            let work = message.downcast_ref::<Work>().ok_or("Work required")?;
            if work.0 == 9000 { std::thread::sleep(Duration::from_millis(10)); }
            parrot_api::message::serde_remote_serialize(work)
        },
        decode: |bytes| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<Work>(bytes)?) as BoxedMessage),
    }
}
#[derive(Clone)]
struct Seen {
    value: u64,
    context: Option<TraceContext>,
}
struct Observer {
    seen: Vec<Seen>,
}
impl Actor for Observer {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(work) = message.downcast_ref::<Work>() {
                self.seen.push(Seen {
                    value: work.0,
                    context: TraceContext::current(),
                });
                Ok(Box::new(Work(work.0 + 1)) as BoxedMessage)
            } else {
                Ok(Box::new(self.seen.clone()) as BoxedMessage)
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    _: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let actor = engine
            .spawn_at(
                Observer { seen: Vec::new() },
                &path,
                None,
                Default::default(),
            )
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(e.to_string()))?;
        Ok(Box::new(actor) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "app.mirror-test", spawn } }
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
fn manifest() -> AppManifest {
    AppManifest::from_toml("name = 'mirror-app'\nversion = '1.0.0'\n[[components]]\nname = 'primary'\nengine = 'parrot'\nartifact = { Props = { factory = 'app.mirror-test' } }\n[[components]]\nname = 'shadow'\nengine = 'parrot'\nartifact = { Props = { factory = 'app.mirror-test' } }\n[[wiring]]\nfrom = 'shadow:/user/out'\nto = 'primary:/user/primary'\n[[mirrors]]\nsource = 'primary'\nshadow = 'shadow'\ndelivery_timeout_ms = 3000\n").unwrap()
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    remote: Arc<RemoteActorSystem>,
    context: AssemblingContext,
    spawner: ThreadPropsSpawner,
}
impl Host {
    fn new() -> Self {
        let engine = ThreadActorSystem::shared(Default::default());
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp(
                format!("mirror-host-{}", uuidless()),
                Some("127.0.0.1:0".parse().unwrap()),
            ),
            Arc::new(ApplicationLookup {
                ts: engine.clone(),
                routes: Arc::new(GatewayRoutes::default()),
            }),
        )
        .unwrap();
        let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
        let context = AssemblingContext::new().with_mirror_host(&remote);
        Self {
            engine,
            remote,
            context,
            spawner,
        }
    }
    async fn assemble(&mut self, manifest: &AppManifest) -> Result<(), DeployError> {
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &self.spawner,
        };
        self.context
            .assemble(
                &plan(manifest, &LocalTopology).unwrap(),
                &deployer,
                manifest,
                &Default::default(),
            )
            .await
    }
    async fn teardown(&mut self) {
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &self.spawner,
        };
        self.context.teardown(&deployer).await.unwrap();
    }
    async fn close(mut self) {
        self.teardown().await;
        self.remote.shutdown().await.unwrap();
        self.engine.shutdown_internal().await.unwrap();
    }
    async fn seen(&self, path: &str) -> Vec<Seen> {
        *self
            .engine
            .get_actor_ref(path)
            .unwrap()
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<Vec<Seen>>()
            .unwrap()
    }
    async fn expect_count(&self, path: &str, count: usize) -> Vec<Seen> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let seen = self.seen(path).await;
                if seen.len() >= count {
                    assert_eq!(seen.len(), count);
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("actor execution deadline")
    }
}
fn uuidless() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

async fn component_stop_case(mirrored: bool, cloned: bool) {
    let mut m = manifest();
    if !mirrored {
        m.mirrors.clear();
    }
    let mut host = Host::new();
    host.assemble(&m).await.unwrap();
    let original = host.context.component_ref("primary").unwrap();
    let copy = original.clone_boxed();
    let actor = if cloned { &copy } else { original };
    let shadow = host.engine.get_actor_ref("/user/shadow").unwrap();
    drop(actor.stop());
    assert!(actor.is_alive().await, "an unpolled stop must do nothing");
    actor.send(Box::new(Work(10))).await.unwrap();
    if mirrored {
        host.expect_count("/user/shadow", 1).await;
    }
    tokio::time::timeout(Duration::from_secs(5), actor.stop())
        .await
        .expect("component stop deadline")
        .expect("component ownership must survive Mirror wrapping");
    assert!(!actor.is_alive().await);
    assert!(actor.send(Box::new(Work(11))).await.is_err());
    assert!(shadow.is_alive().await, "stop must not be mirrored");
    assert_eq!(host.seen("/user/shadow").await.len(), usize::from(mirrored));
    host.teardown().await;
    assert!(host.context.started().is_empty());
    assert!(!shadow.is_alive().await);
    host.close().await;
}
#[tokio::test]
async fn plain_component_retains_stop_ownership() {
    component_stop_case(false, false).await;
}
#[tokio::test]
async fn plain_component_clone_retains_stop_ownership() {
    component_stop_case(false, true).await;
}
#[tokio::test]
async fn mirrored_component_retains_stop_ownership() {
    component_stop_case(true, false).await;
}
#[tokio::test]
async fn mirrored_component_clone_retains_stop_ownership() {
    component_stop_case(true, true).await;
}
#[tokio::test]
async fn mirrored_wire_and_clone_do_not_gain_stop_ownership() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let wire = host.context.wired_ref("shadow:/user/out").unwrap();
    let clone = wire.clone_boxed();
    for actor in [&wire, &clone] {
        assert!(matches!(
            actor.stop().await,
            Err(parrot_api::errors::ActorError::MessageHandlingError(_))
        ));
        actor.send(Box::new(Work(10))).await.unwrap();
    }
    host.expect_count("/user/primary", 2).await;
    host.expect_count("/user/shadow", 2).await;
    host.close().await;
}
#[tokio::test]
async fn mirrored_component_stop_after_drain_is_fenced_at_poll() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let actor = host.context.component_ref("primary").unwrap().clone_boxed();
    let pending = actor.stop();
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &host.spawner,
    };
    host.context.drain(&deployer).await.unwrap();
    assert!(matches!(
        pending.await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(matches!(
        actor.clone_boxed().stop().await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(
        host.engine
            .get_actor_ref("/user/primary")
            .unwrap()
            .is_alive()
            .await
    );
    assert!(
        host.engine
            .get_actor_ref("/user/shadow")
            .unwrap()
            .is_alive()
            .await
    );
    host.close().await;
}
#[tokio::test]
async fn mirrored_component_old_stop_cannot_stop_replacement() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let actor = host.context.component_ref("primary").unwrap().clone_boxed();
    let pending = actor.stop();
    host.teardown().await;
    host.assemble(&manifest()).await.unwrap();
    assert!(matches!(
        pending.await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(matches!(
        actor.clone_boxed().stop().await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    host.context
        .component_ref("primary")
        .unwrap()
        .send(Box::new(Work(10)))
        .await
        .unwrap();
    host.expect_count("/user/primary", 1).await;
    host.expect_count("/user/shadow", 1).await;
    host.close().await;
}
#[tokio::test]
async fn mirrored_pool_component_stop_is_instance_scoped() {
    let mut m = manifest();
    m.wiring.clear();
    m.components[0].instances = InstancePolicy::Pool(2);
    m.components[1].instances = InstancePolicy::Pool(2);
    let mut host = Host::new();
    host.assemble(&m).await.unwrap();
    let actor = host.context.component_refs("primary").unwrap()[1].clone_boxed();
    actor.stop().await.unwrap();
    assert!(!actor.is_alive().await);
    host.context.component_refs("primary").unwrap()[0]
        .send(Box::new(Work(10)))
        .await
        .unwrap();
    host.expect_count("/user/shadow-0", 1).await;
    assert!(
        host.engine
            .get_actor_ref("/user/shadow-1")
            .unwrap()
            .is_alive()
            .await
    );
    assert!(host.seen("/user/shadow-1").await.is_empty());
    host.close().await;
}
#[tokio::test]
async fn cancelled_policy_does_not_revoke_component_stop() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let actor = host.context.component_ref("primary").unwrap().clone_boxed();
    let removed = host.remote.remove_wire_mirror("/user/primary").unwrap();
    removed.wait_closed().await;
    actor.stop().await.unwrap();
    assert!(!actor.is_alive().await);
    assert!(
        host.engine
            .get_actor_ref("/user/shadow")
            .unwrap()
            .is_alive()
            .await
    );
    host.close().await;
}
#[tokio::test]
async fn replacement_policy_does_not_revoke_stop_or_gain_application_ownership() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let actor = host.context.component_ref("primary").unwrap().clone_boxed();
    let successor = host
        .remote
        .register_wire_mirror(
            parrot_remote::mirror::WireMirrorPolicy::new("/user/primary", "/user/shadow"),
            Default::default(),
        )
        .unwrap();
    actor.stop().await.unwrap();
    assert!(!actor.is_alive().await);
    assert!(!successor.is_closed());
    assert_eq!(successor.stats().enqueued, 0);
    assert!(
        host.engine
            .get_actor_ref("/user/shadow")
            .unwrap()
            .is_alive()
            .await
    );
    host.teardown().await;
    assert!(
        !successor.is_closed(),
        "teardown must preserve another policy owner"
    );
    successor.close();
    successor.wait_closed().await;
    host.close().await;
}
#[tokio::test]
async fn dropped_context_fences_unpolled_component_stop_without_stopping_actors() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let actor = host.context.component_ref("primary").unwrap().clone_boxed();
    let primary = host.engine.get_actor_ref("/user/primary").unwrap();
    let shadow = host.engine.get_actor_ref("/user/shadow").unwrap();
    let pending = actor.stop();
    drop(std::mem::take(&mut host.context));
    assert!(matches!(
        pending.await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(primary.is_alive().await);
    assert!(shadow.is_alive().await);
    primary.stop().await.unwrap();
    shadow.stop().await.unwrap();
    host.close().await;
}
async fn local_case(kind: u8, traced: bool, wire: bool) {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let actor = if wire {
        host.context.wired_ref("shadow:/user/out").unwrap()
    } else {
        host.context.component_ref("primary").unwrap().clone_boxed()
    };
    let root = traced.then(TraceContext::root);
    let unrelated = TraceContext::root();
    let call: BoxedFuture<'_, ActorResult<()>> = trace::in_scope(root.clone(), || match kind {
        0 => Box::pin(async_from_reply(actor.send(Box::new(Work(40))))),
        1 => Box::pin(async_from_reply(
            actor.send_with_timeout(Box::new(Work(40)), Some(Duration::from_secs(3))),
        )),
        2 => actor.deliver(Box::new(Work(40))),
        3 => Box::pin(async_from_reply(
            actor.send_with_options(Box::new(Work(40)), Default::default()),
        )),
        _ => actor.deliver_with_options(Box::new(Work(40)), Default::default()),
    });
    trace::scope(Some(unrelated), call).await.unwrap();
    for path in ["/user/primary", "/user/shadow"] {
        let seen = host.expect_count(path, 1).await;
        assert_eq!(seen[0].value, 40);
        match (&root, &seen[0].context) {
            (Some(parent), Some(child)) => {
                assert_eq!(parent.trace_id(), child.trace_id());
                assert_eq!(Some(parent.span_id()), child.parent_span_id());
            }
            (None, None) => {}
            _ => panic!("construction-time trace presence changed"),
        }
    }
    let pending = actor.deliver(Box::new(Work(99)));
    host.teardown().await;
    assert!(matches!(
        pending.await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(!actor.is_alive().await);
    host.close().await;
}
async fn async_from_reply(reply: BoxedFuture<'_, ActorResult<BoxedMessage>>) -> ActorResult<()> {
    assert_eq!(reply.await?.downcast::<Work>().unwrap().0, 41);
    Ok(())
}
macro_rules! local {
    ($name:ident, $kind:expr, $traced:expr, $wired:expr) => {
        #[tokio::test]
        async fn $name() {
            local_case($kind, $traced, $wired).await;
        }
    };
}
local!(component_ask, 0, true, false);
local!(component_timeout, 1, true, false);
local!(component_tell, 2, true, false);
local!(component_ask_options, 3, true, false);
local!(component_tell_options, 4, true, false);
local!(component_explicit_absent_trace, 0, false, false);
local!(wiring_ask, 0, true, true);
local!(wiring_tell, 2, true, true);
local!(wiring_explicit_absent_trace, 4, false, true);
#[tokio::test]
async fn unpolled_local_calls_do_not_copy_or_execute() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let actor = host.context.component_ref("primary").unwrap();
    drop(actor.send(Box::new(Work(40))));
    assert!(host.seen("/user/primary").await.is_empty());
    assert!(host.seen("/user/shadow").await.is_empty());
    host.close().await;
}
#[tokio::test]
async fn unknown_codec_keeps_primary_local_semantics() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let reply = host
        .context
        .component_ref("primary")
        .unwrap()
        .send(Box::new(()))
        .await
        .unwrap();
    assert!(reply.downcast::<Vec<Seen>>().unwrap().is_empty());
    assert!(host.seen("/user/shadow").await.is_empty());
    host.close().await;
}
#[tokio::test]
async fn pool_instances_pair_by_index_and_old_refs_never_rebind() {
    let mut m = manifest();
    m.wiring.clear();
    m.components[0].instances = InstancePolicy::Pool(2);
    m.components[1].instances = InstancePolicy::Pool(2);
    let mut host = Host::new();
    host.assemble(&m).await.unwrap();
    let actor = host.context.component_refs("primary").unwrap()[1].clone_boxed();
    actor.send(Box::new(Work(40))).await.unwrap();
    host.expect_count("/user/shadow-1", 1).await;
    assert!(host.seen("/user/shadow-0").await.is_empty());
    host.teardown().await;
    host.assemble(&m).await.unwrap();
    assert!(actor.deliver(Box::new(Work(50))).await.is_err());
    assert!(host.seen("/user/shadow-1").await.is_empty());
    host.close().await;
}
#[tokio::test]
async fn manifest_without_bound_host_rejects_before_spawning() {
    let m = manifest();
    let mut host = Host::new();
    host.context = AssemblingContext::new();
    assert!(host.assemble(&m).await.is_err());
    assert!(host.context.started().is_empty());
    assert!(host.engine.get_actor_ref("/user/primary").is_none());
    host.close().await;
}
#[tokio::test]
async fn registration_conflict_rolls_back_actors_without_removing_existing_policy() {
    let mut host = Host::new();
    let existing = host
        .remote
        .register_wire_mirror(
            parrot_remote::mirror::WireMirrorPolicy::new("/user/primary", "/prior"),
            Default::default(),
        )
        .unwrap();
    assert!(host.assemble(&manifest()).await.is_err());
    assert!(host.context.started().is_empty());
    assert!(!existing.is_closed());
    assert!(host.engine.get_actor_ref("/user/primary").is_none());
    host.close().await;
}
#[tokio::test]
async fn tcp_ingress_uses_manifest_policy_exactly_once() {
    let mut host = Host::new();
    host.remote.start().await.unwrap();
    host.assemble(&manifest()).await.unwrap();
    let peer = Host::new();
    peer.remote.start().await.unwrap();
    peer.remote
        .connect(&NodeAddr::tcp(
            &host.remote.config.node_id,
            host.remote.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let actor = peer
        .remote
        .remote_ref(&format!(
            "parrot://{}/user/primary",
            host.remote.config.node_id
        ))
        .unwrap();
    actor.send(Box::new(Work(40))).await.unwrap();
    host.expect_count("/user/shadow", 1).await;
    actor
        .deliver_with_ack(Box::new(Work(41)), Duration::from_secs(3))
        .await
        .unwrap();
    host.expect_count("/user/shadow", 2).await;
    actor.deliver(Box::new(Work(42))).await.unwrap();
    host.expect_count("/user/shadow", 3).await;
    host.expect_count("/user/primary", 3).await;
    peer.close().await;
    host.close().await;
}
#[test]
fn manifest_roundtrip_keeps_policies_and_validates_counts_before_expansion() {
    let mut m = manifest();
    assert_eq!(AppManifest::from_toml(&m.to_toml().unwrap()).unwrap(), m);
    m.components[0].instances = InstancePolicy::Pool(usize::MAX);
    assert!(parrot_app::validate(&m).is_err());
    let mut m = manifest();
    m.components[1].instances = InstancePolicy::Pool(2);
    assert!(parrot_app::validate(&m).is_err());
    let mut m = manifest();
    m.mirrors.push(MirrorSpec::new("primary", "shadow"));
    assert!(parrot_app::validate(&m).is_err());
}
#[test]
fn cluster_supervisor_cannot_commit_or_silently_restore_unapplied_mirrors() {
    use parrot_app::orchestrator::{
        AppSupervisor, DesiredStateRecord, DesiredStateStore, MemStateStore, SystemClock,
    };
    let store = Arc::new(MemStateStore::default());
    let m = manifest();
    assert!(AppSupervisor::new(m.clone(), store.clone(), Arc::new(SystemClock)).is_err());
    assert!(
        store.latest().unwrap().is_none(),
        "rejection must precede persistence"
    );
    let mut plain = m.clone();
    plain.mirrors.clear();
    let mut supervisor =
        AppSupervisor::new(plain.clone(), store.clone(), Arc::new(SystemClock)).unwrap();
    assert!(supervisor.submit(m.clone()).is_err());
    assert!(store.latest().unwrap().unwrap().manifest.mirrors.is_empty());
    let stored = DesiredStateRecord {
        mirrors: Default::default(),
        revision: 2,
        manifest: m,
        pending: Default::default(),
    };
    store.commit(Some(1), &stored).unwrap();
    assert!(AppSupervisor::new(plain, store, Arc::new(SystemClock)).is_err());
}
#[tokio::test]
async fn local_encoding_consumes_explicit_primary_timeout_budget() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let actor = host.context.component_ref("primary").unwrap();
    assert!(matches!(
        actor
            .send_with_timeout(Box::new(Work(9000)), Some(Duration::from_millis(1)))
            .await,
        Err(parrot_api::errors::ActorError::Timeout)
    ));
    let options = parrot_api::message::MessageOptions {
        timeout: Some(Duration::from_millis(1)),
        ..Default::default()
    };
    assert!(matches!(
        actor
            .send_with_options(Box::new(Work(9000)), options.clone())
            .await,
        Err(parrot_api::errors::ActorError::Timeout)
    ));
    assert!(matches!(
        actor
            .deliver_with_options(Box::new(Work(9000)), options)
            .await,
        Err(parrot_api::errors::ActorError::Timeout)
    ));
    assert!(host.seen("/user/primary").await.is_empty());
    host.close().await;
}
#[tokio::test]
async fn drained_application_does_not_leave_wire_policy_active() {
    let mut host = Host::new();
    host.assemble(&manifest()).await.unwrap();
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &host.spawner,
    };
    host.context.drain(&deployer).await.unwrap();
    assert!(host.remote.remove_wire_mirror("/user/primary").is_none());
    assert!(host
        .context
        .component_ref("primary")
        .unwrap()
        .deliver(Box::new(Work(40)))
        .await
        .is_err());
    assert!(
        host.engine.get_actor_ref("/user/primary").is_some(),
        "drain alone must not stop the primary"
    );
    host.close().await;
}
#[tokio::test]
async fn several_source_instances_can_fan_in_to_one_shadow() {
    let mut m = manifest();
    m.wiring.clear();
    m.components[0].instances = InstancePolicy::Pool(2);
    let mut host = Host::new();
    host.assemble(&m).await.unwrap();
    for actor in host.context.component_refs("primary").unwrap() {
        actor.send(Box::new(Work(40))).await.unwrap();
    }
    host.expect_count("/user/shadow", 2).await;
    for path in ["/user/primary-0", "/user/primary-1"] {
        assert_eq!(host.context.mirror_stats(path).unwrap().enqueued, 1);
    }
    host.close().await;
}
#[test]
fn shipped_mirror_manifest_passes_real_planning() {
    let m = AppManifest::from_toml(include_str!("../examples/mirror-counter.app.toml")).unwrap();
    plan(&m, &LocalTopology).unwrap();
    assert_eq!(m.mirrors.len(), 1);
    assert_eq!(m.mirrors[0], MirrorSpec::new("primary", "shadow"));
}
