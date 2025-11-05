//! parrot-node: one process assembles a facade, thread engine and remote transport.
//!
//! Configuration precedence: explicit environment > PARROT_CONFIG (required
//! when set), otherwise optional ./parrot.toml > node defaults. The same resolved
//! configuration constructs both engines. Environment overrides:
//! - PARROT_NODE_ID: node identity (default node-1).
//! - PARROT_BIND: numeric listen address (default 127.0.0.1:9801).
//! - PARROT_SCHEME: tcp (default), quic or mem. QUIC requires mTLS or an
//!   explicit insecure-development configuration.
//! - PARROT_SEEDS: comma-separated id=host:port or scheme://id@host:port;
//!   mem://node for in-process memory transport. Empty explicitly clears seeds.
//! - PARROT_TOPOLOGY_ROLE: normal, hub, border, directory. Directory requires
//!   explicit remote.directory storage/group/voting peers; see NETWORK_DIRECTORY.md.
//! - PARROT_ACTORS: builtin names (default echo,counter,kv,slow); empty disables
//!   all. Unknown names and empty interior entries are errors.
//! - PARROT_READY_FILE: optional path for the complete actual endpoint record.
//! - PARROT_WASM_EPOCH_TICKS: positive u32 host budget; requires wasm feature.
//! - PARROT_TRUST_NATIVE_SHA256: executable-only, comma-separated trusted native
//!   SHA-256 digests; requires dylib feature. This is operator authorization of
//!   the complete unsafe ABI/dependency contract, not artifact authentication.
//!   NodeSettings and safe default startup cannot grant native authority.
//!
//! Invalid configuration fails before startup. Seed DNS has a shared five-second
//! deadline; initial connection failures retry in the remote bootstrap task.
//! Readiness prints `PARROT_NODE_READY=<actual endpoint>` only after local startup
//! and a successful requested readiness-file write. It does not imply all seeds
//! are connected. For mem, the endpoint is mem://node rather than a TCP address.
//! Signal handlers are installed before readiness; shutdown awaits remote and
//! local engine shutdown. One NODE singleton is supported per process.
//!
//! PARROT_HEALTHCHECK=1 loads the same configuration and requires a Parrot
//! handshake with the expected identity within two seconds (TCP or QUIC).
//! Wildcard IPv4/IPv6 binds probe their corresponding loopback family. Port zero
//! requires PARROT_READY_FILE; the file records an address, not current liveness.
//! Cross-process mem probes fail explicitly. This tests protocol availability,
//! not actor health, authorization, seed convergence or cluster consistency.

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
use parrot_remote::LocalLookup;

pub mod configuration;
pub mod directory;
#[cfg(feature = "dylib")]
pub mod native;
pub mod runtime;
#[cfg(feature = "dylib")]
pub use native::NativeTrust;
#[cfg(feature = "dylib")]
pub use parrot_app::host::dylib::{DylibAsk, DylibReply, DylibTell};

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

reg!(
    directory::QueryLocal,
    "bin:parrot_node::directory::QueryLocal#v1"
);
reg!(
    directory::LocalEntry,
    "bin:parrot_node::directory::LocalEntry#v1"
);
reg!(directory::Inspect, "bin:parrot_node::directory::Inspect#v1");
reg!(
    directory::Observation,
    "bin:parrot_node::directory::Observation#v1"
);
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

pub mod executor_v2;

pub use executor_v2::{exec_command_v2, exec_deploy_v2, ArtifactChannel, NodeComponentExecutor};

// ══════════════════════════════════════════════════════════════════════════
// C 阶段（DEV_09 §3.3）：WasmActor——wasm 组件的引擎形态接线
// ══════════════════════════════════════════════════════════════════════════

/// Shared Wasm host adapter; the application manifest runner and node embeddings
/// use the same lifecycle and wire contracts, including Admin V2 Wasm deployment.
#[cfg(feature = "wasm")]
pub use parrot_app::host::wasm::{WasmActor, WasmAsk, WasmReply, WasmTell};

// ══════════════════════════════════════════════════════════════════════════
// bin 辅助
// ══════════════════════════════════════════════════════════════════════════

/// 内置 actor spawn（name: echo|counter|kv|slow → `/user/<name>`）。
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

pub async fn spawn_builtin(
    ts: &Arc<ThreadActorSystem>,
    which: &str,
) -> Result<(), runtime::NodeError> {
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
        "echo" => ts
            .spawn_at(EchoActor, &path, None, ThreadActorConfig::default())
            .await
            .map(boxed),
        _ => return Err(format!("unknown builtin actor: {which}").into()),
    };
    r.map(|_| ()).map_err(Into::into)
}
