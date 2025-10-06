//! parrot-app CLI（A4）：`parrot-app run --manifest <path> [--bind host:port]`。
//!
//! 本地同构宿主（09 §2.3）：
//! ① parrot-config 装配（overlay > parrot.toml > 默认——Y3）
//! ② Planner 拓扑排序 → AssemblingContext 依序装配
//! ③ parrot 组件 → ThreadActorSystem::shared() + PropsFactory；
//!    akka/ray/erlang → 真实子进程网关（registry 模式注册，回环 TCP）
//! ④ wiring 冒烟（每组件 ask 一发 HealthPing 同构探针）
//! ⑤ Ctrl-C → 逆依赖序 teardown → 网关子进程组 kill

use std::path::PathBuf;
use std::sync::Arc;

use parrot::thread::system::ThreadActorSystem;
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};

use parrot_app::assemble::{AssemblingContext, LocalDeployer};
use parrot_app::host::{HostLookup, ProcessGatewayFactory, ThreadPropsSpawner};
use parrot_app::manifest::AppManifest;
use parrot_app::planner::{plan, LocalTopology};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => {
            let manifest = args
                .iter()
                .position(|a| a == "--manifest")
                .and_then(|p| args.get(p + 1))
                .expect("usage: parrot-app run --manifest <path> [--bind host:port]")
                .clone();
            let bind = args
                .iter()
                .position(|a| a == "--bind")
                .and_then(|p| args.get(p + 1))
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:19880".into());
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let rc = rt.block_on(cmd_run(&PathBuf::from(manifest), &bind));
            std::process::exit(rc);
        }
        _ => {
            eprintln!("parrot-app 0.1 (DEV_09 A4)");
            eprintln!("  parrot-app run --manifest <app.toml> [--bind host:port]");
            std::process::exit(2);
        }
    }
}

/// 本地组装入口（`parrot app run`——同构调试基石）。
pub async fn cmd_run(manifest_path: &std::path::Path, bind: &str) -> i32 {
    let m = match AppManifest::from_file(manifest_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[app] manifest load failed: {e}");
            return 2;
        }
    };
    println!(
        "[app] manifest: {} v{}（{} 组件）",
        m.name,
        m.version,
        m.components.len()
    );

    // ① 配置装配（PARROT_CONFIG 标准入口 + overlay 并入由 assemble 内完成）
    let cfg = match parrot_config::ParrotConfig::builder().load_default_locations() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[app] config load failed: {e}");
            return 2;
        }
    };

    // ② 规划
    let plan = match plan(&m, &LocalTopology) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[app] plan failed: {e}");
            return 2;
        }
    };
    for w in &plan.warnings {
        eprintln!("[app] plan warning: {w}");
    }
    println!(
        "[app] plan: {}",
        plan.order
            .iter()
            .map(|c| c.spec.name.as_str())
            .collect::<Vec<_>>()
            .join(" → ")
    );

    // ③ 引擎 + 远程 + 网关工厂
    let ts = ThreadActorSystem::shared(Default::default());
    let sa: std::net::SocketAddr = bind.parse().expect("--bind parse");
    let remote = match RemoteActorSystem::new(
        RemoteConfig::tcp("app-host", Some(sa)),
        Arc::new(HostLookup { ts: ts.clone() }),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[app] remote init failed: {e}");
            return 2;
        }
    };
    if let Err(e) = remote.start().await {
        eprintln!("[app] remote start failed: {e}");
        return 2;
    }
    println!("[app] listening {}", remote.local_addr().expect("bound"));

    let repo_root = std::env::var("PARROT_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| find_repo_root());
    let gw = ProcessGatewayFactory::new(repo_root, remote.clone());
    let spawner = ThreadPropsSpawner::new(ts.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &gw,
        parrot_spawner: &spawner,
    };

    // ④ 装配（失败自动回滚）
    let mut ctx = AssemblingContext::new();
    if let Err(e) = ctx.assemble(&plan, &deployer, &m, &cfg).await {
        eprintln!("[app] assemble failed: {e}");
        gw.procs().kill_all();
        let _ = remote.shutdown().await;
        return 1;
    }
    println!("[app] assembled: {:?}", ctx.started());

    // ⑤ 就绪契约 + 常驻
    println!("PARROT_APP_READY={}", remote.local_addr().expect("bound"));
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    if let Ok(f) = std::env::var("PARROT_APP_READY_FILE") {
        std::fs::write(&f, remote.local_addr().expect("bound").to_string()).ok();
    }

    // Ctrl-C / SIGTERM → 优雅退出
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.expect("ctrl_c handler");
    }

    eprintln!("[app] shutting down");
    let _ = ctx.teardown(&deployer).await;
    gw.procs().kill_all();
    let _ = remote.shutdown().await;
    0
}

/// 从可执行文件位置向上找仓库根（含 interop/ 的目录）。
fn find_repo_root() -> PathBuf {
    let mut dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    loop {
        if dir.join("interop/erlang/parrot_gw.erl").exists() {
            return dir;
        }
        if !dir.pop() {
            return PathBuf::from(".");
        }
    }
}
