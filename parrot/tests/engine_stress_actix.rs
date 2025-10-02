//! Actix 引擎压测套件（与 thread 版逻辑逐字节对齐）。
//!
//! 运行：`cargo test --test engine_stress_actix -- --nocapture --ignored`

mod engine_stress_common;

use engine_stress_common::*;
use parrot::actix::actor::ActixActor;
use parrot::actix::context::ActixContext;
use parrot::actix::system::ActixActorSystem;
use parrot::system::ParrotActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::{ActorRef, ActorRefExt};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 测试 actor（逻辑与 thread 版一致）
// ---------------------------------------------------------------------------

pub struct BenchActor {
    pub ops: Arc<AtomicU64>,
    pub cpu_sink: Arc<AtomicU64>,
    pub messages: Arc<AtomicU64>,
}

impl BenchActor {
    pub fn new(ops: Arc<AtomicU64>, cpu_sink: Arc<AtomicU64>) -> Self {
        Self { ops, cpu_sink, messages: Arc::new(AtomicU64::new(0)) }
    }
}

impl Actor for BenchActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Err(ActorError::MessageHandlingError("use engine path".into())) })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        let out: BoxedMessage = if let Some(t) = msg.downcast_ref::<CpuTask>() {
            // Pre-mark so tests can observe the task has entered the handler.
            self.ops.fetch_add(1, Ordering::Relaxed);
            let r = burn_cpu(t.iterations, t.salt);
            self.cpu_sink.fetch_add(r, Ordering::Relaxed);
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(r)
        } else if let Some(t) = msg.downcast_ref::<IoTask>() {
            // 同步 handler 中不能 sleep；记为延迟值（延迟模拟见 bench 内 tokio::spawn）
            Box::new(t.duration_ms)
        } else if let Some(t) = msg.downcast_ref::<LongRunningTask>() {
            // Mark start BEFORE the compute so the starvation test can
            // observe the task is actually running.
            self.ops.fetch_add(1, Ordering::Relaxed);
            let r = burn_cpu(t.total_iterations, 7);
            self.cpu_sink.fetch_add(r, Ordering::Relaxed);
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(r)
        } else if let Some(e) = msg.downcast_ref::<Echo>() {
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(e.value)
        } else if msg.is::<GetCount>() {
            Box::new(self.ops.load(Ordering::Relaxed))
        } else {
            return Some(Err(ActorError::MessageHandlingError("unknown msg".into())));
        };
        self.messages.fetch_add(1, Ordering::Relaxed);
        Some(Ok(out))
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// Message impls（与 thread 版相同）
impl Message for CpuTask { type Result = u64; fn extract_result(r: BoxedMessage) -> ActorResult<u64> { r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into())) } }
impl Message for IoTask { type Result = u64; fn extract_result(r: BoxedMessage) -> ActorResult<u64> { r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into())) } }
impl Message for LongRunningTask { type Result = u64; fn extract_result(r: BoxedMessage) -> ActorResult<u64> { r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into())) } }
impl Message for Echo { type Result = u64; fn extract_result(r: BoxedMessage) -> ActorResult<u64> { r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into())) } }
impl Message for GetCount { type Result = u64; fn extract_result(r: BoxedMessage) -> ActorResult<u64> { r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into())) } }

// ---------------------------------------------------------------------------
// 环境搭建
// ---------------------------------------------------------------------------

async fn setup() -> anyhow::Result<(ParrotActorSystem, ActixActorSystem)> {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await?;
    let actix_sys = ActixActorSystem::new().await?;
    parrot.register_actix_system("bench-actix".into(), actix_sys.clone(), true).await?;
    Ok((parrot, actix_sys))
}

fn spawn_actor(sys: &ActixActorSystem, ops: Arc<AtomicU64>, cpu_sink: Arc<AtomicU64>) -> Box<dyn ActorRef> {
    sys.spawn_root_typed(BenchActor::new(ops, cpu_sink), EmptyConfig)
        .now_or_never()
        .expect("spawn ready")
        .expect("spawn ok")
}

use futures::FutureExt;

// ---------------------------------------------------------------------------
// 异步 handler actor：验证 actix 适配层的 async IO 能力
// ---------------------------------------------------------------------------

/// `use_async_handler() == true`：所有消息走 `receive_message`（async fn），
/// handler 内部做真实异步等待（模拟 IO）。与 thread 版 IoActor 逻辑对齐。
pub struct IoAsyncActor {
    pub done: Arc<AtomicU64>,
}

impl Actor for IoAsyncActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let done = self.done.clone();
        Box::pin(async move {
            // 真实异步等待（IO 模拟）：await 期间释放 arbiter 线程
            tokio::time::sleep(Duration::from_millis(10)).await;
            done.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(1u64) as BoxedMessage)
        })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
    }

    /// 关键开关：让 actix 引擎把消息路由到异步 receive_message 路径
    fn use_async_handler(&self) -> bool {
        true
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ---------------------------------------------------------------------------
// 基准函数（与 thread 版对齐）
// ---------------------------------------------------------------------------

async fn bench_sequential_ask(report: &Report, name: &str, actor: &dyn ActorRef, n: u64) -> anyhow::Result<()> {
    let mut samples = Vec::with_capacity(n as usize);
    let start = Instant::now();
    for i in 0..n {
        let t0 = Instant::now();
        let r = actor.ask(Echo { value: i }).await?;
        assert_eq!(r, i);
        samples.push(t0.elapsed().as_micros());
    }
    report.push(BenchResult {
        name: name.into(), engine: "actix", total_messages: n,
        elapsed: start.elapsed(), latencies: latency_stats(samples),
        correctness: true, note: String::new(),
    });
    Ok(())
}

async fn bench_concurrent_ask(
    report: &Report, name: &str, actor_ref: Box<dyn ActorRef>,
    concurrency: usize, per_task: u64,
) -> anyhow::Result<()> {
    let mut samples = Vec::with_capacity(concurrency * per_task as usize);
    let start = Instant::now();
    let mut handles = Vec::with_capacity(concurrency);
    for c in 0..concurrency {
        let ar = actor_ref.clone_boxed();
        handles.push(tokio::spawn(async move {
            let mut local = Vec::with_capacity(per_task as usize);
            for i in 0..per_task {
                let t0 = Instant::now();
                let r = ar.ask(Echo { value: i + c as u64 }).await?;
                assert_eq!(r, i + c as u64);
                local.push(t0.elapsed().as_micros());
            }
            anyhow::Ok(local)
        }));
    }
    let mut correct = true;
    for h in handles {
        match h.await { Ok(Ok(mut s)) => samples.append(&mut s), _ => correct = false }
    }
    report.push(BenchResult {
        name: name.into(), engine: "actix",
        total_messages: (concurrency * per_task as usize) as u64,
        elapsed: start.elapsed(), latencies: latency_stats(samples),
        correctness: correct, note: format!("concurrency={}", concurrency),
    });
    Ok(())
}

async fn bench_tell_throughput(
    report: &Report, name: &str, sys: &ActixActorSystem,
    actor: Box<dyn ActorRef>, n: u64, ops: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let start = Instant::now();
    for i in 0..n {
        actor.send(Box::new(Echo { value: i }) as BoxedMessage).await?;
    }
    let ok = wait_until(
        || ops.load(Ordering::Relaxed) >= n,
        Duration::from_secs(120), Duration::from_millis(5),
    ).await;
    report.push(BenchResult {
        name: name.into(), engine: "actix", total_messages: n,
        elapsed: start.elapsed(), latencies: latency_stats(vec![]),
        correctness: ok, note: if ok { String::new() } else { "DRAIN TIMEOUT".into() },
    });
    let _ = sys;
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试入口
// ---------------------------------------------------------------------------

#[test]
fn actix_engine_full_suite() {
    // Actix 需要 System 上下文；在其内运行 tokio multi-thread 不冲突（actix System 自带 arbiter）
    actix::System::new().block_on(async {
        // tokio IO/timer 能力：actix System 基于 tokio，可直接使用
        let report = Report::new();
        println!("==================== ACTIX ENGINE STRESS ====================");

        let (parrot, sys) = setup().await.expect("setup");

        // ---------- 1. Echo ask 基线 ----------
        let ops = Arc::new(AtomicU64::new(0));
        let cpu_sink = Arc::new(AtomicU64::new(0));
        let a1 = spawn_actor(&sys, ops.clone(), cpu_sink.clone());
        bench_sequential_ask(&report, "seq-ask-echo-1k", a1.as_ref(), 1_000).await.unwrap();

        let ops2 = Arc::new(AtomicU64::new(0));
        let a2 = spawn_actor(&sys, ops2.clone(), cpu_sink.clone());
        bench_concurrent_ask(&report, "conc-ask-echo-c8-m1000", a2, 8, 1_000).await.unwrap();

        let ops3 = Arc::new(AtomicU64::new(0));
        let a3 = spawn_actor(&sys, ops3.clone(), cpu_sink.clone());
        bench_concurrent_ask(&report, "conc-ask-echo-c64-m200", a3, 64, 200).await.unwrap();

        // ---------- 2. tell 吞吐 ----------
        let ops4 = Arc::new(AtomicU64::new(0));
        let a4 = spawn_actor(&sys, ops4.clone(), cpu_sink.clone());
        bench_tell_throughput(&report, "tell-echo-100k", &sys, a4, 100_000, ops4.clone()).await.unwrap();

        // ---------- 3. CPU 密集 ----------
        {
            let ops5 = Arc::new(AtomicU64::new(0));
            let a5 = spawn_actor(&sys, ops5.clone(), cpu_sink.clone());
            let mut samples = Vec::new();
            let start = Instant::now();
            for i in 0..200u64 {
                let t0 = Instant::now();
                let r = a5.ask(CpuTask { iterations: 200_000, salt: i }).await.unwrap();
                assert!(r > 0);
                samples.push(t0.elapsed().as_micros());
            }
            report.push(BenchResult {
                name: "cpu-serial-200x200k".into(), engine: "actix",
                total_messages: 200, elapsed: start.elapsed(),
                latencies: latency_stats(samples), correctness: true,
                note: "~200µs work/msg".into(),
            });
        }
        {
            // 并行 CPU：actor 分散到 arbiter 池（默认 CPU 数量个 OS 线程）
            let ops_multi = Arc::new(AtomicU64::new(0));
            let mut refs = Vec::new();
            for _ in 0..64 {
                refs.push(spawn_actor(&sys, ops_multi.clone(), cpu_sink.clone()));
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for (i, r) in refs.into_iter().enumerate() {
                handles.push(tokio::spawn(async move {
                    let mut samples = Vec::new();
                    for k in 0..25u64 {
                        let t0 = Instant::now();
                        let v = r.ask(CpuTask { iterations: 200_000, salt: i as u64 * 1000 + k }).await?;
                        samples.push(t0.elapsed().as_micros());
                        assert!(v > 0);
                    }
                    anyhow::Ok(samples)
                }));
            }
            let mut all = Vec::new();
            for h in handles { all.extend(h.await.unwrap().unwrap()); }
            report.push(BenchResult {
                name: "cpu-parallel-8actors-200k".into(), engine: "actix",
                total_messages: 64 * 25, elapsed: start.elapsed(),
                latencies: latency_stats(all), correctness: true,
                note: "64 actors × 25 msgs × 200k iters (pooled arbiters)".into(),
            });
        }

        // ---------- 4. 洪泛 ----------
        {
            let ops_flood = Arc::new(AtomicU64::new(0));
            let af = spawn_actor(&sys, ops_flood.clone(), cpu_sink.clone());
            let n = 500_000u64;
            let start = Instant::now();
            let mut jhs = Vec::new();
            for p in 0..4 {
                let ar = af.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    for i in 0..(n / 4) {
                        ar.send(Box::new(Echo { value: i + p }) as BoxedMessage).await?;
                    }
                    anyhow::Ok(())
                }));
            }
            let mut ok = true;
            for jh in jhs { if jh.await.unwrap().is_err() { ok = false; } }
            let sent_elapsed = start.elapsed();
            let drained = wait_until(
                || ops_flood.load(Ordering::Relaxed) >= n,
                Duration::from_secs(300), Duration::from_millis(20),
            ).await;
            report.push(BenchResult {
                name: "flood-500k-tell".into(), engine: "actix",
                total_messages: n, elapsed: start.elapsed(),
                latencies: latency_stats(vec![]), correctness: drained && ok,
                note: format!("sent in {:.3}s; {}", sent_elapsed.as_secs_f64(), if drained { "drained" } else { "TIMEOUT" }),
            });
        }

        // ---------- 5. ask 超时边界 ----------
        // 探测 ask 从独立 OS 线程发出（自带 current-thread runtime），带 1ms
        // 超时。actor 所在 arbiter 正在跑重任务时，探测必然超时——验证
        // 超时语义在新调度路径下不变。
        {
            let ops_t = Arc::new(AtomicU64::new(0));
            let at = spawn_actor(&sys, ops_t.clone(), cpu_sink.clone());
            // fire-and-forget 两个重任务（每个 ~260ms）
            for s in 1..=2u64 {
                at.tell(CpuTask { iterations: 50_000_000, salt: s });
            }
            let started = wait_until(
                || ops_t.load(Ordering::Relaxed) >= 1,
                Duration::from_secs(10),
                Duration::from_millis(1),
            ).await;
            assert!(started, "heavy task must start");
            // Probe from a dedicated OS thread with its own runtime: the
            // 1ms timeout must fire while the actor's arbiter is busy.
            let probe_ref = at.clone_boxed();
            let probe = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    probe_ref
                        .send_with_timeout(Box::new(Echo { value: 1 }), Some(Duration::from_millis(1)))
                        .await
                })
            });
            let r = probe.join().expect("probe thread");
            let correct = r.is_err();
            report.push(BenchResult {
                name: "ask-timeout-short".into(), engine: "actix",
                total_messages: 2, elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]), correctness: correct,
                note: format!("short-timeout ask result err: {:?}", r.err().map(|e| e.to_string())),
            });
        }

        // ---------- 6. 停止后发送 ----------
        {
            let ops_s = Arc::new(AtomicU64::new(0));
            let asref = spawn_actor(&sys, ops_s.clone(), cpu_sink.clone());
            let alive_before = asref.is_alive().await;
            asref.stop().await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let alive_after = asref.is_alive().await;
            let r = asref.send(Box::new(Echo { value: 1 }) as BoxedMessage).await;
            report.push(BenchResult {
                name: "send-after-stop".into(), engine: "actix",
                total_messages: 1, elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]), correctness: r.is_err(),
                note: format!(
                    "alive before={}, after={}; send => {:?}",
                    alive_before, alive_after,
                    r.err().map(|e| e.to_string())
                ),
            });
        }

        // ---------- 7. 大量 actor ----------
        {
            let start = Instant::now();
            let mut refs = Vec::new();
            let mut ok = 0;
            for i in 0..20_000 {
                let ops_h = Arc::new(AtomicU64::new(0));
                let r = spawn_actor(&sys, ops_h, cpu_sink.clone());
                // 唯一路径后注册表不再互相覆盖
                let _ = i;
                refs.push(r);
            }
            let spawn_elapsed = start.elapsed();
            let mut jhs = Vec::new();
            for r in refs {
                let a = r.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    a.send(Box::new(Echo { value: 1 }) as BoxedMessage).await.is_ok()
                }));
            }
            for jh in jhs { if jh.await.unwrap() { ok += 1; } }
            report.push(BenchResult {
                name: "herd-20000-actors".into(), engine: "actix",
                total_messages: 20_000, elapsed: spawn_elapsed,
                latencies: latency_stats(vec![]), correctness: ok == 20_000,
                note: format!("spawned 20000 in {:.3}s; all accepted send", spawn_elapsed.as_secs_f64()),
            });
        }

        // ---------- 8. 长时程 ----------
        {
            let ops_l = Arc::new(AtomicU64::new(0));
            let al = spawn_actor(&sys, ops_l.clone(), cpu_sink.clone());
            let start = Instant::now();
            let r = al.ask(LongRunningTask { total_iterations: 2_000_000_000 }).await?;
            let elapsed = start.elapsed();
            report.push(BenchResult {
                name: "longrun-2G-iters".into(), engine: "actix",
                total_messages: 1, elapsed,
                latencies: latency_stats(vec![elapsed.as_micros()]), correctness: r > 0,
                note: "single 2G-iteration compute".into(),
            });
        }


        // ---------- 9. 饥饿交叉验证（ticker 探测法） ----------
        {
            let ops_a = Arc::new(AtomicU64::new(0));
            let ops_b = Arc::new(AtomicU64::new(0));
            let aa = spawn_actor(&sys, ops_a.clone(), cpu_sink.clone());
            let ab = spawn_actor(&sys, ops_b.clone(), cpu_sink.clone());

            let stop = Arc::new(AtomicU64::new(0));
            let t_origin = Instant::now();
            let samples = Arc::new(std::sync::Mutex::new(Vec::<(u128, u128)>::new())); // (elapsed_ms, lat_us)
            let ticker = {
                let ab = ab.clone_boxed();
                let stop = stop.clone();
                let samples = samples.clone();
                tokio::spawn(async move {
                    let mut i = 0u64;
                    while stop.load(Ordering::Relaxed) == 0 {
                        let t0 = Instant::now();
                        if let Ok(v) = ab.ask(Echo { value: i }).await {
                            assert_eq!(v, i);
                            samples.lock().unwrap().push((t_origin.elapsed().as_millis(), t0.elapsed().as_micros()));
                        }
                        i += 1;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
            };
            tokio::time::sleep(Duration::from_millis(150)).await;
            let baseline: Vec<u128> = samples.lock().unwrap().iter().map(|(_, l)| *l).collect();
            let base_stats = latency_stats(baseline);

            let heavy_start = Instant::now();
            let heavy = tokio::spawn({
                let ar = aa.clone_boxed();
                async move {
                    let _ = ar.send(Box::new(LongRunningTask { total_iterations: 1_000_000_000 }) as BoxedMessage).await;
                }
            });
            let started = wait_until(
                || ops_a.load(Ordering::Relaxed) > 0,
                Duration::from_secs(10),
                Duration::from_millis(1),
            ).await;
            // 注意：单 arbiter 下 wait_until 与 handler 互斥，此处 started==true 时计算可能已结束
            let compute_t0_ms = t_origin.elapsed().as_millis();
            let compute_window_start = Instant::now();

            tokio::time::sleep(Duration::from_millis(1500)).await;
            let compute_t1_ms = t_origin.elapsed().as_millis();
            let in_window: Vec<u128> = samples
                .lock()
                .unwrap()
                .iter()
                .filter(|(ts, _)| *ts >= compute_t0_ms && *ts <= compute_t1_ms)
                .map(|(_, l)| *l)
                .collect();
            let in_window_stats = latency_stats(in_window);
            let during: Vec<u128> = samples.lock().unwrap().iter().map(|(_, l)| *l).collect();
            let during_new: Vec<u128> = if during.len() > base_stats.count as usize {
                during[base_stats.count as usize..].to_vec()
            } else { during.clone() };
            let during_stats = latency_stats(during_new);

            let _ = heavy.await;
            let compute_elapsed = heavy_start.elapsed();
            stop.store(1, Ordering::Relaxed);
            let _ = ticker.await;

            report.push(BenchResult {
                name: "starve-echo-during-longrun".into(),
                engine: "actix",
                total_messages: during_stats.count,
                elapsed: compute_window_start.elapsed(),
                latencies: during_stats.clone(),
                correctness: true,
                note: format!(
                    "heavy={:.1}s; probes-in-window={} (p99={:.2}ms max={:.2}ms); baseline p99={:.2}ms; started_observed={}",
                    compute_elapsed.as_secs_f64(),
                    in_window_stats.count,
                    in_window_stats.p99_us as f64 / 1000.0,
                    in_window_stats.max_us as f64 / 1000.0,
                    base_stats.p99_us as f64 / 1000.0,
                    started,
                ),
            });
        }


        // ---------- 10. IO 密集：async handler 内真实 await ----------
        // `use_async_handler() == true` 的 actor：消息走 receive_message
        // （async fn），handler 内 tokio::time::sleep(10ms) 模拟 IO 等待。
        // 64 actor × 各 20 条：await 期间 arbiter 线程被释放去跑其他
        // actor 的 handler，总墙钟时间应远小于串行时间。
        {
            let io_done = Arc::new(AtomicU64::new(0));
            let mut refs = Vec::new();
            for _ in 0..64 {
                refs.push(
                    sys.spawn_root_typed(IoAsyncActor { done: io_done.clone() }, EmptyConfig)
                        .now_or_never()
                        .expect("spawn ready")
                        .expect("spawn ok"),
                );
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for r in refs {
                handles.push(tokio::spawn(async move {
                    for _ in 0..20u64 {
                        r.send(Box::new(Echo { value: 1 }) as BoxedMessage).await?;
                    }
                    anyhow::Ok(())
                }));
            }
            for h in handles { h.await.unwrap().unwrap(); }
            let drained = wait_until(
                || io_done.load(Ordering::Relaxed) >= 64 * 20,
                Duration::from_secs(120), Duration::from_millis(5),
            ).await;
            let elapsed = start.elapsed();
            let total = 64 * 20;
            let serial_expected = total as f64 * 10.0 / 1000.0;
            report.push(BenchResult {
                name: "io-async-64actors-10ms".into(),
                engine: "actix",
                total_messages: total as u64,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: drained,
                note: format!(
                    "async sleep(10ms) in handler; {} tasks wall={:.2}s (serial would be {:.2}s; speedup {:.1}x)",
                    total, elapsed.as_secs_f64(), serial_expected, serial_expected / elapsed.as_secs_f64(),
                ),
            });
        }

        println!("==================== ACTIX REPORT ====================");
        println!("{}", report.dump_markdown());
        std::fs::write("/tmp/parrot_bench_actix.md", report.dump_markdown()).ok();

        let _ = sys.shutdown().await;
        let _ = parrot;
        anyhow::Ok(())
    }).unwrap();
}
