//! K6 集成语义复跑（DEV_02 §7）：三节点 mem 集群 + 随机 kill + RC 复跑 +
//! QUIC/TLS 全链路冒烟。
//!
//! 断言不变（与 RC1-RC8 同规则），环境加噪：
//! - 三节点 A-B-C（mem 双联 A-B、A-C），RC1/RC4/RC5 在多成员下复跑
//! - kill C（shutdown）后 N ask 仍达多数派成员（A、B）
//! - QUIC 冒烟：ask/echo 过 QUIC 载体（K3 链路验证）

mod common;

#[allow(unused_imports)]
use common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig as RCfg};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ===========================================================================
// 消息（bincode 注册）
// ===========================================================================

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct K6Ping(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct K6Pong(pub u64);

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

remote_msg!(K6Ping, "bin:k6_cluster::K6Ping#v1");
remote_msg!(K6Pong, "bin:k6_cluster::K6Pong#v1");

// ===========================================================================
// echo actor（thread 引擎）
// ===========================================================================

struct K6Echo;

impl Actor for K6Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(K6Ping(v)) = msg.downcast_ref::<K6Ping>() {
                return Ok(Box::new(K6Pong(*v)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError("unhandled".into()))
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

async fn eventually<F, Fut>(timeout: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met within {timeout:?}");
}

async fn spawn_node(
    id: &str,
) -> (Arc<RemoteActorSystem>, Arc<ParrotActorSystem>, Arc<ThreadActorSystem>) {
    let facade = Arc::new(ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap());
    let ts = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    ts.spawn_at(K6Echo, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let rs = RemoteActorSystem::new(
        RCfg::mem(id.to_string()),
        Arc::new(FacadeLookup { facade: facade.clone() }),
    )
    .unwrap();
    rs.start().await.unwrap();
    (rs, facade, ts)
}

/// 三节点集群（A-B、A-C 星型 mem 双联）。
async fn trio_cluster() -> (
    Arc<RemoteActorSystem>,
    Arc<RemoteActorSystem>,
    Arc<RemoteActorSystem>,
) {
    let (a, _, _) = spawn_node("k6-a").await;
    let (b, _, _) = spawn_node("k6-b").await;
    let (c, _, _) = spawn_node("k6-c").await;
    a.connect_mem_pair(&b).await.unwrap();
    a.connect_mem_pair(&c).await.unwrap();
    // 等双链握手完成
    eventually(Duration::from_secs(3), || async {
        a.remote_ref("parrot://k6-b/user/echo").is_ok()
            && a.remote_ref("parrot://k6-c/user/echo").is_ok()
    })
    .await;
    (a, b, c)
}

// ===========================================================================
// 用例
// ===========================================================================

/// RC1 恰好一次（三节点）：A 并发 128 ask → B；A 并发 128 ask → C。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k6_rc1_trio_exactly_once() {
    let (a, b, c) = trio_cluster().await;

    // A → B / A → C 各 128 并发 ask（boxed 统一 future 类型）
    let echo_b = a.remote_ref("parrot://k6-b/user/echo").unwrap();
    let echo_c = a.remote_ref("parrot://k6-c/user/echo").unwrap();
    let mut futs: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = u64> + Send>>> =
        Vec::new();
    for i in 0..128u64 {
        let e = echo_b.clone();
        futs.push(Box::pin(async move {
            let r = e.send(Box::new(K6Ping(i))).await.unwrap();
            r.downcast_ref::<K6Pong>().unwrap().0
        }));
        let e = echo_c.clone();
        futs.push(Box::pin(async move {
            let r = e.send(Box::new(K6Ping(i))).await.unwrap();
            r.downcast_ref::<K6Pong>().unwrap().0
        }));
    }
    let results = futures::future::join_all(futs).await;
    assert_eq!(results.len(), 256, "both targets × 128");
    // 值域恰为 0..128（每个 i 在 B/C 各应答一次）
    let mut counts = vec![0u32; 128];
    for v in results {
        counts[v as usize] += 1;
    }
    assert!(counts.iter().all(|c| *c == 2), "each ping answered once per target: {counts:?}");

    a.shutdown().await.ok();
    b.shutdown().await.ok();
    c.shutdown().await.ok();
}

/// kill 加噪：kill C 后 N ask 仍达多数派（A、B 存活）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k6_kill_noise_majority_alive() {
    let (a, b, c) = trio_cluster().await;

    // kill C（shutdown——进程语义等价）
    c.shutdown().await.unwrap();

    // kill 后 N=64 ask A→B 全部成功（多数派仍服务）
    let echo_b = a.remote_ref("parrot://k6-b/user/echo").unwrap();
    let t0 = Instant::now();
    for i in 0..64u64 {
        let r = echo_b
            .send(Box::new(K6Ping(i)))
            .await
            .unwrap_or_else(|e| panic!("ask {i} failed after kill: {e:?}"));
        assert_eq!(r.downcast_ref::<K6Pong>().unwrap().0, i);
    }
    // A→C 必须失败（链路已断——ConnectionLost）
    let echo_c = a.remote_ref("parrot://k6-c/user/echo");
    if let Ok(ec) = echo_c {
        let r = ec.send(Box::new(K6Ping(0))).await;
        assert!(r.is_err(), "ask to killed node must fail, got {:?}", r);
    }
    let _ = t0;
    a.shutdown().await.ok();
    b.shutdown().await.ok();
}

/// QUIC 全链路冒烟（K3）：A --QUIC--> B ask/echo。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k6_quic_smoke() {
    // provider 安装在 QuicTransport 构造侧（幂等）
    // B：echo（QUIC listen :0——经 local_addr 取真实端口，消除端口探测竞态）
    let (b, fb, tsb) = spawn_node_quic("k6-qb").await;
    let _ = (fb, tsb);
    let port = b.local_addr().expect("quic bound").port();
    let (a, _, _) = spawn_node_quic("k6-qa").await; // 客户端（不 listen 业务口）
    a.connect(&parrot_remote::NodeAddr::quic(
        "k6-qb",
        format!("127.0.0.1:{port}").parse().unwrap(),
    ))
    .await
    .unwrap();
    eventually(Duration::from_secs(5), || async {
        a.remote_ref("parrot://k6-qb/user/echo").is_ok()
    })
    .await;
    let echo = a.remote_ref("parrot://k6-qb/user/echo").unwrap();
    for i in 0..16u64 {
        let r = echo.send(Box::new(K6Ping(i))).await.unwrap();
        assert_eq!(r.downcast_ref::<K6Pong>().unwrap().0, i);
    }
    a.shutdown().await.ok();
    b.shutdown().await.ok();
}

// rustls crypto provider 已在 QuicTransport 构造侧安装（parrot-remote
// 内部）——无本地安装步骤（文档锚点）。

async fn spawn_node_quic(
    id: &str,
) -> (Arc<RemoteActorSystem>, Arc<ParrotActorSystem>, Arc<ThreadActorSystem>) {
    let facade = Arc::new(ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap());
    let ts = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    ts.spawn_at(K6Echo, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let rs = RemoteActorSystem::new(
        RCfg::quic(id.to_string(), Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(FacadeLookup { facade: facade.clone() }),
    )
    .unwrap();
    rs.start().await.unwrap();
    (rs, facade, ts)
}
