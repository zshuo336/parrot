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
// bin 辅助
// ══════════════════════════════════════════════════════════════════════════

/// 内置 actor spawn（name: echo|counter|kv|slow → /user/<name>）。
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
