#![cfg(feature = "host")]
use parrot::thread::{
    config::{ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::address::ActorRef;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotAppHook},
    host::{ThreadPropsFactory, ThreadPropsSpawner},
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Events = Arc<Mutex<Vec<u8>>>;
struct Observe;
struct Root {
    child: BoxedActorRef,
    events: Events,
}
struct Hook {
    events: Events,
    bad_ack: bool,
}
impl Actor for Hook {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let phase = message
                .downcast_ref::<ParrotAppHook>()
                .expect("hook type")
                .0;
            self.events.lock().unwrap().push(phase);
            Ok(Box::new(ParrotAppHook(if self.bad_ack { 99 } else { phase })) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
impl Actor for Root {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Observe>() {
                return Ok(Box::new(self.events.clone()) as BoxedMessage);
            }
            if let Some(hook) = message.downcast_ref::<ParrotAppHook>() {
                self.events.lock().unwrap().push(hook.0 + 10); // Wrong destination is observable.
            }
            Ok(message)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(3);
            self.child.stop().await
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        if config
            .as_ref()
            .and_then(|c| c.get("panic_suffix"))
            .and_then(toml::Value::as_str)
            .is_some_and(|suffix| path.ends_with(suffix))
        {
            panic!("factory failure before the next instance");
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let bad_ack = config
            .as_ref()
            .and_then(|c| c.get("bad_ack"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let child = engine
            .spawn_at(
                Hook {
                    events: events.clone(),
                    bad_ack,
                },
                &format!("{path}/lifecycle"),
                None,
                ThreadActorConfig::default(),
            )
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(e.to_string()))?;
        let root = engine
            .spawn_at(
                Root {
                    child: Box::new(child.clone()),
                    events,
                },
                &path,
                None,
                ThreadActorConfig::default(),
            )
            .await;
        match root {
            Ok(root) => Ok(Box::new(root) as BoxedActorRef),
            Err(error) => {
                child.stop().await?;
                Err(parrot_api::errors::ActorError::InternalError(
                    error.to_string(),
                ))
            }
        }
    })
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "test.lifecycle", spawn } }
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
    AppManifest::from_toml("name = 'lifecycle'\nversion = '1.2.3'\n[[components]]\nname = 'worker'\nengine = 'parrot'\nartifact = { Props = { factory = 'test.lifecycle' } }\ninstances = { Pool = 3 }\n[components.hooks]\non_start = 'lifecycle'\non_drain = '/user/worker/lifecycle'\non_stop = 'lifecycle'").unwrap()
}
fn engine() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig::default())
}
#[tokio::test]
async fn real_pool_hooks_use_child_paths_and_acknowledged_drain_runs_once() {
    let engine = engine();
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let manifest = manifest();
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    let mut journals = Vec::new();
    for actor in context.component_refs("worker").unwrap() {
        let reply = actor.send(Box::new(Observe)).await.unwrap();
        let events = reply.downcast_ref::<Events>().unwrap().clone();
        assert_eq!(*events.lock().unwrap(), vec![0]);
        journals.push(events);
    }
    context.drain(&deployer).await.unwrap();
    context.drain(&deployer).await.unwrap();
    for events in &journals {
        assert_eq!(*events.lock().unwrap(), vec![0, 2]);
    }
    context.teardown(&deployer).await.unwrap();
    for events in &journals {
        assert_eq!(*events.lock().unwrap(), vec![0, 2, 1, 3]);
    }
    for index in 0..3 {
        assert!(engine
            .get_actor_ref(&format!("/user/worker-{index}"))
            .is_none());
        assert!(engine
            .get_actor_ref(&format!("/user/worker-{index}/lifecycle"))
            .is_none());
    }
    engine.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn wrong_child_ack_rolls_back_real_actor_and_children() {
    let engine = engine();
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut manifest = manifest();
    manifest.components[0].config = Some(toml::from_str("bad_ack = true").unwrap());
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
    assert!(error.to_string().contains("acknowledgement"));
    assert!(context.started().is_empty());
    for index in 0..3 {
        assert!(engine
            .get_actor_ref(&format!("/user/worker-{index}"))
            .is_none());
        assert!(engine
            .get_actor_ref(&format!("/user/worker-{index}/lifecycle"))
            .is_none());
    }
    engine.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn teardown_does_not_stop_a_new_actor_reusing_the_owned_instances_path() {
    let engine = engine();
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut manifest = manifest();
    manifest.components[0].instances = parrot_app::InstancePolicy::Singleton;
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    context
        .component_ref("worker")
        .unwrap()
        .stop()
        .await
        .unwrap();
    let replacement = spawn(engine.clone(), "/user/worker".into(), None)
        .await
        .unwrap();
    context.teardown(&deployer).await.unwrap();
    assert!(replacement
        .send_with_timeout(Box::new(Observe), Some(Duration::from_secs(2)))
        .await
        .is_ok());
    let reply = replacement.send(Box::new(Observe)).await.unwrap();
    assert!(
        reply
            .downcast_ref::<Events>()
            .unwrap()
            .lock()
            .unwrap()
            .is_empty(),
        "old hooks must not target the replacement or its child"
    );
    replacement.stop().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn panicking_props_factory_reclaims_prior_pool_instances() {
    let engine = engine();
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut manifest = manifest();
    manifest.components[0].config = Some(toml::from_str("panic_suffix = '-1'").unwrap());
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
    assert!(error.to_string().contains("panicked"));
    assert!(context.started().is_empty());
    for index in 0..3 {
        assert!(engine
            .get_actor_ref(&format!("/user/worker-{index}"))
            .is_none());
        assert!(engine
            .get_actor_ref(&format!("/user/worker-{index}/lifecycle"))
            .is_none());
    }
    engine.shutdown_internal().await.unwrap();
}

#[derive(Debug)]
struct WireState(Vec<u8>);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:test.LifecycleState",
        type_id: std::any::TypeId::of::<WireState>(),
        encode: |message: &BoxedMessage| Ok(message.downcast_ref::<WireState>().ok_or("WireState")?.0.clone()),
        decode: |bytes: &[u8]| Ok(Box::new(WireState(bytes.to_vec())) as BoxedMessage),
    }
}
struct ObservedGateway {
    inner: parrot_app::host::ProcessGatewayFactory,
    stopped_phases: Mutex<Vec<Vec<u8>>>,
}
async fn phases(actor: &BoxedActorRef) -> Vec<u8> {
    actor
        .send_with_timeout(Box::new(WireState(vec![])), Some(Duration::from_secs(3)))
        .await
        .unwrap()
        .downcast_ref::<WireState>()
        .unwrap()
        .0
        .clone()
}
#[async_trait::async_trait]
impl GatewayFactory for ObservedGateway {
    async fn configure(&self, cfg: &parrot_config::Resolved) -> Result<(), DeployError> {
        self.inner.configure(cfg).await
    }
    async fn start_gateway(
        &self,
        comp: &ComponentSpec,
        paths: &[String],
        version: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.inner.start_gateway(comp, paths, version).await
    }
    async fn hook_target(
        &self,
        comp: &ComponentSpec,
        path: &str,
    ) -> Result<BoxedActorRef, DeployError> {
        self.inner.hook_target(comp, path).await
    }
    async fn stop_gateway(&self, comp: &ComponentSpec) -> Result<(), DeployError> {
        // This must be the original spec, not a fabricated Singleton/Props spec.
        assert_eq!(comp.instances, parrot_app::InstancePolicy::Pool(2));
        assert_eq!(comp.artifact.kind(), "jvm");
        let node = self.inner.component_node(&comp.name).unwrap();
        for index in 0..2 {
            let actor: BoxedActorRef = Box::new(
                self.inner
                    .remote
                    .remote_ref(&format!("parrot://{node}/user/{}-{index}", comp.name))
                    .unwrap(),
            );
            let result = phases(&actor).await;
            self.stopped_phases.lock().unwrap().push(result);
        }
        self.inner.stop_gateway(comp).await
    }
}
#[tokio::test]
#[ignore = "requires built JVM lifecycle fixture; run make test-app-host"]
async fn real_jvm_child_hooks_round_trip_every_phase_for_every_instance() {
    use parrot_app::host::{HostLookup, ProcessGatewayFactory};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root.join("interop/jvm/target");
    assert!(
        target
            .join("test-classes/parrot/protocol/jvm/RustLifecycleFixture.class")
            .is_file(),
        "run make build-jvm"
    );
    let dependencies = std::fs::read_to_string(target.join("cp.txt")).unwrap();
    let classpath = std::env::join_paths(
        [target.join("classes"), target.join("test-classes")]
            .into_iter()
            .chain(std::env::split_paths(dependencies.trim())),
    )
    .unwrap();
    let engine = engine();
    let remote = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("lifecycle-host", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    remote.start().await.unwrap();
    let gateway = ObservedGateway {
        inner: ProcessGatewayFactory::new(root, remote.clone()).with_jvm_classpath(classpath),
        stopped_phases: Mutex::new(vec![]),
    };
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gateway,
        parrot_spawner: &spawner,
    };
    let mut manifest = manifest();
    manifest.components[0].engine = parrot_app::EngineKind::Akka;
    manifest.components[0].artifact = parrot_app::ArtifactRef::Jvm {
        main_class: "parrot.protocol.jvm.RustLifecycleFixture".into(),
        coords: None,
        uri: None,
    };
    manifest.components[0].instances = parrot_app::InstancePolicy::Pool(2);
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    for actor in context.component_refs("worker").unwrap() {
        assert_eq!(phases(actor).await, vec![0]);
    }
    context.drain(&deployer).await.unwrap();
    context.drain(&deployer).await.unwrap();
    for actor in context.component_refs("worker").unwrap() {
        assert_eq!(phases(actor).await, vec![0, 2]);
    }
    context.teardown(&deployer).await.unwrap();
    assert_eq!(
        *gateway.stopped_phases.lock().unwrap(),
        vec![vec![0, 2, 1]; 2]
    );
    assert_eq!(gateway.inner.procs().child_count(), 0);
    gateway.inner.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn declared_connection_selects_the_exact_owned_pool_child() {
    let engine = engine();
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut manifest = manifest();
    manifest.wiring.push(parrot_app::WireSpec {
        from: "worker:/user/selected-child".into(),
        to: "worker:/user/worker-1/lifecycle".into(),
        qos: "lan".into(),
    });
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    let wire = context.wired_ref("worker:/user/selected-child").unwrap();
    let reply = wire.send(Box::new(ParrotAppHook(0))).await.unwrap();
    assert_eq!(reply.downcast_ref::<ParrotAppHook>().unwrap().0, 0);
    for (index, actor) in context.component_refs("worker").unwrap().iter().enumerate() {
        let reply = actor.send(Box::new(Observe)).await.unwrap();
        let actual = reply
            .downcast_ref::<Events>()
            .unwrap()
            .lock()
            .unwrap()
            .clone();
        assert_eq!(actual, if index == 1 { vec![0, 0] } else { vec![0] });
    }
    context.teardown(&deployer).await.unwrap();
    assert!(wire.send(Box::new(ParrotAppHook(0))).await.is_err());
    assert!(engine.get_actor_ref("/user/worker-1/lifecycle").is_none());
    engine.shutdown_internal().await.unwrap();
}
