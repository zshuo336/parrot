//! CFG：配置切面集成测试（业务逻辑与物理部署解耦——依赖倒置）。
//!
//! 三层优先级契约：`代码显式设置 > TOML 文件 > 编译期默认`。
//! 用例覆盖：
//! - 文件驱动的真实星型拓扑（hub+spoke 全部参数来自 TOML）
//! - thread mailbox / ask_timeout 参数映射与 facade 构造
//! - golden vectors 与默认路径零扰动（回归保护）

mod common;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_config::{ParrotConfig, TopologyRoleValue};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig as RCfg};
use std::sync::Arc;
use std::time::Duration;

// ---- 消息（独立键空间——本测试二进制独占进程） ----
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct CPing(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct CPong(pub u64);

macro_rules! remote_msg {
    ($t:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &parrot_api::types::BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or(concat!("downcast ", $key))?;
                    parrot_api::message::serde_remote_serialize(&m)
                },
                decode: |bytes| {
                    let m: $t = parrot_api::message::serde_remote_deserialize(bytes)?;
                    Ok(Box::new(m) as parrot_api::types::BoxedMessage)
                },
            }
        }
    };
}
remote_msg!(CPing, "bin:config_aspect::CPing#v1");
remote_msg!(CPong, "bin:config_aspect::CPong#v1");

struct ContextPing(u64);

struct EchoActor;
impl Actor for EchoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(p) = msg.downcast_ref::<CPing>() {
                return Ok(Box::new(CPong(p.0)) as BoxedMessage);
            }
            if let Some(p) = msg.downcast_ref::<ContextPing>() {
                let system = ctx.system_weak_ref().upgrade().ok_or_else(|| {
                    parrot_api::errors::ActorError::InternalError("context owner missing".into())
                })?;
                let target = system
                    .get_actor_ref("/user/context-target")
                    .ok_or_else(|| {
                        parrot_api::errors::ActorError::InternalError(
                            "context target missing".into(),
                        )
                    })?;
                return target.send(Box::new(CPing(p.0))).await;
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unhandled".into(),
            ))
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

struct FacadeLookup {
    facade: Arc<ParrotActorSystem>,
}
#[async_trait::async_trait]
impl LocalLookup for FacadeLookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        self.facade.get_actor(&ActorPath::placeholder(path)).await
    }
}

/// 临时 TOML 工厂（测试结束自清理）。
struct TmpToml(std::path::PathBuf);
impl TmpToml {
    fn new(content: &str, tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "parrot_cfg_{}_{}_{}.toml",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::write(&p, content).unwrap();
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TmpToml {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

// ===========================================================================
// CFG1：TOML 全量驱动星型拓扑（hub + spoke，全部部署参数外置）
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cfg1_file_driven_star_topology() {
    // hub 配置文件：角色 hub + 自定义心跳/队列
    let hub_toml = TmpToml::new(
        r#"
[remote.node]
node_id = "cfg-hub"
bind = "127.0.0.1:0"
topology_role = "hub"

[remote.security]
mode = "insecure-development"

[remote.transport]
heartbeat_interval_ms = 1500
outbound_queue = 2048
"#,
        "hub",
    );
    let hub_resolved = ParrotConfig::builder()
        .load_file(hub_toml.path())
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        hub_resolved.remote.node.topology_role,
        TopologyRoleValue::Hub
    );

    let hub = RemoteActorSystem::new(
        RCfg::from_resolved(&hub_resolved).unwrap(),
        Arc::new(FacadeLookup {
            facade: Arc::new(
                ParrotActorSystem::new(ActorSystemConfig::default())
                    .await
                    .unwrap(),
            ),
        }),
    )
    .unwrap();
    hub.start().await.unwrap();
    let hub_port = hub.local_addr().unwrap().port();

    // The configured seed must establish the link; environment interpolation is
    // covered separately without mutating this parallel test process.
    let spoke_toml = TmpToml::new(
        &format!(
            r#"
[remote.node]
node_id = "cfg-spoke"
bind = "127.0.0.1:0"
seeds = ["tcp://cfg-hub@127.0.0.1:{hub_port}"]

[remote.security]
mode = "insecure-development"
"#
        ),
        "spoke",
    );
    let spoke_resolved = ParrotConfig::builder()
        .load_file(spoke_toml.path())
        .unwrap()
        .build()
        .unwrap();

    // spoke 本地引擎：thread 参数也全部来自文件
    let spoke_ts =
        ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&spoke_resolved));
    spoke_ts
        .spawn_at(EchoActor, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let spoke = RemoteActorSystem::new(
        RCfg::from_resolved(&spoke_resolved).unwrap(),
        Arc::new(FacadeLookup {
            facade: {
                let f = Arc::new(
                    ParrotActorSystem::new(ActorSystemConfig::default())
                        .await
                        .unwrap(),
                );
                f.register_thread_system("eng".into(), spoke_ts.clone(), true)
                    .await
                    .unwrap();
                f
            },
        }),
    )
    .unwrap();
    spoke.start().await.unwrap();

    // 连接建立（links 非空）
    for _ in 0..100 {
        if !spoke.links_snapshot().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!spoke.links_snapshot().await.is_empty(), "hub 连接未建立");

    // 远程 ask 全链路（文件驱动配置下的真实通信：spoke 本地 actor 回环）
    let echo = spoke.remote_ref("parrot://cfg-spoke/user/echo").unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), echo.send(Box::new(CPing(42))))
        .await
        .expect("cfg ask timeout")
        .unwrap();
    let pong = reply.downcast::<CPong>().unwrap();
    assert_eq!(pong.0, 42);

    let _ = spoke.shutdown().await;
    let _ = hub.shutdown().await;
}

// Heartbeat deadlines and handshake/business frame hop limits are exercised
// against raw TCP peers in parrot-remote/tests/config_wiring.rs. Merely reading
// config fields or counting links cannot verify those runtime contracts.

// ===========================================================================
// CFG4：thread 域文件参数流入 ThreadActorSystemConfig（mailbox/timeout）
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cfg4_thread_section_maps_to_system_config() {
    let toml = TmpToml::new(
        r#"
[thread]
shared_pool_size = 2
shared_queue_capacity = 777
default_mailbox_capacity = 99
default_ask_timeout_ms = 1234
shutdown_timeout_ms = 2345
"#,
        "thread",
    );
    let res = ParrotConfig::builder()
        .load_file(toml.path())
        .unwrap()
        .build()
        .unwrap();
    let tc = ThreadActorSystemConfig::from_resolved(&res);
    assert_eq!(tc.shared_pool_size, 2);
    assert_eq!(tc.shared_queue_capacity, 777);
    assert_eq!(tc.default_mailbox_capacity, 99);
    assert_eq!(tc.default_ask_timeout, Duration::from_millis(1234));
    assert_eq!(tc.shutdown_timeout, Duration::from_millis(2345));
}

// ===========================================================================
// CFG5：facade 一站式（ParrotActorSystem::from_config）
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cfg5_facade_from_config() {
    let res = ParrotConfig::builder()
        .remote_node_id("cfg-facade-node")
        .thread_default_mailbox_capacity(64)
        .build()
        .unwrap();
    let ts = ParrotActorSystem::from_config(&res).await.unwrap();
    assert_eq!(ts.config().default_mailbox_capacity, 64);
    ts.spawn_at(
        EchoActor,
        "/user/context-target",
        None,
        ThreadActorConfig::default(),
    )
    .await
    .unwrap();
    ts.spawn_at(EchoActor, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    // Use the returned engine itself and require an actual actor context to
    // find and call a second actor owned by that same engine.
    let r = ts.get_actor_ref("/user/echo").expect("actor not found");
    let reply =
        tokio::time::timeout(Duration::from_secs(2), r.send(Box::new(ContextPing(7)))).await;
    ts.shutdown_internal().await.unwrap();
    let reply = reply
        .expect("context-routed ask timeout")
        .expect("configured engine context has no owner");
    assert_eq!(reply.downcast::<CPong>().unwrap().0, 7);
}

// ===========================================================================
// CFG6：示例配置文件合法（parrot.toml.example 永不腐烂）
// ===========================================================================
#[test]
fn cfg6_example_file_parses() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parrot.toml.example");
    let res = ParrotConfig::builder()
        .load_file(&root)
        .unwrap()
        .build()
        .unwrap();
    // 全注释文件 → 全默认（示例即文档：默认值展示必须与真实默认一致）
    assert_eq!(res.remote.transport.heartbeat_interval_ms, 2000);
    assert_eq!(res.remote.transport.heartbeat_max_loss, 5);
    assert_eq!(res.remote.transport.outbound_queue, 1024);
    assert_eq!(res.remote.transport.default_hop_limit, 8);
    assert_eq!(res.remote.reorder.gap_timeout_ms, 250);
    assert_eq!(res.remote.reorder.buffer_cap, 1024);
}
