//! E2 集成（DEV_03 §3.3）：显式 WAL 与 owned outbox 的真实 Thread/mem 投递。
//!
//! - `durable_offline_replay`：追加 100 条后重开 WAL，由测试显式读取并投递，
//!   断言完整业务序列及重复投递去重；不覆盖自动代理或网络 ACK。
//! - `durable_offline_5min_real`（#[ignore]）：5min WAL 积压保留实验，
//!   不覆盖真实断网、重连或 DEV_00 §4.2 的自动恢复门禁。
//! - `owned_outbox_recovers_and_confirms_real_thread_actor_execution`：自动恢复100条，
//!   真实Thread actor处理完成后回执驱动WAL清理，不手动connect/replay/ack。
//! - `wal_ack_latency_report`：显式运行的磁盘同步延迟报告（不裁剪尾部）
//!
//! 拓扑：测试驱动的 cloud 发送端 --mem--> edge。

mod common;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_remote::durable::{DedupDecision, DedupLimits, DedupTable, Wal, WalRecord};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig as RCfg};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct DTell(pub u64);

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

remote_msg!(DTell, "bin:durable::DTell#v1");
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RetryTell {
    seq: u64,
    fail: bool,
}
remote_msg!(RetryTell, "bin:durable::RetryTell#v1");
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Processed(Vec<u64>);
remote_msg!(Processed, "bin:durable::Processed#v1");

/// 端侧收件 actor：去重 + 计数（处理完成即 ACK 语义源）。
struct EdgeSink {
    dedup: DedupTable,
    processed: Vec<u64>,
}

impl Actor for EdgeSink {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<Processed>() {
                return Ok(Box::new(Processed(self.processed.clone())) as BoxedMessage);
            }
            let attempt = msg
                .downcast_ref::<DTell>()
                .map(|m| (m.0, false))
                .or_else(|| msg.downcast_ref::<RetryTell>().map(|m| (m.seq, m.fail)));
            if let Some((seq, fail)) = attempt {
                let decision = self.dedup.begin("cloud-proxy", seq).map_err(|e| {
                    parrot_api::errors::ActorError::MessageHandlingError(e.to_string())
                })?;
                if let DedupDecision::Process(permit) = decision {
                    tokio::task::yield_now().await;
                    if fail {
                        return Err(parrot_api::errors::ActorError::MessageHandlingError(
                            "injected business failure before effect".into(),
                        ));
                    }
                    self.processed.push(seq);
                    permit.commit();
                }
                return Ok(Box::new(DTell(seq)) as BoxedMessage);
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

async fn edge_node(id: &str) -> (Arc<RemoteActorSystem>, Arc<ParrotActorSystem>) {
    edge_node_with_dedup(id, DedupTable::new()).await
}

async fn edge_node_with_dedup(
    id: &str,
    dedup: DedupTable,
) -> (Arc<RemoteActorSystem>, Arc<ParrotActorSystem>) {
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    ts.spawn_at(
        EdgeSink {
            dedup,
            processed: vec![],
        },
        "/user/sink",
        None,
        ThreadActorConfig {
            supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let rs = RemoteActorSystem::new(
        RCfg::mem(id.to_string()),
        Arc::new(FacadeLookup {
            facade: facade.clone(),
        }),
    )
    .unwrap();
    rs.start().await.unwrap();
    (rs, facade)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_receiver_failure_and_capacity_are_retryable() {
    let table = DedupTable::with_limits(DedupLimits {
        max_pending_sequences: 2,
        ..Default::default()
    })
    .unwrap();
    let (edge, edge_facade) = edge_node_with_dedup("retry-edge", table).await;
    let (source, source_facade) = edge_node("retry-source").await;
    source.connect_mem_pair(&edge).await.unwrap();
    let sink = source.remote_ref("parrot://retry-edge/user/sink").unwrap();
    assert!(
        sink.send(Box::new(RetryTell { seq: 2, fail: true }))
            .await
            .is_err()
    );
    let snapshot = sink.send(Box::new(Processed(vec![]))).await.unwrap();
    assert!(snapshot.downcast_ref::<Processed>().unwrap().0.is_empty());
    for seq in [2, 3] {
        let reply = sink.send(Box::new(DTell(seq))).await.unwrap();
        assert_eq!(reply.downcast_ref::<DTell>(), Some(&DTell(seq)));
    }
    // This resource rejection must not run business code or acknowledge seq 4.
    assert!(sink.send(Box::new(DTell(4))).await.is_err());
    // The missing prefix can still reserve while the gap budget is full. Its
    // failure releases the reservation, then its successful retry reclaims gaps.
    assert!(
        sink.send(Box::new(RetryTell { seq: 1, fail: true }))
            .await
            .is_err()
    );
    for seq in [1, 4, 2, 3, 4] {
        let reply = sink.send(Box::new(DTell(seq))).await.unwrap();
        assert_eq!(reply.downcast_ref::<DTell>(), Some(&DTell(seq)));
    }
    let snapshot = sink.send(Box::new(Processed(vec![]))).await.unwrap();
    assert_eq!(
        snapshot.downcast_ref::<Processed>().unwrap().0,
        [2, 3, 1, 4]
    );
    source.shutdown().await.unwrap();
    edge.shutdown().await.unwrap();
    drop(source_facade);
    drop(edge_facade);
}

/// durable_offline_replay：WAL 断线积压 → 重连按序重放 → 端侧去重零重复。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_offline_replay() {
    let dir = std::env::temp_dir().join(format!("parrot-durable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let wal = Wal::open(&dir).unwrap();

    // “断线期间”云端积压 100 条 tell（WAL 追加）
    for seq in 1..=100u64 {
        wal.append(WalRecord {
            endpoint: "edge-1".into(),
            sender: "cloud-proxy".into(),
            seq,
            path: "parrot://edge-1/user/sink".into(),
            type_key: "bin:durable::DTell#v1".into(),
            payload: parrot_api::message::serde_remote_serialize(&DTell(seq))
                .unwrap()
                .into(),
        })
        .unwrap();
    }
    wal.sync().unwrap();
    assert_eq!(wal.pending_len().unwrap(), 100);

    // “重连”：新 proxy 实例（重启等价——WAL 恢复）
    drop(wal);
    let wal2 = Wal::open(&dir).unwrap();
    let replay = wal2.replay("edge-1").unwrap();
    assert_eq!(replay.len(), 100, "all durable tells survive offline");

    // 重放到端侧（按 seq 序）——首达全处理
    let (edge_rs, edge_facade) = edge_node("edge-1").await;
    let (proxy_rs, proxy_facade) = edge_node("cloud-proxy").await;
    proxy_rs.connect_mem_pair(&edge_rs).await.unwrap();
    let sink = proxy_rs.remote_ref("parrot://edge-1/user/sink").unwrap();
    for rec in &replay {
        let msg: DTell = parrot_api::message::serde_remote_deserialize(&rec.payload).unwrap();
        sink.send(Box::new(msg)).await.unwrap();
    }

    // 模拟重复投递（重放窗口重叠——QoS1 语义）：再投一遍全部 100 条
    for rec in &replay {
        let msg: DTell = parrot_api::message::serde_remote_deserialize(&rec.payload).unwrap();
        let _ = sink.send(Box::new(msg)).await.unwrap();
    }

    let observed = sink.send(Box::new(Processed(vec![]))).await.unwrap();
    assert_eq!(
        observed.downcast_ref::<Processed>().unwrap().0,
        (1..=100).collect::<Vec<_>>(),
        "every sequence is processed once even after duplicate remote delivery"
    );

    // ACK 水位推进 → WAL 清空
    wal2.ack("edge-1", "cloud-proxy", 100).unwrap();
    assert_eq!(wal2.pending_len().unwrap(), 0, "watermark drains wal");
    wal2.sync().unwrap();
    drop(wal2);
    let recovered = Wal::open(&dir).unwrap();
    assert!(recovered.replay("edge-1").unwrap().is_empty());
    drop(recovered);
    proxy_rs.shutdown().await.unwrap();
    edge_rs.shutdown().await.unwrap();
    drop(proxy_facade);
    drop(edge_facade);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Five-minute WAL retention experiment; no network fault or automatic proxy is exercised.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "5min WAL retention experiment; NOT the automatic offline/reconnect acceptance gate"]
async fn durable_offline_5min_real() {
    let dir = std::env::temp_dir().join("parrot-durable-5min");
    let _ = std::fs::remove_dir_all(&dir);
    let wal = Wal::open(&dir).unwrap();
    // 真实 5min：每 3s 一条 → 100 条
    let t0 = Instant::now();
    for seq in 1..=100u64 {
        wal.append(WalRecord {
            endpoint: "edge-1".into(),
            sender: "cloud-proxy".into(),
            seq,
            path: String::new(),
            type_key: "bin:durable::DTell#v1".into(),
            payload: parrot_api::message::serde_remote_serialize(&DTell(seq))
                .unwrap()
                .into(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(t0.elapsed() >= Duration::from_secs(290), "real 5min span");
    assert_eq!(wal.replay("edge-1").unwrap().len(), 100);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Explicit disk latency experiment; no percentile trimming or universal
/// filesystem latency assertion. Run in isolation on a documented target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "disk latency experiment: run in isolation with --release --ignored --nocapture"]
async fn wal_ack_latency_report() {
    let dir = std::env::temp_dir().join(format!("parrot-ack-lat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let wal = Wal::open(&dir).unwrap();

    let rec = WalRecord {
        endpoint: "e".into(),
        sender: "s".into(),
        seq: 1,
        path: String::new(),
        type_key: "k".into(),
        payload: parrot_remote::bytes::Bytes::from_static(b"x"),
    };

    // 预热（页缓存 + 分配器 steady state）
    for i in 0..50u64 {
        wal.append(WalRecord {
            seq: 1 + i,
            ..rec.clone()
        })
        .unwrap();
        wal.flush().unwrap();
        wal.ack("e", "s", 1 + i).unwrap();
    }

    // 无 ACK 基线：append+flush
    let mut base = Vec::new();
    for i in 0..200u64 {
        let t0 = Instant::now();
        wal.append(WalRecord {
            seq: 51 + i,
            ..rec.clone()
        })
        .unwrap();
        wal.flush().unwrap();
        base.push(t0.elapsed().as_nanos() as u64);
    }
    // 有 ACK 路径：append+flush+ack
    let mut with_ack = Vec::new();
    for i in 0..200u64 {
        let t0 = Instant::now();
        wal.append(WalRecord {
            seq: 251 + i,
            ..rec.clone()
        })
        .unwrap();
        wal.flush().unwrap();
        wal.ack("e", "s", 251 + i).unwrap();
        with_ack.push(t0.elapsed().as_nanos() as u64);
    }
    base.sort_unstable();
    with_ack.sort_unstable();
    let b99 = base[(base.len() * 99).div_ceil(100) - 1];
    let a99 = with_ack[(with_ack.len() * 99).div_ceil(100) - 1];
    println!("WAL append+sync p99={b99}ns; append+sync+ack p99={a99}ns (untrimmed)");
    assert_eq!(wal.pending_len().unwrap(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Exercise the automatic worker through a real Thread engine, without test-side
/// WAL replay, manual connect, manual ACK, or sender-side decoding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_outbox_recovers_and_confirms_real_thread_actor_execution() {
    use parrot_remote::durable::{DurableOutbox, OutboxConfig};
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let directory = Directory(std::env::temp_dir().join(format!(
        "parrot-owned-outbox-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )));
    std::fs::create_dir(&directory.0).unwrap();
    let (source, source_facade) = edge_node("owned-outbox-source").await;
    let peer = parrot_remote::NodeAddr::mem("owned-outbox-edge");
    let config = OutboxConfig {
        retry_min: Duration::from_millis(10),
        retry_max: Duration::from_millis(50),
        ..Default::default()
    };
    let mut outbox = DurableOutbox::open(
        &directory.0,
        &source,
        peer.clone(),
        "/user/sink",
        "session",
        config.clone(),
    )
    .await
    .unwrap();
    for seq in 1..=100 {
        let bytes = parrot_api::message::serde_remote_serialize(&DTell(seq)).unwrap();
        assert_eq!(
            outbox
                .handle()
                .enqueue("bin:durable::DTell#v1", &bytes)
                .await
                .unwrap(),
            seq
        );
    }
    assert_eq!(outbox.handle().status().pending, 100);
    outbox.close().await.unwrap();
    let (edge, edge_facade) = edge_node("owned-outbox-edge").await;
    let mut outbox =
        DurableOutbox::open(&directory.0, &source, peer, "/user/sink", "session", config)
            .await
            .unwrap();
    tokio::time::timeout(Duration::from_secs(15), outbox.handle().wait_acked(100))
        .await
        .unwrap()
        .unwrap();
    let reference = source
        .remote_ref("parrot://owned-outbox-edge/user/sink")
        .unwrap();
    let observed = reference
        .send_with_timeout(Box::new(Processed(vec![])), Some(Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(
        observed.downcast_ref::<Processed>().unwrap().0,
        (1..=100).collect::<Vec<_>>()
    );
    assert_eq!(outbox.handle().status().pending, 0);
    outbox.close().await.unwrap();
    assert_eq!(Wal::open(&directory.0).unwrap().pending_len().unwrap(), 0);
    source.shutdown().await.unwrap();
    edge.shutdown().await.unwrap();
    drop(source_facade);
    drop(edge_facade);
}
