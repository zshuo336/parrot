//! parrot-node：标准单节点运行时进程（部署环境最小单元）。
//!
//! 一个进程 = 一个 Parrot 节点（生产部署形态）：
//! - 真实 ThreadActorSystem 引擎（非桩——消息经真实邮箱/调度器/监督）
//! - RemoteActorSystem（TCP listen + 可选 seeds 组网）
//! - K0 admin PropsFactory（远程 spawn：deploy.echo / deploy.counter /
//!   deploy.kv / deploy.slow）
//! - facade 三级路由（本地 default → 本地遍历 → parrot:// 远程出口）
//!
//! 配置（环境变量，缺省安全）：
//!   PARROT_NODE_ID     节点名（缺省 "node-1"）
//!   PARROT_BIND        监听地址（缺省 0.0.0.0:9801）
//!   PARROT_SEEDS       逗号分隔种子 "node_id=host:port"（缺省空——单节点）
//!   PARROT_ACTORS      逗号分配置于 spawn 的内置 actor（缺省 echo,counter,kv,slow）
//!   PARROT_READY_FILE  就绪后写入实际监听地址的文件（缺省不打）
//!
//! 就绪协议（编排器契约）：stdout 第一行 `PARROT_NODE_READY=<bind_addr>`。
//! Docker healthcheck：`PARROT_HEALTHCHECK=1`——TCP 连自身监听端口即健康
//! （比进程存活探针强：能证明 accept 循环在收连接）。

use std::sync::Arc;
use std::time::Duration;

use parrot::system::{ParrotActorSystem, RemoteGateway};
use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::system::ActorSystem;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use parrot_remote::admin::PropsFactory;
use parrot_remote::{LocalLookup, NodeAddr};

// ══════════════════════════════════════════════════════════════════════════
// 消息
// ══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NEcho(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NEchoed(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NInc(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NGetTotal;
/// u64 计数回复（counter total / kv size 共用）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NTotal(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NPut(pub String, pub String);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NGet(pub String);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NGot(pub Option<String>);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NSlowEcho(pub u64);

// ══════════════════════════════════════════════════════════════════════════
// Actor 族（真实引擎 actor）
// ══════════════════════════════════════════════════════════════════════════

/// 回声：NEcho(v) → NEchoed(v)。
pub struct EchoActor;

impl Actor for EchoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(e) = msg.downcast_ref::<NEcho>() {
                return Ok(Box::new(NEchoed(e.0)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "echo: unhandled".into(),
            ))
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// 计数器：NInc(v) 累加 / NGetTotal → NTotal。
#[derive(Default)]
pub struct CounterActor {
    total: u64,
}

impl Actor for CounterActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(i) = msg.downcast_ref::<NInc>() {
                self.total += i.0;
                return Ok(Box::new(NTotal(self.total)) as BoxedMessage);
            }
            if msg.downcast_ref::<NGetTotal>().is_some() {
                return Ok(Box::new(NTotal(self.total)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "counter: unhandled".into(),
            ))
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// KV：NPut/NGet/NGetTotal(→size)。
#[derive(Default)]
pub struct KvActor {
    map: std::collections::BTreeMap<String, String>,
}

impl Actor for KvActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(p) = msg.downcast_ref::<NPut>() {
                self.map.insert(p.0.clone(), p.1.clone());
                return Ok(Box::new(NGot(Some(p.1.clone()))) as BoxedMessage);
            }
            if let Some(g) = msg.downcast_ref::<NGet>() {
                return Ok(Box::new(NGot(self.map.get(&g.0).cloned())) as BoxedMessage);
            }
            if msg.downcast_ref::<NGetTotal>().is_some() {
                return Ok(Box::new(NTotal(self.map.len() as u64)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "kv: unhandled".into(),
            ))
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// 慢处理：NSlowEcho(ms) 睡后回——验证慢 actor 不阻塞其它 actor 的并发性。
pub struct SlowActor;

impl Actor for SlowActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(s) = msg.downcast_ref::<NSlowEcho>() {
                tokio::time::sleep(Duration::from_millis(s.0)).await;
                return Ok(Box::new(NEchoed(s.0)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "slow: unhandled".into(),
            ))
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ══════════════════════════════════════════════════════════════════════════
// codec 注册（inventory——跨进程 wire 解码必需；两侧进程同 registry）
// ══════════════════════════════════════════════════════════════════════════

macro_rules! reg {
    ($t:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or(concat!("downcast ", $key))?;
                    parrot_api::message::serde_remote_serialize(&m)
                },
                decode: |b: &[u8]| {
                    let v: $t = parrot_api::message::serde_remote_deserialize(b)?;
                    Ok(Box::new(v) as BoxedMessage)
                },
            }
        }
    };
}

reg!(NEcho, "bin:deploy::NEcho#v1");
reg!(NEchoed, "bin:deploy::NEchoed#v1");
reg!(NInc, "bin:deploy::NInc#v1");
reg!(NGetTotal, "bin:deploy::NGetTotal#v1");
reg!(NTotal, "bin:deploy::NTotal#v1");
reg!(NPut, "bin:deploy::NPut#v1");
reg!(NGet, "bin:deploy::NGet#v1");
reg!(NGot, "bin:deploy::NGot#v1");
reg!(NSlowEcho, "bin:deploy::NSlowEcho#v1");

// ══════════════════════════════════════════════════════════════════════════
// 进程全局（工厂 fn 指针经静态取 facade/ts）
// ══════════════════════════════════════════════════════════════════════════

/// 进程组装态（facade + 引擎句柄——PropsFactory fn 指针经静态取用）。
pub struct NodeState {
    pub facade: Arc<ParrotActorSystem>,
    pub ts: Arc<ThreadActorSystem>,
}

/// 进程单例（bin main 组装后写入）。
pub static NODE: std::sync::OnceLock<NodeState> = std::sync::OnceLock::new();

pub(crate) fn node() -> &'static NodeState {
    NODE.get().expect("parrot-node: state not initialized")
}

/// 远程 spawn echo：fn 指针形态（PropsFactory 契约）。
fn spawn_remote_echo(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    spawn_on_engine(path, EchoActor)
}
fn spawn_remote_counter(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    spawn_on_engine(path, CounterActor::default())
}
fn spawn_remote_kv(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    spawn_on_engine(path, KvActor::default())
}
fn spawn_remote_slow(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    spawn_on_engine(path, SlowActor)
}

fn spawn_on_engine<A>(path: &str, actor: A) -> BoxedFuture<'static, ActorResult<BoxedActorRef>>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    let path = path.to_string();
    Box::pin(async move {
        let r = node()
            .ts
            .spawn_at(actor, &path, None, ThreadActorConfig::default())
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(format!("spawn: {e}")))?;
        Ok(Box::new(r) as BoxedActorRef)
    })
}

parrot_api::message::inventory::submit! {
    PropsFactory {
        name: "deploy.echo",
        spawn: spawn_remote_echo,
    }
}
parrot_api::message::inventory::submit! {
    PropsFactory {
        name: "deploy.counter",
        spawn: spawn_remote_counter,
    }
}
parrot_api::message::inventory::submit! {
    PropsFactory {
        name: "deploy.kv",
        spawn: spawn_remote_kv,
    }
}
parrot_api::message::inventory::submit! {
    PropsFactory {
        name: "deploy.slow",
        spawn: spawn_remote_slow,
    }
}

// ══════════════════════════════════════════════════════════════════════════
// LocalLookup：远程入站帧 → facade 本地三级路由
// ══════════════════════════════════════════════════════════════════════════

/// 远程入站 → facade（三级路由出口）。
pub struct FacadeLookup;

#[async_trait::async_trait]
impl LocalLookup for FacadeLookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        node()
            .facade
            .get_actor(&parrot_api::address::ActorPath::placeholder(path))
            .await
    }
}

/// RemoteGateway 适配器（parrot::RemoteGateway ← parrot-remote RemoteGatewayImpl）。
pub struct GatewayAdapter(pub parrot_remote::system::RemoteGatewayImpl);

impl RemoteGateway for GatewayAdapter {
    fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        self.0.lookup(path)
    }
}

// ══════════════════════════════════════════════════════════════════════════
// B2（DEV_09 §5.1）：admin-v2 parrot Executor
//   ArtifactChannel（uri→本地 + sha256）+ exec_deploy_v2（Props→工厂）+
//   NodeComponentExecutor（四命令接线）+ caps ARTIFACTS。
// ══════════════════════════════════════════════════════════════════════════

pub mod executor_v2 {
    //! admin-v2 组件执行器（parrot 方言）。
    //!
    //! 职责边界：
    //! - `ArtifactChannel`：artifact uri → 本地缓存（file:// 首期；http 后续）
    //!   + sha256 校验。Wasm/Dylib 形态走同一条通道（C/D 阶段消费）。
    //! - `exec_deploy_v2`：Props artifact → `find_factory` 按名 spawn
    //!   （复用 K0 inventory 机制）；多实例按 `/user/{name}[-{i}]` 展开。
    //! - `NodeComponentExecutor`：impl `ComponentExecutor`——Deploy/Drain/
    //!   Stop/Status 四命令落到 thread 引擎 registry。

    use std::sync::Arc;

    use parrot::thread::system::ThreadActorSystem;
    use parrot_remote::admin::find_factory;
    use parrot_remote::admin_v2::{
        failed_v2, v2_err, AdminArtifactRef, AdminCommandV2, AdminInstancePolicy, AdminReplyV2,
        AdminReplyV2 as R, ComponentDeploy, ComponentExecutor, ComponentStateReport,
        MetricsSnapshot,
    };

    // ─────────────────────────────────────────────────────────────
    // ArtifactChannel
    // ─────────────────────────────────────────────────────────────

    /// artifact 获取/校验通道（目标节点侧）。
    ///
    /// 首期仅 `file://` 与裸路径（同机/挂载卷语义）；http(s) 后续阶段接入。
    /// Props 形态无 artifact 传输需求（工厂已在进程内注册），直接放行。
    #[derive(Debug, Clone, Default)]
    pub struct ArtifactChannel;

    impl ArtifactChannel {
        /// 缓存根目录：`$PARROT_ARTIFACT_DIR` 或 `/tmp/parrot-artifacts`。
        pub fn cache_dir() -> std::path::PathBuf {
            std::env::var_os("PARROT_ARTIFACT_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp/parrot-artifacts"))
        }

        /// uri → 本地路径（含校验）。`file://` / 裸路径 / `Props` 直通。
        ///
        /// Wasm/Dylib 带 digest → fetch 后 `verify`；不匹配按
        /// `v2_err::ARTIFACT_DIGEST` 拒收（调用方映射回执码）。
        pub fn fetch(
            &self,
            artifact: &AdminArtifactRef,
        ) -> Result<std::path::PathBuf, (u16, String)> {
            match artifact {
                // Props 工厂已在进程内注册（inventory）——无 artifact 传输
                AdminArtifactRef::Props { .. } => Ok(std::path::PathBuf::new()),
                AdminArtifactRef::Wasm { digest, uri }
                | AdminArtifactRef::Dylib { digest, uri, .. } => {
                    let local = Self::materialize(uri).map_err(|e| (v2_err::ARTIFACT_FETCH, e))?;
                    Self::verify(&local, digest).map_err(|e| (v2_err::ARTIFACT_DIGEST, e))?;
                    Ok(local)
                }
                // 非本方言 artifact：Executor 侧应在路由前拒绝（DIALECT_MISMATCH）
                AdminArtifactRef::Beam { .. }
                | AdminArtifactRef::PyModule { .. }
                | AdminArtifactRef::Jvm { .. } => Err((
                    v2_err::DIALECT_MISMATCH,
                    format!("parrot executor cannot fetch {artifact:?}"),
                )),
            }
        }

        /// uri → 本地路径。`file://host/path` 与 `file:///path` 均取 path 段；
        /// 裸路径直取。非 file 协议首期报错（http 阶段接入）。
        fn materialize(uri: &str) -> Result<std::path::PathBuf, String> {
            if let Some(rest) = uri.strip_prefix("file://") {
                // file://host/path → 跳过 host 段（localhost 语义）；空 host
                // （file:///path）rest 首字符即 '/'
                let p = if rest.starts_with('/') {
                    rest
                } else {
                    rest.split_once('/').map(|(_, path)| path).unwrap_or(rest)
                };
                let path = std::path::PathBuf::from(format!("/{p}"));
                if path.is_file() {
                    return Ok(path);
                }
                return Err(format!("file uri not found: {uri}"));
            }
            // 裸路径（同机语义）
            let path = std::path::PathBuf::from(uri);
            if uri.starts_with('/') && path.is_file() {
                return Ok(path);
            }
            Err(format!("unsupported or missing artifact uri: {uri}"))
        }

        /// sha256 校验：`sha256:<hex64>` 或裸 hex64；空 digest 跳过（测试便利）。
        pub fn verify(path: &std::path::Path, digest: &str) -> Result<(), String> {
            if digest.is_empty() {
                return Ok(());
            }
            use sha2::{Digest, Sha256};
            let bytes = std::fs::read(path).map_err(|e| format!("read {path:?}: {e}"))?;
            let actual = hex::encode(Sha256::digest(&bytes));
            let expect = digest
                .strip_prefix("sha256:")
                .unwrap_or(digest)
                .to_ascii_lowercase();
            if actual != expect {
                return Err(format!(
                    "sha256 mismatch: want {expect}, got {actual} ({path:?})"
                ));
            }
            Ok(())
        }
    }

    // ─────────────────────────────────────────────────────────────
    // deploy 核心（Props → find_factory）
    // ─────────────────────────────────────────────────────────────

    /// 实例路径展开（与 parrot-app assemble 同规）：
    /// Singleton → `/user/{name}`；Pool/Sharded(n) → `/user/{name}-{i}`。
    pub fn instance_paths(name: &str, policy: AdminInstancePolicy) -> Vec<String> {
        match policy {
            AdminInstancePolicy::Singleton => vec![format!("/user/{name}")],
            AdminInstancePolicy::Pool { count } | AdminInstancePolicy::Sharded { count } => {
                (0..count).map(|i| format!("/user/{name}-{i}")).collect()
            }
        }
    }

    /// DeployComponent 执行（Props 方言核心）。
    ///
    /// 非 Props artifact → DIALECT_MISMATCH（ray/erl/jvm 网关各自处理）；
    /// 工厂未注册 → FACTORY_NOT_FOUND；任一实例 spawn 失败 → SPAWN_FAILED
    /// （已 spawn 的实例保留——StopComponent 按前缀清理，语义为"尽力而为"）。
    ///
    /// `ts` 必须是进程单例 `NODE` 的引擎（PropsFactory fn 指针经静态取
    /// 引擎 spawn——K0 机制既定形态；不一致说明装配错误，按 SPAWN_FAILED 拒）。
    pub async fn exec_deploy_v2(
        ts: &Arc<ThreadActorSystem>,
        cmd: ComponentDeploy,
    ) -> Result<Vec<String>, (u16, String)> {
        let factory_name = match &cmd.artifact {
            AdminArtifactRef::Props { factory } => factory.clone(),
            other => {
                return Err((
                    v2_err::DIALECT_MISMATCH,
                    format!("parrot executor expects Props artifact, got {other:?}"),
                ))
            }
        };
        // artifact 通道统一过（Props 形态零成本直通——为 Wasm/Dylib 预留同路）
        ArtifactChannel.fetch(&cmd.artifact)?;
        // 装配一致性：PropsFactory 经 NODE 单例取引擎——ts 必须同源
        let node_ts_ok = crate::NODE
            .get()
            .map(|n| Arc::<ThreadActorSystem>::ptr_eq(&n.ts, ts))
            .unwrap_or(false);
        if !node_ts_ok {
            return Err((
                v2_err::SPAWN_FAILED,
                "engine mismatch: PropsFactory spawns on NODE singleton".into(),
            ));
        }
        let factory = find_factory(&factory_name).ok_or_else(|| {
            (
                v2_err::FACTORY_NOT_FOUND,
                format!("PropsFactory '{factory_name}' not registered"),
            )
        })?;
        let paths = instance_paths(&cmd.name, cmd.instances);
        let mut spawned = Vec::with_capacity(paths.len());
        for p in &paths {
            match (factory.spawn)(p).await {
                Ok(_) => spawned.push(p.clone()),
                Err(e) => {
                    return Err((
                        v2_err::SPAWN_FAILED,
                        format!("spawn {p} failed: {e:?} (spawned {})", spawned.len()),
                    ))
                }
            }
        }
        Ok(spawned)
    }

    // ─────────────────────────────────────────────────────────────
    // NodeComponentExecutor（ComponentExecutor 实现）
    // ─────────────────────────────────────────────────────────────

    /// parrot 节点组件执行器：Deploy/Drain/Stop/Status → thread 引擎。
    pub struct NodeComponentExecutor {
        /// thread 引擎（spawn/stop/registry 查询）。
        ts: Arc<ThreadActorSystem>,
        /// 组件版本登记（Deploy 写入 / Status 报告 / Stop 清除）。
        versions: std::sync::Mutex<std::collections::HashMap<String, String>>,
    }

    impl NodeComponentExecutor {
        pub fn new(ts: Arc<ThreadActorSystem>) -> Self {
            Self {
                ts,
                versions: std::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }

        /// 前缀匹配 registry 存活实例（`/user/comp` 前缀不误吞
        /// `/user/comp2`——要求整段相等或后随 `-`）。
        fn matching_paths(&self, prefix: &str) -> Vec<String> {
            self.ts
                .actor_paths()
                .into_iter()
                .filter(|p| {
                    p == prefix
                        || (p.starts_with(prefix) && p.as_bytes().get(prefix.len()) == Some(&b'-'))
                })
                .collect()
        }

        /// 路径 → 组件名（`/user/{name}` 或 `/user/{name}-{i}`）。
        fn path_to_component(path: &str) -> String {
            let rest = path.strip_prefix("/user/").unwrap_or(path);
            // `-` 也可能是组件名自带字符：优先按已知版本表反查（status
            // 已有路径集合；此处统一取最长匹配）。首版规则：rsplitonce
            // 取 `-` 前段（多实例命名约定优先），无 `-` 整段即名。
            match rest.rsplit_once('-') {
                Some((name, tail))
                    if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) =>
                {
                    name.to_string()
                }
                _ => rest.to_string(),
            }
        }

        /// 清除路径集合覆盖的组件版本登记（实例全停后版本未知）。
        fn forget_versions(&self, paths: &[String]) {
            let comps: std::collections::HashSet<String> =
                paths.iter().map(|p| Self::path_to_component(p)).collect();
            self.versions
                .lock()
                .unwrap()
                .retain(|k, _| !comps.contains(k));
        }
    }

    #[async_trait::async_trait]
    impl ComponentExecutor for NodeComponentExecutor {
        async fn deploy(&self, req_id: u64, c: &ComponentDeploy) -> AdminReplyV2 {
            match exec_deploy_v2(&self.ts, c.clone()).await {
                Ok(instances) => {
                    self.versions
                        .lock()
                        .unwrap()
                        .insert(c.name.clone(), c.version.clone());
                    R::Deployed { req_id, instances }
                }
                Err((code, detail)) => failed_v2(req_id, code, detail),
            }
        }

        async fn drain(&self, req_id: u64, prefix: &str, timeout_ms: u64) -> AdminReplyV2 {
            let paths = self.matching_paths(prefix);
            if paths.is_empty() {
                return failed_v2(
                    req_id,
                    v2_err::COMPONENT_NOT_FOUND,
                    format!("no instances under prefix {prefix}"),
                );
            }
            // 排空语义：逐实例等邮箱清空（在途消息处理完）再优雅停；
            // deadline 共享（总预算 = timeout_ms，单实例不独占）。
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_millis(timeout_ms.min(60_000));
            let mut drained = 0usize;
            let mut aborted = 0usize;
            for p in &paths {
                let mb_empty = match self.ts.get_mailbox(p) {
                    Some(mb) => loop {
                        if mb.is_empty().await {
                            break true;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            break false;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    },
                    None => false,
                };
                if mb_empty && self.ts.stop_actor(p).await.is_ok() {
                    drained += 1;
                } else {
                    // 超时/邮箱无法排空 → 中止（保留实例——发起方决定 Stop 强停）
                    aborted += 1;
                }
            }
            if aborted == 0 {
                self.forget_versions(&paths);
            }
            R::Drained {
                req_id,
                drained,
                aborted,
            }
        }

        async fn stop(&self, req_id: u64, prefix: &str) -> AdminReplyV2 {
            let paths = self.matching_paths(prefix);
            if paths.is_empty() {
                return failed_v2(
                    req_id,
                    v2_err::COMPONENT_NOT_FOUND,
                    format!("no instances under prefix {prefix}"),
                );
            }
            for p in &paths {
                let _ = self.ts.stop_actor(p).await;
            }
            self.forget_versions(&paths);
            R::Stopped { req_id }
        }

        async fn status(&self, req_id: u64, prefix: &str) -> AdminReplyV2 {
            let paths = self.matching_paths(prefix);
            if paths.is_empty() {
                return failed_v2(
                    req_id,
                    v2_err::COMPONENT_NOT_FOUND,
                    format!("no instances under prefix {prefix}"),
                );
            }
            let versions = self.versions.lock().unwrap().clone();
            let states: Vec<ComponentStateReport> = paths
                .into_iter()
                .map(|p| {
                    let name = Self::path_to_component(&p);
                    ComponentStateReport {
                        version: versions.get(&name).cloned().unwrap_or_default(),
                        state: "running".into(),
                        path: p,
                    }
                })
                .collect();
            R::Status { req_id, states }
        }

        /// 观测五件套：本地引擎指标快照（组件登记表 + actor 计数）。
        async fn metrics(&self, req_id: u64) -> AdminReplyV2 {
            use std::time::{SystemTime, UNIX_EPOCH};
            let versions = self.versions.lock().unwrap().clone();
            let states: Vec<ComponentStateReport> = versions
                .iter()
                .flat_map(|(name, ver)| {
                    let prefix = format!("/user/{name}");
                    self.matching_paths(&prefix)
                        .into_iter()
                        .map(move |p| ComponentStateReport {
                            version: ver.clone(),
                            state: "running".into(),
                            path: p,
                        })
                })
                .collect();
            let snap = MetricsSnapshot {
                ts_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                runtime: format!("parrot/{}", env!("CARGO_PKG_VERSION")),
                connections: 1, // 进程内 executor——单一宿主
                handshakes_ok: 1,
                components: versions.len() as u64,
                component_states: states,
                processes: self.ts.actor_count() as u64,
                uptime_start_ms: 0,
                ..Default::default()
            };
            R::Metrics {
                req_id,
                snapshot: snap,
            }
        }
    }

    /// 命令直通辅助（handle_admin_command_v2 之外的进程内入口——
    /// 测试与 bin 安装共用；回包已含 req_id）。
    pub async fn exec_command_v2(ts: &Arc<ThreadActorSystem>, cmd: AdminCommandV2) -> AdminReplyV2 {
        let ex = NodeComponentExecutor::new(ts.clone());
        match cmd {
            AdminCommandV2::DeployComponent { req_id, component } => {
                ex.deploy(req_id, &component).await
            }
            AdminCommandV2::DrainComponent {
                req_id,
                path_prefix,
                timeout_ms,
            } => ex.drain(req_id, &path_prefix, timeout_ms).await,
            AdminCommandV2::StopComponent {
                req_id,
                path_prefix,
            } => ex.stop(req_id, &path_prefix).await,
            AdminCommandV2::ComponentStatus {
                req_id,
                path_prefix,
            } => ex.status(req_id, &path_prefix).await,
            // 观测五件套：指标快照（ThreadActorSystem 方言——本地引擎状态）
            AdminCommandV2::MetricsReport { req_id } => ex.metrics(req_id).await,
        }
    }
}

pub use executor_v2::{exec_command_v2, exec_deploy_v2, ArtifactChannel, NodeComponentExecutor};

// ══════════════════════════════════════════════════════════════════════════
// C 阶段（DEV_09 §3.3）：WasmActor——wasm 组件的引擎形态接线
// ══════════════════════════════════════════════════════════════════════════

/// wasm actor 消息（与 RemoteEnvelope 同构：type_key + bytes——codec_registry 惯例）。
#[cfg(feature = "wasm")]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WasmAsk {
    pub type_key: String,
    pub payload: Vec<u8>,
}

/// wasm actor 回复（handle 出参原样 bytes）。
#[cfg(feature = "wasm")]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WasmReply(pub Vec<u8>);

#[cfg(feature = "wasm")]
impl parrot_api::message::Message for WasmAsk {
    type Result = WasmReply;
}

/// wasm 组件 actor：每实例包一个 `WasmComponent`（fuel/epoch 沙箱内
/// 执行 handle/tell）。消息 `WasmAsk` → 组件 handle → `WasmReply`；
/// trap → `ActorError::OverQuota`（OutOfFuel）或 `MessageHandlingError`
/// （其余 trap——监督层按常规重启策略处置）。
#[cfg(feature = "wasm")]
pub struct WasmActor {
    component: parrot_wasm::runtime::WasmComponent,
}

#[cfg(feature = "wasm")]
impl WasmActor {
    /// 从已实例化组件构造（init 时 on_start）。
    pub fn new(mut component: parrot_wasm::runtime::WasmComponent) -> ActorResult<Self> {
        component.on_start().map_err(|e| {
            parrot_api::errors::ActorError::InitializationError(format!("wasm on_start: {e}"))
        })?;
        Ok(Self { component })
    }

    /// 组件指标（take 语义——宿主监控用）。
    pub fn take_metrics(&mut self) -> parrot_wasm::WasmMetrics {
        self.component.take_metrics()
    }

    /// drain 钩子透传。
    pub fn drain(&mut self) -> Result<(), parrot_wasm::ComponentError> {
        self.component.on_drain()
    }
}

#[cfg(feature = "wasm")]
impl Actor for WasmActor {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(ask) = msg.downcast_ref::<WasmAsk>() {
                return match self.component.handle(&ask.type_key, &ask.payload) {
                    Ok(out) => Ok(Box::new(WasmReply(out)) as BoxedMessage),
                    // fuel 耗尽 → OverQuota 语义（MessageHandlingError 带
                    // 标记前缀——监督层可辨识限频）
                    Err(parrot_wasm::ComponentError::OutOfFuel) => {
                        Err(parrot_api::errors::ActorError::MessageHandlingError(
                            "wasm OverQuota: out of fuel".into(),
                        ))
                    }
                    Err(e) => Err(parrot_api::errors::ActorError::MessageHandlingError(
                        format!("wasm trap: {e}"),
                    )),
                };
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "wasm actor: unhandled message".into(),
            ))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ══════════════════════════════════════════════════════════════════════════
// bin 辅助
// ══════════════════════════════════════════════════════════════════════════

/// 内置 actor spawn（name: echo|counter|kv|slow → /user/<name>）。
/// 内置组件清单（G2/M3 DEV_09）：builtin_app.toml 驱动 + PARROT_ACTORS
/// overlay 过滤。
///
/// 语义（与旧 env 路径等价）：
/// - PARROT_ACTORS 未设 → Manifest 全部组件；
/// - PARROT_ACTORS="echo,kv" → 交集过滤（保持旧逗号分置语义）；
/// - 过滤后为空 → 空（显式不起任何内置 actor——合法）。
pub fn builtin_manifest_components() -> Vec<String> {
    builtin_manifest_components_from(std::env::var("PARROT_ACTORS").ok().as_deref())
}

/// 可测形态（filter 显式注入）。
pub fn builtin_manifest_components_from(filter: Option<&str>) -> Vec<String> {
    // Manifest 与二进制同目录分发（include_str 编译期嵌入——部署零文件依赖）
    static BUILTIN_TOML: &str = include_str!("../builtin_app.toml");
    let m = parrot_app::manifest::AppManifest::from_toml(BUILTIN_TOML)
        .expect("builtin_app.toml 编译期已验证（单测锚定）");
    let all: Vec<String> = m.components.iter().map(|c| c.name.clone()).collect();
    match filter {
        None => all,
        Some(f) if f.trim().is_empty() => vec![],
        Some(f) => {
            let want: std::collections::BTreeSet<&str> = f
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            all.into_iter()
                .filter(|n| want.contains(n.as_str()))
                .collect()
        }
    }
}

pub async fn spawn_builtin(ts: &Arc<ThreadActorSystem>, which: &str) {
    use parrot_api::types::BoxedActorRef as BRef;
    fn boxed<A>(r: parrot::thread::address::ThreadActorRef<A>) -> BRef
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        Box::new(r)
    }
    let path = format!("/user/{which}");
    let r: Result<BRef, _> = match which {
        "counter" => ts
            .spawn_at(
                CounterActor::default(),
                &path,
                None,
                ThreadActorConfig::default(),
            )
            .await
            .map(boxed),
        "kv" => ts
            .spawn_at(
                KvActor::default(),
                &path,
                None,
                ThreadActorConfig::default(),
            )
            .await
            .map(boxed),
        "slow" => ts
            .spawn_at(SlowActor, &path, None, ThreadActorConfig::default())
            .await
            .map(boxed),
        _ => ts
            .spawn_at(EchoActor, &path, None, ThreadActorConfig::default())
            .await
            .map(boxed),
    };
    if let Err(e) = r {
        panic!("builtin actor '{which}' spawn failed: {e}");
    }
}

/// PARROT_SEEDS 解析："id=host:port,id=host:port"（host 可为域名——
/// 异步 DNS 解析；解析失败的条目跳过并告警，不炸节点启动）。
pub async fn parse_seeds(s: String) -> Vec<NodeAddr> {
    use std::net::ToSocketAddrs;
    let mut out = Vec::new();
    for p in s.split(',').filter(|p| !p.is_empty()) {
        let Some((id, addr)) = p.split_once('=') else {
            continue;
        };
        let (id, addr) = (id.trim(), addr.trim());
        let sa: Option<std::net::SocketAddr> = match addr.parse() {
            Ok(ip) => Some(ip),
            Err(_) => {
                let hostport = addr.to_string();
                tokio::task::spawn_blocking(move || {
                    hostport.to_socket_addrs().ok().and_then(|mut i| i.next())
                })
                .await
                .ok()
                .flatten()
            }
        };
        match sa {
            Some(sa) => out.push(NodeAddr::tcp(id, sa)),
            None => eprintln!("[parrot-node] WARN: seed '{id}' addr unresolved: {addr}"),
        }
    }
    out
}

/// UNIX：SIGTERM + CTRL-C；其它平台：CTRL-C。
pub async fn wait_for_shutdown() {
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
}

/// TCP 连自身监听端口成功即健康（accept 循环活着的最小证明；容器探针）。
pub fn healthcheck() -> i32 {
    let bind = std::env::var("PARROT_BIND").unwrap_or_else(|_| "0.0.0.0:9801".into());
    let Ok(addr) = bind.parse::<std::net::SocketAddr>() else {
        return 1;
    };
    let target = if addr.ip().is_unspecified() {
        std::net::SocketAddr::from(([127, 0, 0, 1], addr.port()))
    } else {
        addr
    };
    match std::net::TcpStream::connect_timeout(&target, std::time::Duration::from_secs(2)) {
        Ok(_) => 0,
        Err(_) => 1,
    }
}
