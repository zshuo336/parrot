//! MG1-4（DEV_09 §5.2）：单 App 跨 parrot+erlang+ray+jvm 四引擎部署全链。
//!
//! **ignored 形态**（执行入口：`make test-app-polyglot`）——需本机工具链（erl / python3+ray / java+jar），CI 与
//! G3 验收显式拉起。
//!
//! 验收语义（TECH_DESIGN_09 MG1-4 行）：
//! 当前验证 deploy → 四引擎业务 ASK → 真实组件状态 → teardown。
//! TELL 扇出、drain、wiring 端到端语义仍须另外验收。
//!
//! 形态：`test_assemble` helper（G2/M5）+ ProcessGatewayFactory 真实子
//! 进程网关（registry 模式——MG11 四方言同一路径）。

#![cfg(feature = "host")]
#![cfg(not(target_os = "windows"))]

use std::sync::Arc;

use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};

use parrot_app::host::{HostLookup, ProcessGatewayFactory, ThreadPropsSpawner};
use parrot_app::manifest::{
    AppManifest, ArtifactRef, ComponentSpec, EngineKind, InstancePolicy, PlacementConstraint,
    UpgradePolicy,
};
use parrot_app::planner::LocalTopology;
use parrot_app::test_support::test_assemble_default_cfg;

// ---------------- 本地对照组件 ----------------

pub struct HubActor;

impl Actor for HubActor {
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

fn spawn_hub(
    ts: Arc<ThreadActorSystem>,
    path: String,
    _config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let r = ts
            .spawn_at(HubActor, &path, None, ThreadActorConfig::default())
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(format!("spawn: {e}")))?;
        Ok(Box::new(r) as BoxedActorRef)
    })
}

parrot_api::message::inventory::submit! {
    parrot_app::host::ThreadPropsFactory {
        name: "app.mg.hub",
        spawn: spawn_hub,
    }
}

// 真实 crawler 业务载荷与结果契约。
#[path = "support/crawler_contract.rs"]
mod crawler_contract;

// ---------------- 公共设施（app_run_scenarios 同款） ----------------

fn thread_system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(Default::default())
}

fn repo_root() -> std::path::PathBuf {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.parent().unwrap().parent().unwrap().to_path_buf()
}

static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(20601);
fn alloc_port() -> u16 {
    NEXT_PORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// 网关场景串行锁（同时拉三网关子进程争资源——MG 全链必须串行）。
static GW_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        hooks: Default::default(),
    }
}

// ---------------- MG1-4：跨四引擎部署全链（ignored——DoD §5.6 拉起） ----------------

/// 单 App 四引擎：deploy → 全 Running（refs 就绪）→ 跨引擎 ask 各方言键
/// → status 聚合（started 全序 + 各组件 refs 非空）→ teardown。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "DoD §5.6: 需 erl/java(jar)/python3+ray 工具链——显式 --ignored 拉起"]
async fn mg_multiengine_deploy_running_ask_status_full_chain() {
    let _g = GW_LOCK.lock().await;
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;

    // 三网关组件（engine 即 artifact 方言）+ parrot hub（显式依赖三网关
    // ——无依赖时 Kahn 按字典序，hub 会插队；deps 声明装配序 = 依赖序）
    let frontier = comp(
        "frontier",
        EngineKind::Erlang,
        ArtifactRef::Beam {
            app: "frontier".into(),
            uri: Some(format!(
                "file://{}",
                repo_root().join("apps/crawler-lab/erlang").display()
            )),
        },
    );
    let mut index = comp(
        "index",
        EngineKind::Ray,
        ArtifactRef::PyModule {
            module: "index_builder".into(),
            runtime_env: None,
            uri: Some(format!(
                "file://{}",
                repo_root().join("apps/crawler-lab/python").display()
            )),
        },
    );
    let mut search = comp(
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
    );
    let mut hub = comp(
        "hub",
        EngineKind::Parrot,
        ArtifactRef::Props {
            factory: "app.mg.hub".into(),
        },
    );
    // 网关间序（frontier → index → search）+ hub 殿后
    index.deps = vec!["frontier".into()];
    search.deps = vec!["index".into()];
    hub.deps = vec!["frontier".into(), "index".into(), "search".into()];
    let m = AppManifest {
        name: "mg-app".into(),
        version: "1.0.0".into(),
        components: vec![frontier, index, search, hub],
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    };

    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());

    // ── deploy：依赖序装配（三网关先注册，hub 本地 spawn）──
    let mut a = test_assemble_default_cfg(
        &m,
        Box::new(LocalTopology),
        Box::new(gw2(&gw)),
        Box::new(spawner),
    )
    .await
    .expect("MG deploy（四引擎装配）");

    // ── 组件全 Running：started 全序 + refs 全非空 ──
    assert_eq!(
        a.started(),
        &[
            "frontier".to_string(),
            "index".to_string(),
            "search".to_string(),
            "hub".to_string(),
        ],
        "deploy 序 = 依赖序（三网关 → hub）"
    );
    for c in ["frontier", "index", "search", "hub"] {
        assert!(
            a.refs(c).map(|r| !r.is_empty()).unwrap_or(false),
            "组件 {c} refs 空——未 Running"
        );
    }

    // ── 跨引擎 ask：方言键探针（erl +3 / ray +2 / jvm 搜索类回执）──
    crawler_contract::frontier(&a.first("frontier").unwrap()).await;
    crawler_contract::ray_index(&a.first("index").unwrap()).await;
    crawler_contract::jvm_search(&a.first("search").unwrap()).await;

    let h = a.first("hub").expect("hub ref");
    let r = h
        .send(Box::new("mg".to_string()) as BoxedMessage)
        .await
        .expect("hub ask");
    assert_eq!(*r.downcast::<String>().unwrap(), "mg");

    // ── status 聚合视图：paths 全解析（/user/<name> 惯例）──
    for c in ["frontier", "index", "search", "hub"] {
        let paths = a.paths(c).expect("paths");
        assert!(!paths.is_empty(), "{c} paths 空");
    }

    for name in ["frontier", "index", "search"] {
        let node = gw.component_node(name).expect("owned gateway identity");
        let status = remote
            .component_status(&node, &format!("/user/{name}"))
            .await
            .unwrap();
        assert_eq!(status.len(), 1, "{name} must report its deployed instance");
        assert_eq!(status[0].version, m.version);
        assert_eq!(status[0].state, "running");
    }
    assert!(h.is_alive().await);

    a.teardown().await.expect("teardown");
    gw.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

// ---------------- 分引擎门禁（回归定位用——单网关失败时快速归因） ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "DoD §5.6 子项：单 erl 网关——快速归因用"]
async fn mg_erlang_gateway_smoke() {
    let _g = GW_LOCK.lock().await;
    single_engine_smoke(comp(
        "frontier",
        EngineKind::Erlang,
        ArtifactRef::Beam {
            app: "frontier".into(),
            uri: Some(format!(
                "file://{}",
                repo_root().join("apps/crawler-lab/erlang").display()
            )),
        },
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "DoD §5.6 子项：单 ray 网关——快速归因用"]
async fn mg_ray_gateway_smoke() {
    let _g = GW_LOCK.lock().await;
    single_engine_smoke(comp(
        "index",
        EngineKind::Ray,
        ArtifactRef::PyModule {
            module: "index_builder".into(),
            runtime_env: None,
            uri: Some(format!(
                "file://{}",
                repo_root().join("apps/crawler-lab/python").display()
            )),
        },
    ))
    .await;
}

async fn single_engine_smoke(c: ComponentSpec) {
    let ts = thread_system();
    let remote = make_remote(ts.clone(), alloc_port()).await;
    let m = AppManifest {
        name: format!("mg-{}", c.name),
        version: "1.0.0".into(),
        components: vec![c],
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    };
    let gw = ProcessGatewayFactory::new(repo_root(), remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let mut a = test_assemble_default_cfg(
        &m,
        Box::new(LocalTopology),
        Box::new(gw2(&gw)),
        Box::new(spawner),
    )
    .await
    .expect("assemble");
    let name = a.started().first().unwrap().clone();
    let r = a.first(&name).expect("ref");
    match name.as_str() {
        "frontier" => crawler_contract::frontier(&r).await,
        "index" => crawler_contract::ray_index(&r).await,
        other => panic!("unexpected component {other}"),
    }
    a.teardown().await.unwrap();
    gw.procs().kill_all().await.unwrap();
    remote.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

// ---------------- 私有 helper ----------------

async fn make_remote(ts: Arc<ThreadActorSystem>, port: u16) -> Arc<RemoteActorSystem> {
    let sa: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let r = RemoteActorSystem::new(
        RemoteConfig::tcp("mg-multiengine-test", Some(sa)),
        Arc::new(HostLookup { ts }),
    )
    .unwrap();
    r.start().await.unwrap();
    r
}

/// A clone shares owned process state without raw-pointer lifetime assertions.
fn gw2(gw: &ProcessGatewayFactory) -> ProcessGatewayFactory {
    gw.clone()
}
