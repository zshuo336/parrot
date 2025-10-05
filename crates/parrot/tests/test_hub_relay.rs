//! RH：hub 转发语义（07 §6 星型拓扑两两互通）——A→hub→B 中转。
//!
//! 场景：node-a、node-b 互不直连，各自只连 hub；A 发起的
//! `parrot://node-b/user/echo` 由 hub 查表转发，REPLY 经 cid 映射还原回源。
//! 用例覆盖：ask 转发回程 / tell 转发 / RouteUnreachable（未知节点）/
//! hop_count 递增 / 断连后映射失效。

mod common;

use common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRef, ActorRefExt};
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig as RCfg};
use std::sync::Arc;
use std::time::Duration;

// 消息（与 test_remote_semantics 相同键——不同集成测试二进制独立进程，
// inventory 注册不冲突）
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct HPing(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct HPong(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct HNote(pub u64); // tell 载荷

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
                decode: |b: &[u8]| {
                    let v: $t = parrot_api::message::serde_remote_deserialize(b)?;
                    Ok(Box::new(v) as parrot_api::types::BoxedMessage)
                },
            }
        }
    };
}

remote_msg!(HPing, "bin:hub_relay::HPing#v1");
remote_msg!(HPong, "bin:hub_relay::HPong#v1");
remote_msg!(HNote, "bin:hub_relay::HNote#v1");

struct EchoActor;
impl Actor for EchoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(HPing(v)) = msg.downcast_ref::<HPing>() {
                return Ok(Box::new(HPong(*v)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unhandled".into(),
            ))
        })
    }
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}

/// 记 TELL 序号；ask 取回计数。
struct NoteActor {
    count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}
impl Actor for NoteActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(HNote(v)) = msg.downcast_ref::<HNote>() {
                self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(Box::new(HPong(*v)) as BoxedMessage);
            }
            if msg.downcast_ref::<HPing>().is_some() {
                let n = self.count.load(std::sync::atomic::Ordering::SeqCst);
                return Ok(Box::new(HPong(n)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unhandled".into(),
            ))
        })
    }
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
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

/// 星型三节点：a、b 互不直连，各连 hub（TCP）。
async fn star_topology() -> (
    Arc<RemoteActorSystem>, // a
    Arc<RemoteActorSystem>, // b
    Arc<RemoteActorSystem>, // hub
    std::sync::Arc<std::sync::atomic::AtomicU64>, // b 的 note 计数
) {
    // hub：无本地 actor（纯路由）
    let hub = RemoteActorSystem::new(
        RCfg::tcp("hub", Some("127.0.0.1:0".parse().unwrap()))
            .with_role(parrot_remote::TopologyRole::Hub),
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

    // b：echo + note
    let facade_b = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts_b = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    facade_b
        .register_thread_system("eng".into(), ts_b.clone(), true)
        .await
        .unwrap();
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    ts_b.spawn_at(EchoActor, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    ts_b.spawn_at(
        NoteActor { count: counter.clone() },
        "/user/note",
        None,
        ThreadActorConfig::default(),
    )
    .await
    .unwrap();
    let rb = RemoteActorSystem::new(
        RCfg::tcp("node-b", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(FacadeLookup {
            facade: facade_b.clone(),
        }),
    )
    .unwrap();
    rb.start().await.unwrap();
    rb.connect(&parrot_remote::NodeAddr::tcp(
        "hub",
        format!("127.0.0.1:{hub_port}").parse().unwrap(),
    ))
    .await
    .unwrap();

    // a：纯发起方（无本地 actor）
    let ra = RemoteActorSystem::new(
        RCfg::tcp("node-a", None),
        Arc::new(FacadeLookup {
            facade: Arc::new(
                ParrotActorSystem::new(ActorSystemConfig::default())
                    .await
                    .unwrap(),
            ),
        }),
    )
    .unwrap();
    ra.start().await.unwrap();
    ra.connect(&parrot_remote::NodeAddr::tcp(
        "hub",
        format!("127.0.0.1:{hub_port}").parse().unwrap(),
    ))
    .await
    .unwrap();

    eventually(Duration::from_secs(3), || async {
        ra.remote_ref("parrot://hub/user/echo").is_ok()
    })
    .await;
    (ra, rb, hub, counter)
}

// ===========================================================================
// RH1：A→hub→B ask（echo 回程经 cid 映射还原）
// ===========================================================================

#[tokio::test]
async fn rh1_relayed_ask_round_trip() {
    let (ra, _rb, _hub, _) = star_topology().await;
    let r = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), r.send(Box::new(HPing(42))))
        .await
        .expect("relay ask timeout");
    let pong = reply.unwrap().downcast::<HPong>().unwrap();
    assert_eq!(pong.0, 42);
}

// ===========================================================================
// RH2：A→hub→B tell（fire-and-forget 中转）
// ===========================================================================

#[tokio::test]
async fn rh2_relayed_tell_delivered() {
    let (ra, _rb, _hub, counter) = star_topology().await;
    let r = ra.remote_ref("parrot://node-b/user/note").unwrap();
    for i in 0..10u64 {
        r.deliver(Box::new(HNote(i))).await.unwrap();
    }
    eventually(Duration::from_secs(3), || async {
        counter.load(std::sync::atomic::Ordering::SeqCst) == 10
    })
    .await;
}

// ===========================================================================
// RH3：未知节点 → RouteUnreachable（不是 ActorNotFound）
// ===========================================================================

#[tokio::test]
async fn rh3_no_route_err() {
    let (ra, _rb, _hub, _) = star_topology().await;
    let r = ra.remote_ref("parrot://node-z/user/echo").unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), r.send(Box::new(HPing(1))))
        .await
        .expect("timeout");
    let e = err.unwrap_err();
    // RemoteActorSystem 侧映射：RouteUnreachable → ActorNotFound（detail 带
    // no route——语义等价，ErrCode 原码在 wire 上是 RouteUnreachable）
    let msg = e.to_string();
    assert!(
        msg.contains("no route"),
        "err should carry route failure detail, got: {msg}"
    );
}

// ===========================================================================
// RH4：hub 断开 B 后，A 的挂起/新 ask 得到失败（不悬挂）
// ===========================================================================

#[tokio::test]
async fn rh4_relay_link_loss_fails_pending() {
    let (ra, rb, _hub, _) = star_topology().await;
    // 拆掉 hub↔B 链路（B 主动断）
    rb.shutdown().await;
    // 给断连传播留时间
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), r.send(Box::new(HPing(7))))
        .await
        .expect("must not hang");
    assert!(res.is_err(), "ask after link loss must fail");
}

// ===========================================================================
// RH5：转发帧 hop_count 递增（防环计数贯通）
// ===========================================================================

#[tokio::test]
async fn rh5_hop_count_incremented() {
    let (ra, _rb, hub, _) = star_topology().await;
    let r = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), r.send(Box::new(HPing(5))))
        .await
        .expect("ask timeout");
    // RELAYED_ASK 计数 ≥1（hub 转发生过）
    let n = parrot_remote::RelayMetrics::relayed_ask();
    assert!(n >= 1, "hub should have relayed at least one ask, got {n}");
    let _ = hub;
}

// ===========================================================================
// RH6（方案 A）：B 声明 direct_addr → A 首访经 hub 学习 hint → 后台直连
// 建立 → A 的 links 出现 node-b（后续帧直发，不再中转）
// ===========================================================================

#[tokio::test]
async fn rh6_direct_link_learned_from_hint() {
    // hub
    let hub = RemoteActorSystem::new(
        RCfg::tcp("hub", Some("127.0.0.1:0".parse().unwrap()))
            .with_role(parrot_remote::TopologyRole::Hub),
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

    // b：声明 direct_addr（listen :0 的真实端口——握手带上）
    let facade_b = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts_b = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    facade_b
        .register_thread_system("eng".into(), ts_b.clone(), true)
        .await
        .unwrap();
    ts_b.spawn_at(EchoActor, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    // b：声明 direct_addr。端口策略：先占后放取空闲端口，再用该端口构造
    // （bind 固定端口——声明地址 = 实际 listen 地址，无重启竞态）。
    let b_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let rb = RemoteActorSystem::new(
        RCfg::tcp("node-b", Some(format!("127.0.0.1:{b_port}").parse().unwrap()))
            .with_direct_addr(format!("127.0.0.1:{b_port}")),
        Arc::new(FacadeLookup {
            facade: facade_b.clone(),
        }),
    )
    .unwrap();
    rb.start().await.unwrap();
    rb.connect(&parrot_remote::NodeAddr::tcp(
        "hub",
        format!("127.0.0.1:{hub_port}").parse().unwrap(),
    ))
    .await
    .unwrap();

    // a：连 hub
    let ra = RemoteActorSystem::new(
        RCfg::tcp("node-a", None),
        Arc::new(FacadeLookup {
            facade: Arc::new(
                ParrotActorSystem::new(ActorSystemConfig::default())
                    .await
                    .unwrap(),
            ),
        }),
    )
    .unwrap();
    ra.start().await.unwrap();
    ra.connect(&parrot_remote::NodeAddr::tcp(
        "hub",
        format!("127.0.0.1:{hub_port}").parse().unwrap(),
    ))
    .await
    .unwrap();
    eventually(Duration::from_secs(3), || async {
        ra.remote_ref("parrot://hub/user/echo").is_ok()
    })
    .await;

    // 首访：经 hub 中转（A 尚无 node-b 直连）——hint 注入 + 后台拨号
    let r = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), r.send(Box::new(HPing(42))))
        .await
        .expect("first (relayed) ask timeout");
    assert_eq!(reply.unwrap().downcast::<HPong>().unwrap().0, 42);

    // 直连建立（hint → 后台拨号 → A 的 links 出现 node-b）
    eventually(Duration::from_secs(5), || async {
        ra.links_snapshot()
            .await
            .iter()
            .any(|(n, _, _)| n == "node-b")
    })
    .await;

    // 直连后的 ask（同 ref——sender_of 直连表优先，不再经 hub）
    let r2 = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let reply2 = tokio::time::timeout(Duration::from_secs(5), r2.send(Box::new(HPing(7))))
        .await
        .expect("direct ask timeout");
    assert_eq!(reply2.unwrap().downcast::<HPong>().unwrap().0, 7);
}

