#![cfg(feature = "host")]
use parrot::thread::{
    config::{ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    message::{MessageOptions, MessagePriority},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{effective_config, AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::{ThreadPropsFactory, ThreadPropsSpawner},
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
};
use std::{sync::Arc, time::Duration};
struct Configured {
    config: toml::Table,
}
struct Block {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl Actor for Configured {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(block) = message.downcast_ref::<Block>() {
                block.entered.notify_one();
                block.release.notified().await;
            }
            Ok(Box::new(self.config.clone()) as BoxedMessage)
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
        let actor = engine
            .spawn_at(
                Configured {
                    config: config.unwrap_or_default(),
                },
                &path,
                None,
                ThreadActorConfig::default(),
            )
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(e.to_string()))?;
        Ok(Box::new(actor) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "app.configured", spawn } }
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
    AppManifest::from_toml("name = 'configured-app'\nversion = '2.3.4'\n[[components]]\nname = 'configured'\nengine = 'parrot'\nartifact = { Props = { factory = 'app.configured' } }\n[components.config]\nlimit = 7\nlabel = '中文'\n[components.config.nested]\nenabled = true\nitems = [1, 2]\n[config_overlay.thread]\nshared_pool_size = 1\nshared_burst_workers_max = 0\nshared_queue_capacity = 1\ndefault_mailbox_capacity = 2").unwrap()
}
fn engine(manifest: &AppManifest) -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(
        &effective_config(manifest, &Default::default()).unwrap(),
    ))
}
#[tokio::test]
async fn configuration_reaches_real_actor_and_mailbox_on_current_thread_runtime() {
    let manifest = manifest();
    let engine = engine(&manifest);
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
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
    let actor = context.component_ref("configured").unwrap().clone_boxed();
    let response = actor.send(Box::new(())).await.unwrap();
    assert_eq!(
        response.downcast_ref::<toml::Table>(),
        manifest.components[0].config.as_ref()
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    struct Release(Arc<tokio::sync::Notify>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    let _release = Release(release.clone());
    let active = actor.clone_boxed();
    let message = Block {
        entered: entered.clone(),
        release: release.clone(),
    };
    let running = tokio::spawn(async move { active.send(Box::new(message)).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    actor.deliver(Box::new(1_u64)).await.unwrap();
    actor.deliver(Box::new(2_u64)).await.unwrap();
    let full = tokio::time::timeout(Duration::from_millis(40), actor.deliver(Box::new(3_u64)))
        .await
        .is_err();
    release.notify_one();
    running.await.unwrap().unwrap();
    assert!(
        full,
        "manifest mailbox capacity must constrain real admission"
    );
    assert_eq!(engine.scheduler_metrics().unwrap().queue_capacity, 1);
    context.teardown(&deployer).await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(!actor.is_alive().await);
}
#[tokio::test]
async fn incompatible_existing_engine_rejects_overlay_before_spawn() {
    let manifest = manifest();
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let result = context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await;
    assert!(matches!(result, Err(DeployError::Config(_))));
    assert!(engine.get_actor_ref("/user/configured").is_none());
    engine.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn same_path_is_isolated_between_engines_and_stopping_one_preserves_the_other() {
    let a = manifest();
    let mut b = a.clone();
    b.components[0]
        .config
        .as_mut()
        .unwrap()
        .insert("limit".into(), 99.into());
    let left = engine(&a);
    let right = engine(&b);
    let left_spawner = ThreadPropsSpawner::new(left.clone());
    let right_spawner = ThreadPropsSpawner::new(right.clone());
    let dl = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &left_spawner,
    };
    let dr = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &right_spawner,
    };
    let mut cl = AssemblingContext::new();
    let mut cr = AssemblingContext::new();
    cl.assemble(
        &plan(&a, &LocalTopology).unwrap(),
        &dl,
        &a,
        &Default::default(),
    )
    .await
    .unwrap();
    cr.assemble(
        &plan(&b, &LocalTopology).unwrap(),
        &dr,
        &b,
        &Default::default(),
    )
    .await
    .unwrap();
    cl.teardown(&dl).await.unwrap();
    left.shutdown_internal().await.unwrap();
    let result = cr
        .component_ref("configured")
        .unwrap()
        .send(Box::new(()))
        .await
        .unwrap();
    assert_eq!(
        result.downcast_ref::<toml::Table>().unwrap()["limit"].as_integer(),
        Some(99)
    );
    cr.teardown(&dr).await.unwrap();
    right.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn partial_pool_failure_stops_only_instances_created_by_that_attempt() {
    let mut manifest = manifest();
    manifest.components[0].instances = parrot_app::InstancePolicy::Pool(3);
    let engine = engine(&manifest);
    let existing = spawn(engine.clone(), "/user/configured-1".into(), None)
        .await
        .unwrap();
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    assert!(context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default()
        )
        .await
        .is_err());
    assert!(engine.get_actor_ref("/user/configured-0").is_none());
    assert!(engine.get_actor_ref("/user/configured-2").is_none());
    assert!(existing.send(Box::new(())).await.is_ok());
    engine.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn pool_of_one_keeps_indexed_path() {
    let mut manifest = manifest();
    manifest.components[0].instances = parrot_app::InstancePolicy::Pool(1);
    let engine = engine(&manifest);
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
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
    assert_eq!(
        context.component_ref("configured").unwrap().path(),
        "/user/configured-0"
    );
    assert!(engine.get_actor_ref("/user/configured").is_none());
    context.teardown(&deployer).await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn incompatible_remote_config_is_rejected_before_local_spawn() {
    use parrot_app::host::{HostLookup, ProcessGatewayFactory};
    let mut manifest = manifest();
    manifest.config_overlay.as_mut().unwrap().insert(
        "remote".into(),
        toml::from_str::<toml::Table>("[transport]\ndefault_hop_limit = 19")
            .unwrap()
            .into(),
    );
    let engine = engine(&manifest);
    let remote = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("injected", None),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    let gateway = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    assert!(matches!(
        context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default()
            )
            .await,
        Err(DeployError::Config(_))
    ));
    assert!(engine.get_actor_ref("/user/configured").is_none());
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}
fn jvm_classpath() -> std::ffi::OsString {
    let target = repo_root().join("interop/jvm/target");
    let classes = target.join("test-classes/parrot/protocol/jvm/RustGatewayFixture.class");
    assert!(
        classes.is_file(),
        "build JVM fixture with make build-jvm: {}",
        classes.display()
    );
    let dependencies = std::fs::read_to_string(target.join("cp.txt"))
        .expect("build JVM classpath with make build-jvm");
    std::env::join_paths(
        [target.join("classes"), target.join("test-classes")]
            .into_iter()
            .chain(std::env::split_paths(dependencies.trim())),
    )
    .unwrap()
}
#[derive(Debug)]
struct WireConfig(Vec<u8>);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot.test.ConfiguredEcho#v1",
        type_id: std::any::TypeId::of::<WireConfig>(),
        encode: |message: &BoxedMessage| Ok(message.downcast_ref::<WireConfig>().ok_or("WireConfig")?.0.clone()),
        decode: |bytes: &[u8]| Ok(Box::new(WireConfig(bytes.to_vec())) as BoxedMessage),
    }
}
fn jvm_component(name: &str, count: usize) -> ComponentSpec {
    let mut component = manifest().components.remove(0);
    component.name = name.into();
    component.engine = parrot_app::EngineKind::Akka;
    component.artifact = parrot_app::ArtifactRef::Jvm {
        main_class: "parrot.protocol.jvm.RustGatewayFixture".into(),
        coords: None,
        uri: None,
    };
    component.instances = parrot_app::InstancePolicy::Pool(count);
    component
}
async fn verify_wire_config(actor: &BoxedActorRef, config: &toml::Table) {
    let result = actor
        .send_with_timeout(
            Box::new(WireConfig(vec![0, 255])),
            Some(Duration::from_secs(3)),
        )
        .await
        .unwrap();
    let bytes = &result.downcast_ref::<WireConfig>().unwrap().0;
    assert!(bytes.ends_with(&[0, 255]));
    let actual: toml::Table =
        toml::from_str(std::str::from_utf8(&bytes[..bytes.len() - 2]).unwrap()).unwrap();
    assert_eq!(&actual, config);
}
#[tokio::test]
#[ignore = "requires built JVM gateway and fixture; run make test-app-host"]
async fn real_jvm_components_preserve_config_version_instances_and_stop_isolation() {
    use parrot_app::host::{ApplicationLookup, GatewayRoutes, ProcessGatewayFactory};
    use parrot_remote::LocalLookup;
    let routes = Arc::new(GatewayRoutes::default());
    let engine = ThreadActorSystem::shared(Default::default());
    let remote = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("app-host-test", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(ApplicationLookup {
            ts: engine.clone(),
            routes: routes.clone(),
        }),
    )
    .unwrap();
    remote.start().await.unwrap();
    let gateway =
        ProcessGatewayFactory::new_with_routes(repo_root(), remote.clone(), routes.clone())
            .with_jvm_classpath(jvm_classpath())
            .with_startup_timeout(Duration::from_secs(30));
    let lookup = ApplicationLookup {
        ts: engine.clone(),
        routes,
    };
    let first = jvm_component("alpha", 2);
    let second = jvm_component("beta", 1);
    let a = gateway
        .start_gateway(
            &first,
            &["/user/alpha-0".into(), "/user/alpha-1".into()],
            "2.3.4",
        )
        .await
        .unwrap();
    let b = gateway
        .start_gateway(&second, &["/user/beta-0".into()], "4.5.6")
        .await
        .unwrap();
    assert_ne!(
        gateway.component_node("alpha"),
        gateway.component_node("beta")
    );
    for actor in &a {
        verify_wire_config(actor, first.config.as_ref().unwrap()).await;
    }
    verify_wire_config(&b[0], second.config.as_ref().unwrap()).await;
    let status = remote
        .component_status(&gateway.component_node("alpha").unwrap(), "/user/alpha")
        .await
        .unwrap();
    assert_eq!(status.len(), 2);
    assert!(status
        .iter()
        .all(|s| s.version == "2.3.4" && s.state == "running"));
    let mut broken = jvm_component("broken", 1);
    broken.artifact = parrot_app::ArtifactRef::Jvm {
        main_class: "missing.Component".into(),
        coords: None,
        uri: None,
    };
    assert!(gateway
        .start_gateway(&broken, &["/user/broken-0".into()], "2.3.4")
        .await
        .is_err());
    assert_eq!(gateway.procs().child_count(), 2);
    assert!(lookup.forwarding_target("/user/broken-0").is_none());
    assert!(lookup.forwarding_target("/user/alpha-0").is_some());
    assert!(lookup.forwarding_target("/user/alpha").is_none());
    tokio::time::timeout(Duration::from_secs(3), async {
        while remote.links_snapshot().await.len() > 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    gateway.stop_gateway(&first).await.unwrap();
    assert_eq!(gateway.procs().child_count(), 1);
    assert!(lookup.forwarding_target("/user/alpha-0").is_none());
    assert!(lookup.forwarding_target("/user/beta-0").is_some());
    assert!(a[0]
        .send_with_timeout(Box::new(WireConfig(vec![])), Some(Duration::from_secs(1)))
        .await
        .is_err());
    verify_wire_config(&b[0], second.config.as_ref().unwrap()).await;
    gateway.procs().kill_all().await.unwrap();
    assert_eq!(gateway.procs().child_count(), 0);
    assert!(lookup.forwarding_target("/user/beta-0").is_none());
    assert!(gateway
        .start_gateway(&second, &["/user/beta-0".into()], "4.5.6")
        .await
        .is_err());
    gateway.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn repeated_assembly_does_not_roll_back_an_already_running_application() {
    let manifest = manifest();
    let engine = engine(&manifest);
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let plan = plan(&manifest, &LocalTopology).unwrap();
    context
        .assemble(&plan, &deployer, &manifest, &Default::default())
        .await
        .unwrap();
    assert!(context
        .assemble(&plan, &deployer, &manifest, &Default::default())
        .await
        .is_err());
    let result = context
        .component_ref("configured")
        .unwrap()
        .send(Box::new(()))
        .await
        .unwrap();
    assert_eq!(
        result.downcast_ref::<toml::Table>(),
        manifest.components[0].config.as_ref()
    );
    context.teardown(&deployer).await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[test]
fn component_payload_distinguishes_missing_and_empty_config_and_preserves_release_version() {
    let mut component = manifest().components.remove(0);
    component.config = None;
    assert_eq!(component.deployment("3.7.9-rc.1").unwrap().config, None);
    component.config = Some(toml::Table::new());
    let payload = component.deployment("3.7.9-rc.1").unwrap();
    assert_eq!(payload.config, Some(vec![]));
    assert_eq!(payload.version, "3.7.9-rc.1");
    component.instances = parrot_app::InstancePolicy::Pool(0);
    assert!(component.deployment("3.7.9").is_err());
}

#[test]
fn nested_manifest_typos_are_rejected_without_restricting_business_config() {
    for section in ["component", "artifact", "hooks", "placement", "upgrade"] {
        let mut value = toml::Value::try_from(manifest()).unwrap();
        let component = &mut value["components"].as_array_mut().unwrap()[0];
        let table = match section {
            "component" => component.as_table_mut().unwrap(),
            "artifact" => component["artifact"]["Props"].as_table_mut().unwrap(),
            "hooks" => component["hooks"].as_table_mut().unwrap(),
            "placement" => component["placement"].as_table_mut().unwrap(),
            "upgrade" => component["upgrade"]["HotSwap"].as_table_mut().unwrap(),
            _ => unreachable!(),
        };
        table.insert("typo_key".into(), true.into());
        assert!(
            AppManifest::from_toml(&toml::to_string(&value).unwrap()).is_err(),
            "unknown field in {section} was dropped"
        );
    }
    assert_eq!(
        manifest().components[0].config.as_ref().unwrap()["label"].as_str(),
        Some("中文")
    );
}

struct ProcessFixture(std::path::PathBuf);
impl ProcessFixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-app-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"));
        for key in [
            "PARROT_CONFIG",
            "PARROT_APP_READY_FILE",
            "PARROT_REPO_ROOT",
            "PARROT_JVM_CLASSPATH",
        ] {
            command.env_remove(key);
        }
        command
            .current_dir(&self.0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        command
    }
    fn manifest(&self, manifest: &AppManifest) -> std::path::PathBuf {
        let path = self.0.join("application.toml");
        std::fs::write(&path, toml::to_string(manifest).unwrap()).unwrap();
        path
    }
}
impl Drop for ProcessFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn ready(child: &mut tokio::process::Child) -> String {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let stdout = child.stdout.take().unwrap();
    let read = async {
        let mut reader = tokio::io::BufReader::new(stdout.take(65537));
        let mut length = 0;
        loop {
            let mut line = String::new();
            let bytes = reader
                .read_line(&mut line)
                .await
                .map_err(|e| e.to_string())?;
            length += bytes;
            if bytes == 0 || length > 65536 {
                return Err("application exited or exceeded startup output limit".to_string());
            }
            if let Some(endpoint) = line.strip_prefix("PARROT_APP_READY=") {
                return Ok(endpoint.trim().to_owned());
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(180), read).await {
        Ok(Ok(endpoint)) => endpoint,
        error => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("startup: {error:?}");
        }
    }
}
#[tokio::test]
async fn cli_rejects_invalid_gateway_policy_before_recording_or_readiness() {
    for flags in [
        vec!["--gateway-process-limit", "0"],
        vec!["--gateway-process-limit", "18446744073709551615"],
        vec!["--gateway-process-limit", "-1"],
        vec!["--gateway-process-limit", "no"],
        vec!["--gateway-process-limit"],
        vec![
            "--gateway-process-limit",
            "1",
            "--gateway-process-limit",
            "2",
        ],
    ] {
        let fixture = ProcessFixture::new();
        let manifest = fixture.manifest(&manifest());
        let recording = fixture.0.join("record.jsonl");
        let readiness = fixture.0.join("ready");
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            fixture
                .command()
                .args(["run", "--manifest"])
                .arg(manifest)
                .arg("--record")
                .arg(&recording)
                .env("PARROT_APP_READY_FILE", &readiness)
                .args(&flags)
                .stderr(std::process::Stdio::piped())
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && output.stdout.is_empty(),
            "{flags:?}: {diagnostics}"
        );
        assert!(
            diagnostics.contains("gateway") && !diagnostics.contains("not registered"),
            "{diagnostics}"
        );
        assert!(!recording.exists() && !readiness.exists());
    }
}

#[tokio::test]
#[ignore = "requires built JVM gateway and fixture; run make test-app-host"]
async fn actual_cli_gateway_limit_rejects_second_component_and_rolls_back() {
    let fixture = ProcessFixture::new();
    let mut app = manifest();
    app.components = vec![jvm_component("first", 1), jvm_component("second", 1)];
    let manifest_path = fixture.manifest(&app);
    let readiness = fixture.0.join("ready");
    let output = tokio::time::timeout(
        Duration::from_secs(90),
        fixture
            .command()
            .args(["run", "--manifest"])
            .arg(manifest_path)
            .args(["--gateway-process-limit", "1", "--bind", "127.0.0.1:0"])
            .env("PARROT_APP_READY_FILE", &readiness)
            .env("PARROT_REPO_ROOT", repo_root())
            .env("PARROT_JVM_CLASSPATH", jvm_classpath())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{diagnostics}");
    assert!(
        diagnostics.contains("gateway process group budget exhausted (in use 1, limit 1)"),
        "{diagnostics}"
    );
    assert!(!readiness.exists() && output.stdout.is_empty());
}

#[tokio::test]
async fn cli_rejects_invalid_effective_config_before_attempting_component_spawn() {
    let fixture = ProcessFixture::new();
    let mut app = manifest();
    app.config_overlay = None;
    let manifest = fixture.manifest(&app);
    let config = fixture.0.join("config.toml");
    std::fs::write(&config, "[thread]\nshared_pool_size = 0").unwrap();
    let mut child = fixture
        .command()
        .args(["run", "--manifest"])
        .arg(&manifest)
        .env("PARROT_CONFIG", &config)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    use tokio::io::AsyncReadExt;
    let diagnostics = tokio::spawn(async move {
        let mut result = String::new();
        stderr
            .take(65536)
            .read_to_string(&mut result)
            .await
            .unwrap();
        result
    });
    let status = match tokio::time::timeout(Duration::from_secs(180), child.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("invalid config startup deadline");
        }
    };
    let diagnostics = diagnostics.await.unwrap();
    assert!(!status.success());
    assert!(
        diagnostics.contains("thread.shared_pool_size"),
        "{diagnostics}"
    );
    assert!(
        !diagnostics.contains("not registered"),
        "configuration must fail before factory lookup"
    );
}
#[tokio::test]
#[ignore = "requires built JVM gateway and fixture; run make test-app-host"]
async fn actual_cli_consumes_file_overlay_and_serves_then_shuts_down() {
    use parrot_app::host::HostLookup;
    let fixture = ProcessFixture::new();
    let mut app = manifest();
    app.components = vec![jvm_component("cli-component", 1)];
    app.config_overlay.as_mut().unwrap().insert(
        "remote".into(),
        toml::from_str::<toml::Table>(
            "[node]\nnode_id = 'overlay-host'\n[transport]\ndefault_hop_limit = 19",
        )
        .unwrap()
        .into(),
    );
    let manifest_path = fixture.manifest(&app);
    let ready_file = fixture.0.join("ready");
    let config = fixture.0.join("config.toml");
    std::fs::write(&config, "[remote.node]\nnode_id = 'file-host'\nbind = '127.0.0.1:0'\n[thread]\nshared_pool_size = 2\ndefault_mailbox_capacity = 3").unwrap();
    let recording_path = fixture.0.join("wire.jsonl");
    let mut child = fixture
        .command()
        .args(["run", "--manifest"])
        .arg(&manifest_path)
        .env("PARROT_CONFIG", &config)
        .env("PARROT_APP_READY_FILE", &ready_file)
        .env("PARROT_REPO_ROOT", repo_root())
        .env("PARROT_JVM_CLASSPATH", jvm_classpath())
        .arg("--record")
        .arg(&recording_path)
        .args(["--gateway-process-limit", "1"])
        .spawn()
        .unwrap();
    let endpoint = ready(&mut child).await;
    assert_eq!(std::fs::read_to_string(&ready_file).unwrap(), endpoint);
    let engine = ThreadActorSystem::shared(Default::default());
    let client = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("cli-probe", None),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    client.start().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect(&parrot_remote::NodeAddr::tcp(
            "overlay-host",
            endpoint.parse().unwrap(),
        )),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(client
        .links_snapshot()
        .await
        .iter()
        .any(|(id, _, _)| id == "overlay-host"));
    // This codec exists only in this test executable, not in the stock host.
    // The host must forward opaque bytes to the deployed gateway and correlate
    // its reply without requiring every application's business codec.
    let actor: BoxedActorRef = Box::new(
        client
            .remote_ref("parrot://overlay-host/user/cli-component-0")
            .unwrap(),
    );
    let response = actor
        .send_with_timeout(
            Box::new(WireConfig(vec![0, 255])),
            Some(Duration::from_secs(3)),
        )
        .await;
    let business_ok = match response {
        Ok(reply) => {
            let bytes = &reply.downcast_ref::<WireConfig>().unwrap().0;
            let expected = app.components[0]
                .deployment(&app.version)
                .unwrap()
                .config
                .unwrap();
            bytes == &[expected, vec![0, 255]].concat()
        }
        Err(ref error) => {
            eprintln!("business invocation failed: {error}");
            false
        }
    };
    #[cfg(unix)]
    {
        assert!(tokio::process::Command::new("kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success());
        match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
            Ok(status) => assert!(status.unwrap().success()),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                panic!("application graceful shutdown deadline");
            }
        }
    }
    #[cfg(not(unix))]
    {
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
    #[cfg(unix)]
    {
        let log = parrot_app::debug::ReplayLog::load_capture(&recording_path).unwrap();
        assert!(log
            .records
            .iter()
            .any(|r| r.frame_type == parrot_remote::frame::frame_type::ASK
                && r.dir == "rx"
                && r.path.ends_with("/user/cli-component-0")));
        assert!(log.records.iter().all(|r| r
            .wire
            .as_ref()
            .is_some_and(|w| w.local_node == "overlay-host")));
    }
    client.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(
        business_ok,
        "CLI must expose deployed gateway components at application paths"
    );
}

#[tokio::test]
async fn cli_recording_never_overwrites_existing_evidence_or_starts_components() {
    let fixture = ProcessFixture::new();
    let app = manifest();
    let manifest_path = fixture.manifest(&app);
    let path = fixture.0.join("existing.jsonl");
    std::fs::write(&path, "preserve this evidence").unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        fixture
            .command()
            .args(["run", "--manifest"])
            .arg(manifest_path)
            .args(["--bind", "127.0.0.1:0", "--record"])
            .arg(&path)
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "preserve this evidence"
    );
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(
        !diagnostics.contains("not registered"),
        "must reject the existing file before starting components: {diagnostics}"
    );
}

#[tokio::test]
async fn missing_declared_wire_target_fails_assembly_and_rolls_back() {
    let mut manifest = manifest();
    manifest.wiring.push(parrot_app::WireSpec {
        from: "configured:/user/output".into(),
        to: "configured:/user/configured/missing".into(),
        qos: "lan".into(),
    });
    let engine = engine(&manifest);
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let result = context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await;
    let retained = context.started().to_vec();
    context.teardown(&deployer).await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(
        result.is_err(),
        "missing declared wiring target was silently ignored"
    );
    assert!(
        retained.is_empty(),
        "failed wiring did not roll back {retained:?}"
    );
}

fn wired_manifest() -> AppManifest {
    AppManifest::from_toml(
        r#"
name = "wired-app"
version = "1.0.0"
[[components]]
name = "source"
engine = "parrot"
artifact = { Props = { factory = "app.configured" } }
[components.config]
label = "source"
[[components]]
name = "sink"
engine = "parrot"
artifact = { Props = { factory = "app.configured" } }
[components.config]
label = "target"
[[wiring]]
from = "source:/user/output"
to = "sink:/user/sink"
qos = "lan"
"#,
    )
    .unwrap()
}

#[tokio::test]
async fn declared_wire_sends_to_target_and_does_not_grant_stop_ownership() {
    let manifest = wired_manifest();
    let engine = engine(&manifest);
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
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
    assert!(context.wired_ref("sink:/user/output").is_none());
    let wire = context.wired_ref("source:/user/output").unwrap();
    // A rejected second assemble must not revoke the still-running generation.
    assert!(context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default()
        )
        .await
        .is_err());
    let reply = wire
        .send_with_options(
            Box::new(()),
            MessageOptions {
                timeout: Some(Duration::from_secs(1)),
                priority: MessagePriority::HIGH,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        reply.downcast_ref::<toml::Table>().unwrap()["label"].as_str(),
        Some("target")
    );
    assert!(wire.stop().await.is_err());
    assert!(context.component_ref("sink").unwrap().is_alive().await);
    // Tell actually reaches an actor, rather than just returning an enqueue stub.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    wire.deliver_with_options(
        Box::new(Block {
            entered: entered.clone(),
            release: release.clone(),
        }),
        MessageOptions {
            priority: MessagePriority::HIGH,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        wire.send_with_options(
            Box::new(()),
            MessageOptions {
                timeout: Some(Duration::from_millis(5)),
                ..Default::default()
            }
        )
        .await,
        Err(parrot_api::errors::ActorError::Timeout
            | parrot_api::errors::ActorError::TimeoutDetail(_))
    ));
    release.notify_one();
    context.teardown(&deployer).await.unwrap();
    assert!(!wire.is_alive().await);
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn drain_closes_unpolled_sends_and_reassembly_never_revives_old_wires() {
    let manifest = wired_manifest();
    let engine = engine(&manifest);
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let plan = plan(&manifest, &LocalTopology).unwrap();
    let mut context = AssemblingContext::new();
    context
        .assemble(&plan, &deployer, &manifest, &Default::default())
        .await
        .unwrap();
    let wire = context.wired_ref("source:/user/output").unwrap();
    let clone = wire.clone_boxed();
    let unpolled = wire.send(Box::new(()));
    let unpolled_option_ask = wire.send_with_options(
        Box::new(()),
        MessageOptions {
            priority: MessagePriority::HIGH,
            ..Default::default()
        },
    );
    let unpolled_option_tell = wire.deliver_with_options(
        Box::new(()),
        MessageOptions {
            priority: MessagePriority::HIGH,
            ..Default::default()
        },
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let active = clone.clone_boxed();
    let message = Block {
        entered: entered.clone(),
        release: release.clone(),
    };
    let in_flight = tokio::spawn(async move { active.send(Box::new(message)).await });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    context.drain(&deployer).await.unwrap();
    assert!(context.wired_ref("source:/user/output").is_none());
    assert!(matches!(
        unpolled.await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(matches!(
        unpolled_option_ask.await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(matches!(
        unpolled_option_tell.await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(matches!(
        clone.deliver(Box::new(())).await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert!(matches!(
        clone
            .send_with_timeout(Box::new(()), Some(Duration::from_secs(1)))
            .await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    release.notify_one();
    in_flight.await.unwrap().unwrap();
    context.teardown(&deployer).await.unwrap();
    context
        .assemble(&plan, &deployer, &manifest, &Default::default())
        .await
        .unwrap();
    assert!(context
        .wired_ref("source:/user/output")
        .unwrap()
        .send(Box::new(()))
        .await
        .is_ok());
    assert!(matches!(
        clone.send(Box::new(())).await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    let current = context.wired_ref("source:/user/output").unwrap();
    drop(context);
    assert!(matches!(
        current.send(Box::new(())).await,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn unsupported_wan_policy_fails_before_any_actor_is_spawned() {
    let mut manifest = wired_manifest();
    manifest.wiring[0].qos = "wan".into();
    let engine = engine(&manifest);
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let result = context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("WAN QoS"));
    assert!(engine.get_actor_ref("/user/source").is_none());
    assert!(context.started().is_empty());
    engine.shutdown_internal().await.unwrap();
}

struct FaultyWireResolver {
    inner: ThreadPropsSpawner,
    panic: bool,
    entered: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl parrot_app::assemble::ParrotSpawner for FaultyWireResolver {
    fn configure(&self, config: &parrot_config::Resolved) -> Result<(), DeployError> {
        self.inner.configure(config)
    }
    async fn spawn_props(
        &self,
        factory: &str,
        paths: &[String],
        config: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.inner.spawn_props(factory, paths, config).await
    }
    async fn stop_paths(&self, paths: &[String]) -> Result<(), DeployError> {
        self.inner.stop_paths(paths).await
    }
    async fn hook_target(&self, _: &str) -> Result<BoxedActorRef, DeployError> {
        assert!(!self.panic, "injected connection resolver panic");
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn connection_lookup_panic_and_timeout_roll_back_all_components() {
    for panic in [false, true] {
        let mut manifest = wired_manifest();
        manifest.wiring[0].to = "sink:/user/sink/child".into();
        let engine = engine(&manifest);
        let spawner = FaultyWireResolver {
            inner: ThreadPropsSpawner::new(engine.clone()),
            panic,
            entered: tokio::sync::Notify::new(),
        };
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &spawner,
        };
        let mut context = AssemblingContext::new();
        context.set_hooks_timeout(Duration::from_millis(25));
        let error = context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default(),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(if panic { "panicked" } else { "timed out" }),
            "{error}"
        );
        assert!(context.started().is_empty());
        assert!(context.wired_ref("source:/user/output").is_none());
        assert!(engine.get_actor_ref("/user/source").is_none());
        assert!(engine.get_actor_ref("/user/sink").is_none());
        engine.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn cancelled_wire_resolution_never_publishes_partial_connections() {
    let mut manifest = wired_manifest();
    manifest.wiring.push(parrot_app::WireSpec {
        from: "source:/user/blocked".into(),
        to: "sink:/user/sink/child".into(),
        qos: "lan".into(),
    });
    let engine = engine(&manifest);
    let spawner = FaultyWireResolver {
        inner: ThreadPropsSpawner::new(engine.clone()),
        panic: false,
        entered: tokio::sync::Notify::new(),
    };
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let plan = plan(&manifest, &LocalTopology).unwrap();
    let config = Default::default();
    let mut assembling = Box::pin(context.assemble(&plan, &deployer, &manifest, &config));
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            biased;
            result = &mut assembling => panic!("blocked assembly completed: {result:?}"),
            _ = spawner.entered.notified() => {},
        }
    })
    .await
    .expect("assembly never reached the blocked wire resolver");
    drop(assembling);
    assert_eq!(context.started().len(), 2);
    assert!(context.wired_ref("source:/user/output").is_none());
    assert!(context.wired_ref("source:/user/blocked").is_none());
    context.teardown(&deployer).await.unwrap();
    assert!(context.started().is_empty());
    assert!(engine.get_actor_ref("/user/source").is_none());
    assert!(engine.get_actor_ref("/user/sink").is_none());
    engine.shutdown_internal().await.unwrap();
}

#[cfg(unix)]
async fn signal_during_actual_jvm_initialization(signal: &str, partial: bool) {
    let fixture = ProcessFixture::new();
    let mut app = manifest();
    let mut blocked = jvm_component("blocked", 1);
    blocked.artifact = parrot_app::ArtifactRef::Jvm {
        main_class: "parrot.protocol.jvm.RustBlockingStartupFixture".into(),
        coords: None,
        uri: None,
    };
    app.components = if partial {
        blocked.deps.push("prior".into());
        vec![jvm_component("prior", 1), blocked]
    } else {
        vec![blocked]
    };
    let manifest_path = fixture.manifest(&app);
    let marker = fixture.0.join("blocked.json");
    let readiness = fixture.0.join("ready");
    let recording = fixture.0.join("wire.jsonl");
    let mut child = fixture
        .command()
        .args(["run", "--manifest"])
        .arg(manifest_path)
        .args(["--bind", "127.0.0.1:0", "--record"])
        .arg(&recording)
        .env("PARROT_REPO_ROOT", repo_root())
        .env("PARROT_JVM_CLASSPATH", jvm_classpath())
        .env("PARROT_APP_READY_FILE", &readiness)
        .env("PARROT_TEST_BLOCKING_STARTUP_MARKER", &marker)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !marker.is_file() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "host exited before real JVM initialization"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let identity: Vec<u64> = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    let (pid, port) = (identity[0], identity[1] as u16);
    assert!(
        !readiness.exists(),
        "blocked component cannot establish readiness"
    );
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
        "real component listener missing"
    );
    if let Some(directory) = std::env::var_os("PARROT_TEST_OBSERVATION_DIR") {
        let receipt = std::path::PathBuf::from(directory).join(format!("{pid}-{port}.json"));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !receipt.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("independent OS observer did not see the component listener");
    }
    assert!(std::process::Command::new("kill")
        .args(["-s", signal, &child.id().unwrap().to_string()])
        .status()
        .unwrap()
        .success());
    let started = std::time::Instant::now();
    let stopped = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let timely = stopped.is_ok();
    if !timely {
        // Retire our own failed baseline before asserting; never leave its
        // guardian/JVM running just because the cancellation contract failed.
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
    // Observe immediately after host wait, before reading inherited output
    // pipes: guardian stderr EOF could otherwise conceal an early host exit.
    let retired_at_exit = std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
    let output = child.wait_with_output().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8(state.stdout).unwrap();
            if (state.trim().is_empty() || state.trim().starts_with('Z'))
                && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("JVM or component listener survived host exit");
    assert!(timely, "startup signal waited for deployment timeout");
    assert!(
        retired_at_exit,
        "host exited before owned gateway listener retirement"
    );
    assert!(
        output.status.success(),
        "{signal}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !readiness.exists()
            && !String::from_utf8_lossy(&output.stdout).contains("PARROT_APP_READY=")
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("[app] recorded"),
        "recording was not finalized"
    );
    let capture = parrot_app::debug::ReplayLog::load_capture(&recording).unwrap();
    assert!(
        !capture.records.is_empty(),
        "must record actual gateway protocol traffic before cancellation"
    );
    let peers: std::collections::BTreeSet<_> = capture
        .records
        .iter()
        .filter_map(|record| record.wire.as_ref()?.peer_node.as_deref())
        .filter(|node| node.starts_with("akka-"))
        .collect();
    assert_eq!(
        peers.len(),
        if partial { 2 } else { 1 },
        "missing actual gateway traffic"
    );
    println!(
        "PARROT_CLI_SHUTDOWN_CASE={}",
        serde_json::json!({
            "signal": signal, "partial": partial, "seconds": started.elapsed().as_secs_f64(),
            "jvm_pid": pid, "port": port, "gateway_peers": peers.len(),
            "recorded_frames": capture.records.len(), "exit": output.status.code(),
            "readiness_published": false, "listener_retired": true, "retired_at_host_exit": retired_at_exit
        })
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires built JVM gateway and fixture; run make test-app-host"]
async fn actual_cli_sigterm_cancels_first_deployment() {
    signal_during_actual_jvm_initialization("TERM", false).await;
}
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires built JVM gateway and fixture; run make test-app-host"]
async fn actual_cli_sigint_cancels_first_deployment() {
    signal_during_actual_jvm_initialization("INT", false).await;
}
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires built JVM gateway and fixture; run make test-app-host"]
async fn actual_cli_sigterm_cancels_partial_deployment() {
    signal_during_actual_jvm_initialization("TERM", true).await;
}
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires built JVM gateway and fixture; run make test-app-host"]
async fn actual_cli_sigint_cancels_partial_deployment() {
    signal_during_actual_jvm_initialization("INT", true).await;
}
