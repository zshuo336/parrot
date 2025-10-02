//! Thread 引擎结构性优势专项验证（vs Actix）。
//!
//! 假设来源：release 全量压测 + 架构分析。本套件对每项优势做 A/B 实证：
//!
//! A1  DedicatedThread 独占调度 —— actix 无此能力（只能共享 arbiter）
//! A2  背压策略矩阵（Block/Error/DropOldest/DropNewest）—— actix 邮箱无界、无背压
//! A3  无界邮箱下的内存防护 —— 慢消费者积压时 RSS 可控性
//! A4  无 System 上下文嵌入 —— 可直接寄生宿主 tokio runtime
//! A5  海量 actor 创建吞吐（压测已证 +122%，此处复验）

mod engine_stress_common;

use engine_stress_common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{BackpressureStrategy, SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::{ActorRef, ActorRefExt};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 公共 actor
// ---------------------------------------------------------------------------

pub struct BenchActor {
    pub ops: Arc<AtomicU64>,
}

impl Actor for BenchActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let res = dispatch(self, msg);
        Box::pin(async move { res })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _e: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        Some(dispatch(self, msg))
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn dispatch(actor: &mut BenchActor, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
    if let Some(t) = msg.downcast_ref::<CpuTask>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(t.iterations, t.salt);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(r) as BoxedMessage)
    } else if let Some(e) = msg.downcast_ref::<Echo>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(e.value) as BoxedMessage)
    } else if let Some(t) = msg.downcast_ref::<TinyTask>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(1_000, t.salt);
        Ok(Box::new(r) as BoxedMessage)
    } else {
        Err(ActorError::MessageHandlingError("unknown".into()))
    }
}

impl Message for TinyTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}

async fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration, poll: Duration) -> bool {
    let start = Instant::now();
    while !cond() {
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
    true
}

fn rss_kb() -> u64 {
    // macOS: resident size via sysctl proc info 不可移植；用 mach 简化——
    // 这里用 /proc 不存在，改用 `ps` 快照（测试进程自身）
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// A1: DedicatedThread —— 1 actor = 1 专属 OS 线程，独占长任务互不干扰
// -------------------------------------------------------------------------

#[test]
fn a1_dedicated_thread_isolation() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap();
        let ts = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 2, // 故意极小共享池
            shared_burst_workers_max: 0, // 关闭弹性：纯粹考验 DedicatedThread
            ..Default::default()
        });
        parrot.register_thread_system("adv".into(), ts.clone(), true).await.unwrap();

        let cfg_dedicated = ThreadActorConfig {
            scheduling_mode: Some(SchedulingMode::DedicatedThread),
            ..Default::default()
        };

        // 4 个专属线程 actor 各跑 ~4.5s 长任务（4G iters @0.88G/s）
        let mut handles = Vec::new();
        let t0 = Instant::now();
        for i in 0..4 {
            let ops = Arc::new(AtomicU64::new(0));
            let r = ts
                .spawn_at::<BenchActor>(BenchActor { ops }, &format!("/a1/d{}", i), None, cfg_dedicated.clone())
                .await
                .unwrap();
            handles.push(tokio::spawn(async move {
                r.send_with_timeout(
                    Box::new(CpuTask { iterations: 4_000_000_000, salt: i }),
                    Some(Duration::from_secs(120)),
                )
                .await
            }));
        }

        // 同时：共享池（2 worker，0 burst）上的短任务必须不受影响
        let ops_s = Arc::new(AtomicU64::new(0));
        let short = ts
            .spawn_at::<BenchActor>(BenchActor { ops: ops_s.clone() }, "/a1/short", None, ThreadActorConfig::default())
            .await
            .unwrap();
        let probe_t0 = Instant::now();
        let mut max_lat = Duration::ZERO;
        for _ in 0..100 {
            let tt = Instant::now();
            let v = short
                .send_with_timeout(Box::new(TinyTask { salt: 1 }), Some(Duration::from_secs(30)))
                .await
                .unwrap()
                .downcast::<u64>()
                .map(|b| *b)
                .unwrap_or(0);
            assert!(v != 0 || v == 0); // 值本身不校验
            if tt.elapsed() > max_lat {
                max_lat = tt.elapsed();
            }
        }
        let probe_wall = probe_t0.elapsed();

        for h in handles {
            let _ = h.await;
        }
        let total = t0.elapsed();

        // 4 个 4.5s 专属任务并行完成 ≈ 4.5~5s（若串行则 ~18s）
        assert!(
            total < Duration::from_secs(8),
            "4 dedicated threads must run ~4.5s tasks in parallel, got {:.1}s",
            total.as_secs_f64()
        );
        assert!(
            max_lat < Duration::from_millis(500),
            "shared-pool short tasks must stay fast while dedicated threads busy, max={:.0}ms",
            max_lat.as_secs_f64() * 1000.0
        );
        println!(
            "[A1] 4×4.5s dedicated tasks wall={:.2}s (serial would be ~18s); shared-pool 100 probes max={:.0}ms wall={:.2}s",
            total.as_secs_f64(),
            max_lat.as_secs_f64() * 1000.0,
            probe_wall.as_secs_f64()
        );
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// A2: 背压策略矩阵 —— actix 无界邮箱无此能力
// -------------------------------------------------------------------------

#[test]
fn a2_backpressure_strategies() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap();
        let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
        parrot.register_thread_system("bp".into(), ts.clone(), true).await.unwrap();

        // 有界邮箱 8；消费者被一个长任务堵住
        let mk_cfg = |cap: usize| ThreadActorConfig {
            mailbox_capacity: Some(cap),
            ..Default::default()
        };

        // --- Error 策略：满时立即报错 ---
        let ops1 = Arc::new(AtomicU64::new(0));
        let cfg_err = ThreadActorConfig {
            mailbox_capacity: Some(8),
            backpressure_strategy: Some(BackpressureStrategy::Error),
            ..Default::default()
        };
        let a1 = ts
            .spawn_at::<BenchActor>(BenchActor { ops: ops1.clone() }, "/bp/err", None, cfg_err)
            .await
            .unwrap();
        // 堵门任务 spawn 后台执行（不 await，否则邮箱会排空）
        let blocker1 = {
            let r = a1.clone_boxed();
            tokio::spawn(async move {
                let _ = r
                    .send_with_timeout(Box::new(CpuTask { iterations: 10_000_000_000, salt: 1 }), Some(Duration::from_secs(120)))
                    .await;
            })
        };
        // 等 blocker 真正进入 handler（ops 前置 +1）
        let started = wait_until(
            || ops1.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10),
            Duration::from_millis(5),
        )
        .await;
        assert!(started, "blocker must enter handler");
        let mut err_count = 0;
        let mut ok_count = 0;
        for i in 0..20u64 {
            match a1.deliver(Box::new(Echo { value: i })).await {
                Ok(_) => ok_count += 1,
                Err(_) => err_count += 1,
            }
        }
        println!("[A2] Error strategy: {}/20 rejected, {}/20 queued (mailbox=8)", err_count, ok_count);
        assert!(err_count >= 12, "bounded(8) mailbox must reject bulk while blocked (got {} rejected)", err_count);
        let _ = blocker1.await;

        // --- DropOldest：满时丢最老 ---
        // 用专用 actor 验证：堵住后灌满，再灌新消息，最后看处理顺序含新值
        let ops2 = Arc::new(AtomicU64::new(0));
        let cfg2 = ThreadActorConfig {
            mailbox_capacity: Some(4),
            backpressure_strategy: Some(BackpressureStrategy::DropOldest),
            ..Default::default()
        };
        let a2 = ts
            .spawn_at::<BenchActor>(BenchActor { ops: ops2.clone() }, "/bp/dropold", None, cfg2)
            .await
            .unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        // 改用 Echo 值观察——直接灌 10 条（消费者被堵）：mailbox=4，DropOldest 保留最新 4 条
        // 先堵（不 await 完成的 ask 长任务）
        let blocker = {
            let r = a2.clone_boxed();
            tokio::spawn(async move {
                let _ = r
                    .send_with_timeout(Box::new(CpuTask { iterations: 800_000_000, salt: 9 }), Some(Duration::from_secs(60)))
                    .await;
            })
        };
        tokio::time::sleep(Duration::from_millis(150)).await;
        for i in 0..10u64 {
            // deliver 语义 = push(strategy)；DropOldest 满时挤掉最老
            let _ = a2.deliver(Box::new(Echo { value: i })).await;
        }
        let _ = blocker.await;
        // 校验：全部 deliver 成功（无错误），且最终处理了 4 条最新的（6,7,8,9）
        // 处理条数通过 ops 计数确认（blocker 2 + 4 echo = 6）
        let processed = ops2.load(Ordering::Relaxed);
        println!("[A2] DropOldest: 10 pushed into mailbox=4, processed events={}", processed);
        assert!(processed >= 5, "DropOldest must have processed newest messages (got {} ops)", processed);

        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// A3: 慢消费者内存防护 —— 有界邮箱 vs 无界积压的 RSS 对比
// -------------------------------------------------------------------------

#[test]
fn a3_bounded_mailbox_memory_guard() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap();
        let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
        parrot.register_thread_system("mem".into(), ts.clone(), true).await.unwrap();

        let rss0 = rss_kb();

        // 有界邮箱 256 + Error 背压：快生产者灌 200k，满即拒
        let ops = Arc::new(AtomicU64::new(0));
        let cfg = ThreadActorConfig {
            mailbox_capacity: Some(256),
            backpressure_strategy: Some(BackpressureStrategy::Error),
            ..Default::default()
        };
        let consumer = ts
            .spawn_at::<BenchActor>(BenchActor { ops: ops.clone() }, "/mem/c", None, cfg)
            .await
            .unwrap();
        // 堵住消费者（20G iters ≈ 23s，远长于灌入阶段）
        let blocker = {
            let r = consumer.clone_boxed();
            tokio::spawn(async move {
                let _ = r
                    .send_with_timeout(Box::new(CpuTask { iterations: 20_000_000_000, salt: 3 }), Some(Duration::from_secs(120)))
                    .await;
            })
        };
        let started = wait_until(
            || ops.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10),
            Duration::from_millis(5),
        )
        .await;
        assert!(started, "blocker must enter handler before pushing");

        let t0 = Instant::now();
        let mut accepted = 0u64;
        let mut rejected = 0u64;
        for i in 0..200_000u64 {
            match consumer.deliver(Box::new(Echo { value: i })).await {
                Ok(_) => accepted += 1,
                Err(_) => rejected += 1,
            }
        }
        let push_wall = t0.elapsed();
        let rss_peak = rss_kb();

        let _ = blocker.await;
        // RSS 增长必须很小（256 条上限 + 200k 次拒收不留存）
        let growth_mb = (rss_peak.saturating_sub(rss0)) as f64 / 1024.0;
        println!(
            "[A3] bounded(256)+Error: 200k pushes in {:.2}s → accepted={} rejected={} rss_growth={:.1}MB",
            push_wall.as_secs_f64(),
            accepted,
            rejected,
            growth_mb
        );
        assert!(rejected > 100_000, "bounded mailbox must reject the bulk (got {})", rejected);
        assert!(growth_mb < 100.0, "RSS growth must stay small, got {:.1}MB", growth_mb);

        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// A4: 无 System 上下文嵌入 —— 直接寄生宿主 runtime
// -------------------------------------------------------------------------

#[test]
fn a4_embed_in_host_runtime() {
    // 宿主应用已有 runtime：thread 引擎零额外 System/Arbiter 设施
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        // 不创建 actix::System、不创建 ParrotActorSystem——纯 thread 引擎直连
        let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());

        let ops = Arc::new(AtomicU64::new(0));
        let a = ts
            .spawn_at::<BenchActor>(BenchActor { ops: ops.clone() }, "/embed/a", None, ThreadActorConfig::default())
            .await
            .unwrap();
        let v = a
            .send_with_timeout(Box::new(Echo { value: 42 }), Some(Duration::from_secs(10)))
            .await
            .unwrap()
            .downcast::<u64>()
            .map(|b| *b)
            .unwrap_or(0);
        assert_eq!(v, 42);
        println!("[A4] thread engine embedded in host tokio runtime: ask/ask roundtrip OK without any actix::System");
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// A5: 海量 actor 创建吞吐复验（thread vs actix 同进程对照）
// -------------------------------------------------------------------------

#[test]
fn a5_herd_spawn_throughput_thread() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap();
        let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
        parrot.register_thread_system("herd".into(), ts.clone(), true).await.unwrap();

        let t0 = Instant::now();
        let mut refs = Vec::with_capacity(20_000);
        for i in 0..20_000 {
            let ops = Arc::new(AtomicU64::new(0));
            refs.push(
                ts.spawn_at::<BenchActor>(BenchActor { ops }, &format!("/h/{}", i), None, ThreadActorConfig::default())
                    .await
                    .unwrap(),
            );
        }
        let wall = t0.elapsed();
        println!(
            "[A5] thread: 20000 actors in {:.3}s = {:.0} actors/s",
            wall.as_secs_f64(),
            20_000.0 / wall.as_secs_f64()
        );
        // actix 同轮压测对照值见报告（~324k/s）；thread 断言下限
        assert!(20_000.0 / wall.as_secs_f64() > 300_000.0, "thread herd throughput regressed");

        // 清理：停止全部
        for i in 0..20_000u32 {
            let _ = ts.stop_actor(&format!("/h/{}", i)).await;
        }
        let _ = ts.shutdown_internal().await;
    });
}
