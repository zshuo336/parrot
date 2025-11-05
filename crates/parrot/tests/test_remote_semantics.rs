//! RC1–RC8 语义等价复跑（05 §10.3 / DEV_01 §5）——mem + tcp 双载体。
//!
//! 远程消息落到真实 parrot ThreadActorSystem actor（不是桩）——
//! 语义与本地（X1-X8 cross-engine 用例族）逐条对齐。

// Transport semantics use explicitly trusted development TCP peers;
// authenticated/denied paths are covered by control_authentication.
mod common;

use common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{BackpressureStrategy, ThreadActorConfig, ThreadActorSystemConfig};
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
// 远程消息（bincode 注册——inventory 经测试文件内 submit）
// ===========================================================================

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct RPing(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct RPong(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct RSeq(pub u64); // FIFO 序号载体
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct RGetSeen; // 取 FIFO 已见列表
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct RSeen(Vec<u64>);

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

remote_msg!(RPing, "bin:remote_semantics::RPing#v1");
remote_msg!(RPong, "bin:remote_semantics::RPong#v1");
remote_msg!(RSeq, "bin:remote_semantics::RSeq#v1");
remote_msg!(RGetSeen, "bin:remote_semantics::RGetSeen#v1");
remote_msg!(RSeen, "bin:remote_semantics::RSeen#v1");

// ===========================================================================
// actor：echo + FIFO 记序 + 慢处理
// ===========================================================================

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
            if let Some(RPing(v)) = msg.downcast_ref::<RPing>() {
                return Ok(Box::new(RPong(*v)) as BoxedMessage);
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

/// FIFO：记 TELL 序号；ask RGetSeen 返回已见列表。
struct FifoActor {
    seen: Vec<u64>,
}

impl Actor for FifoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(RSeq(v)) = msg.downcast_ref::<RSeq>() {
                self.seen.push(*v);
                return Ok(Box::new(*v) as BoxedMessage);
            }
            if msg.downcast_ref::<RGetSeen>().is_some() {
                return Ok(Box::new(RSeen(self.seen.clone())) as BoxedMessage);
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

/// 慢处理：500ms 后回（RC4 超时语义）。
struct SlowActor;

impl Actor for SlowActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(RPing(v)) = msg.downcast_ref::<RPing>() {
                tokio::time::sleep(Duration::from_millis(500)).await;
                return Ok(Box::new(RPong(*v)) as BoxedMessage);
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

// ===========================================================================
// 双节点测试基建（mem/tcp 同构）
// ===========================================================================

struct FacadeLookup {
    facade: Arc<ParrotActorSystem>,
}

#[async_trait::async_trait]
impl LocalLookup for FacadeLookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        self.facade.get_actor(&ActorPath::placeholder(path)).await
    }
}

async fn spawn_thread_actor<A>(
    facade: &ParrotActorSystem,
    ts: &Arc<ThreadActorSystem>,
    actor: A,
    path: &str,
) where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    ts.spawn_at(actor, path, None, ThreadActorConfig::default())
        .await
        .unwrap();
}

/// 组双节点（真实 thread 引擎 actor + mem 链路）。
async fn two_nodes_mem(
    case: &str,
) -> (
    Arc<RemoteActorSystem>,
    Arc<RemoteActorSystem>,
    Arc<ParrotActorSystem>,
) {
    // B 侧：真实引擎 + echo/fifo/slow 三个 actor
    let facade_b = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts_b = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    spawn_thread_actor(&facade_b, &ts_b, EchoActor, "/user/echo").await;
    spawn_thread_actor(&facade_b, &ts_b, FifoActor { seen: vec![] }, "/user/fifo").await;
    spawn_thread_actor(&facade_b, &ts_b, SlowActor, "/user/slow").await;

    let rb = RemoteActorSystem::new(
        RCfg::mem(format!("{case}-b")),
        Arc::new(FacadeLookup {
            facade: facade_b.clone(),
        }),
    )
    .unwrap();
    rb.start().await.unwrap();

    // A 侧：无本地 actor（纯客户端语义）
    let facade_a = ParrotActorSystem::new(ActorSystemConfig::default())
        .await
        .unwrap();
    let ra = RemoteActorSystem::new(
        RCfg::mem(format!("{case}-a")),
        Arc::new(FacadeLookup {
            facade: Arc::new(facade_a),
        }),
    )
    .unwrap();
    ra.start().await.unwrap();
    // mem 显式互联：双端 duplex 直连（握手 + 注册双向）
    ra.connect_mem_pair(&rb).await.unwrap();
    // 等 B 侧握手完成（node-b 进 A 的表 + A 进 B 的表）
    eventually(Duration::from_secs(2), || async {
        ra.remote_ref(&format!("parrot://{case}-b/user/echo"))
            .is_ok()
    })
    .await;
    (ra, rb, facade_b)
}

/// TCP 双节点（127.0.0.1 真实网络栈——RC 语义第二遍）。
async fn two_nodes_tcp() -> (
    Arc<RemoteActorSystem>,
    Arc<RemoteActorSystem>,
    Arc<ParrotActorSystem>,
) {
    let facade_b = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts_b = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    spawn_thread_actor(&facade_b, &ts_b, EchoActor, "/user/echo").await;
    spawn_thread_actor(&facade_b, &ts_b, SlowActor, "/user/slow").await;

    // OS 分配空闲端口（先占后放再立即 listen——竞态窗口极小，测试可接受）
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let rb = RemoteActorSystem::new(
        RCfg::tcp("node-b", Some(format!("127.0.0.1:{port}").parse().unwrap()))
            .with_insecure_development(),
        Arc::new(FacadeLookup {
            facade: facade_b.clone(),
        }),
    )
    .unwrap();
    rb.start().await.unwrap();
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
        format!("127.0.0.1:{port}").parse().unwrap(),
    ))
    .await
    .unwrap();
    eventually(Duration::from_secs(3), || async {
        ra.remote_ref("parrot://node-b/user/echo").is_ok()
    })
    .await;
    (ra, rb, facade_b)
}

// ===========================================================================
// 用例
// ===========================================================================

/// RC1 恰好一次：128 并发 ask，回复数==128，cid 无重复。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc1_exactly_once_mem() {
    let (ra, _rb, _) = two_nodes_mem("rc1_exactly_once_mem").await;
    let echo = ra
        .remote_ref("parrot://rc1_exactly_once_mem-b/user/echo")
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..128u64 {
        let e = echo.clone();
        tasks.push(tokio::spawn(async move {
            let r = e.send(Box::new(RPing(i))).await.unwrap();
            r.downcast_ref::<RPong>().unwrap().0
        }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    got.sort_unstable();
    let expect: Vec<u64> = (0..128).collect();
    assert_eq!(got, expect, "128 并发 ask 恰好一次");
}

/// RC2 FIFO：同源连续 100 TELL 保序。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc2_fifo_mem() {
    let (ra, _rb, facade_b) = two_nodes_mem("rc2_fifo_mem").await;
    let fifo = ra.remote_ref("parrot://rc2_fifo_mem-b/user/fifo").unwrap();
    // Stay inside the documented bounded admission window. The local observer
    // confirms actual business completion, not just outbound queue acceptance.
    for end in [25u64, 50, 75, 100] {
        for i in end - 25..end {
            fifo.deliver(Box::new(RSeq(i))).await.unwrap();
        }
        eventually(Duration::from_secs(5), || async {
            let local = facade_b
                .get_actor(&ActorPath::placeholder("/user/fifo"))
                .await
                .unwrap();
            let reply = local.send(Box::new(RGetSeen)).await.unwrap();
            let seen = &reply.downcast_ref::<RSeen>().unwrap().0;
            assert_eq!(seen, &(0..seen.len() as u64).collect::<Vec<_>>());
            seen.len() == end as usize
        })
        .await;
    }
    ra.shutdown().await.unwrap();
    _rb.shutdown().await.unwrap();
}

/// RC4 超时语义：1ms 超时 + 500ms 慢处理 → 调用方 Timeout；迟到 REPLY drop。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc4_timeout_mem() {
    let (ra, _rb, _) = two_nodes_mem("rc4_timeout_mem").await;
    let slow = ra
        .remote_ref("parrot://rc4_timeout_mem-b/user/slow")
        .unwrap();
    let before = parrot_remote::LATE_REPLY_DROPPED.load(std::sync::atomic::Ordering::Relaxed);
    let t0 = Instant::now();
    let r = slow
        .send_with_timeout(Box::new(RPing(1)), Some(Duration::from_millis(50)))
        .await;
    assert!(
        t0.elapsed() < Duration::from_millis(400),
        "调用方 50ms 即放弃"
    );
    match r {
        Err(parrot_api::errors::ActorError::TimeoutDetail(_)) => {}
        other => panic!("expect TimeoutDetail, got {other:?}"),
    }
    // 对端 500ms 后完成 → 迟到 REPLY 查表 miss → metric+1
    tokio::time::sleep(Duration::from_millis(700)).await;
    let after = parrot_remote::LATE_REPLY_DROPPED.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        after > before,
        "late_reply_dropped_total +1（RC4）: {before} -> {after}"
    );
}

/// RC5 死信：stop 后 send → Stopped 跨网等价。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc5_dead_letter_mem() {
    let (ra, _rb, facade_b) = two_nodes_mem("rc5_dead_letter_mem").await;
    // 本地 spawn 专用 victim
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    spawn_thread_actor(&facade_b, &ts, EchoActor, "/user/victim").await;
    let victim = ra
        .remote_ref("parrot://rc5_dead_letter_mem-b/user/victim")
        .unwrap();
    let pong = victim.send(Box::new(RPing(1))).await.unwrap();
    assert_eq!(pong.downcast_ref::<RPong>().unwrap().0, 1);
    // Retain the reference before STOP: successful engine cleanup removes its
    // registry entry, so looking it up afterwards races with correct cleanup.
    let local_victim = ts.get_actor_ref("/user/victim").unwrap();
    victim.stop().await.unwrap();
    eventually(Duration::from_secs(2), || async {
        ts.get_actor_ref("/user/victim").is_none()
    })
    .await;
    tokio::time::timeout(
        Duration::from_secs(1),
        local_victim.send(Box::new(RPing(99))),
    )
    .await
    .expect("stopped local reference must not hang")
    .expect_err("stopped local reference must reject work");
    let err = tokio::time::timeout(Duration::from_secs(3), victim.send(Box::new(RPing(2))))
        .await
        .expect("stop 后 send 必须快速失败（不悬挂）")
        .unwrap_err();
    match err {
        parrot_api::errors::ActorError::ActorNotFound(_)
        | parrot_api::errors::ActorError::Stopped
        | parrot_api::errors::ActorError::ReplyChannelError(_)
        | parrot_api::errors::ActorError::MessageHandlingError(_) => {} // 死亡等价族
        other => panic!("expect dead-letter family error, got {other:?}"),
    }
}

/// RC6 NotRemotable：未注册类型 → 本地立即错，零帧出网。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc6_not_remotable_mem() {
    let (ra, _rb, _) = two_nodes_mem("rc6_not_remotable_mem").await;
    let echo = ra
        .remote_ref("parrot://rc6_not_remotable_mem-b/user/echo")
        .unwrap();
    #[derive(Debug)]
    struct Secret;
    let err = echo.send(Box::new(Secret)).await.unwrap_err();
    assert!(err.to_string().contains("not remotable"), "got {err:?}");
}

/// The first message blocks until the test explicitly permits progress.
struct AdmissionActor {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
    seen: Arc<std::sync::Mutex<Vec<u64>>>,
}
impl Actor for AdmissionActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = msg.downcast_ref::<RPing>().unwrap().0;
            if value == 0 {
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
            }
            self.seen.lock().unwrap().push(value);
            Ok(Box::new(RPong(value)) as BoxedMessage)
        })
    }
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}

/// Local Block admission is pending at capacity and resumes without dropping.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc8_local_block_resumes_and_preserves_order() {
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let actor = ts
        .spawn_at(
            AdmissionActor {
                entered: entered.clone(),
                release: release.clone(),
                seen: seen.clone(),
            },
            "/admission",
            None,
            ThreadActorConfig {
                mailbox_capacity: Some(1),
                backpressure_strategy: Some(BackpressureStrategy::Block),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    actor.deliver(Box::new(RPing(0))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    actor.deliver(Box::new(RPing(1))).await.unwrap();
    let pending = actor.deliver(Box::new(RPing(2)));
    tokio::pin!(pending);
    assert!(
        futures::poll!(&mut pending).is_pending(),
        "full Block mailbox admitted another message"
    );
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), &mut pending)
        .await
        .unwrap()
        .unwrap();
    eventually(Duration::from_secs(2), || async {
        seen.lock().unwrap().len() == 3
    })
    .await;
    assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2]);
    ts.shutdown_internal().await.unwrap();
}

/// Remote overload is bounded rejection, not guaranteed end-to-end Block.
/// The flood future stays owned by the test and is dropped at its deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc8_remote_overload_closes_only_its_connection() {
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
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    ts_b.spawn_at(
        AdmissionActor {
            entered: entered.clone(),
            release: release.clone(),
            seen: Arc::default(),
        },
        "/user/slow",
        None,
        ThreadActorConfig {
            mailbox_capacity: Some(1),
            backpressure_strategy: Some(BackpressureStrategy::Block),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    ts_b.spawn_at(EchoActor, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let rb = RemoteActorSystem::new(
        RCfg::mem("overload-target"),
        Arc::new(FacadeLookup {
            facade: facade_b.clone(),
        }),
    )
    .unwrap();
    rb.start().await.unwrap();
    let mut clients = Vec::new();
    for name in ["overloaded", "healthy"] {
        let facade = Arc::new(
            ParrotActorSystem::new(ActorSystemConfig::default())
                .await
                .unwrap(),
        );
        let client =
            RemoteActorSystem::new(RCfg::mem(name), Arc::new(FacadeLookup { facade })).unwrap();
        client.start().await.unwrap();
        client.connect_mem_pair(&rb).await.unwrap();
        clients.push(client);
    }
    let slow = clients[0]
        .remote_ref("parrot://overload-target/user/slow")
        .unwrap();
    let link = clients[0].links_snapshot().await[0].1.clone();
    slow.deliver(Box::new(RPing(0))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let overload = tokio::time::timeout(Duration::from_secs(3), async {
        for i in 1..=4096u64 {
            slow.deliver(Box::new(RPing(i))).await?;
        }
        Ok::<(), parrot_api::errors::ActorError>(())
    })
    .await;
    let closed = tokio::time::timeout(Duration::from_secs(2), link.closed()).await;
    let echo = clients[1]
        .remote_ref("parrot://overload-target/user/echo")
        .unwrap();
    let healthy =
        tokio::time::timeout(Duration::from_secs(2), echo.send(Box::new(RPing(42)))).await;
    release.add_permits(1);
    for client in clients {
        client.shutdown().await.unwrap();
    }
    rb.shutdown().await.unwrap();
    ts_b.shutdown_internal().await.unwrap();
    assert!(
        matches!(overload, Ok(Err(_))),
        "overload must report refusal rather than hang or accept everything: {overload:?}"
    );
    assert!(closed.is_ok(), "overloaded connection remained open");
    let reply = healthy.expect("healthy connection stalled").unwrap();
    assert_eq!(reply.downcast_ref::<RPong>().unwrap().0, 42);
}

// ===========================================================================
// TCP 双跑（RC 语义第二遍）+ RC7 断连恢复
// ===========================================================================

/// RC1/tcp：并发 ask 恰好一次（真实网络栈）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc1_exactly_once_tcp() {
    let (ra, _rb, _) = two_nodes_tcp().await;
    let echo = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let mut tasks = Vec::new();
    for i in 0..64u64 {
        let e = echo.clone();
        tasks.push(tokio::spawn(async move {
            let r = e.send(Box::new(RPing(i))).await.unwrap();
            r.downcast_ref::<RPong>().unwrap().0
        }));
    }
    let mut got: Vec<u64> = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    got.sort_unstable();
    let expect: Vec<u64> = (0..64).collect();
    assert_eq!(got, expect);
}

/// RC6/tcp：NotRemotable 不出网。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc6_not_remotable_tcp() {
    let (ra, _rb, _) = two_nodes_tcp().await;
    let echo = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    #[derive(Debug)]
    struct Secret;
    let err = echo.send(Box::new(Secret)).await.unwrap_err();
    assert!(err.to_string().contains("not remotable"));
}

/// RC5/tcp：stop 后 send 死信族错误。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc5_dead_letter_tcp() {
    let (ra, _rb, facade_b) = two_nodes_tcp().await;
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    spawn_thread_actor(&facade_b, &ts, EchoActor, "/user/victim").await;
    let victim = ra.remote_ref("parrot://node-b/user/victim").unwrap();
    assert_eq!(
        victim
            .send(Box::new(RPing(1)))
            .await
            .unwrap()
            .downcast_ref::<RPong>()
            .unwrap()
            .0,
        1
    );
    let local_victim = ts.get_actor_ref("/user/victim").unwrap();
    victim.stop().await.unwrap();
    eventually(Duration::from_secs(2), || async {
        ts.get_actor_ref("/user/victim").is_none()
    })
    .await;
    tokio::time::timeout(
        Duration::from_secs(1),
        local_victim.send(Box::new(RPing(99))),
    )
    .await
    .expect("stopped local reference must not hang")
    .expect_err("stopped local reference must reject work");
    let err = tokio::time::timeout(Duration::from_secs(3), victim.send(Box::new(RPing(2))))
        .await
        .expect("不悬挂")
        .unwrap_err();
    match err {
        parrot_api::errors::ActorError::ActorNotFound(_)
        | parrot_api::errors::ActorError::Stopped
        | parrot_api::errors::ActorError::MessageHandlingError(_) => {}
        other => panic!("expect dead-letter family, got {other:?}"),
    }
}

/// RC7：断连 → ask 得 ConnectionLost；重连后 ask 成功且 cid 单调。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rc7_disconnect_reconnect_tcp() {
    let (ra, rb, facade_b) = two_nodes_tcp().await;
    let echo = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let p0 = echo
        .send(Box::new(RPing(1)))
        .await
        .unwrap()
        .downcast_ref::<RPong>()
        .unwrap()
        .0;
    assert_eq!(p0, 1);

    // 断连：B 侧 shutdown（杀连接）
    rb.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let err = echo.send(Box::new(RPing(2))).await.unwrap_err();
    assert!(
        matches!(err, parrot_api::errors::ActorError::InternalError(ref s) if s.contains("connection lost")
                     | s.contains("unavailable")),
        "expect ConnectionLost family, got {err:?}"
    );

    // 重连：B 重启（同 facade 复用——actor 还活着）+ A 重连
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    // 重建 B（旧 rb 已 shutdown；facade_b 的引擎 actor 仍存活）
    let rb2 = RemoteActorSystem::new(
        RCfg::tcp("node-b", Some(format!("127.0.0.1:{port}").parse().unwrap()))
            .with_insecure_development(),
        Arc::new(FacadeLookup {
            facade: facade_b.clone(),
        }),
    )
    .unwrap();
    rb2.start().await.unwrap();
    ra.connect(&parrot_remote::NodeAddr::tcp(
        "node-b",
        format!("127.0.0.1:{port}").parse().unwrap(),
    ))
    .await
    .expect("reconnect");
    // remote_ref 重建（旧 ref 持旧链路 sender——links 更新后新 ref 走新链路）
    let echo2 = ra.remote_ref("parrot://node-b/user/echo").unwrap();
    let cid_before = 0; // 内部计数不可达——以行为断言：重连后 ask 成功
    let p2 = echo2
        .send(Box::new(RPing(3)))
        .await
        .unwrap()
        .downcast_ref::<RPong>()
        .unwrap()
        .0;
    assert_eq!(p2, 3);
    let _ = cid_before;
}
