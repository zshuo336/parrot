//! Sharded（亲和性）调度器专项基准 + 新优势域验证（ADR-14）。
//!
//! S1 亲和性 ask 吞吐：Sharded vs SharedPool 同 actor 数同消息量
//! S2 亲和域缓存局部性：同 key 消息序列（模拟会话粘滞）
//! S3 分片隔离：单分片过载不影响其他分片延迟
//! S4 分配开销验证：ask 一次分配（ADR-13）后 seq-ask 提升复验

mod engine_stress_common;

use engine_stress_common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRefExt;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::time::{Duration, Instant};

// 复用压测 BenchActor 形态（本地重定义，逻辑对等）
struct BenchActor {
    ops: Arc<std::sync::atomic::AtomicU64>,
}

impl Actor for BenchActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let r = dispatch(self, m);
        Box::pin(async move { r })
    }
    fn receive_message_with_engine<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        Some(dispatch(self, m))
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

fn dispatch(a: &mut BenchActor, m: BoxedMessage) -> ActorResult<BoxedMessage> {
    if let Some(e) = m.downcast_ref::<Echo>() {
        a.ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Box::new(e.value) as BoxedMessage)
    } else if let Some(t) = m.downcast_ref::<CpuTask>() {
        let r = burn_cpu(t.iterations, t.salt);
        Ok(Box::new(r) as BoxedMessage)
    } else {
        Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into()))
    }
}

use parrot_api::address::ActorRef as ActorRefTrait;
use parrot_api::errors::ActorError;

impl Message for Echo {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for CpuTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}

async fn ask_echo(r: &parrot::thread::address::ThreadActorRef<BenchActor>, v: u64) -> u64 {
    let rep = r.ask(Box::new(Echo { value: v }) as BoxedMessage).await.unwrap();
    *rep.downcast::<u64>().unwrap()
}

async fn setup() -> (ParrotActorSystem, Arc<ThreadActorSystem>) {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap();
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    parrot.register_thread_system("shardbench".into(), ts.clone(), true).await.unwrap();
    (parrot, ts)
}

fn cfg_sharded(key: &str) -> ThreadActorConfig {
    ThreadActorConfig {
        scheduling_mode: Some(SchedulingMode::Sharded { affinity_key: key.into() }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// S1: 亲和性 ask 吞吐 —— Sharded vs SharedPool
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn s1_sharded_vs_shared_ask_throughput() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(8).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup().await;
        const N_ACTORS: usize = 8;
        const PER: u64 = 20_000;

        // --- Sharded ---
        let mut shard_refs = Vec::new();
        for i in 0..N_ACTORS {
            shard_refs.push(
                ts.spawn_at::<BenchActor>(BenchActor { ops: Arc::new(std::sync::atomic::AtomicU64::new(0)) },
                    &format!("/s1/sh/{}", i), None, cfg_sharded(&format!("domain-{}", i % 4))).await.unwrap()
            );
        }
        // warmup
        for r in &shard_refs { for _ in 0..200 { ask_echo(r, 1).await; } }
        let t0 = Instant::now();
        let mut hs = Vec::new();
        for (i, r) in shard_refs.into_iter().enumerate() {
            hs.push(tokio::spawn(async move {
                for k in 0..PER {
                    let v = ask_echo(&r, i as u64 * 1000 + k).await;
                    assert_eq!(v, i as u64 * 1000 + k);
                }
            }));
        }
        for h in hs { h.await.unwrap(); }
        let shard_wall = t0.elapsed();

        // --- SharedPool ---
        let mut pool_refs = Vec::new();
        for i in 0..N_ACTORS {
            pool_refs.push(
                ts.spawn_at::<BenchActor>(BenchActor { ops: Arc::new(std::sync::atomic::AtomicU64::new(0)) },
                    &format!("/s1/pool/{}", i), None, ThreadActorConfig::default()).await.unwrap()
            );
        }
        for r in &pool_refs { for _ in 0..200 { ask_echo(r, 1).await; } }
        let t1 = Instant::now();
        let mut hs = Vec::new();
        for (i, r) in pool_refs.into_iter().enumerate() {
            hs.push(tokio::spawn(async move {
                for k in 0..PER {
                    let v = ask_echo(&r, i as u64 * 1000 + k).await;
                    assert_eq!(v, i as u64 * 1000 + k);
                }
            }));
        }
        for h in hs { h.await.unwrap(); }
        let pool_wall = t1.elapsed();

        let total = (N_ACTORS as u64) * PER;
        println!(
            "[S1] sharded: {} msgs in {:.3}s = {:.0}k/s | shared-pool: {:.3}s = {:.0}k/s | ratio {:.2}x",
            total, shard_wall.as_secs_f64(), total as f64 / shard_wall.as_secs_f64() / 1000.0,
            pool_wall.as_secs_f64(), total as f64 / pool_wall.as_secs_f64() / 1000.0,
            pool_wall.as_secs_f64() / shard_wall.as_secs_f64(),
        );
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// S2: 分片隔离 —— 一个分片跑重 CPU，其他分片 ask 延迟不受影响
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn s2_shard_isolation_under_overload() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(8).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup().await;

        // victim 分片：domain-heavy 上跑 4 个持续重 CPU actor
        let mut heavy = Vec::new();
        for i in 0..4 {
            heavy.push(
                ts.spawn_at::<BenchActor>(BenchActor { ops: Arc::new(std::sync::atomic::AtomicU64::new(0)) },
                    &format!("/s2/heavy/{}", i), None, cfg_sharded("domain-heavy")).await.unwrap()
            );
        }
        // probe 分片：domain-probe 上的轻量 echo
        let probe = ts.spawn_at::<BenchActor>(BenchActor { ops: Arc::new(std::sync::atomic::AtomicU64::new(0)) },
            "/s2/probe", None, cfg_sharded("domain-probe")).await.unwrap();

        // 启动重载（fire-and-forget 长任务 × 4，同分片彻底打满）
        let mut hh = Vec::new();
        for (i, r) in heavy.into_iter().enumerate() {
            let rb = r.clone_boxed();
            hh.push(tokio::spawn(async move {
                let _ = rb.send_with_timeout(
                    Box::new(CpuTask { iterations: 8_000_000_000, salt: i as u64 }),
                    Some(Duration::from_secs(120))).await;
            }));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 探针：100 次 echo ask
        let mut max_lat = Duration::ZERO;
        let mut lats = Vec::new();
        for i in 0..100u64 {
            let t = Instant::now();
            let v = ask_echo(&probe, i).await;
            assert_eq!(v, i);
            let e = t.elapsed();
            lats.push(e.as_micros() as u64);
            if e > max_lat { max_lat = e; }
        }
        let st = latency_stats(lats.iter().map(|&x| x as u128).collect());
        println!(
            "[S2] probe on separate shard while 4×~9s CPU saturate another: p50={:.0}µs p99={:.0}µs max={:.0}µs",
            st.p50_us, st.p99_us, st.max_us
        );
        assert!(max_lat < Duration::from_millis(50), "probe shard must be isolated, max={:?}", max_lat);
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// S4: ask 分配优化复验（ADR-13 后 seq-ask 吞吐对比历史值）
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn s4_alloc_optimized_seq_ask() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(8).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup().await;
        let a = ts.spawn_at::<BenchActor>(BenchActor { ops: Arc::new(std::sync::atomic::AtomicU64::new(0)) },
            "/s4/echo", None, ThreadActorConfig::default()).await.unwrap();
        for _ in 0..2000 { ask_echo(&a, 1).await; } // warmup
        let mut lats = Vec::new();
        let t0 = Instant::now();
        for i in 0..10_000u64 {
            let t = Instant::now();
            let v = ask_echo(&a, i).await;
            assert_eq!(v, i);
            lats.push(t.elapsed().as_micros());
        }
        let wall = t0.elapsed();
        let st = latency_stats(lats);
        println!(
            "[S4] seq-ask-10k after ADR-13: {:.3}s = {:.0}k/s (p50={:.0}µs p99={:.0}µs) | baseline seq-ask-1k was ~76-85k/s",
            wall.as_secs_f64(), 10_000.0 / wall.as_secs_f64() / 1000.0, st.p50_us, st.p99_us
        );
        let _ = ts.shutdown_internal().await;
    });
}
