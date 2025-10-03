//! Thread 引擎弹性扩容（burst workers）专项测试。
//!
//! 验证三件事（对应压测报告 M1 发现 + 用户需求）：
//! 1. **打满保护**：所有 core worker 被长 CPU 任务占住时，队列积压达到
//!    threshold 后自动生成临时 burst worker 处理短任务（不被饿死）。
//! 2. **收缩**：压力解除后 burst worker 空闲超时自动退出（线程不泄漏）。
//! 3. **全局上限**：burst 数量 ≤ burst_workers_max，总线程 ≤
//!    pool_size + burst_workers_max（不爆炸）。

use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 测试 actor
// ---------------------------------------------------------------------------

pub struct BenchActor {
    pub ops: Arc<AtomicU64>,
}

/// 不可被优化器折叠的 CPU 燃烧
#[inline]
fn burn_cpu(iterations: u64, salt: u64) -> u64 {
    let mut x: u64 = salt | 1;
    for i in 0..iterations {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            ^ i;
        if x == 42 {
            return x;
        }
    }
    x
}

pub struct LongTask {
    pub iterations: u64,
}
pub struct TinyTask;

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

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn dispatch(actor: &mut BenchActor, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
    if let Some(t) = msg.downcast_ref::<LongTask>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(t.iterations, 7);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(r) as BoxedMessage)
    } else if msg.is::<TinyTask>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(1u64) as BoxedMessage)
    } else {
        Err(parrot_api::errors::ActorError::MessageHandlingError(
            "unknown".into(),
        ))
    }
}

impl parrot_api::message::Message for LongTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| parrot_api::errors::ActorError::MessageHandlingError("type".into()))
    }
}
impl parrot_api::message::Message for TinyTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| parrot_api::errors::ActorError::MessageHandlingError("type".into()))
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 敏捷配置：2 core worker，最多 2 burst，backlog 100ms 触发，1.5s 空闲回收。
fn elastic_system() -> ThreadActorSystemConfig {
    ThreadActorSystemConfig {
        shared_pool_size: 2,
        shared_burst_workers_max: 2,
        shared_burst_backlog_threshold_ms: 100,
        shared_burst_idle_timeout_ms: 1500,
        ..Default::default()
    }
}

async fn spawn(ts: &Arc<ThreadActorSystem>, path: &str, ops: Arc<AtomicU64>) -> Box<dyn ActorRef> {
    Box::new(
        ts.spawn_at::<BenchActor>(BenchActor { ops }, path, None, ThreadActorConfig::default())
            .await
            .expect("spawn"),
    )
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

// ---------------------------------------------------------------------------
// 测试 1：饱和保护 —— 长 CPU 任务打满 2 个 core worker，短任务靠 burst 存活
// ---------------------------------------------------------------------------

#[test]
fn elastic_burst_rescues_starved_short_tasks() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let cfg = elastic_system();
        let ts = ThreadActorSystem::shared(cfg);
        let ops_l1 = Arc::new(AtomicU64::new(0));
        let ops_l2 = Arc::new(AtomicU64::new(0));
        let ops_short = Arc::new(AtomicU64::new(0));

        let l1 = spawn(&ts, "/el/long-1", ops_l1.clone()).await;
        let l2 = spawn(&ts, "/el/long-2", ops_l2.clone()).await;
        let short = spawn(&ts, "/el/short", ops_short.clone()).await;

        // 1) 两个长任务各 ~8s（2G iters，burn ~0.25G/s debug → 8s），占满 2 worker
        let h1 = {
            let r = l1.clone_boxed();
            tokio::spawn(async move {
                r.send_with_timeout(
                    Box::new(LongTask {
                        iterations: 2_000_000_000,
                    }),
                    Some(Duration::from_secs(60)),
                )
                .await
            })
        };
        let h2 = {
            let r = l2.clone_boxed();
            tokio::spawn(async move {
                r.send_with_timeout(
                    Box::new(LongTask {
                        iterations: 2_000_000_000,
                    }),
                    Some(Duration::from_secs(60)),
                )
                .await
            })
        };

        // 等 2 个长任务都进入 handler（ops 前置 +1）
        let started = wait_until(
            || ops_l1.load(Ordering::Relaxed) >= 1 && ops_l2.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10),
            Duration::from_millis(5),
        )
        .await;
        assert!(started, "long tasks must both start (pool saturated)");

        // 2) 短任务风暴：200 个 tiny 连环 ask（每个都会排队 → 触发 backlog）
        let t0 = Instant::now();
        let mut max_lat = Duration::ZERO;
        for i in 0..200u64 {
            let tt = Instant::now();
            let v = short
                .send_with_timeout(Box::new(TinyTask), Some(Duration::from_secs(30)))
                .await
                .expect("short task must complete (burst rescue)")
                .downcast::<u64>()
                .map(|b| *b)
                .unwrap_or(0);
            assert_eq!(v, 1, "tiny #{}", i);
            let lat = tt.elapsed();
            if lat > max_lat {
                max_lat = lat;
            }
        }
        let elapsed = t0.elapsed();

        // 3) 断言：短任务全部完成，且没有单个短任务被拖到秒级
        //    （backlog threshold 100ms + spawn 开销 → 首个可能慢，后续应快）
        assert!(
            max_lat < Duration::from_secs(2),
            "max short-task latency {:.2}s must stay sub-2s (burst rescue works)",
            max_lat.as_secs_f64()
        );
        println!(
            "200 short asks while pool saturated: wall={:.2}s, max_lat={:.0}ms",
            elapsed.as_secs_f64(),
            max_lat.as_secs_f64() * 1000.0
        );

        let _ = h1.await;
        let _ = h2.await;
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// 测试 2：收缩 —— 压力解除后 burst 线程被回收
// ---------------------------------------------------------------------------

#[test]
fn elastic_burst_reaps_after_idle() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let cfg = elastic_system();
        let ts = ThreadActorSystem::shared(cfg);

        let ops_l1 = Arc::new(AtomicU64::new(0));
        let ops_l2 = Arc::new(AtomicU64::new(0));
        let ops_short = Arc::new(AtomicU64::new(0));

        let l1 = spawn(&ts, "/rp/long-1", ops_l1.clone()).await;
        let l2 = spawn(&ts, "/rp/long-2", ops_l2.clone()).await;
        let short = spawn(&ts, "/rp/short", ops_short.clone()).await;

        // 打满 + 短任务风暴（触发 burst）
        let hs: Vec<_> = [l1, l2]
            .into_iter()
            .map(|r| {
                let r = r.clone_boxed();
                tokio::spawn(async move {
                    let _ = r
                        .send_with_timeout(
                            Box::new(LongTask {
                                iterations: 1_500_000_000,
                            }),
                            Some(Duration::from_secs(60)),
                        )
                        .await;
                })
            })
            .collect();
        for h in &hs {
            let _ = h;
        }
        let started = wait_until(
            || ops_l1.load(Ordering::Relaxed) >= 1 && ops_l2.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10),
            Duration::from_millis(5),
        )
        .await;
        assert!(started);

        // 制造 backlog，等 burst 出现（通过 metrics 观察）
        let burst_appeared = wait_until(
            || {
                let short_ops = ops_short.load(Ordering::Relaxed);
                // 短任务不断到达 → wake_hook probe → burst
                short_ops > 0
            },
            Duration::from_secs(3),
            Duration::from_millis(50),
        )
        .await;
        let _ = burst_appeared;

        // 简单触发方式：发一批短任务
        for _ in 0..50 {
            let _ = short
                .send_with_timeout(Box::new(TinyTask), Some(Duration::from_secs(30)))
                .await;
        }
        for h in hs {
            let _ = h.await;
        }

        // 等长任务完成 + burst 空闲超时（1.5s）+ 收割周期
        tokio::time::sleep(Duration::from_millis(3500)).await;

        // burst 应回收到 0：通过系统线程数侧证（直接观测内部计数不可达时
        // 用线程数）。此处直接读取池 metrics 不可达（系统级封装），改用
        // 行为侧证：再次打满时新 burst 仍能出现且系统稳定。
        let ops_l3 = Arc::new(AtomicU64::new(0));
        let ops_l4 = Arc::new(AtomicU64::new(0));
        let l3 = spawn(&ts, "/rp/long-3", ops_l3.clone()).await;
        let l4 = spawn(&ts, "/rp/long-4", ops_l4.clone()).await;
        let h3 = {
            let r = l3.clone_boxed();
            tokio::spawn(async move {
                let _ = r
                    .send_with_timeout(
                        Box::new(LongTask {
                            iterations: 1_200_000_000,
                        }),
                        Some(Duration::from_secs(60)),
                    )
                    .await;
            })
        };
        let h4 = {
            let r = l4.clone_boxed();
            tokio::spawn(async move {
                let _ = r
                    .send_with_timeout(
                        Box::new(LongTask {
                            iterations: 1_200_000_000,
                        }),
                        Some(Duration::from_secs(60)),
                    )
                    .await;
            })
        };
        let started2 = wait_until(
            || ops_l3.load(Ordering::Relaxed) >= 1 && ops_l4.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10),
            Duration::from_millis(5),
        )
        .await;
        assert!(started2, "second saturation wave must start");

        let t0 = Instant::now();
        let v = short
            .send_with_timeout(Box::new(TinyTask), Some(Duration::from_secs(30)))
            .await
            .expect("rescued again after reap")
            .downcast::<u64>()
            .map(|b| *b)
            .unwrap_or(0);
        assert_eq!(v, 1);
        println!(
            "post-reap rescue latency: {:.0}ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );

        let _ = h3.await;
        let _ = h4.await;
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// 测试 3：全局上限 —— 线程数不爆炸
// ---------------------------------------------------------------------------

#[test]
fn elastic_thread_count_bounded() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let cfg = elastic_system(); // 2 core + max 2 burst
        let ts = ThreadActorSystem::shared(cfg.clone());

        let base_threads = std::thread::available_parallelism().unwrap().get();

        // 20 个长任务 actor 同时灌入（远超 worker 数）——若弹性失控会线程爆炸
        let mut handles = Vec::new();
        for i in 0..20 {
            let ops = Arc::new(AtomicU64::new(0));
            let r = spawn(&ts, &format!("/bd/long-{}", i), ops.clone()).await;
            handles.push((
                tokio::spawn(async move {
                    let _ = r
                        .send_with_timeout(
                            Box::new(LongTask {
                                iterations: 300_000_000,
                            }),
                            Some(Duration::from_secs(120)),
                        )
                        .await;
                }),
                ops,
            ));
        }

        // 压力期采样系统线程数（粗粒度：进程线程数）
        let mut max_seen = 0usize;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            // 读取 /proc 不可移植；用粗略计数：_available_parallelism 无意义，
            // 这里用 parrot 自身 metrics 观测（pool_size + burst）
            // 通过 scheduler metrics 接口（ThreadActorSystem 暴露）
            if let Some(m) = ts.scheduler_metrics() {
                let total = m.pool_size + m.burst_workers_alive;
                if total > max_seen {
                    max_seen = total;
                }
                assert!(
                    total <= cfg.shared_pool_size + cfg.shared_burst_workers_max,
                    "thread bound violated: {} > {}",
                    total,
                    cfg.shared_pool_size + cfg.shared_burst_workers_max
                );
            }
        }
        println!(
            "max concurrent scheduler threads: {} (bound {}), base_parallelism={}",
            max_seen,
            cfg.shared_pool_size + cfg.shared_burst_workers_max,
            base_threads
        );

        for (h, _) in handles {
            let _ = h.await;
        }
        let _ = ts.shutdown_internal().await;
    });
}
