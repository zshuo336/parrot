//! parrot-node bin：标准单节点运行时进程（部署最小单元）。
//!
//! 组装形态与环境变量契约见 `parrot_node` lib 文档。

use std::sync::Arc;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::ThreadActorSystemConfig;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::system::ActorSystemConfig;
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};

use parrot_node::{
    spawn_builtin, wait_for_shutdown, FacadeLookup, GatewayAdapter, NodeComponentExecutor,
    NodeState, NODE,
};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    // healthcheck 子命令：TCP 探测自身（容器内零依赖探活）
    if std::env::var("PARROT_HEALTHCHECK").as_deref() == Ok("1") {
        std::process::exit(parrot_node::healthcheck());
    }

    let node_id = std::env::var("PARROT_NODE_ID").unwrap_or_else(|_| "node-1".into());
    let bind = std::env::var("PARROT_BIND").unwrap_or_else(|_| "0.0.0.0:9801".into());
    let seeds = parrot_node::parse_seeds(std::env::var("PARROT_SEEDS").unwrap_or_default()).await;

    // 1. facade + 真实 thread 引擎
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .expect("facade init"),
    );
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .expect("register engine");
    NODE.set(NodeState {
        facade: facade.clone(),
        ts: ts.clone(),
    })
    .ok()
    .expect("node state singleton");

    // 2. 内置 actor 族（G2/M3：builtin_app.toml Manifest 驱动——
    //    PARROT_ACTORS 语义不变，变为 overlay 过滤器）
    for name in parrot_node::builtin_manifest_components() {
        spawn_builtin(&ts, &name).await;
    }

    // 3. 远程系统（TCP listen）；B2：caps 置位 ARTIFACTS + 安装组件执行器
    let mut cfg = RemoteConfig::tcp(&node_id, Some(bind.parse().expect("PARROT_BIND parse")));
    cfg.extra_caps |= parrot_remote::handshake::caps::ARTIFACTS;
    let remote = RemoteActorSystem::new(cfg, Arc::new(FacadeLookup)).expect("remote system");
    remote.set_component_executor(Some(Arc::new(NodeComponentExecutor::new(ts.clone()))));
    facade
        .register_remote_gateway(Arc::new(GatewayAdapter(remote.gateway())))
        .await
        .expect("register remote gateway");
    remote.start().await.expect("remote listen");
    let local = remote.local_addr().expect("listening addr");

    // 4. 种子互联（失败容忍——对端就绪有时差；compose depends_on 缓解）
    for s in &seeds {
        match remote.connect(s).await {
            Ok(_) => eprintln!("[parrot-node] seed ok: {}", s.node_id),
            Err(e) => eprintln!("[parrot-node] seed defer ({}): {e}", s.node_id),
        }
    }

    // 5. 就绪契约
    println!("PARROT_NODE_READY={local}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    if let Ok(f) = std::env::var("PARROT_READY_FILE") {
        std::fs::write(&f, local.to_string()).ok();
    }

    // 6. 常驻：CTRL-C / SIGTERM 优雅退出
    wait_for_shutdown().await;
    eprintln!("[parrot-node] shutting down");
    let _ = remote.shutdown().await;
}
