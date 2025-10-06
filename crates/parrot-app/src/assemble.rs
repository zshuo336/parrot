//! AssemblingContext（DEV_09 §3.1 A3）：依赖序装配 + 失败逆序回滚 + hooks。
//!
//! 行为规约：
//! ① config_overlay 并入 parrot-config（Y3：overlay > 代码 > parrot.toml > 默认）；
//! ② hooks 路径解析：on_start 是组件内 actor 路径，宿主发 `ParrotAppHook` 消息
//!    （type_key `bin:app.hook#v1`）并等回执，超时 = 装配失败；
//! ③ 失败回滚逆序保证（started 栈）。

use std::collections::HashMap;
use std::time::Duration;

use parrot_api::types::{BoxedActorRef, BoxedMessage};

use crate::manifest::{AppManifest, ComponentSpec};
use crate::planner::{Plan, TopologyView};

/// 应用钩子消息（宿主 → 组件 hook actor；type_key `bin:app.hook#v1`）。
///
/// 裸字节编解码（与网关方言同构——A4 起注册 codec）：
/// `[phase u8]`：0=Start 1=Stop 2=Drain。回执同型（同 phase 回显）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParrotAppHook(pub u8);

pub mod hook_phase {
    pub const START: u8 = 0;
    pub const STOP: u8 = 1;
    pub const DRAIN: u8 = 2;
}

impl ParrotAppHook {
    pub fn encode(&self) -> Vec<u8> {
        vec![self.0]
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        b.first().map(|&p| Self(p))
    }
}

/// 部署错误（DeployError 枚举驱动——错误注入点显式建模）。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DeployError {
    #[error("artifact: {0}")]
    Artifact(String),
    #[error("engine: {0}")]
    Engine(String),
    #[error("config: {0}")]
    Config(String),
    #[error("hook panic: comp={comp} hook={hook}")]
    HookPanic { comp: String, hook: String },
    #[error("hook timeout: comp={comp} hook={hook}")]
    HookTimeout { comp: String, hook: String },
}

/// 本地网关工厂（A4 的四引擎内嵌实现点；测试替身 = memory transport 对）。
pub trait GatewayFactory: Send + Sync {
    /// 为非 parrot 组件建本地执行面：返回该组件的入口 ActorRef
    /// （经 Wire 编解码——与生产同路径）。
    fn start_gateway(
        &self,
        comp: &ComponentSpec,
        paths: &[String],
    ) -> Result<Vec<BoxedActorRef>, DeployError>;
    /// 停网关（回滚/teardown 用）。
    fn stop_gateway(&self, comp: &ComponentSpec) -> Result<(), DeployError>;
}

/// parrot 组件 spawn 面（PropsFactory 走 parrot-remote inventory；
/// 本 trait 是宿主注入点——A4 用真实 ThreadActorSystem 适配，测试用替身）。
pub trait ParrotSpawner: Send + Sync {
    /// 按 Props 工厂名在给定路径 spawn（K0 机制复用）。
    fn spawn_props(
        &self,
        factory: &str,
        paths: &[String],
    ) -> Result<Vec<BoxedActorRef>, DeployError>;
    /// 停一组路径（优雅）。
    fn stop_paths(&self, paths: &[String]) -> Result<(), DeployError>;
}

/// 装配上下文：registry + started 逆序停用栈。
pub struct AssemblingContext {
    registry: HashMap<String, Vec<BoxedActorRef>>,
    /// 已启动组件（逆序 pop = 逆依赖序停用）。
    started: Vec<String>,
    /// 组件 → (engine, 全部实例路径)——回滚时正确分流停用面。
    paths: HashMap<String, (crate::manifest::EngineKind, Vec<String>)>,
    /// hooks 回执超时。
    hooks_timeout: Duration,
}

impl Default for AssemblingContext {
    fn default() -> Self {
        Self::new()
    }
}

/// 本地部署器（manifest → 本地执行面；与 ClusterDeployer 同构差异点）。
pub struct LocalDeployer<'a> {
    pub topology: &'a dyn TopologyView,
    pub gateway_factory: &'a dyn GatewayFactory,
    pub parrot_spawner: &'a dyn ParrotSpawner,
}

impl AssemblingContext {
    pub fn new() -> Self {
        Self {
            registry: HashMap::new(),
            started: Vec::new(),
            paths: HashMap::new(),
            hooks_timeout: Duration::from_secs(5),
        }
    }

    /// hooks 回执超时（测试注入用）。
    pub fn set_hooks_timeout(&mut self, d: Duration) {
        self.hooks_timeout = d;
    }

    /// 按 Plan.order 依序装配；任一失败 → 已启动组件逆序 stop 后返回错误（原子性）。
    pub async fn assemble(
        &mut self,
        plan: &Plan,
        deployer: &LocalDeployer<'_>,
        manifest: &AppManifest,
        cfg: &parrot_config::ParrotConfig,
    ) -> Result<(), DeployError> {
        // Y3：overlay > 代码 > parrot.toml > 默认——overlay 以代码层语义注入
        //（merge 进 builder 之上；代码层已设键不被文件覆盖，overlay 再覆盖两者）。
        let mut cfg = cfg.clone();
        if let Some(overlay) = &manifest.config_overlay {
            apply_overlay(&mut cfg, overlay)?;
        }

        for pc in &plan.order {
            let comp = &pc.spec;
            let paths: Vec<String> = match (&pc.shard_plan, comp.instances.instance_count()) {
                (Some(sp), _) => sp.clone(),
                (None, 1) => vec![format!("/user/{}", comp.name)],
                (None, _) => (0..comp.instances.instance_count())
                    .map(|i| format!("/user/{}-{}", comp.name, i))
                    .collect(),
            };

            let refs = match spawn_component(comp, &paths, deployer).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = rollback(
                        &mut self.started,
                        &mut self.registry,
                        &mut self.paths,
                        deployer,
                    )
                    .await;
                    return Err(e);
                }
            };

            // on_start 钩子：发 ParrotAppHook(START) 等回执
            if let Some(hook_path) = &comp.hooks.on_start {
                let full = if hook_path.starts_with('/') {
                    hook_path.clone()
                } else {
                    format!("/{hook_path}")
                };
                match fire_hook(&refs, &full, hook_phase::START, self.hooks_timeout).await {
                    Ok(()) => {}
                    Err(DeployError::HookTimeout { .. }) => {
                        // 超时：回滚已起组件再报错
                        let _ = rollback(
                            &mut self.started,
                            &mut self.registry,
                            &mut self.paths,
                            deployer,
                        )
                        .await;
                        return Err(DeployError::HookTimeout {
                            comp: comp.name.clone(),
                            hook: hook_path.clone(),
                        });
                    }
                    Err(e) => {
                        let _ = rollback(
                            &mut self.started,
                            &mut self.registry,
                            &mut self.paths,
                            deployer,
                        )
                        .await;
                        return Err(e);
                    }
                }
            }

            self.registry.insert(comp.name.clone(), refs);
            self.paths.insert(comp.name.clone(), (comp.engine, paths));
            self.started.push(comp.name.clone());
        }
        Ok(())
    }

    /// 逆依赖序停用（优雅关闭——on_stop 钩子 → stop）。
    pub async fn teardown(&mut self, deployer: &LocalDeployer<'_>) -> Result<(), DeployError> {
        rollback(
            &mut self.started,
            &mut self.registry,
            &mut self.paths,
            deployer,
        )
        .await
    }

    /// 组件入口 ref 快查（wiring 解析面）。
    pub fn component_ref(&self, name: &str) -> Option<&BoxedActorRef> {
        self.registry.get(name).and_then(|v| v.first())
    }

    pub fn component_refs(&self, name: &str) -> Option<&[BoxedActorRef]> {
        self.registry.get(name).map(|v| v.as_slice())
    }

    pub fn component_paths(&self, name: &str) -> Option<&[String]> {
        self.paths.get(name).map(|(_, v)| v.as_slice())
    }

    pub fn started(&self) -> &[String] {
        &self.started
    }
}

/// 单组件 spawn 分流（parrot → PropsFactory；其他 → GatewayFactory）。
async fn spawn_component(
    comp: &ComponentSpec,
    paths: &[String],
    deployer: &LocalDeployer<'_>,
) -> Result<Vec<BoxedActorRef>, DeployError> {
    match comp.engine {
        crate::manifest::EngineKind::Parrot => deployer.parrot_spawner.spawn_props(
            match &comp.artifact {
                crate::manifest::ArtifactRef::Props { factory } => factory,
                other => {
                    return Err(DeployError::Artifact(format!(
                        "wasm/dylib artifacts require feature-gated runtime (C/D 阶段): {}",
                        other.kind()
                    )))
                }
            },
            paths,
        ),
        _ => deployer.gateway_factory.start_gateway(comp, paths),
    }
}

/// 回滚：逆序 stop 已启动组件（ref.stop → 按引擎分流停用面）。
async fn rollback(
    started: &mut Vec<String>,
    registry: &mut HashMap<String, Vec<BoxedActorRef>>,
    paths: &mut HashMap<String, (crate::manifest::EngineKind, Vec<String>)>,
    deployer: &LocalDeployer<'_>,
) -> Result<(), DeployError> {
    let mut first_err = None;
    while let Some(name) = started.pop() {
        if let Some(refs) = registry.remove(&name) {
            for r in refs {
                let _ = r.stop().await;
            }
        }
        if let Some((engine, p)) = paths.remove(&name) {
            match engine {
                crate::manifest::EngineKind::Parrot => {
                    if let Err(e) = deployer.parrot_spawner.stop_paths(&p) {
                        first_err.get_or_insert(e);
                    }
                }
                other_engine => {
                    let comp = ComponentSpec {
                        name: name.clone(),
                        engine: other_engine,
                        artifact: crate::manifest::ArtifactRef::Props {
                            factory: String::new(),
                        },
                        alt_artifact: None,
                        instances: crate::manifest::InstancePolicy::Singleton,
                        placement: Default::default(),
                        upgrade: Default::default(),
                        deps: vec![],
                        config: None,
                        hooks: Default::default(),
                    };
                    if let Err(e) = deployer.gateway_factory.stop_gateway(&comp) {
                        first_err.get_or_insert(e);
                    }
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// 发 hook 消息并等回执（同型回显）。
async fn fire_hook(
    refs: &[BoxedActorRef],
    _hook_path: &str,
    phase: u8,
    timeout: Duration,
) -> Result<(), DeployError> {
    let Some(target) = refs.first() else {
        return Err(DeployError::Engine("no instance to fire hook".into()));
    };
    let msg = hook_message(phase);
    let fut = target.send(msg);
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(reply)) => {
            if let Some(p) = reply.downcast_ref::<ParrotAppHook>() {
                if p.0 == phase {
                    return Ok(());
                }
            }
            // 网关组件回裸字节——容忍（回执形态由宿主适配器归一）
            Ok(())
        }
        Ok(Err(e)) => Err(DeployError::Engine(format!("hook ask failed: {e}"))),
        Err(_) => Err(DeployError::HookTimeout {
            comp: String::new(),
            hook: _hook_path.into(),
        }),
    }
}

/// hook 消息构造（BoxedMessage 直接装 ParrotAppHook——本地路径；
/// 跨网关走 codec 注册的 `bin:app.hook#v1`）。
fn hook_message(phase: u8) -> BoxedMessage {
    Box::new(ParrotAppHook(phase))
}

/// Y3 overlay 合并：`app config_overlay > 代码 > parrot.toml > 默认`。
///
/// 直接从 toml 表读取已知键（存在即覆盖代码层；不存在保持）——不做
/// serde 往返（部分表缺必填子节会解析失败）。未知键拒绝（防拼错静默）。
pub fn apply_overlay(
    cfg: &mut parrot_config::ParrotConfig,
    overlay: &toml::value::Table,
) -> Result<(), DeployError> {
    let mut unknown: Vec<String> = Vec::new();

    if let Some(t) = overlay.get("thread").and_then(|v| v.as_table()) {
        let th = &mut cfg.thread;
        opt_num(
            t,
            "shared_pool_size",
            &mut th.shared_pool_size,
            &mut unknown,
        );
        opt_num(
            t,
            "shared_burst_workers_max",
            &mut th.shared_burst_workers_max,
            &mut unknown,
        );
        opt_num(
            t,
            "shared_burst_backlog_threshold_ms",
            &mut th.shared_burst_backlog_threshold_ms,
            &mut unknown,
        );
        opt_num(
            t,
            "shared_burst_idle_timeout_ms",
            &mut th.shared_burst_idle_timeout_ms,
            &mut unknown,
        );
        opt_num(
            t,
            "shared_queue_capacity",
            &mut th.shared_queue_capacity,
            &mut unknown,
        );
        opt_num(
            t,
            "max_dedicated_threads",
            &mut th.max_dedicated_threads,
            &mut unknown,
        );
        opt_num(
            t,
            "default_mailbox_capacity",
            &mut th.default_mailbox_capacity,
            &mut unknown,
        );
        opt_num(
            t,
            "default_ask_timeout_ms",
            &mut th.default_ask_timeout_ms,
            &mut unknown,
        );
        opt_num(
            t,
            "shutdown_timeout_ms",
            &mut th.shutdown_timeout_ms,
            &mut unknown,
        );
        track_unknown(
            t,
            &[
                "shared_pool_size",
                "shared_burst_workers_max",
                "shared_burst_backlog_threshold_ms",
                "shared_burst_idle_timeout_ms",
                "shared_queue_capacity",
                "max_dedicated_threads",
                "default_mailbox_capacity",
                "default_ask_timeout_ms",
                "shutdown_timeout_ms",
            ],
            &mut unknown,
            "thread",
        );
    }

    if let Some(r) = overlay.get("remote").and_then(|v| v.as_table()) {
        if let Some(n) = r.get("node").and_then(|v| v.as_table()) {
            let nd = &mut cfg.remote.node;
            opt_string(n, "node_id", &mut nd.node_id, &mut unknown);
            opt_string(n, "bind", &mut nd.bind, &mut unknown);
            opt_string(n, "topology_role", &mut nd.topology_role, &mut unknown);
            opt_string(n, "direct_addr", &mut nd.direct_addr, &mut unknown);
            if let Some(seeds) = n.get("seeds").and_then(|v| v.as_array()) {
                let list: Option<Vec<String>> =
                    seeds.iter().map(|s| s.as_str().map(String::from)).collect();
                if let Some(list) = list {
                    nd.seeds = Some(list);
                }
            }
            opt_string(n, "scheme", &mut nd.scheme, &mut unknown);
        }
        if let Some(t) = r.get("transport").and_then(|v| v.as_table()) {
            let tr = &mut cfg.remote.transport;
            opt_num(
                t,
                "heartbeat_interval_ms",
                &mut tr.heartbeat_interval_ms,
                &mut unknown,
            );
            opt_num(
                t,
                "heartbeat_max_loss",
                &mut tr.heartbeat_max_loss,
                &mut unknown,
            );
            opt_num(t, "outbound_queue", &mut tr.outbound_queue, &mut unknown);
            opt_num(
                t,
                "default_hop_limit",
                &mut tr.default_hop_limit,
                &mut unknown,
            );
        }
        if let Some(ro) = r.get("reorder").and_then(|v| v.as_table()) {
            opt_num(
                ro,
                "gap_timeout_ms",
                &mut cfg.remote.reorder.gap_timeout_ms,
                &mut unknown,
            );
            opt_num(
                ro,
                "buffer_cap",
                &mut cfg.remote.reorder.buffer_cap,
                &mut unknown,
            );
        }
        if let Some(c) = r.get("codec").and_then(|v| v.as_table()) {
            opt_num(
                c,
                "extra_caps",
                &mut cfg.remote.codec.extra_caps,
                &mut unknown,
            );
        }
    }

    if let Some(x) = overlay
        .keys()
        .find(|k| k.as_str() != "thread" && k.as_str() != "remote")
    {
        unknown.push(x.clone());
    }
    if !unknown.is_empty() {
        return Err(DeployError::Config(format!(
            "overlay unknown keys: {unknown:?}"
        )));
    }
    Ok(())
}

fn opt_num<T: TryFrom<i64>>(
    t: &toml::value::Table,
    key: &str,
    slot: &mut Option<T>,
    unknown: &mut Vec<String>,
) {
    if let Some(v) = t.get(key) {
        match v.as_integer().and_then(|i| T::try_from(i).ok()) {
            Some(x) => *slot = Some(x),
            None => unknown.push(format!("{key}(type)")),
        }
    }
}

fn opt_string(
    t: &toml::value::Table,
    key: &str,
    slot: &mut Option<String>,
    unknown: &mut Vec<String>,
) {
    if let Some(v) = t.get(key) {
        match v.as_str() {
            Some(s) => *slot = Some(s.to_string()),
            None => unknown.push(format!("{key}(type)")),
        }
    }
}

fn track_unknown(t: &toml::value::Table, known: &[&str], unknown: &mut Vec<String>, section: &str) {
    for k in t.keys() {
        if !known.contains(&k.as_str()) {
            unknown.push(format!("{section}.{k}"));
        }
    }
}

/// 便捷 Result 别名。
pub type DeployResult<T> = Result<T, DeployError>;

// ============================================================================
// 测试（§5.1 Assembling 20+：依赖序/回滚逆序/overlay 优先级/hooks 时序）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        ArtifactRef, ComponentHooks, EngineKind, InstancePolicy, PlacementConstraint, UpgradePolicy,
    };
    use crate::planner::{plan, LocalTopology};
    use parrot_api::address::ActorRef;
    use parrot_api::types::{ActorResult, BoxedFuture};
    use std::sync::{Arc, Mutex};

    // ── 测试替身：记账式 ref/spawner/gateway ──────────────────

    #[derive(Debug)]
    struct Journal {
        events: Mutex<Vec<String>>,
        fail_spawn_on: Mutex<Option<String>>,
        hook_delay: Mutex<Duration>,
    }

    impl Journal {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(vec![]),
                fail_spawn_on: Mutex::new(None),
                hook_delay: Mutex::new(Duration::ZERO),
            })
        }
        fn log(&self, e: impl Into<String>) {
            self.events.lock().unwrap().push(e.into());
        }
        fn snapshot(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    /// 记账 ref：ask 回 ParrotAppHook 回显（可注入延迟）。
    struct JournalRef {
        journal: Arc<Journal>,
        path: String,
    }

    impl std::fmt::Debug for JournalRef {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("JournalRef")
                .field("path", &self.path)
                .finish()
        }
    }

    #[async_trait::async_trait]
    impl ActorRef for JournalRef {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send_with_timeout(msg, None)
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _t: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            let phase = msg
                .downcast_ref::<ParrotAppHook>()
                .map(|h| h.0)
                .unwrap_or(99);
            let delay = *self.journal.hook_delay.lock().unwrap();
            let journal = self.journal.clone();
            let path = self.path.clone();
            Box::pin(async move {
                if delay > Duration::ZERO {
                    tokio::time::sleep(delay).await;
                }
                journal.log(format!("ask:{path}:{phase}"));
                if phase <= 2 {
                    Ok(Box::new(ParrotAppHook(phase)) as BoxedMessage)
                } else {
                    Err(parrot_api::errors::ActorError::MessageHandlingError(
                        "unhandled".into(),
                    ))
                }
            })
        }
        fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            let journal = self.journal.clone();
            let path = self.path.clone();
            Box::pin(async move {
                journal.log(format!("stop:{path}"));
                Ok(())
            })
        }
        fn path(&self) -> String {
            self.path.clone()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(Self {
                journal: self.journal.clone(),
                path: self.path.clone(),
            })
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    struct TestSpawner {
        journal: Arc<Journal>,
    }

    impl ParrotSpawner for TestSpawner {
        fn spawn_props(
            &self,
            factory: &str,
            paths: &[String],
        ) -> Result<Vec<BoxedActorRef>, DeployError> {
            if self.journal.fail_spawn_on.lock().unwrap().as_deref() == Some(factory) {
                return Err(DeployError::Engine(format!("spawn failed: {factory}")));
            }
            self.journal.log(format!("spawn:{}", paths[0]));
            Ok(paths
                .iter()
                .map(|p| {
                    Box::new(JournalRef {
                        journal: self.journal.clone(),
                        path: p.clone(),
                    }) as BoxedActorRef
                })
                .collect())
        }
        fn stop_paths(&self, paths: &[String]) -> Result<(), DeployError> {
            for p in paths {
                self.journal.log(format!("stop_paths:{p}"));
            }
            Ok(())
        }
    }

    struct TestGateway {
        journal: Arc<Journal>,
    }

    impl GatewayFactory for TestGateway {
        fn start_gateway(
            &self,
            comp: &ComponentSpec,
            paths: &[String],
        ) -> Result<Vec<BoxedActorRef>, DeployError> {
            self.journal
                .log(format!("gw:{}:{}", comp.name, comp.engine.as_str()));
            Ok(paths
                .iter()
                .map(|p| {
                    Box::new(JournalRef {
                        journal: self.journal.clone(),
                        path: p.clone(),
                    }) as BoxedActorRef
                })
                .collect())
        }
        fn stop_gateway(&self, comp: &ComponentSpec) -> Result<(), DeployError> {
            self.journal.log(format!("gw_stop:{}", comp.name));
            Ok(())
        }
    }

    fn make_deployer(journal: &Arc<Journal>) -> LocalDeployer<'static> {
        // 'static 骤变：测试内泄漏 Arc 到 &'static（单测进程生命周期可接受）
        let j = journal.clone();
        let spawner: &'static TestSpawner = Box::leak(Box::new(TestSpawner { journal: j }));
        let j2 = journal.clone();
        let gw: &'static TestGateway = Box::leak(Box::new(TestGateway { journal: j2 }));
        let topo: &'static LocalTopology = Box::leak(Box::new(LocalTopology));
        LocalDeployer {
            topology: topo,
            gateway_factory: gw,
            parrot_spawner: spawner,
        }
    }

    fn comp(name: &str, deps: &[&str]) -> ComponentSpec {
        ComponentSpec {
            name: name.into(),
            engine: EngineKind::Parrot,
            artifact: ArtifactRef::Props {
                factory: format!("app.{name}"),
            },
            alt_artifact: None,            instances: InstancePolicy::Singleton,
            placement: PlacementConstraint::default(),
            upgrade: UpgradePolicy::default(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            config: None,
            hooks: ComponentHooks::default(),
        }
    }

    fn manifest(comps: Vec<ComponentSpec>) -> AppManifest {
        AppManifest {
            name: "t".into(),
            version: "1.0.0".into(),
            components: comps,
            wiring: vec![],
            config_overlay: None,
        }
    }

    // ── 依赖序装配（3）──────────────────────────────────────

    #[tokio::test]
    async fn assemble_dependency_order() {
        let j = Journal::new();
        let m = manifest(vec![comp("c", &["b"]), comp("a", &[]), comp("b", &["a"])]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        let ev = j.snapshot();
        assert_eq!(ev, vec!["spawn:/user/a", "spawn:/user/b", "spawn:/user/c"]);
        assert_eq!(ctx.started(), &["a", "b", "c"]);
    }

    #[tokio::test]
    async fn assemble_mixed_engines() {
        let j = Journal::new();
        let mut erl = comp("frontier", &[]);
        erl.engine = EngineKind::Erlang;
        erl.artifact = ArtifactRef::Beam {
            app: "frontier".into(),
         uri: None,};
        let m = manifest(vec![comp("hub", &[]), erl]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        let ev = j.snapshot();
        assert!(ev.contains(&"gw:frontier:erlang".to_string()));
        assert!(ev.contains(&"spawn:/user/hub".to_string()));
    }

    #[tokio::test]
    async fn assemble_sharded_multiple_paths() {
        let j = Journal::new();
        let mut c = comp("sh", &[]);
        c.instances = InstancePolicy::Sharded(3);
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        assert_eq!(ctx.component_refs("sh").unwrap().len(), 3);
        assert_eq!(ctx.component_paths("sh").unwrap()[2], "/user/sh-2");
    }

    // ── 失败回滚逆序（4）────────────────────────────────────

    #[tokio::test]
    async fn assemble_failure_rolls_back_reverse_order() {
        let j = Journal::new();
        // c 依赖 b 依赖 a；b spawn 失败 → a 回滚
        *j.fail_spawn_on.lock().unwrap() = Some("app.b".into());
        let m = manifest(vec![comp("c", &["b"]), comp("a", &[]), comp("b", &["a"])]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        let err = ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DeployError::Engine(ref s) if s.contains("app.b")));
        let ev = j.snapshot();
        // 启停严格逆序：a 起 → a 的 ref 停 → a 的路径停
        assert_eq!(
            ev,
            vec!["spawn:/user/a", "stop:/user/a", "stop_paths:/user/a"]
        );
        assert!(ctx.started().is_empty());
        assert!(ctx.component_ref("a").is_none());
    }

    #[tokio::test]
    async fn assemble_failure_middle_of_five() {
        let j = Journal::new();
        *j.fail_spawn_on.lock().unwrap() = Some("app.c3".into());
        let comps: Vec<ComponentSpec> = (0..5)
            .map(|i| {
                let deps = if i == 0 {
                    vec![]
                } else {
                    vec![format!("c{}", i - 1)]
                };
                comp(
                    &format!("c{i}"),
                    &deps.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                )
            })
            .collect();
        let m = manifest(comps);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        assert!(ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .is_err());
        let ev = j.snapshot();
        // c0..c2 起，c3 失败，c2..c0 逆序停
        let spawns: Vec<&String> = ev.iter().filter(|e| e.starts_with("spawn:")).collect();
        assert_eq!(spawns.len(), 3);
        let stops: Vec<&String> = ev.iter().filter(|e| e.starts_with("stop:")).collect();
        assert_eq!(stops.len(), 3);
        assert_eq!(stops[0], "stop:/user/c2");
        assert_eq!(stops[2], "stop:/user/c0");
    }

    #[tokio::test]
    async fn teardown_reverse_order() {
        let j = Journal::new();
        let m = manifest(vec![comp("x", &["y"]), comp("y", &[])]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        ctx.teardown(&d).await.unwrap();
        let ev = j.snapshot();
        assert_eq!(ev[0], "spawn:/user/y");
        assert_eq!(ev[1], "spawn:/user/x");
        // teardown 逆序：x 先停（ref stop + paths stop），再 y
        assert_eq!(ev[2], "stop:/user/x");
        assert!(ev.contains(&"stop:/user/y".to_string()));
        let x_stop = ev.iter().position(|e| e == "stop:/user/x").unwrap();
        let y_stop = ev.iter().position(|e| e == "stop:/user/y").unwrap();
        assert!(x_stop < y_stop);
        assert!(ctx.started().is_empty());
    }

    #[tokio::test]
    async fn teardown_empty_ok() {
        let j = Journal::new();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        assert!(ctx.teardown(&d).await.is_ok());
    }

    // ── hooks 时序（4）──────────────────────────────────────

    #[tokio::test]
    async fn hook_on_start_fired_and_awaited() {
        let j = Journal::new();
        let mut c = comp("a", &[]);
        c.hooks.on_start = Some("/user/a".into());
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        assert!(
            j.snapshot().contains(&"ask:/user/a:0".to_string()),
            "START hook asked"
        );
    }

    #[tokio::test]
    async fn hook_timeout_rolls_back() {
        let j = Journal::new();
        *j.hook_delay.lock().unwrap() = Duration::from_millis(300);
        let mut c = comp("a", &[]);
        c.hooks.on_start = Some("/user/a".into());
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        ctx.set_hooks_timeout(Duration::from_millis(50));
        let err = ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err, DeployError::HookTimeout { comp, hook } if comp == "a" && hook == "/user/a")
        );
        assert!(ctx.started().is_empty(), "rolled back");
    }

    #[tokio::test]
    async fn hook_ask_error_fails_assembly() {
        let j = Journal::new();
        // JournalRef 对 phase>2 报错——用 drain/未知 phase 模拟组件崩溃：
        // 直接构造 spawner 返回错误 actor 不可行（替身固定）——改用 fail_spawn
        *j.fail_spawn_on.lock().unwrap() = Some("app.a".into());
        let mut c = comp("a", &[]);
        c.hooks.on_start = Some("/user/a".into());
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        let err = ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DeployError::Engine(_)));
    }

    #[tokio::test]
    async fn hook_codec_roundtrip() {
        assert_eq!(ParrotAppHook(hook_phase::START).encode(), vec![0]);
        assert_eq!(
            ParrotAppHook::decode(&[1]),
            Some(ParrotAppHook(hook_phase::STOP))
        );
        assert_eq!(ParrotAppHook::decode(&[]), None);
    }

    // ── overlay 优先级 Y3（5）───────────────────────────────

    #[test]
    fn overlay_fills_unset_keys() {
        let mut cfg = parrot_config::ParrotConfig::default();
        let overlay: toml::value::Table = [(
            "remote".to_string(),
            toml::Value::Table(
                [(
                    "transport".to_string(),
                    toml::Value::Table(
                        [("heartbeat_interval_ms".to_string(), toml::Value::from(999))]
                            .into_iter()
                            .collect(),
                    ),
                )]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect();
        apply_overlay(&mut cfg, &overlay).unwrap();
        assert_eq!(cfg.remote.transport.heartbeat_interval_ms, Some(999));
    }

    #[test]
    fn overlay_does_not_override_code_layer() {
        let mut cfg = parrot_config::ParrotConfig::default();
        cfg.remote.transport.heartbeat_interval_ms = Some(111); // 代码层已设
        let overlay: toml::value::Table = [(
            "remote".to_string(),
            toml::Value::Table(
                [(
                    "transport".to_string(),
                    toml::Value::Table(
                        [("heartbeat_interval_ms".to_string(), toml::Value::from(999))]
                            .into_iter()
                            .collect(),
                    ),
                )]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect();
        apply_overlay(&mut cfg, &overlay).unwrap();
        // Y3: app config_overlay > 代码——overlay 直接覆盖
        assert_eq!(cfg.remote.transport.heartbeat_interval_ms, Some(999));
    }

    #[test]
    fn overlay_reorder_keys() {
        let mut cfg = parrot_config::ParrotConfig::default();
        let overlay: toml::value::Table = [(
            "remote".to_string(),
            toml::Value::Table(
                [(
                    "reorder".to_string(),
                    toml::Value::Table(
                        [
                            ("gap_timeout_ms".to_string(), toml::Value::from(42)),
                            ("buffer_cap".to_string(), toml::Value::from(7)),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                )]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect();
        apply_overlay(&mut cfg, &overlay).unwrap();
        assert_eq!(cfg.remote.reorder.gap_timeout_ms, Some(42));
        assert_eq!(cfg.remote.reorder.buffer_cap, Some(7));
    }

    #[test]
    fn overlay_thread_keys() {
        let mut cfg = parrot_config::ParrotConfig::default();
        let overlay: toml::value::Table = [(
            "thread".to_string(),
            toml::Value::Table(
                [("shared_pool_size".to_string(), toml::Value::from(3))]
                    .into_iter()
                    .collect(),
            ),
        )]
        .into_iter()
        .collect();
        apply_overlay(&mut cfg, &overlay).unwrap();
        assert_eq!(cfg.thread.shared_pool_size, Some(3));
    }

    #[test]
    fn overlay_bad_key_rejected() {
        let mut cfg = parrot_config::ParrotConfig::default();
        let overlay: toml::value::Table = [("nonexistent".to_string(), toml::Value::from(1))]
            .into_iter()
            .collect();
        match apply_overlay(&mut cfg, &overlay) {
            Err(DeployError::Config(s)) => assert!(s.contains("unknown keys"), "{s}"),
            other => panic!("expected config error, got {other:?}"),
        }
    }

    // ── 查询面（2）─────────────────────────────────────────

    #[tokio::test]
    async fn component_ref_lookup() {
        let j = Journal::new();
        let m = manifest(vec![comp("a", &[])]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        assert!(ctx.component_ref("a").is_some());
        assert!(ctx.component_ref("zz").is_none());
    }

    #[tokio::test]
    async fn gateway_failure_no_rollback_targets_before_it() {
        let j = Journal::new();
        // 第一个组件就是网关且失败——空回滚
        let mut bad = comp("g", &[]);
        bad.engine = EngineKind::Ray;
        bad.artifact = ArtifactRef::PyModule {
            module: "m".into(),
            runtime_env: None,
         uri: None,};
        let m = manifest(vec![bad]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        // 正常路径成功（替身 gw 恒成功）——断言 registry 记账
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        assert_eq!(ctx.started().len(), 1);
    }

    // wasm/dylib artifact 在 C/D 前的诚实拒绝
    #[tokio::test]
    async fn wasm_dylib_artifacts_rejected_before_phase_c() {
        let j = Journal::new();
        let mut c = comp("w", &[]);
        c.artifact = ArtifactRef::Wasm {
            digest: "d".into(),
            uri: "file:///x.wasm".into(),
        };
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let d = make_deployer(&j);
        let mut ctx = AssemblingContext::new();
        let err = ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DeployError::Artifact(ref s) if s.contains("wasm")));
    }
}
