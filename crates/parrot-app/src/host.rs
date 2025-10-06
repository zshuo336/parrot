//! 宿主层（feature `host`——09 §2.2 分层：宿主 Runner 依赖引擎）。
//!
//! A4 交付：
//! - [`ThreadPropsSpawner`]：parrot 组件 → `ThreadActorSystem::shared()` +
//!   PropsFactory（K0 机制复用——与 parrot-node 同形态）；
//! - [`ProcessGatewayFactory`]：akka/ray/erlang 组件 → 真实子进程网关
//!   （本机 spawn + registry 模式注册 + 回环 TCP——Wire 序列化路径与生产一致）。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use parrot::thread::system::ThreadActorSystem;
use parrot_api::types::BoxedActorRef;
use parrot_remote::system::RemoteActorSystem;
use parrot_remote::LocalLookup;

use crate::assemble::{DeployError, GatewayFactory, ParrotSpawner};
use crate::manifest::{ArtifactRef, ComponentSpec, EngineKind};

// ============================================================================
// parrot 组件 spawn 面（ThreadActorSystem + PropsFactory）
// ============================================================================

/// Thread 引擎 PropsFactory spawn 适配器（K0 复用）。
pub struct ThreadPropsSpawner {
    engine: Arc<ThreadActorSystem>,
}

impl ThreadPropsSpawner {
    pub fn new(engine: Arc<ThreadActorSystem>) -> Self {
        Self { engine }
    }
}

impl ParrotSpawner for ThreadPropsSpawner {
    fn spawn_props(
        &self,
        factory: &str,
        paths: &[String],
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        let f = parrot_remote::admin::find_factory(factory).ok_or_else(|| {
            DeployError::Artifact(format!(
                "props factory '{factory}' not registered (inventory)"
            ))
        })?;
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let r = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on((f.spawn)(p))
            })
            .map_err(|e| DeployError::Engine(format!("spawn {p}: {e}")))?;
            out.push(r);
        }
        Ok(out)
    }

    fn stop_paths(&self, paths: &[String]) -> Result<(), DeployError> {
        for p in paths {
            if let Some(r) = self.engine.get_actor_ref(p) {
                let _ = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(r.stop())
                });
            }
        }
        Ok(())
    }
}

// ============================================================================
// 网关组件：真实子进程 + registry 模式注册
// ============================================================================

/// LocalLookup 适配：宿主 thread 引擎本地查找。
pub struct HostLookup {
    pub ts: Arc<ThreadActorSystem>,
}

#[async_trait::async_trait]
impl LocalLookup for HostLookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        // registry 直查（thread 引擎本地段）
        self.ts
            .get_actor_ref(path)
            .map(|r| Box::new(ArcActorRef(r)) as Box<dyn parrot_api::address::ActorRef>)
    }
}

/// Arc<dyn ActorRef> → Box<dyn ActorRef> 适配（clone_boxed 保共享）。
#[derive(Debug)]
struct ArcActorRef(Arc<dyn parrot_api::address::ActorRef>);

#[async_trait::async_trait]
impl parrot_api::address::ActorRef for ArcActorRef {
    fn send<'a>(
        &'a self,
        msg: parrot_api::types::BoxedMessage,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        self.0.send(msg)
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: parrot_api::types::BoxedMessage,
        t: Option<std::time::Duration>,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        self.0.send_with_timeout(msg, t)
    }
    fn deliver<'a>(
        &'a self,
        msg: parrot_api::types::BoxedMessage,
    ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<()>> {
        self.0.deliver(msg)
    }
    fn stop<'a>(
        &'a self,
    ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<()>> {
        self.0.stop()
    }
    fn path(&self) -> String {
        self.0.path()
    }
    fn is_alive<'a>(&'a self) -> parrot_api::types::BoxedFuture<'a, bool> {
        self.0.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self(self.0.clone()))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// 网关子进程组（teardown 时 kill）。
#[derive(Default)]
pub struct GatewayProcesses {
    procs: std::sync::Mutex<Vec<tokio::process::Child>>,
}

impl GatewayProcesses {
    pub fn kill_all(&self) {
        let mut g = self.procs.lock().unwrap();
        for c in g.iter_mut() {
            let _ = c.start_kill();
        }
        g.clear();
    }
}

/// 真实子进程网关工厂（用户裁定：一律真实子进程，不 mock）。
pub struct ProcessGatewayFactory {
    /// 仓库根（定位 interop/ 三方言）。
    pub repo_root: PathBuf,
    /// 应用 RemoteActorSystem（网关 registry 模式注册目标）。
    pub remote: Arc<RemoteActorSystem>,
    pub procs: Arc<GatewayProcesses>,
    entries: std::sync::Mutex<std::collections::HashMap<String, Vec<BoxedActorRef>>>,
    node_ids: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl ProcessGatewayFactory {
    pub fn new(repo_root: PathBuf, remote: Arc<RemoteActorSystem>) -> Self {
        Self {
            repo_root,
            remote,
            procs: Arc::new(GatewayProcesses::default()),
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
            node_ids: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn procs(&self) -> Arc<GatewayProcesses> {
        self.procs.clone()
    }

    /// 网关约定 node_id（与各网关握手自报一致——run-lab 契约：
    /// erl=erl-gw-1 / ray=ray-gw-1 / jvm=jvm-search-1）。
    fn node_id_for(comp: &ComponentSpec) -> String {
        match (&comp.engine, &comp.artifact) {
            (EngineKind::Erlang, ArtifactRef::Beam { app }) => {
                if app == "frontier" {
                    "erl-gw-1".into()
                } else {
                    format!("erl-gw-{app}")
                }
            }
            (EngineKind::Ray, _) => "ray-gw-1".into(),
            (EngineKind::Akka, ArtifactRef::Jvm { main_class, .. }) => {
                if main_class.contains("Crawler") {
                    "jvm-search-1".into()
                } else {
                    "jvm-gw-1".into()
                }
            }
            _ => format!("{}-gw-1", comp.engine.as_str()),
        }
    }

    /// 启动一网关子进程并等其注册（registry 模式——run-lab.sh 同款语义）。
    async fn spawn_gateway_process(&self, comp: &ComponentSpec) -> Result<String, DeployError> {
        let app_addr = self
            .remote
            .local_addr()
            .map(|a| a.to_string())
            .ok_or_else(|| DeployError::Engine("app not listening".into()))?;

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .current_dir(&self.repo_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match comp.engine {
            EngineKind::Erlang => {
                cmd.arg(format!(
                    "cd interop/erlang && erlc parrot_gw.erl 2>/dev/null; exec erl -noshell -pa . -eval 'parrot_gw:main([0, \"parrot={app_addr}\"])'"
                ));
            }
            EngineKind::Ray => {
                cmd.arg(format!(
                    "cd interop/python && exec env PYTHONPATH=. python3 -m parrot_protocol.ray_gw 0 parrot={app_addr}"
                ));
            }
            EngineKind::Akka => {
                let main_class = match &comp.artifact {
                    ArtifactRef::Jvm { main_class, .. } => main_class.clone(),
                    _ => "parrot.protocol.jvm.ParrotGatewayMain".into(),
                };
                cmd.arg(format!(
                    "cd interop/jvm/target && exec java -cp \"parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)\" {main_class} 0 parrot={app_addr} 7200"
                ));
            }
            other => {
                return Err(DeployError::Engine(format!(
                    "engine {other:?} has no local gateway process form"
                )))
            }
        }
        let child = cmd
            .spawn()
            .map_err(|e| DeployError::Engine(format!("gateway spawn: {e}")))?;
        self.procs.procs.lock().unwrap().push(child);

        // 本函数运行于 block_in_place 的阻塞上下文——用 std timer（tokio
        // timer 在 futures executor 下不驱动，会永远 Pending）。
        let node_id = Self::node_id_for(comp);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            if self.remote.nodes.get(&node_id).is_some() {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err(DeployError::Engine(format!(
                    "gateway '{node_id}' not registered within 90s"
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        // links 就绪窗口（register_link 异步完成——run-lab 同款；超时放行，
        // NodeTable 已就绪，links 由心跳兜底）
        let links_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let snapshot = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async { self.remote.links_snapshot().await })
            });
            if snapshot.iter().any(|(n, _, _)| n == &node_id) {
                break;
            }
            if std::time::Instant::now() > links_deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Ok(node_id)
    }
}

impl GatewayFactory for ProcessGatewayFactory {
    fn start_gateway(
        &self,
        comp: &ComponentSpec,
        paths: &[String],
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        // GatewayFactory 是同步 trait（assemble 异步上下文内调用）——
        // block_in_place 让出调度（multi-thread runtime）后阻塞等注册。
        let node_id = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { self.spawn_gateway_process(comp).await })
        })?;
        let mut refs = Vec::with_capacity(paths.len());
        for p in paths {
            let full = format!("parrot://{node_id}{p}");
            let r = self
                .remote
                .remote_ref(&full)
                .map_err(|e| DeployError::Engine(format!("remote_ref {full}: {e}")))?;
            refs.push(Box::new(r) as BoxedActorRef);
        }
        self.entries.lock().unwrap().insert(
            comp.name.clone(),
            refs.iter().map(|r| r.clone_boxed()).collect(),
        );
        self.node_ids
            .lock()
            .unwrap()
            .insert(comp.name.clone(), node_id);
        Ok(refs)
    }
    fn stop_gateway(&self, comp: &ComponentSpec) -> Result<(), DeployError> {
        let had = self.entries.lock().unwrap().remove(&comp.name).is_some();
        self.node_ids.lock().unwrap().remove(&comp.name);
        if had {
            self.procs.kill_all();
        }
        Ok(())
    }
}
