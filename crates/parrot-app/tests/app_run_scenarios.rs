//! A4 场景测试（MG12 前置——DEV_09 §5.2）：`app run` 本地四引擎真实组装。
//!
//! 形态：parrot 组件（PropsFactory + ThreadActorSystem）+ erlang/ray/jvm
//! 真实子进程网关（registry 模式）——与生产 Wire 序列化路径一致。
//!
//! 需要本机工具链（erl / java+mvn 已构建 jar / python3+ray）——
//! 用户裁定：一律真实子进程。缺工具链的用例显式失败（不 SKIP）。
#![cfg(feature = "host")]

use std::sync::Arc;
use std::time::Duration;

use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};

#[path = "support/crawler_contract.rs"]
mod crawler_contract;

use parrot_app::assemble::{AssemblingContext, LocalDeployer};
use parrot_app::host::{HostLookup, ProcessGatewayFactory, ThreadPropsSpawner};
use parrot_app::manifest::{
    AppManifest, ArtifactRef, ComponentHooks, ComponentSpec, EngineKind, InstancePolicy,
    PlacementConstraint, UpgradePolicy, WireSpec,
};
use parrot_app::planner::{plan, LocalTopology};

// ---------------- 测试用 parrot 组件（inventory PropsFactory） ----------------

type HookJournal = Arc<std::sync::Mutex<Vec<u8>>>;
struct ObserveHooks;
#[derive(Default)]
pub struct AppEchoActor {
    hooks: HookJournal,
}

impl Actor for AppEchoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<ObserveHooks>() {
                return Ok(Box::new(self.hooks.clone()) as BoxedMessage);
            }
            if let Some(hook) = msg.downcast_ref::<parrot_app::ParrotAppHook>() {
                self.hooks.lock().unwrap().push(hook.0);
            }
            Ok(msg)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn spawn_app_echo(
    ts: Arc<ThreadActorSystem>,
    path: String,
    _config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let r = ts
            .spawn_at(
                AppEchoActor::default(),
                &path,
                None,
                ThreadActorConfig::default(),
            )
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(format!("spawn: {e}")))?;
        Ok(Box::new(r) as BoxedActorRef)
    })
}

parrot_api::message::inventory::submit! {
    parrot_app::host::ThreadPropsFactory {
        name: "app.tests.echo",
        spawn: spawn_app_echo,
    }
}

/// Each test owns its engine; factories receive that engine explicitly.
fn thread_system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(Default::default())
}

fn repo_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR = crates/parrot-app → 仓库根是两级向上
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.parent().unwrap().parent().unwrap().to_path_buf()
}

fn comp(name: &str, engine: EngineKind, artifact: ArtifactRef) -> ComponentSpec {
    ComponentSpec {
        name: name.into(),
        engine,
        artifact,
        alt_artifact: None,
        instances: InstancePolicy::Singleton,
        placement: PlacementConstraint::default(),
        upgrade: UpgradePolicy::default(),
        deps: vec![],
        config: None,
        hooks: ComponentHooks::default(),
    }
}

/// 独立端口分配器（测试并行不撞端口）。
static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(19901);
fn alloc_port() -> u16 {
    NEXT_PORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// 网关场景串行锁（多测试同时拉 ray/erl/jvm 子进程会争资源——DEV_09 §6 条 1 同族坑）。
/// 测试体跨 await 持守——tokio Mutex（clippy await_holding_lock 合规）。
static GW_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn gw_lock() -> tokio::sync::MutexGuard<'static, ()> {
    GW_LOCK.lock().await
}

async fn make_remote(ts: Arc<ThreadActorSystem>, port: u16) -> Arc<RemoteActorSystem> {
    let sa: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let r = RemoteActorSystem::new(
        RemoteConfig::tcp("app-host-test", Some(sa)),
        Arc::new(HostLookup { ts }),
    )
    .unwrap();
    r.start().await.unwrap();
    r
}

// ---------------- 场景 1：纯 parrot 组件本地组装 ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_parrot_only_assemble_and_smoke() {
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let m = AppManifest {
        name: "parrot-only".into(),
        version: "1.0.0".into(),
        components: vec![comp(
            "echo",
            EngineKind::Parrot,
            ArtifactRef::Props {
                factory: "app.tests.echo".into(),
            },
        )],
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    };
    let plan = plan(&m, &LocalTopology).unwrap();
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(
        &plan,
        &deployer,
        &m,
        &parrot_config::ParrotConfig::default(),
    )
    .await
    .expect("assemble");

    // 冒烟：ask echo 组件
    let r = ctx
        .component_ref("echo")
        .unwrap()
        .send(Box::new("hello".to_string()))
        .await
        .expect("ask");
    assert_eq!(*r.downcast_ref::<String>().unwrap(), "hello");

    ctx.teardown(&deployer).await.expect("teardown");
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

// ---------------- 场景 2：erlang 真实子进程网关 ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_erlang_gateway_real_subprocess() {
    let _gw_guard = gw_lock().await;
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let m = AppManifest {
        name: "erl-app".into(),
        version: "1.0.0".into(),
        components: vec![
            comp(
                "frontier",
                EngineKind::Erlang,
                // R3+R4：制品 = app 构建产物（apps/crawler-lab/erlang/
                // frontier.beam）——URI 指定目录，网关复制制品并启动独立 BEAM 宿主。
                ArtifactRef::Beam {
                    app: "frontier".into(),
                    uri: Some(format!(
                        "file://{}",
                        repo_root().join("apps/crawler-lab/erlang").display()
                    )),
                },
            ),
            comp(
                "driver",
                EngineKind::Parrot,
                ArtifactRef::Props {
                    factory: "app.tests.echo".into(),
                },
            ),
        ],
        wiring: vec![WireSpec {
            from: "driver:/user/service".into(),
            to: "frontier:/user/frontier".into(),
            qos: "lan".into(),
        }],
        mirrors: Vec::new(),
        config_overlay: None,
    };
    let plan = plan(&m, &LocalTopology).unwrap();
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(
        &plan,
        &deployer,
        &m,
        &parrot_config::ParrotConfig::default(),
    )
    .await
    .expect("assemble (erl escript 子进程注册)");

    let gateway_node = gw.component_node("frontier").unwrap();
    let measured_before = remote.measured_metrics(&gateway_node).await;
    let service = ctx
        .wired_ref("driver:/user/service")
        .expect("declared service connection");
    crawler_contract::frontier(&service).await;
    use parrot_api::address::ActorRef;
    let target = remote
        .remote_ref(&format!("parrot://{gateway_node}/user/frontier"))
        .unwrap();
    let url = b"https://completion.parrot.dev/";
    let entry = [
        77u64.to_le_bytes().to_vec(),
        (url.len() as u32).to_le_bytes().to_vec(),
        url.to_vec(),
        2u16.to_le_bytes().to_vec(),
    ]
    .concat();
    let completion = target
        .deliver_with_ack(
            Box::new(crawler_contract::FrontierPush(
                [1u32.to_le_bytes().to_vec(), entry.clone()].concat(),
            )),
            Duration::from_secs(5),
        )
        .await;
    // Next transfers one layer at a time. The earlier depth-zero URL must
    // remain ahead of the depth-two effect confirmed by the receipt.
    // The receipt itself carries no FrontierAck business payload.
    let shallow_effect = target
        .send_with_timeout(
            Box::new(crawler_contract::FrontierNext(
                100u32.to_le_bytes().to_vec(),
            )),
            Some(Duration::from_secs(5)),
        )
        .await;
    let completed_effect = target
        .send_with_timeout(
            Box::new(crawler_contract::FrontierNext(
                100u32.to_le_bytes().to_vec(),
            )),
            Some(Duration::from_secs(5)),
        )
        .await;
    let rejected_completion = target
        .deliver_with_ack(
            Box::new(crawler_contract::IndexStats(vec![])),
            Duration::from_secs(5),
        )
        .await;
    let measured_after = remote.measured_metrics(&gateway_node).await;

    ctx.teardown(&deployer).await.unwrap();
    gw.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();

    // Assert after teardown so a capability failure still retires owned processes.
    completion.expect("real Rust-to-Erlang handler completion receipt");
    let shallow = shallow_effect.expect("query preserves the earlier shallow layer");
    let shallow = &shallow
        .downcast_ref::<crawler_contract::FrontierBatch>()
        .unwrap()
        .0;
    let original_url = b"https://parrot.dev/";
    let expected_shallow = [
        1u32.to_le_bytes().to_vec(),
        7u64.to_le_bytes().to_vec(),
        (original_url.len() as u32).to_le_bytes().to_vec(),
        original_url.to_vec(),
        0u16.to_le_bytes().to_vec(),
    ]
    .concat();
    assert_eq!(*shallow, expected_shallow);
    let effect = completed_effect.expect("query after successful completion");
    let effect = &effect
        .downcast_ref::<crawler_contract::FrontierBatch>()
        .unwrap()
        .0;
    assert_eq!(*effect, [1u32.to_le_bytes().to_vec(), entry].concat());
    assert!(matches!(rejected_completion,
        Err(parrot_api::errors::ActorError::MessageHandlingError(ref detail))
            if detail.contains("unknown type key")));
    let before = measured_before.expect("real Erlang gateway measured metrics before business");
    let after = measured_after.expect("real Erlang gateway measured metrics after business");
    use parrot_remote::metrics::field;
    assert!(before.has(field::TRANSPORT | field::COMPONENTS | field::COMPONENT_STATES));
    assert_eq!(before.value(field::COMPONENTS), Some(1));
    assert_eq!(before.value(field::CONNECTIONS), Some(1));
    assert_eq!(before.component_states.len(), 1);
    assert_eq!(before.process_kind, "erlang_processes");
    assert!(before.value(field::PROCESSES).unwrap() > 0);
    assert!(!before.epoch.is_empty());
    assert_eq!(before.epoch, after.epoch);
    assert!(after.value(field::ASKS_RX).unwrap() > before.value(field::ASKS_RX).unwrap());
    assert!(after.value(field::BYTES_RX).unwrap() > before.value(field::BYTES_RX).unwrap());
    assert!(after.value(field::BYTES_TX).unwrap() > before.value(field::BYTES_TX).unwrap());
}

// ---------------- 场景 3：ray 真实子进程网关 ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_ray_gateway_real_subprocess() {
    let _gw_guard = gw_lock().await;
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let m = AppManifest {
        name: "ray-app".into(),
        version: "1.0.0".into(),
        components: vec![
            comp(
                "indexer",
                EngineKind::Ray,
                // R1：组件制品 = app 源码（apps/crawler-lab/python），网关
                // deploy 时以 uri 为 working_dir 载入 parrot_entry 起组件。
                ArtifactRef::PyModule {
                    module: "index_builder".into(),
                    runtime_env: None,
                    uri: Some(format!(
                        "file://{}",
                        repo_root().join("apps/crawler-lab/python").display()
                    )),
                },
            ),
            comp(
                "driver",
                EngineKind::Parrot,
                ArtifactRef::Props {
                    factory: "app.tests.echo".into(),
                },
            ),
        ],
        wiring: vec![WireSpec {
            from: "driver:/user/service".into(),
            to: "indexer:/user/indexer".into(),
            qos: "lan".into(),
        }],
        mirrors: Vec::new(),
        config_overlay: None,
    };
    let plan = plan(&m, &LocalTopology).unwrap();
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(
        &plan,
        &deployer,
        &m,
        &parrot_config::ParrotConfig::default(),
    )
    .await
    .expect("assemble (python ray 子进程注册)");

    let gateway_node = gw.component_node("indexer").unwrap();
    let measured_before = remote.measured_metrics(&gateway_node).await;
    let service = ctx
        .wired_ref("driver:/user/service")
        .expect("declared service connection");
    crawler_contract::ray_index(&service).await;
    let measured_after = remote.measured_metrics(&gateway_node).await;

    ctx.teardown(&deployer).await.unwrap();
    gw.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();

    // Preserve process cleanup even when an older gateway lacks the capability.
    let before = measured_before.expect("real Ray gateway measured metrics before business");
    let after = measured_after.expect("real Ray gateway measured metrics after business");
    use parrot_remote::metrics::field;
    assert!(before.has(field::TRANSPORT | field::COMPONENTS | field::COMPONENT_STATES));
    assert_eq!(before.value(field::COMPONENTS), Some(1));
    assert_eq!(before.value(field::CONNECTIONS), Some(1));
    assert_eq!(before.component_states.len(), 1);
    assert_eq!(before.process_kind, "python_threads");
    assert!(before.value(field::PROCESSES).unwrap() > 0);
    assert!(!before.epoch.is_empty());
    assert_eq!(before.epoch, after.epoch);
    assert!(after.value(field::ASKS_RX).unwrap() > before.value(field::ASKS_RX).unwrap());
    assert!(after.value(field::BYTES_RX).unwrap() > before.value(field::BYTES_RX).unwrap());
    assert!(after.value(field::BYTES_TX).unwrap() > before.value(field::BYTES_TX).unwrap());
}

// ---------------- 场景 4：jvm(akka) 真实子进程网关 ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_ray_cancelled_deploy_and_stop_keep_retirement_ownership() {
    use parrot_app::assemble::GatewayFactory;
    let _gw_guard = gw_lock().await;
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let gateway = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let artifact = tempfile::tempdir().unwrap();
    let entered = artifact.path().join("entered");
    std::fs::write(artifact.path().join("blocked.py"), format!(
        "from pathlib import Path\nimport time\ndef parrot_entry(ctx):\n Path({}).write_text('entered')\n time.sleep(120)\n",
        serde_json::to_string(&entered).unwrap()
    )).unwrap();
    let mut component = comp(
        "cancelled-ray",
        EngineKind::Ray,
        ArtifactRef::PyModule {
            module: "blocked".into(),
            runtime_env: None,
            uri: Some(format!("file://{}", artifact.path().display())),
        },
    );
    let factory = gateway.clone();
    let spec = component.clone();
    let deploy = tokio::spawn(async move {
        factory
            .start_gateway(&spec, &["/user/cancelled-ray".into()], "1.0.0")
            .await
    });
    let reached_entry = tokio::time::timeout(Duration::from_secs(60), async {
        while !entered.exists() {
            assert!(
                !deploy.is_finished(),
                "deploy ended before entering the actual Ray component"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    deploy.abort();
    let result = deploy.await;
    // The unpublished gateway must remain awaitable through the component's
    // retirement receipt, even though component_node was never populated.
    gateway.stop_gateway(&component).await.unwrap();
    reached_entry.unwrap();
    assert!(result.unwrap_err().is_cancelled());
    component.artifact = ArtifactRef::PyModule {
        module: "index_builder".into(),
        runtime_env: None,
        uri: Some(format!(
            "file://{}",
            repo_root().join("apps/crawler-lab/python").display()
        )),
    };
    let actors = gateway
        .start_gateway(&component, &["/user/cancelled-ray".into()], "2.0.0")
        .await
        .unwrap();
    crawler_contract::ray_index(&actors[0]).await;
    let mut stop = Box::pin(gateway.stop_gateway(&component));
    assert!(futures::poll!(&mut stop).is_pending());
    assert!(gateway.component_node(&component.name).is_none());
    drop(stop);
    gateway.stop_gateway(&component).await.unwrap();
    gateway.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_jvm_gateway_real_subprocess() {
    let _gw_guard = gw_lock().await;
    let jar = repo_root().join("interop/jvm/target/parrot-protocol-jvm-0.1.0.jar");
    assert!(
        jar.exists(),
        "JVM jar 未构建：先 make build-jvm（mvn package）"
    );
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let m = AppManifest {
        name: "jvm-app".into(),
        version: "1.0.0".into(),
        components: vec![
            comp(
                "search",
                EngineKind::Akka,
                ArtifactRef::Jvm {
                    main_class: "crawler.search.SearchComponent".into(),
                    coords: None,
                    uri: Some(format!(
                        "file://{}",
                        repo_root()
                            .join("apps/crawler-lab/jvm/target/crawler-lab-jvm-1.0.0.jar")
                            .display()
                    )),
                },
            ),
            comp(
                "driver",
                EngineKind::Parrot,
                ArtifactRef::Props {
                    factory: "app.tests.echo".into(),
                },
            ),
        ],
        wiring: vec![WireSpec {
            from: "driver:/user/service".into(),
            to: "search:/user/search".into(),
            qos: "lan".into(),
        }],
        mirrors: Vec::new(),
        config_overlay: None,
    };
    let plan = plan(&m, &LocalTopology).unwrap();
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(
        &plan,
        &deployer,
        &m,
        &parrot_config::ParrotConfig::default(),
    )
    .await
    .expect("assemble (java 子进程注册)");

    let gateway_node = gw.component_node("search").unwrap();
    let measured_before = remote.measured_metrics(&gateway_node).await;
    let service = ctx
        .wired_ref("driver:/user/service")
        .expect("declared service connection");
    crawler_contract::jvm_search(&service).await;
    let measured_after = remote.measured_metrics(&gateway_node).await;

    ctx.teardown(&deployer).await.unwrap();
    gw.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();

    // Preserve process cleanup even when an older gateway lacks the capability.
    let before = measured_before.expect("real JVM gateway measured metrics before business");
    let after = measured_after.expect("real JVM gateway measured metrics after business");
    use parrot_remote::metrics::field;
    assert!(before.has(field::TRANSPORT | field::COMPONENTS | field::COMPONENT_STATES));
    assert_eq!(before.value(field::COMPONENTS), Some(1));
    assert_eq!(before.value(field::CONNECTIONS), Some(1));
    assert_eq!(before.component_states.len(), 1);
    assert_eq!(before.process_kind, "jvm_threads");
    assert!(before.value(field::PROCESSES).unwrap() > 0);
    assert!(!before.epoch.is_empty());
    assert_eq!(before.epoch, after.epoch);
    assert!(after.value(field::ASKS_RX).unwrap() > before.value(field::ASKS_RX).unwrap());
    assert!(after.value(field::BYTES_RX).unwrap() > before.value(field::BYTES_RX).unwrap());
    assert!(after.value(field::BYTES_TX).unwrap() > before.value(field::BYTES_TX).unwrap());
}

// ---------------- 场景 5：装配失败回滚（网关进程被杀清理） ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_failure_kills_gateway_procs() {
    let _gw_guard = gw_lock().await;
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let mut m = AppManifest {
        name: "fail-app".into(),
        version: "1.0.0".into(),
        components: vec![
            comp(
                "erl1",
                EngineKind::Erlang,
                ArtifactRef::Beam {
                    app: "frontier".into(),
                    uri: Some(format!(
                        "file://{}",
                        repo_root().join("apps/crawler-lab/erlang").display()
                    )),
                },
            ),
            comp(
                "bad",
                EngineKind::Parrot,
                ArtifactRef::Props {
                    factory: "no.such.factory".into(),
                },
            ),
        ],
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    };
    m.components[1].deps = vec!["erl1".into()];
    let plan = plan(&m, &LocalTopology).unwrap();
    assert_eq!(
        plan.order
            .iter()
            .map(|c| c.spec.name.as_str())
            .collect::<Vec<_>>(),
        vec!["erl1", "bad"]
    );
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    let err = ctx
        .assemble(
            &plan,
            &deployer,
            &m,
            &parrot_config::ParrotConfig::default(),
        )
        .await
        .expect_err("bad factory must fail");
    assert!(
        matches!(err, parrot_app::DeployError::Artifact(ref s) if s.contains("no.such.factory"))
    );
    assert!(ctx.started().is_empty(), "回滚清空");
    // 网关进程清理（防泄漏）
    assert_eq!(
        gw.procs().child_count(),
        0,
        "assembly rollback must reap the previously started gateway"
    );
    gw.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

// ---------------- 场景 6：sharded parrot 组件 + wiring 校验 ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_sharded_with_wiring() {
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let mut sh = comp(
        "pool",
        EngineKind::Parrot,
        ArtifactRef::Props {
            factory: "app.tests.echo".into(),
        },
    );
    sh.instances = InstancePolicy::Pool(3);
    let m = AppManifest {
        name: "sharded".into(),
        version: "1.0.0".into(),
        components: vec![sh],
        wiring: vec![WireSpec {
            from: "pool:/user/pool-0".into(),
            to: "pool:/user/pool-1".into(),
            qos: "lan".into(),
        }],
        mirrors: Vec::new(),
        config_overlay: None,
    };
    let plan = plan(&m, &LocalTopology).unwrap();
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(
        &plan,
        &deployer,
        &m,
        &parrot_config::ParrotConfig::default(),
    )
    .await
    .expect("assemble");
    assert_eq!(ctx.component_refs("pool").unwrap().len(), 3);
    assert_eq!(ctx.component_paths("pool").unwrap()[1], "/user/pool-1");

    let wired = ctx
        .wired_ref("pool:/user/pool-0")
        .expect("declared pool connection");
    let reply = wired.send(Box::new("wired".to_string())).await.unwrap();
    assert_eq!(reply.downcast_ref::<String>().unwrap(), "wired");

    // 三实例各自可达
    for i in 0..3 {
        let r = ctx.component_refs("pool").unwrap()[i]
            .send(Box::new(format!("m{i}")))
            .await
            .unwrap();
        assert_eq!(*r.downcast_ref::<String>().unwrap(), format!("m{i}"));
    }
    ctx.teardown(&deployer).await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

// ---------------- 场景 7：hooks 全生命周期（on_start/on_stop） ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_hooks_lifecycle() {
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let mut c = comp(
        "hooked",
        EngineKind::Parrot,
        ArtifactRef::Props {
            factory: "app.tests.echo".into(),
        },
    );
    c.hooks = ComponentHooks {
        on_start: Some("/user/hooked".into()),
        on_stop: Some("/user/hooked".into()),
        on_drain: Some("/user/hooked".into()),
    };
    let m = AppManifest {
        name: "hooks".into(),
        version: "1.0.0".into(),
        components: vec![c],
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    };
    let plan = plan(&m, &LocalTopology).unwrap();
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    // AppEchoActor 回显任意消息——hook 回执是 ParrotAppHook 会被原样回显，
    // fire_hook 校验 phase 匹配即过（echo 语义满足回执契约）
    ctx.assemble(
        &plan,
        &deployer,
        &m,
        &parrot_config::ParrotConfig::default(),
    )
    .await
    .expect("assemble with hooks");
    let reply = ctx
        .component_ref("hooked")
        .unwrap()
        .send(Box::new(ObserveHooks))
        .await
        .unwrap();
    let events = reply.downcast_ref::<HookJournal>().unwrap();
    assert_eq!(*events.lock().unwrap(), vec![0]);
    ctx.teardown(&deployer).await.unwrap();
    assert_eq!(*events.lock().unwrap(), vec![0, 2, 1]);
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

// ---------------- 场景 8：overlay 注入远程配置（Y3 端到端） ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_config_overlay_flows() {
    let m = AppManifest {
        name: "ov".into(),
        version: "1.0.0".into(),
        components: vec![comp(
            "ov-echo",
            EngineKind::Parrot,
            ArtifactRef::Props {
                factory: "app.tests.echo".into(),
            },
        )],
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: Some(
            [(
                "remote".to_string(),
                toml::Value::Table(
                    [(
                        "transport".to_string(),
                        toml::Value::Table(
                            [("heartbeat_interval_ms".to_string(), toml::Value::from(250))]
                                .into_iter()
                                .collect(),
                        ),
                    )]
                    .into_iter()
                    .collect(),
                ),
            )]
            .into_iter()
            .collect(),
        ),
    };
    let base = parrot_config::ParrotConfig::builder()
        .remote_node_id("overlay-scenario")
        .remote_bind("127.0.0.1:0");
    let resolved = parrot_app::assemble::effective_config(&m, &base).unwrap();
    let ts = ThreadActorSystem::shared(
        parrot::thread::config::ThreadActorSystemConfig::from_resolved(&resolved),
    );
    let remote = RemoteActorSystem::new(
        RemoteConfig::from_resolved(&resolved).unwrap(),
        Arc::new(HostLookup { ts: ts.clone() }),
    )
    .unwrap();
    remote.start().await.unwrap();
    let plan = plan(&m, &LocalTopology).unwrap();
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    // overlay 合法（键已知）——装配成功
    ctx.assemble(&plan, &deployer, &m, &base)
        .await
        .expect("overlay applied");
    let reply = ctx
        .component_ref("ov-echo")
        .unwrap()
        .send(Box::new("configured".to_string()))
        .await
        .unwrap();
    assert_eq!(reply.downcast_ref::<String>().unwrap(), "configured");
    ctx.teardown(&deployer).await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
}
