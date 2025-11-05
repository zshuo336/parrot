//! RH：hub 转发语义（07 §6 星型拓扑两两互通）——A→hub→B 中转。
//!
//! 场景：node-a、node-b 互不直连，各自只连 hub；A 发起的
//! `parrot://node-b/user/echo` 由 hub 查表转发，REPLY 经 cid 映射还原回源。
//! 用例覆盖：ask 转发回程 / tell 转发 / RouteUnreachable（未知节点）/
//! hop_count 递增 / 断连后映射失效。

// Transport semantics use explicitly trusted development TCP peers;
// authenticated/denied paths are covered by control_authentication.
mod common;

use common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig as RCfg};
use std::sync::Arc;
use std::time::Duration;

fn hint_only_source() -> RCfg {
    let mut config = RCfg::tcp("node-a", None).with_insecure_development();
    // Isolate ROUTE_HINT learning from independent membership discovery.
    config.heartbeat_enabled = false;
    config
}

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

type StarTopology = (
    Arc<RemoteActorSystem>,                       // a
    Arc<RemoteActorSystem>,                       // b
    Arc<RemoteActorSystem>,                       // hub
    std::sync::Arc<std::sync::atomic::AtomicU64>, // b 的 note 计数
);

/// 星型三节点：a、b 互不直连，各连 hub（TCP）。
async fn star_topology() -> StarTopology {
    star_topology_with_capture(None).await
}

async fn star_topology_with_capture(
    capture: Option<parrot_remote::capture::FrameCapture>,
) -> StarTopology {
    // hub：无本地 actor（纯路由）
    let mut config = RCfg::tcp("hub", Some("127.0.0.1:0".parse().unwrap()))
        .with_insecure_development()
        .with_role(parrot_remote::TopologyRole::Hub);
    if let Some(capture) = capture {
        config = config.with_capture(capture);
    }
    let hub = RemoteActorSystem::new(
        config,
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
        NoteActor {
            count: counter.clone(),
        },
        "/user/note",
        None,
        ThreadActorConfig::default(),
    )
    .await
    .unwrap();
    let rb = RemoteActorSystem::new(
        RCfg::tcp("node-b", Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
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
        RCfg::tcp("node-a", None)
            .with_insecure_development()
            .with_topology(parrot_remote::topology::TopologyConfig {
                mode: parrot_remote::topology::TopologyMode::Hub,
                relay_fallback: false,
                ..Default::default()
            }),
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
// RH4: a new ASK fails after the hub observes physical destination disconnect.
// ===========================================================================

#[tokio::test]
async fn rh4_new_ask_fails_after_destination_disconnect() {
    let (ra, rb, hub, _) = star_topology().await;
    // 拆掉 hub↔B 链路（B 主动断）
    rb.shutdown().await.unwrap();
    eventually(Duration::from_secs(3), || async {
        !hub.missing_direct_connections(&["node-b"]).is_empty()
    })
    .await;
    let r = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), r.send(Box::new(HPing(7))))
        .await
        .expect("must not hang");
    assert!(res.is_err(), "ask after link loss must fail");
    ra.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
}

// ===========================================================================
// RH5：转发帧 hop_count 递增（防环计数贯通）
// ===========================================================================

#[tokio::test]
async fn rh5_hop_count_incremented() {
    use parrot_remote::capture::{CaptureDirection, CaptureLimits, FrameCapture};
    let (capture, mut receiver) = FrameCapture::channel(CaptureLimits::default()).unwrap();
    let recording = std::thread::spawn(move || {
        let mut hops = Vec::new();
        while let Some(event) = receiver.blocking_recv() {
            if event.frame.header.frame_type == parrot_remote::frame::frame_type::ASK
                && event.frame.path == "parrot://node-b/user/echo"
            {
                hops.push((
                    event.direction == CaptureDirection::Tx,
                    event.frame.header.hop_count,
                ));
            }
        }
        hops
    });
    let (ra, rb, hub, _) = star_topology_with_capture(Some(capture.clone())).await;
    let r = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), r.send(Box::new(HPing(5)))).await;
    for system in [&ra, &rb, &hub] {
        system.shutdown().await.unwrap();
    }
    capture.close();
    let mut hops = tokio::task::spawn_blocking(move || recording.join().unwrap())
        .await
        .unwrap();
    hops.sort_unstable();
    assert_eq!(
        reply
            .expect("ask timeout")
            .expect("relayed ask failed")
            .downcast::<HPong>()
            .unwrap()
            .0,
        5
    );
    assert_eq!(
        capture.stats().lost,
        0,
        "hop proof requires complete capture"
    );
    assert_eq!(
        hops,
        vec![(false, 0), (true, 1)],
        "this hub must receive hop0 and forward hop1"
    );
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
            .with_insecure_development()
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
        RCfg::tcp(
            "node-b",
            Some(format!("127.0.0.1:{b_port}").parse().unwrap()),
        )
        .with_insecure_development()
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
        hint_only_source(),
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
    let reply2 = tokio::time::timeout(Duration::from_secs(5), r.send(Box::new(HPing(7))))
        .await
        .expect("direct ask timeout");
    assert_eq!(reply2.unwrap().downcast::<HPong>().unwrap().0, 7);
}

// ===========================================================================
// RH7: per-phase order with the relay phase fully drained before direct traffic.
// This does not prove FIFO when relay and direct frames are simultaneously in flight.
// ===========================================================================

/// 顺序录制 actor：记 HNote 到达序（Vec<u64>）。
struct OrderActor {
    order: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
}
impl Actor for OrderActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let order = self.order.clone();
        Box::pin(async move {
            if let Some(HNote(v)) = msg.downcast_ref::<HNote>() {
                order.lock().unwrap().push(*v);
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

#[tokio::test]
async fn rh7_tell_order_with_drained_relay_before_switch() {
    // 拓扑复用 RH6 形态：hub + b（direct_addr）+ a
    let hub = RemoteActorSystem::new(
        RCfg::tcp("hub", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development()
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
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    ts_b.spawn_at(
        OrderActor {
            order: order.clone(),
        },
        "/user/order",
        None,
        ThreadActorConfig::default(),
    )
    .await
    .unwrap();
    let b_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let rb = RemoteActorSystem::new(
        RCfg::tcp(
            "node-b",
            Some(format!("127.0.0.1:{b_port}").parse().unwrap()),
        )
        .with_insecure_development()
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

    let ra = RemoteActorSystem::new(
        hint_only_source(),
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

    // 阶段1（中转期）：A 连发 N 条 TELL（走 uplink 中转——帧带 seq 但
    // hub 中转路径 B 侧 bypass 直投；TCP FIFO 保序）
    let r = ra.remote_ref("parrot://node-b/user/order").unwrap();
    for i in 0..20u64 {
        r.deliver(Box::new(HNote(i))).await.unwrap();
    }
    eventually(Duration::from_secs(3), || async {
        order.lock().unwrap().len() == 20
    })
    .await;
    assert_eq!(
        order.lock().unwrap().clone(),
        (0..20u64).collect::<Vec<_>>(),
        "中转期顺序正确（TCP FIFO + bypass）"
    );

    // 阶段2：触发直连学习（一次 ask 换 hint——与 tell 流不同 ref 无干扰）
    let r_ask = ra.remote_ref("parrot://node-b/user/order").unwrap();
    // OrderActor 只回 HNote；ask HPing 会 MessageHandlingError——改发 HNote
    // ask：send 走 ASK，OrderActor 回 HPong
    let marker = tokio::time::timeout(Duration::from_secs(5), r_ask.send(Box::new(HNote(999))))
        .await
        .expect("relay ask timeout")
        .expect("phase marker must execute successfully");
    assert_eq!(marker.downcast::<HPong>().unwrap().0, 999);
    eventually(Duration::from_secs(5), || async {
        ra.links_snapshot()
            .await
            .iter()
            .any(|(n, _, _)| n == "node-b")
    })
    .await;

    // The relay phase has already drained. This verifies a new direct phase,
    // not ordering of messages simultaneously in flight over different routes.
    for i in 20..40u64 {
        r.deliver(Box::new(HNote(i))).await.unwrap();
    }
    eventually(Duration::from_secs(5), || async {
        let g = order.lock().unwrap();
        g.len() == 41
    })
    .await;
    let got = order.lock().unwrap().clone();
    // 终序：中转期 0..20 → ask 999（中转 bypass）→ 直连期 20..40
    // （冷启动 250ms 放行后整段连 drain）
    let expect: Vec<u64> = (0..20u64).chain([999u64]).chain(20..40u64).collect();
    assert_eq!(
        got, expect,
        "drained relay and direct phases preserve their exact sequence"
    );
    ra.shutdown().await.unwrap();
    rb.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
    ts_b.shutdown_internal().await.unwrap();
}

// ===========================================================================
// RH8：显式限制在途窗口的 5000 帧直连 FIFO 正确性；不作为吞吐/无损反压证明。
// ===========================================================================

#[tokio::test]
async fn rh8_direct_fifo_with_bounded_inflight_window() {
    // 两节点直连（无 hub）
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
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    ts_b.spawn_at(
        OrderActor {
            order: order.clone(),
        },
        "/user/order",
        None,
        ThreadActorConfig::default(),
    )
    .await
    .unwrap();
    let rb = RemoteActorSystem::new(
        RCfg::tcp("node-b", Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(FacadeLookup {
            facade: facade_b.clone(),
        }),
    )
    .unwrap();
    rb.start().await.unwrap();
    let b_port = rb.local_addr().unwrap().port();

    let ra = RemoteActorSystem::new(
        RCfg::tcp("node-a", None).with_insecure_development(),
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
        "node-b",
        format!("127.0.0.1:{b_port}").parse().unwrap(),
    ))
    .await
    .unwrap();
    eventually(Duration::from_secs(3), || async {
        ra.remote_ref("parrot://node-b/user/order").is_ok()
    })
    .await;

    let r = ra.remote_ref("parrot://node-b/user/order").unwrap();
    // TELL success is local queue admission, not a peer acknowledgement. Keep
    // at most 25 business messages outstanding; a separate overload contract
    // verifies that exceeding the remote budget is visible as link failure.
    for end in (25..=5000usize).step_by(25) {
        for i in end - 25..end {
            r.deliver(Box::new(HNote(i as u64))).await.unwrap();
        }
        eventually(Duration::from_secs(5), || async {
            order.lock().unwrap().len() == end
        })
        .await;
    }
    let got = order.lock().unwrap().clone();
    let expect: Vec<u64> = (0..5000u64).collect();
    assert_eq!(
        got, expect,
        "5000 business deliveries must preserve exact order"
    );
    ra.shutdown().await.unwrap();
    rb.shutdown().await.unwrap();
    ts_b.shutdown_internal().await.unwrap();
}

// ===========================================================================
// RH9: TELL fails after the sender observes physical peer shutdown.
// ===========================================================================

#[tokio::test]
async fn rh9_shutdown_no_hang() {
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
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    ts_b.spawn_at(
        OrderActor {
            order: order.clone(),
        },
        "/user/order",
        None,
        ThreadActorConfig::default(),
    )
    .await
    .unwrap();
    let rb = RemoteActorSystem::new(
        RCfg::tcp("node-b", Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(FacadeLookup {
            facade: facade_b.clone(),
        }),
    )
    .unwrap();
    rb.start().await.unwrap();
    let b_port = rb.local_addr().unwrap().port();
    let ra = RemoteActorSystem::new(
        RCfg::tcp("node-a", None).with_insecure_development(),
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
        "node-b",
        format!("127.0.0.1:{b_port}").parse().unwrap(),
    ))
    .await
    .unwrap();
    eventually(Duration::from_secs(3), || async {
        ra.remote_ref("parrot://node-b/user/order").is_ok()
    })
    .await;
    // 建流 + 发送 + 关闭 B（接收方 Ingress 析构 → 重排任务 recv None 退出）
    let r = ra.remote_ref("parrot://node-b/user/order").unwrap();
    r.deliver(Box::new(HNote(1))).await.unwrap();
    eventually(Duration::from_secs(3), || async {
        order.lock().unwrap().as_slice() == [1]
    })
    .await;
    rb.shutdown().await.unwrap();
    eventually(Duration::from_secs(3), || async {
        !ra.missing_direct_connections(&["node-b"]).is_empty()
    })
    .await;
    // A 侧再发：连接已断 → deliver 快速失败（不悬挂）
    let res = tokio::time::timeout(Duration::from_secs(3), r.deliver(Box::new(HNote(2))))
        .await
        .expect("tell after observed peer shutdown must not hang");
    assert!(res.is_err(), "closed peer must not admit a new TELL");
    assert_eq!(order.lock().unwrap().as_slice(), [1]);
    ra.shutdown().await.unwrap();
    ts_b.shutdown_internal().await.unwrap();
}
