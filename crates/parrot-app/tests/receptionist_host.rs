//! Application assembly must inject the real registry and preserve configured ACLs.
#![cfg(feature = "host")]

use parrot::thread::{
    ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    context::ActorContext,
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, GatewayFactory, LocalDeployer},
    host::{HostLookup, ProcessGatewayFactory, ThreadPropsFactory, ThreadPropsSpawner},
    manifest::AppManifest,
    planner::{plan, LocalTopology},
};
use parrot_config::ParrotConfig;
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{io::Write, sync::Arc, time::Duration};

fn key() -> ReceptionistKey {
    ReceptionistKey::new("edge/host-service").unwrap()
}

struct Service;
#[derive(serde::Serialize, serde::Deserialize)]
struct WireQuery(u64);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:receptionist-host::WireQuery#v1",
        type_id: std::any::TypeId::of::<WireQuery>(),
        encode: |message: &BoxedMessage| parrot_api::message::serde_remote_serialize(
            message.downcast_ref::<WireQuery>().ok_or("WireQuery")?),
        decode: |bytes: &[u8]| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<WireQuery>(bytes)?) as BoxedMessage),
    }
}
struct NoActors;
#[async_trait::async_trait]
impl LocalLookup for NoActors {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
impl Actor for Service {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        ctx.receptionist_register(key())
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Ok(match message.downcast::<WireQuery>() {
                Ok(query) => Box::new(WireQuery(query.0 + 1)) as BoxedMessage,
                Err(message) => message,
            })
        })
    }
}

fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    _: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        Ok(Box::new(
            engine
                .spawn_at(Service, &path, None, ThreadActorConfig::default())
                .await
                .map_err(|error| {
                    parrot_api::errors::ActorError::InternalError(error.to_string())
                })?,
        ) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! {
    ThreadPropsFactory { name: "tests.receptionist-host", spawn }
}

fn config(grants: &str) -> ParrotConfig {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(
        file,
        "[remote.node]\nnode_id = 'host'\nbind = '127.0.0.1:0'\n{grants}"
    )
    .unwrap();
    ParrotConfig::builder().load_file(file.path()).unwrap()
}

struct Consumer;
impl Actor for Consumer {
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
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<(String, u64)>() {
                let (path, value) = *message.downcast::<(String, u64)>().unwrap();
                let reply = match ctx.resolve_actor(&path) {
                    Some(target) => Some(
                        ctx.ask(target, Box::new(WireQuery(value)))
                            .await?
                            .downcast::<WireQuery>()
                            .unwrap()
                            .0,
                    ),
                    None => None,
                };
                return Ok(Box::new(reply) as BoxedMessage);
            }
            let value = *message.downcast::<u64>().unwrap();
            let mut stream = ctx.receptionist_subscribe(key()).await?;
            let event = tokio::time::timeout(Duration::from_secs(5), stream.recv())
                .await
                .expect("component service discovery deadline")
                .expect("component service discovery closed");
            let ReceptionistEvent::Registered { remote_path, .. } = event else {
                panic!("expected discovered service registration")
            };
            let target = ctx
                .resolve_actor(&remote_path)
                .expect("component context must resolve its discovered remote path");
            let response = ctx.ask(target, Box::new(WireQuery(value))).await?;
            Ok(
                Box::new((remote_path, response.downcast::<WireQuery>().unwrap().0))
                    as BoxedMessage,
            )
        })
    }
}

async fn call_path(consumer: &BoxedActorRef, path: &str, value: u64) -> Option<u64> {
    *tokio::time::timeout(
        Duration::from_secs(5),
        consumer.send(Box::new((path.to_owned(), value))),
    )
    .await
    .unwrap()
    .unwrap()
    .downcast::<Option<u64>>()
    .unwrap()
}

#[tokio::test]
async fn context_and_system_resolution_preserve_local_identity_and_reject_invalid_routes() {
    use parrot_api::{address::ActorPath, engine::AsyncEngineRuntime, system::ActorSystem};
    let resolved = config("[remote.receptionist.roles]\nhost = ['edge/host-service']")
        .build()
        .unwrap();
    let engine = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&resolved));
    let remote = RemoteActorSystem::new(
        RemoteConfig::from_resolved(&resolved).unwrap(),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    let consumer: BoxedActorRef = Box::new(
        engine
            .spawn_at(
                Consumer,
                "/user/consumer",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(call_path(&consumer, "/user/missing", 5).await, None);
    assert_eq!(
        call_path(&consumer, "parrot://missing/user/service", 5).await,
        None
    );
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let service: BoxedActorRef = Box::new(
        engine
            .spawn_at(Service, "/user/service", None, ThreadActorConfig::default())
            .await
            .unwrap(),
    );
    for path in ["/user/service", "parrot://host/user/service"] {
        assert_eq!(
            call_path(&consumer, path, 39).await,
            Some(40),
            "context must resolve local and self-qualified paths to the exact local actor"
        );
        for target in [
            ActorSystem::get_actor(engine.as_ref(), &ActorPath::placeholder(path)).await,
            AsyncEngineRuntime::get_actor(engine.as_ref(), path).await,
        ] {
            let target =
                target.expect("both public engine lookup interfaces must resolve the path");
            assert_eq!(target.path(), service.path());
            assert_eq!(
                target
                    .send(Box::new(WireQuery(8)))
                    .await
                    .unwrap()
                    .downcast::<WireQuery>()
                    .unwrap()
                    .0,
                9
            );
        }
    }
    for path in [
        "parrot://host/user/../service",
        "parrot://host/user//service",
        "parrot://host/user/service?x",
        "parrot:///user/service",
        "parrot://host",
        "parrot://missing/user/service",
        "parrot://host/user/missing",
    ] {
        assert_eq!(
            call_path(&consumer, path, 5).await,
            None,
            "invalid or unknown route: {path}"
        );
    }
    assert!(
        engine.get_actor_ref("parrot://host/user/service").is_none(),
        "inbound registry lookup must remain local-only"
    );
    remote.shutdown().await.unwrap();
    assert_eq!(
        call_path(&consumer, "parrot://host/user/service", 5).await,
        None,
        "closed remote gateway must not resolve self-qualified paths"
    );
    assert_eq!(call_path(&consumer, "/user/service", 5).await, Some(6));
    engine.shutdown_internal().await.unwrap();
    assert!(engine.resolve_actor("/user/service").is_none());
    assert!(engine.resolve_actor("parrot://host/user/service").is_none());
    drop(spawner);
    let weak = Arc::downgrade(&remote);
    drop(remote);
    assert!(weak.upgrade().is_none());
}

struct CallbackGateway {
    lookup: Box<dyn Fn() + Send + Sync>,
    retire: Box<dyn Fn() + Send + Sync>,
}
impl parrot::system::RemoteGateway for CallbackGateway {
    fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        (self.lookup)();
        None
    }
}
impl Drop for CallbackGateway {
    fn drop(&mut self) {
        (self.retire)();
    }
}
fn null_gateway() -> Arc<dyn parrot::system::RemoteGateway> {
    Arc::new(CallbackGateway {
        lookup: Box::new(|| {}),
        retire: Box::new(|| {}),
    })
}

#[test]
fn thread_remote_gateway_callbacks_can_replace_the_gateway_without_deadlock() {
    const NAME: &str = "thread_remote_gateway_callbacks_can_replace_the_gateway_without_deadlock";
    if std::env::var("PARROT_RESOLUTION_CALLBACK_CHILD")
        .ok()
        .as_deref()
        != Some(NAME)
    {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("PARROT_RESOLUTION_CALLBACK_CHILD", NAME)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "gateway callback child failed: {status}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("gateway lookup or destructor deadlocked while replacing the gateway");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use std::sync::atomic::{AtomicUsize, Ordering};
            let engine = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
            let calls = Arc::new(AtomicUsize::new(0));
            let weak = Arc::downgrade(&engine);
            let count = calls.clone();
            engine.set_remote_gateway(Arc::new(CallbackGateway {
                lookup: Box::new(move || {
                    weak.upgrade().unwrap().set_remote_gateway(null_gateway());
                    count.fetch_add(1, Ordering::SeqCst);
                }),
                retire: Box::new(|| {}),
            }));
            assert!(engine.resolve_actor("parrot://node/user/service").is_none());
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "lookup callback must execute"
            );
            let weak = Arc::downgrade(&engine);
            let count = calls.clone();
            engine.set_remote_gateway(Arc::new(CallbackGateway {
                lookup: Box::new(|| {}),
                retire: Box::new(move || {
                    weak.upgrade().unwrap().set_remote_gateway(null_gateway());
                    count.fetch_add(1, Ordering::SeqCst);
                }),
            }));
            engine.set_remote_gateway(null_gateway());
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "retired gateway must drop outside engine locks"
            );
            engine.shutdown_internal().await.unwrap();
        });
}
fn spawn_consumer(
    engine: Arc<ThreadActorSystem>,
    path: String,
    _: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        Ok(Box::new(
            engine
                .spawn_at(Consumer, &path, None, ThreadActorConfig::default())
                .await
                .map_err(|error| {
                    parrot_api::errors::ActorError::InternalError(error.to_string())
                })?,
        ) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! {
    ThreadPropsFactory { name: "tests.receptionist-consumer", spawn: spawn_consumer }
}

#[tokio::test]
async fn assembled_consumer_discovers_resolves_and_calls_another_hosts_component() {
    let grants = "[remote.security]\nmode = 'insecure-development'\n[remote.receptionist.roles]\nhost = ['edge/host-service']\nclient = ['edge/host-service']";
    let host_base = config(grants);
    let host_resolved = host_base.clone().build().unwrap();
    let host_engine =
        ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&host_resolved));
    let host = RemoteActorSystem::new(
        RemoteConfig::from_resolved(&host_resolved).unwrap(),
        Arc::new(HostLookup {
            ts: host_engine.clone(),
        }),
    )
    .unwrap();
    host.start().await.unwrap();

    let mut client_file = tempfile::NamedTempFile::new().unwrap();
    write!(client_file, "[remote.node]\nnode_id = 'client'\nbind = '127.0.0.1:0'\nseeds = ['tcp://host@{}']\n{grants}", host.local_addr().unwrap()).unwrap();
    let client_base = ParrotConfig::builder()
        .load_file(client_file.path())
        .unwrap();
    let client_resolved = client_base.clone().build().unwrap();
    let client_engine =
        ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&client_resolved));
    let client = RemoteActorSystem::new(
        RemoteConfig::from_resolved(&client_resolved).unwrap(),
        Arc::new(HostLookup {
            ts: client_engine.clone(),
        }),
    )
    .unwrap();
    client.start().await.unwrap();

    let host_spawner = ThreadPropsSpawner::with_remote(host_engine.clone(), host.clone());
    let client_spawner = ThreadPropsSpawner::with_remote(client_engine.clone(), client.clone());
    let host_gateway = ProcessGatewayFactory::new(std::path::PathBuf::new(), host.clone());
    let client_gateway = ProcessGatewayFactory::new(std::path::PathBuf::new(), client.clone());
    let host_deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &host_gateway,
        parrot_spawner: &host_spawner,
    };
    let client_deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &client_gateway,
        parrot_spawner: &client_spawner,
    };
    let manifest: AppManifest = toml::from_str(
        "name = 'service-host'\nversion = '1.0.0'\n[[components]]\nname = 'query'\nengine = 'parrot'\nartifact = { Props = { factory = 'tests.receptionist-host' } }\n",
    ).unwrap();
    let consumer_manifest: AppManifest = toml::from_str(
        "name = 'service-client'\nversion = '1.0.0'\n[[components]]\nname = 'consumer'\nengine = 'parrot'\nartifact = { Props = { factory = 'tests.receptionist-consumer' } }\n",
    ).unwrap();
    let mut host_assembly = AssemblingContext::new();
    let mut client_assembly = AssemblingContext::new();
    host_assembly
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &host_deployer,
            &manifest,
            &host_base,
        )
        .await
        .unwrap();
    client_assembly
        .assemble(
            &plan(&consumer_manifest, &LocalTopology).unwrap(),
            &client_deployer,
            &consumer_manifest,
            &client_base,
        )
        .await
        .unwrap();
    let service = host_assembly.component_ref("query").unwrap().clone_boxed();
    let consumer = client_assembly
        .component_ref("consumer")
        .unwrap()
        .clone_boxed();
    for value in [17_u64, 91] {
        let result = tokio::time::timeout(Duration::from_secs(10), consumer.send(Box::new(value)))
            .await
            .unwrap()
            .unwrap();
        let (path, reply) = *result.downcast::<(String, u64)>().unwrap();
        assert_eq!(path, format!("parrot://host{}", service.path()));
        assert_eq!(
            reply,
            value + 1,
            "component business reply must cross the remote route"
        );
    }
    for invalid in [
        "parrot://host/user/../query",
        "parrot://host/user//query",
        "parrot://host/user/query?x",
        "parrot://host/",
    ] {
        assert!(
            client_engine.resolve_actor(invalid).is_none(),
            "remote gateway must reject malformed known-node paths: {invalid}"
        );
    }
    client_assembly.teardown(&client_deployer).await.unwrap();
    host_assembly.teardown(&host_deployer).await.unwrap();
    assert!(!consumer.is_alive().await && !service.is_alive().await);
    client.shutdown().await.unwrap();
    assert!(
        client_engine
            .resolve_actor(&format!("parrot://host{}", service.path()))
            .is_none(),
        "closed remote gateway must reject known-node paths"
    );
    host.shutdown().await.unwrap();
    host_engine.shutdown_internal().await.unwrap();
    client_engine.shutdown_internal().await.unwrap();
    let host_weak = Arc::downgrade(&host);
    let client_weak = Arc::downgrade(&client);
    drop(host_spawner);
    drop(client_spawner);
    drop(host_gateway);
    drop(client_gateway);
    drop(host);
    drop(client);
    assert!(
        host_weak.upgrade().is_none() && client_weak.upgrade().is_none(),
        "injected message gateways must not form a remote/engine ownership cycle"
    );
}

#[tokio::test]
async fn assembled_component_uses_host_service_gateway_and_retires_its_publication() {
    let base = config("[remote.security]\nmode = 'insecure-development'\n[remote.receptionist.roles]\nhost = ['edge/host-service']\nclient = ['edge/host-service']");
    let resolved = base.clone().build().unwrap();
    let engine = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&resolved));
    let remote = RemoteActorSystem::new(
        RemoteConfig::from_resolved(&resolved).unwrap(),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    remote.start().await.unwrap();
    let mut client_config = RemoteConfig::from_resolved(&resolved).unwrap();
    client_config.node_id = "client".into();
    client_config.seeds = vec![NodeAddr {
        node_id: "host".into(),
        identity_is_explicit: true,
        scheme: "tcp".into(),
        addr: remote.local_addr().unwrap(),
    }];
    let client = RemoteActorSystem::new(client_config, Arc::new(NoActors)).unwrap();
    let mut events = client
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    client.start().await.unwrap();
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let gateway = ProcessGatewayFactory::new(std::path::PathBuf::new(), remote.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gateway,
        parrot_spawner: &spawner,
    };
    let manifest: AppManifest = toml::from_str(
        "name = 'service-host'\nversion = '1.0.0'\n[[components]]\nname = 'query'\nengine = 'parrot'\nartifact = { Props = { factory = 'tests.receptionist-host' } }\n",
    ).unwrap();
    let mut assembly = AssemblingContext::new();
    assembly
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &base,
        )
        .await
        .unwrap();
    let actor = assembly.component_ref("query").unwrap().clone_boxed();
    let response = actor
        .send(Box::new(73_u64))
        .await
        .expect("host must inject its service gateway before component init");
    assert_eq!(*response.downcast::<u64>().unwrap(), 73);
    let path = format!("parrot://host{}", actor.path());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap(),
        Some(ReceptionistEvent::Registered {
            key: key(),
            remote_path: path.clone()
        })
    );
    let discovered = client
        .gateway()
        .lookup(&path)
        .expect("discovered application actor path");
    let response = discovered.send(Box::new(WireQuery(73))).await.unwrap();
    assert_eq!(
        response.downcast::<WireQuery>().unwrap().0,
        74,
        "discovered path must execute the application actor over TCP"
    );
    assembly.teardown(&deployer).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap(),
        Some(ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: path
        })
    );
    assert!(!actor.is_alive().await);
    drop(spawner);
    client.shutdown().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    let weak = Arc::downgrade(&remote);
    drop(gateway);
    drop(remote);
    assert!(
        weak.upgrade().is_none(),
        "injected service gateway must not retain its remote owner"
    );
}

#[tokio::test]
async fn assembly_rejects_service_grants_that_differ_from_the_running_remote_host() {
    let actual = config("[remote.receptionist.roles]\nhost = ['edge']")
        .build()
        .unwrap();
    let engine = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&actual));
    let remote = RemoteActorSystem::new(
        RemoteConfig::from_resolved(&actual).unwrap(),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    let gateway = ProcessGatewayFactory::new(std::path::PathBuf::new(), remote.clone());
    for grants in [
        "[remote.receptionist.roles]\nhost = ['edge/host-service']",
        "[remote.receptionist]\ndefault_deny = false\n[remote.receptionist.roles]\nhost = ['edge']",
        "[remote.receptionist]\nroles = {}",
    ] {
        let resolved = config(grants).build().unwrap();
        assert!(
            gateway.configure(&resolved).await.is_err(),
            "assembly must reject an ACL that differs from the running host"
        );
    }
    gateway.configure(&actual).await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}
