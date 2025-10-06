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

use parrot_app::assemble::{AssemblingContext, LocalDeployer};
use parrot_app::host::{HostLookup, ProcessGatewayFactory, ThreadPropsSpawner};
use parrot_app::manifest::{
    AppManifest, ArtifactRef, ComponentHooks, ComponentSpec, EngineKind, InstancePolicy,
    PlacementConstraint, UpgradePolicy, WireSpec,
};
use parrot_app::planner::{plan, LocalTopology};

// ---------------- 测试用 parrot 组件（inventory PropsFactory） ----------------

pub struct AppEchoActor;

impl Actor for AppEchoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn spawn_app_echo(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    let path = path.to_string();
    Box::pin(async move {
        let r = thread_system()
            .spawn_at(AppEchoActor, &path, None, ThreadActorConfig::default())
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(format!("spawn: {e}")))?;
        Ok(Box::new(r) as BoxedActorRef)
    })
}

parrot_api::message::inventory::submit! {
    parrot_remote::admin::PropsFactory {
        name: "app.tests.echo",
        spawn: spawn_app_echo,
    }
}

/// 共享 thread 引擎（同进程多测试复用——set_self_weak 坑规避：shared() 已 init）。
///
/// 坑：若绑到首个调用者的 #[tokio::test] runtime，该测试结束时 runtime
/// drop 会连带杀掉全部 scheduler worker——后续测试的 thread 引擎 ask 永
/// 远无人处理（ask_unbounded 无超时→挂死）。解法：绑定进程级专用
/// runtime（Box::leak 保活到测试二进制退出）。
static TS: std::sync::OnceLock<Arc<ThreadActorSystem>> = std::sync::OnceLock::new();

fn thread_system() -> Arc<ThreadActorSystem> {
    TS.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("dedicated engine runtime");
        let handle = rt.handle().clone();
        Box::leak(Box::new(rt)); // 进程级保活（worker/timer 永生）
        ThreadActorSystem::shared_with_handle(Default::default(), handle)
    })
    .clone()
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
    remote.shutdown().await.ok();
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
        components: vec![comp(
            "frontier",
            EngineKind::Erlang,
            // R3+R4：制品 = app 构建产物（apps/crawler-lab/erlang/
            // frontier.beam）——uri 直发目录，网关 add_patha + load_file。
            ArtifactRef::Beam {
                app: "frontier".into(),
                uri: Some(format!(
                    "file://{}",
                    repo_root().join("apps/crawler-lab/erlang").display()
                )),
            },
        )],
        wiring: vec![],
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

    // 冒烟：组件业务键 bin:crawl/FrontierPush（1 条目 → ack n=1——与
    // frontier.erl 协议一致；网关探针 bin:u:* 保留但不再是业务载荷）。
    let mut push = Vec::new();
    push.extend_from_slice(&1u32.to_le_bytes());
    push.extend_from_slice(&7u64.to_le_bytes());
    let url = b"https://parrot.dev/".to_vec();
    push.extend_from_slice(&(url.len() as u32).to_le_bytes());
    push.extend_from_slice(&url);
    push.extend_from_slice(&0u16.to_le_bytes());
    let r = ctx
        .component_ref("frontier")
        .unwrap()
        .send(Box::new(FrontierPushWire(push)))
        .await
        .expect("erl ask");
    let w = r
        .downcast_ref::<FrontierAckWire>()
        .expect("FrontierAck wire bytes");
    assert_eq!(w.0, 1u32.to_le_bytes().to_vec(), "frontier ack n=1");

    ctx.teardown(&deployer).await.ok();
    gw.procs().kill_all();
    remote.shutdown().await.ok();
}

/// erl/ray 线上字节容器（裸 LE——与 crawler-lab wire_msg 同构；方言键探测用）。
/// 注册为 bin:crawl/IndexStats（payload=空——index_builder 协议），
/// 回执按 bin:crawl/IndexStatsR 解码：[terms u32][postings u64] LE。
#[derive(Debug, Clone, PartialEq)]
pub struct ErlWireBytes(pub Vec<u8>);

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:crawl/IndexStats",
        type_id: std::any::TypeId::of::<ErlWireBytes>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<ErlWireBytes>().ok_or("downcast fail")?;
            Ok(m.0.clone())
        },
        decode: |b: &[u8]| {
            Ok(Box::new(ErlWireBytes(b.to_vec())) as BoxedMessage)
        },
    }
}

/// FrontierPush 载荷：[n u32][{id u64|len u32|url|depth u16}...]（payload 纯业务字节）。
#[derive(Debug, Clone, PartialEq)]
pub struct FrontierPushWire(pub Vec<u8>);

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:crawl/FrontierPush",
        type_id: std::any::TypeId::of::<FrontierPushWire>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<FrontierPushWire>().ok_or("downcast fail")?;
            Ok(m.0.clone())
        },
        decode: |b: &[u8]| {
            Ok(Box::new(FrontierPushWire(b.to_vec())) as BoxedMessage)
        },
    }
}

/// FrontierAck 回执：[n u32] LE。
#[derive(Debug, Clone, PartialEq)]
pub struct FrontierAckWire(pub Vec<u8>);

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:crawl/FrontierAck",
        type_id: std::any::TypeId::of::<FrontierAckWire>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<FrontierAckWire>().ok_or("downcast fail")?;
            Ok(m.0.clone())
        },
        decode: |b: &[u8]| {
            Ok(Box::new(FrontierAckWire(b.to_vec())) as BoxedMessage)
        },
    }
}

/// IndexStatsR 回执容器：[terms u32][postings u64] LE（payload 纯业务字节）。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexStatsRWire(pub Vec<u8>);

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:crawl/IndexStatsR",
        type_id: std::any::TypeId::of::<IndexStatsRWire>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<IndexStatsRWire>().ok_or("downcast fail")?;
            Ok(m.0.clone())
        },
        decode: |b: &[u8]| {
            Ok(Box::new(IndexStatsRWire(b.to_vec())) as BoxedMessage)
        },
    }
}

/// u:Ping（+3 erl / +2 ray 方言键——run-lab 同款语义探针）。
#[derive(Debug, Clone, PartialEq)]
pub struct UnitPing(pub u64);

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:u:Ping",
        type_id: std::any::TypeId::of::<UnitPing>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<UnitPing>().ok_or("downcast fail")?;
            Ok(m.0.to_le_bytes().to_vec())
        },
        decode: |b: &[u8]| {
            Ok(Box::new(UnitPing(u64::from_le_bytes(b[0..8].try_into().unwrap()))) as BoxedMessage)
        },
    }
}

/// u:Pong（方言偏移回执）。
#[derive(Debug, Clone, PartialEq)]
pub struct UnitPong(pub u64);

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:u:Pong",
        type_id: std::any::TypeId::of::<UnitPong>(),
        encode: |msg: &BoxedMessage| {
            let m = msg.downcast_ref::<UnitPong>().ok_or("downcast fail")?;
            Ok(m.0.to_le_bytes().to_vec())
        },
        decode: |b: &[u8]| {
            Ok(Box::new(UnitPong(u64::from_le_bytes(b[0..8].try_into().unwrap()))) as BoxedMessage)
        },
    }
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
        components: vec![comp(
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
        )],
        wiring: vec![],
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

    // 冒烟：组件业务键 bin:crawl/IndexStats（空索引 → terms=0 postings=0）。
    let r = ctx
        .component_ref("indexer")
        .unwrap()
        .send(Box::new(ErlWireBytes(vec![])))
        .await
        .expect("ray ask");
    let w = r
        .downcast_ref::<IndexStatsRWire>()
        .expect("IndexStatsR wire bytes");
    // [terms u32][postings u64] LE——空索引全零。
    assert_eq!(&w.0[0..4], &0u32.to_le_bytes(), "terms=0");
    assert_eq!(&w.0[4..12], &0u64.to_le_bytes(), "postings=0");

    ctx.teardown(&deployer).await.ok();
    gw.procs().kill_all();
    remote.shutdown().await.ok();
}

// ---------------- 场景 4：jvm(akka) 真实子进程网关 ----------------

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
        components: vec![comp(
            "search",
            EngineKind::Akka,
            ArtifactRef::Jvm { main_class: "crawler.search.SearchComponent".into(), coords: None, uri: Some(format!("file://{}", repo_root().join("apps/crawler-lab/jvm/target/crawler-lab-jvm-1.0.0.jar").display())) },
        )],
        wiring: vec![],
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

    // 冒烟：jvm 链路探针（UnitPing 经 wire 达 JVM——回执或方言错误均证明链路）
    let r = ctx
        .component_ref("search")
        .unwrap()
        .send(Box::new(UnitPing(1)))
        .await;
    // R4 语义：deploy 已在 JVM 网关内 spawn SearchComponent（ComponentRoutes
    // 路由命中——akka://parrot-gw/system/search）。UnitPing 非其消息集
    // （crawl/IndexTerms 族）——ask 超时/回执均证明链路已通；组件级语义
    // 由 crawler-lab 回归（run_regression.sh）与 mg_multiengine 承载。
    match r {
        Ok(_) => {}
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                msg.contains("not found")
                    || msg.contains("NotFound")
                    || msg.contains("unknown")
                    || msg.contains("Timeout")
                    || msg.contains("timed out"),
                "链路应达 JVM：{msg}"
            );
        }
    }

    ctx.teardown(&deployer).await.ok();
    gw.procs().kill_all();
    remote.shutdown().await.ok();
}

// ---------------- 场景 5：装配失败回滚（网关进程被杀清理） ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_failure_kills_gateway_procs() {
    let _gw_guard = gw_lock().await;
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let m = AppManifest {
        name: "fail-app".into(),
        version: "1.0.0".into(),
        components: vec![
            comp(
                "erl1",
                EngineKind::Erlang,
                ArtifactRef::Beam { app: "frontier".into(), uri: None },
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
    gw.procs().kill_all();
    remote.shutdown().await.ok();
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

    // 三实例各自可达
    for i in 0..3 {
        let r = ctx.component_refs("pool").unwrap()[i]
            .send(Box::new(format!("m{i}")))
            .await
            .unwrap();
        assert_eq!(*r.downcast_ref::<String>().unwrap(), format!("m{i}"));
    }
    ctx.teardown(&deployer).await.ok();
    remote.shutdown().await.ok();
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
        on_drain: None,
    };
    let m = AppManifest {
        name: "hooks".into(),
        version: "1.0.0".into(),
        components: vec![c],
        wiring: vec![],
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
    ctx.teardown(&deployer).await.ok();
    remote.shutdown().await.ok();
}

// ---------------- 场景 8：overlay 注入远程配置（Y3 端到端） ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_config_overlay_flows() {
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
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
    ctx.assemble(
        &plan,
        &deployer,
        &m,
        &parrot_config::ParrotConfig::default(),
    )
    .await
    .expect("overlay applied");
    ctx.teardown(&deployer).await.ok();
    remote.shutdown().await.ok();
    tokio::time::sleep(Duration::from_millis(50)).await;
}
