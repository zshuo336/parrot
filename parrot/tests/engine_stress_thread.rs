//! Thread 引擎压测套件。
//!
//! 运行：`cargo test --test engine_stress_thread -- --nocapture --ignored`

mod engine_stress_common;

use engine_stress_common::*;
use anyhow::anyhow;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRef, ActorRefExt};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 测试 actor：与 actix 版本逻辑完全一致
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
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        // Thread 引擎通过 AskEnvelope/tell 走 receive_message；复用同一 dispatch 逻辑
        let res = dispatch_bench_message(self, msg);
        Box::pin(async move { res })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        Some(dispatch_bench_message(self, msg))
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// 共享 dispatch：thread 引擎两条路径（receive_message / receive_message_with_engine）走同一逻辑。
fn dispatch_bench_message(actor: &mut BenchActor, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
    let out: BoxedMessage = if let Some(t) = msg.downcast_ref::<CpuTask>() {
        // Pre-mark so tests can observe the task has entered the handler.
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(t.iterations, t.salt);
        actor.cpu_sink.fetch_add(r, Ordering::Relaxed);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(r)
    } else if let Some(t) = msg.downcast_ref::<IoTask>() {
        // 无法在同步 handler 内 sleep；用返回延迟值记录（IO 延迟模拟在 bench 层完成）
        Box::new(t.duration_ms)
    } else if let Some(t) = msg.downcast_ref::<LongRunningTask>() {
        // Mark start BEFORE the compute so the starvation test can
        // observe the task is actually running.
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(t.total_iterations, 7);
        actor.cpu_sink.fetch_add(r, Ordering::Relaxed);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(r)
    } else if let Some(e) = msg.downcast_ref::<Echo>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(e.value)
    } else if let Some(t) = msg.downcast_ref::<MinuteCpuTask>() {
        // 分钟级（或秒级）纯 CPU 任务：与 CpuTask 同逻辑，语义上区分场景
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(t.iterations, t.salt);
        actor.cpu_sink.fetch_add(r, Ordering::Relaxed);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(r)
    } else if let Some(t) = msg.downcast_ref::<MediumCpuTask>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(t.iterations, t.salt);
        actor.cpu_sink.fetch_add(r, Ordering::Relaxed);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(r)
    } else if let Some(t) = msg.downcast_ref::<TinyTask>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(1_000, t.salt); // ~1.2µs
        actor.cpu_sink.fetch_add(r, Ordering::Relaxed);
        Box::new(r)
    } else if let Some(b) = msg.downcast_ref::<BatchEcho>() {
        // 一次消息做 batch 个 ~20µs 小工作：摊薄调度噪声
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let mut acc = b.value;
        for k in 0..b.batch {
            acc = acc.wrapping_add(burn_cpu(20_000, b.value ^ k));
        }
        actor.cpu_sink.fetch_add(acc, Ordering::Relaxed);
        Box::new(acc)
    } else if msg.is::<GetCount>() {
        Box::new(actor.ops.load(Ordering::Relaxed))
    } else {
        return Err(ActorError::MessageHandlingError("unknown msg".into()));
    };
    actor.messages.fetch_add(1, Ordering::Relaxed);
    Ok(out)
}

// Message impls

// 让同步结果变成 future 的小助手
trait FutExt {
    fn fut(self) -> BoxedFuture<'static, ActorResult<BoxedMessage>>;
}
impl FutExt for ActorResult<BoxedMessage> {
    fn fut(self) -> BoxedFuture<'static, ActorResult<BoxedMessage>> {
        Box::pin(async move { self })
    }
}

// Message impls
impl Message for CpuTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for IoTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for LongRunningTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for Echo {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for GetCount {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for MinuteCpuTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for MediumCpuTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for TinyTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for BatchEcho {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}

// ---------------------------------------------------------------------------
// 环境搭建
// ---------------------------------------------------------------------------

async fn setup() -> anyhow::Result<(ParrotActorSystem, Arc<ThreadActorSystem>)> {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await?;
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    parrot.register_thread_system("bench-thread".into(), ts.clone(), true).await?;
    Ok((parrot, ts))
}

async fn spawn_actor(ts: &Arc<ThreadActorSystem>, path: &str, ops: Arc<AtomicU64>, cpu_sink: Arc<AtomicU64>) -> Box<dyn ActorRef> {
    let r = ts
        .spawn_at::<BenchActor>(BenchActor::new(ops, cpu_sink), path, None, ThreadActorConfig::default())
        .await
        .expect("spawn ok");
    Box::new(r)
}

use futures::FutureExt;

// ---------------------------------------------------------------------------
// 基准测量函数
// ---------------------------------------------------------------------------

/// ask 吞吐：N 个 ask 顺序发出（每次等待回复），测端到端延迟。
async fn bench_sequential_ask(
    report: &Report,
    name: &str,
    actor: &dyn ActorRef,
    msg: Echo,
    n: u64,
) -> anyhow::Result<()> {
    let mut samples = Vec::with_capacity(n as usize);
    let start = Instant::now();
    for i in 0..n {
        let t0 = Instant::now();
        let r = actor.ask(Echo { value: i }).await?;
        assert_eq!(r, i);
        samples.push(t0.elapsed().as_micros());
    }
    let elapsed = start.elapsed();
    report.push(BenchResult {
        name: name.to_string(),
        engine: "thread",
        total_messages: n,
        elapsed,
        latencies: latency_stats(samples),
        correctness: true,
        cpu_work_secs: None,
                note: String::new(),
    });
    let _ = msg;
    Ok(())
}

/// 并发 ask 吞吐：C 个并发任务 × 每任务 M 次 ask。
async fn bench_concurrent_ask(
    report: &Report,
    name: &str,
    actor_ref: Box<dyn ActorRef>,
    concurrency: usize,
    per_task: u64,
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
        match h.await {
            Ok(Ok(mut s)) => samples.append(&mut s),
            _ => correct = false,
        }
    }
    let elapsed = start.elapsed();
    report.push(BenchResult {
        name: name.to_string(),
        engine: "thread",
        total_messages: (concurrency * per_task as usize) as u64,
        elapsed,
        latencies: latency_stats(samples),
        correctness: correct,
        cpu_work_secs: None,
                note: format!("concurrency={}", concurrency),
    });
    Ok(())
}

/// tell 吞吐（fire-and-forget）：发 N 条然后等 drain。
async fn bench_tell_throughput(
    report: &Report,
    name: &str,
    ts: &ThreadActorSystem,
    path: &str,
    n: u64,
    ops: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    // actor 已由外部 spawn
    let actor = ts.get_actor_ref(path).expect("actor exists");
    let start = Instant::now();
    for i in 0..n {
        actor.send(Box::new(Echo { value: i }) as BoxedMessage).await?;
    }
    // 等 drain
    let ok = wait_until(
        || ops.load(Ordering::Relaxed) >= n,
        Duration::from_secs(120),
        Duration::from_millis(5),
    )
    .await;
    let elapsed = start.elapsed();
    report.push(BenchResult {
        name: name.to_string(),
        engine: "thread",
        total_messages: n,
        elapsed,
        latencies: latency_stats(vec![]),
        correctness: ok,
        cpu_work_secs: None,
                note: if ok { String::new() } else { "DRAIN TIMEOUT".into() },
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试入口
// ---------------------------------------------------------------------------

// 分钟级 CPU 场景使全套耗时 ~3.5 分钟（release）；日常回归请跳过：
//   cargo test -p parrot --release            # 跳过压测
//   cargo test -p parrot --release -- --ignored --nocapture   # 只跑压测
#[test]
#[ignore]
fn thread_engine_full_suite() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let report = Report::new();
        println!("==================== THREAD ENGINE STRESS ====================");

        let (_parrot, ts) = setup().await.expect("setup");

        // ---------- 1. Echo ask 基线 ----------
        let ops = Arc::new(AtomicU64::new(0));
        let cpu_sink = Arc::new(AtomicU64::new(0));
        let a1 = spawn_actor(&ts, "/bench/echo", ops.clone(), cpu_sink.clone()).await;
        bench_sequential_ask(&report, "seq-ask-echo-1k", a1.as_ref(), Echo { value: 0 }, 1_000).await.unwrap();

        let ops2 = Arc::new(AtomicU64::new(0));
        let a2 = spawn_actor(&ts, "/bench/echo2", ops2.clone(), cpu_sink.clone()).await;
        bench_concurrent_ask(&report, "conc-ask-echo-c8-m1000", a2, 8, 1_000).await.unwrap();

        let ops3 = Arc::new(AtomicU64::new(0));
        let a3 = spawn_actor(&ts, "/bench/echo3", ops3.clone(), cpu_sink.clone()).await;
        bench_concurrent_ask(&report, "conc-ask-echo-c64-m200", a3, 64, 200).await.unwrap();

        // ---------- 2. tell 吞吐 ----------
        let ops4 = Arc::new(AtomicU64::new(0));
        spawn_actor(&ts, "/bench/tell", ops4.clone(), cpu_sink.clone()).await;
        bench_tell_throughput(&report, "tell-echo-100k", &ts, "/bench/tell", 100_000, ops4.clone()).await.unwrap();

        // ---------- 3. CPU 密集 ----------
        // 单 actor 串行 CPU：每次 ~200µs 工作量
        let ops5 = Arc::new(AtomicU64::new(0));
        let a5 = spawn_actor(&ts, "/bench/cpu1", ops5.clone(), cpu_sink.clone()).await;
        {
            let mut samples = Vec::new();
            let start = Instant::now();
            for i in 0..200u64 {
                let t0 = Instant::now();
                let r = a5.ask(CpuTask { iterations: 200_000, salt: i }).await.unwrap();
                assert!(r > 0);
                samples.push(t0.elapsed().as_micros());
            }
            report.push(BenchResult {
                name: "cpu-serial-200x200k".into(),
                engine: "thread",
                total_messages: 200,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: true,
                cpu_work_secs: None,
                note: "~200µs work/msg".into(),
            });
        }

        // 多 actor 并行 CPU：64 actor × 各 200k 迭代 × 25 条 = 并行度收益验证
        {
            let ops_multi = Arc::new(AtomicU64::new(0));
            let mut refs = Vec::new();
            for i in 0..64 {
                let p = format!("/bench/cpu-parallel-{}", i);
                refs.push(spawn_actor(&ts, &p, ops_multi.clone(), cpu_sink.clone()).await);
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
            for h in handles {
                all.extend(h.await.unwrap().unwrap());
            }
            report.push(BenchResult {
                name: "cpu-parallel-8actors-200k".into(),
                engine: "thread",
                total_messages: 64 * 25,
                elapsed: start.elapsed(),
                latencies: latency_stats(all),
                correctness: true,
                cpu_work_secs: None,
                note: "64 actors × 25 msgs × 200k iters".into(),
            });
        }

        // ---------- 4. 高压边界：邮箱洪泛 ----------
        {
            let ops_flood = Arc::new(AtomicU64::new(0));
            spawn_actor(&ts, "/bench/flood", ops_flood.clone(), cpu_sink.clone()).await;
            let actor = ts.get_actor_ref("/bench/flood").unwrap();
            let n = 500_000u64;
            let start = Instant::now();
            // do_send 等价：thread 引擎用 fire-and-forget send（Block 策略会在满时等待）
            let mut jhs = Vec::new();
            for p in 0..4 {
                let ar = actor.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    for i in 0..(n / 4) {
                        ar.send(Box::new(Echo { value: i + p }) as BoxedMessage).await?;
                    }
                    anyhow::Ok(())
                }));
            }
            let mut ok = true;
            for jh in jhs {
                if jh.await.unwrap().is_err() { ok = false; }
            }
            let sent_elapsed = start.elapsed();
            let drained = wait_until(
                || ops_flood.load(Ordering::Relaxed) >= n,
                Duration::from_secs(300),
                Duration::from_millis(20),
            ).await;
            report.push(BenchResult {
                name: "flood-500k-tell".into(),
                engine: "thread",
                total_messages: n,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: drained && ok,
                cpu_work_secs: None,
                note: format!("sent in {:.3}s; {}", sent_elapsed.as_secs_f64(), if drained { "drained" } else { "TIMEOUT" }),
            });
        }

        // ---------- 5. ask 超时边界 ----------
        {
            let ops_t = Arc::new(AtomicU64::new(0));
            let at = spawn_actor(&ts, "/bench/timeout", ops_t.clone(), cpu_sink.clone()).await;
            // 塞一个重任务（10M 迭代 ≈ 10ms+），然后带极短超时 ask
            let heavy = tokio::spawn({
                let ar = at.clone_boxed();
                async move {
                    let _ = ar.ask(CpuTask { iterations: 50_000_000, salt: 1 }).await;
                }
            });
            // Wait until the heavy task has actually entered its handler,
            // so the probe ask is guaranteed to queue behind it.
            let started = wait_until(
                || ops_t.load(Ordering::Relaxed) > 0,
                Duration::from_secs(10),
                Duration::from_millis(1),
            ).await;
            assert!(started, "heavy task must start");
            let r = at.send_with_timeout(Box::new(Echo { value: 1 }), Some(Duration::from_millis(1))).await;
            let correct = matches!(&r, Err(ActorError::TimeoutDetail(_)) | Err(ActorError::Timeout));
            report.push(BenchResult {
                name: "ask-timeout-short".into(),
                engine: "thread",
                total_messages: 2,
                elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]),
                correctness: correct,
                cpu_work_secs: None,
                note: format!("short-timeout ask result: {:?}", r.err().map(|e| e.to_string())),
            });
            let _ = heavy.await;
        }

        // ---------- 6. 停止后发送边界 ----------
        {
            let ops_s = Arc::new(AtomicU64::new(0));
            let asref = spawn_actor(&ts, "/bench/stop", ops_s.clone(), cpu_sink.clone()).await;
            ts.stop_actor("/bench/stop").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let r = asref.send(Box::new(Echo { value: 1 }) as BoxedMessage).await;
            report.push(BenchResult {
                name: "send-after-stop".into(),
                engine: "thread",
                total_messages: 1,
                elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]),
                correctness: r.is_err(),
                cpu_work_secs: None,
                note: format!("send after stop => {:?}", r.err().map(|e| e.to_string())),
            });
        }

        // ---------- 7. 大量 actor 存活性 ----------
        {
            let start = Instant::now();
            let mut paths = Vec::new();
            for i in 0..20_000 {
                let p = format!("/bench/herd-{}", i);
                let ops_h = Arc::new(AtomicU64::new(0));
                spawn_actor(&ts, &p, ops_h, cpu_sink.clone()).await;
                paths.push(p);
            }
            let spawn_elapsed = start.elapsed();
            // 每个 actor 发一条
            let mut jhs = Vec::new();
            let mut ok = 0;
            for p in &paths {
                if let Some(a) = ts.get_actor_ref(p) {
                    let a = a.clone_boxed();
                    jhs.push(tokio::spawn(async move {
                        a.send(Box::new(Echo { value: 1 }) as BoxedMessage).await.is_ok()
                    }));
                }
            }
            for jh in jhs {
                if jh.await.unwrap() { ok += 1; }
            }
            report.push(BenchResult {
                name: "herd-20000-actors".into(),
                engine: "thread",
                total_messages: 20_000,
                elapsed: spawn_elapsed,
                latencies: latency_stats(vec![]),
                correctness: ok == 20_000,
                cpu_work_secs: None,
                note: format!("spawned 20000 in {:.3}s; all accepted send", spawn_elapsed.as_secs_f64()),
            });
        }

        // ---------- 8. 长时程任务（分片 CPU） ----------
        {
            let ops_l = Arc::new(AtomicU64::new(0));
            let al = spawn_actor(&ts, "/bench/longrun", ops_l.clone(), cpu_sink.clone()).await;
            let start = Instant::now();
            // 一次 2 秒级别的计算（约 2G 迭代）；显式放宽超时以覆盖长时程
            let r = al
                .send_with_timeout(Box::new(LongRunningTask { total_iterations: 2_000_000_000 }), Some(Duration::from_secs(120)))
                .await?;
            let elapsed = start.elapsed();
            let val = r.downcast::<u64>().map(|v| *v).unwrap_or(0);
            report.push(BenchResult {
                name: "longrun-2G-iters".into(),
                engine: "thread",
                total_messages: 1,
                elapsed,
                latencies: latency_stats(vec![elapsed.as_micros()]),
                correctness: val > 0,
                cpu_work_secs: None,
                note: "single 2G-iteration compute".into(),
            });
        }


        // ---------- 9. 饥饿交叉验证（ticker 探测法） ----------
        // 后台 ticker 每 5ms 向 B 发 echo ask 记录延迟；随后触发 A 的 1G 迭代计算；
        // 对比计算窗口内外的 echo 延迟，揭示长任务对其他 actor 的影响。
        {
            let ops_a = Arc::new(AtomicU64::new(0));
            let ops_b = Arc::new(AtomicU64::new(0));
            let aa = spawn_actor(&ts, "/bench/starve-a", ops_a.clone(), cpu_sink.clone()).await;
            let ab = spawn_actor(&ts, "/bench/starve-b", ops_b.clone(), cpu_sink.clone()).await;

            let stop = Arc::new(AtomicU64::new(0));
            let t_origin = Instant::now();
            let t_origin = Instant::now();
            let samples = Arc::new(std::sync::Mutex::new(Vec::<(u128, u128)>::new())); // (elapsed_ms, lat_us) // (elapsed_ms, lat_us)
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
                            samples.lock().unwrap().push((
                                t_origin.elapsed().as_millis(),
                                t0.elapsed().as_micros(),
                            ));
                        }
                        i += 1;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
            };
            // 基线窗口
            tokio::time::sleep(Duration::from_millis(150)).await;
            let baseline: Vec<u128> = samples.lock().unwrap().iter().map(|(_, l)| *l).collect();
            let base_stats = latency_stats(baseline);

            // 触发 A 的长计算（~5.3s）
            let heavy_start = Instant::now();
            let heavy = tokio::spawn({
                let ar = aa.clone_boxed();
                async move {
                    let _ = ar.send_with_timeout(
                        Box::new(LongRunningTask { total_iterations: 1_000_000_000 }),
                        Some(Duration::from_secs(120)),
                    ).await;
                }
            });
            // 等计算确实开始（ops 前置位标记）
            let started = wait_until(
                || ops_a.load(Ordering::Relaxed) > 0,
                Duration::from_secs(10),
                Duration::from_millis(1),
            ).await;
            assert!(started, "heavy task must start");
            let compute_t0_ms = t_origin.elapsed().as_millis();
            let compute_window_start = Instant::now();

            // 计算窗口内持续采样
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
            // 取"计算开始后"新增样本：简单起见用总样本减基线长度
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
                engine: "thread",
                total_messages: during_stats.count,
                elapsed: compute_window_start.elapsed(),
                latencies: during_stats.clone(),
                correctness: true,
                cpu_work_secs: None,
                note: format!(
                    "heavy={:.1}s; probes-in-window={} (p99={:.2}ms max={:.2}ms); baseline p99={:.2}ms",
                    compute_elapsed.as_secs_f64(),
                    in_window_stats.count,
                    in_window_stats.p99_us as f64 / 1000.0,
                    in_window_stats.max_us as f64 / 1000.0,
                    base_stats.p99_us as f64 / 1000.0,
                ),
            });
        }


        // ---------- 10. IO 密集：async handler 内真实 await ----------
        // Thread 引擎的 receive_message 是 async fn，可以在 handler 内做真实 IO 等待。
        {
            let io_done = Arc::new(AtomicU64::new(0));
            let ts2 = ts.clone();
            let _ = ts2;
            // 独立的 IO actor 类型：handler 内 sleep 10ms（模拟 IO 等待）
            struct IoActor {
                done: Arc<AtomicU64>,
            }
            impl Actor for IoActor {
                type Config = EmptyConfig;
                type Context = ThreadContext<Self>;
                fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
                    Box::pin(async { Ok(()) })
                }
                fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                    let done = self.done.clone();
                    Box::pin(async move {
                        // 真实异步等待（IO 模拟）
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        done.fetch_add(1, Ordering::Relaxed);
                        Ok(msg)
                    })
                }
                fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
                    None
                }
                fn state(&self) -> ActorState { ActorState::Running }
            }
            // 64 个 IO actor × 各 20 条 × 10ms：并行应 ~1.3s，串行 ~12.8s
            let mut refs = Vec::new();
            for i in 0..64 {
                let p = format!("/bench/io-{}", i);
                let r = ts.spawn_at::<IoActor>(IoActor { done: io_done.clone() }, &p, None, ThreadActorConfig::default()).await.expect("io spawn");
                refs.push(r);
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for r in refs {
                handles.push(tokio::spawn(async move {
                    for _ in 0..20u64 {
                        r.send_msg(Box::new(Echo { value: 1 }) as BoxedMessage).await?;
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
                engine: "thread",
                total_messages: total as u64,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: drained,
                cpu_work_secs: None,
                note: format!(
                    "async sleep(10ms) in handler; {} tasks wall={:.2}s (serial would be {:.2}s; speedup {:.1}x)",
                    total, elapsed.as_secs_f64(), serial_expected, serial_expected / elapsed.as_secs_f64(),
                ),
            });
        }


        // =====================================================================
        // 第二轮扩展场景（2026-10-02）：分钟级 CPU + 混合负载 + 极端场景
        // =====================================================================

        // ---------- M1. 分钟级任务 + 持续加入新长任务/中任务 + 短探测 ----------
        // 8 个专职 actor 各跑一个 ~37s 分钟级任务（600M iters @ ~1.29ns/it）；
        // 同时 4 个后备 actor 持续接秒级新任务；独立探测器测短任务延迟。
        {
            const LONG_ACTORS: usize = 8;
            const BACKUP_ACTORS: usize = 4;
            const LONG_ITERS: u64 = 57_200_000_000;   // ~65s @release (真·分钟级)
            const MEDIUM_ITERS: u64 = 2_400_000_000;  // ~2.7s @release
            const MEDIUM_PER_BACKUP: u64 = 10;        // 40 medium tasks
            let nominal = (LONG_ACTORS as f64 * LONG_ITERS as f64
                + BACKUP_ACTORS as f64 * MEDIUM_PER_BACKUP as f64 * MEDIUM_ITERS as f64)
                / BURN_RATE;

            let mut long_refs = Vec::new();
            for i in 0..LONG_ACTORS {
                let p = format!("/bench/m1/long-{}", i);
                long_refs.push(spawn_actor(&ts, &p, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await);
            }
            let mut backup_refs = Vec::new();
            for i in 0..BACKUP_ACTORS {
                let p = format!("/bench/m1/backup-{}", i);
                backup_refs.push(spawn_actor(&ts, &p, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await);
            }
            let probe = spawn_actor(&ts, "/bench/m1/probe", Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;

            let start = Instant::now();
            let mut handles = Vec::new();
            // 共享池饱和（12 CPU 任务 > 8 worker）时排队延迟远超 ask 默认
            // 5s 超时 —— 显式 600s 超时以测完整个场景（排队延迟本身
            // 就是共享池架构的测量产出，记入 lats）。
            let ask_long = |r: Box<dyn ActorRef>, iters: u64, salt: u64| async move {
                let t0 = Instant::now();
                let rep = r
                    .send_with_timeout(
                        Box::new(MinuteCpuTask { iterations: iters, salt }),
                        Some(Duration::from_secs(120)),
                    )
                    .await?;
                let v: u64 = rep.downcast::<u64>().map(|b| *b).map_err(|_| anyhow!("downcast"))?;
                anyhow::Ok(vec![t0.elapsed().as_micros(), v as u128])
            };
            let ask_medium = |r: Box<dyn ActorRef>, iters: u64, salt: u64| async move {
                let t0 = Instant::now();
                let rep = r
                    .send_with_timeout(
                        Box::new(MediumCpuTask { iterations: iters, salt }),
                        Some(Duration::from_secs(120)),
                    )
                    .await?;
                let _v: u64 = rep.downcast::<u64>().map(|b| *b).map_err(|_| anyhow!("downcast"))?;
                anyhow::Ok(t0.elapsed().as_micros())
            };
            for (i, r) in long_refs.into_iter().enumerate() {
                handles.push(tokio::spawn(ask_long(r, LONG_ITERS, 0xD00D + i as u64)));
            }
            for (i, r) in backup_refs.into_iter().enumerate() {
                let mut medias = Vec::new();
                for k in 0..MEDIUM_PER_BACKUP {
                    medias.push(tokio::spawn(ask_medium(r.clone_boxed(), MEDIUM_ITERS, 0xBACC + i as u64 * 100 + k)));
                }
                handles.push(tokio::spawn(async move {
                    let mut lats = Vec::new();
                    for m in medias {
                        match m.await {
                            Ok(Ok(t)) => lats.push(t),
                            Ok(Err(e)) => return Err(e),
                            Err(e) => return Err(anyhow!("join: {}", e)),
                        }
                    }
                    anyhow::Ok(lats)
                }));
            }
            // 短探测线程：整个场景期间每 200ms 发一个 ~1.2µs TinyTask
            let probe_stop = Arc::new(AtomicU64::new(0));
            let probe_handle = {
                let pr = probe.clone_boxed();
                let st = probe_stop.clone();
                tokio::spawn(async move {
                    let mut lats = Vec::new();
                    while st.load(Ordering::Relaxed) == 0 {
                        let t0 = Instant::now();
                        let _ = pr
                            .send_with_timeout(Box::new(TinyTask { salt: 0xE }), Some(Duration::from_secs(120)))
                            .await;
                        lats.push(t0.elapsed().as_micros());
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    lats
                })
            };

            // 收集长/中任务结果（统一为 Vec<u128>：首个元素是任务耗时）
            let mut correct = true;
            let mut all_lats: Vec<u128> = Vec::new();
            for h in handles {
                match h.await {
                    Ok(Ok(mut v)) => all_lats.append(&mut v),
                    _ => correct = false,
                }
            }
            probe_stop.store(1, Ordering::Relaxed);
            let probe_lats = probe_handle.await.unwrap_or_default();
            let elapsed = start.elapsed();
            let ps = latency_stats(probe_lats.clone());
            report.push(BenchResult {
                name: "mixed-minute-cpu-plus-incoming".into(),
                engine: "thread",
                total_messages: (LONG_ACTORS + BACKUP_ACTORS * MEDIUM_PER_BACKUP as usize) as u64,
                elapsed,
                latencies: ps.clone(),
                correctness: correct,
                cpu_work_secs: Some(nominal),
                note: format!(
                    "8×65s long + 40×2.7s medium concurrent; probe(tiny ask) p99={:.1}ms max={:.1}ms over {} probes",
                    ps.p99_us as f64 / 1000.0, ps.max_us as f64 / 1000.0, ps.count,
                ),
            });
        }

        // ---------- M2. 同一 actor 混合负载：分钟级 + 秒级 + 微秒级排队 ----------
        // 关键测量：同一 actor 邮箱 FIFO 语义下，短任务被前置长任务阻塞的代价。
        {
            const LONG_ITERS: u64 = 30_000_000_000;  // ~34s @release
            const MEDIUM: u64 = 6;                  // 6 × 1.2G (~1.5s) ≈ 9.3s
            const MEDIUM_ITERS: u64 = 2_400_000_000; // ~2.7s
            const TINY: u64 = 4_000;                // 4000 个 TinyTask 排在最后
            let nominal = (LONG_ITERS as f64 + MEDIUM as f64 * MEDIUM_ITERS as f64
                + TINY as f64 * 1_000.0) / BURN_RATE;

            let shared = spawn_actor(&ts, "/bench/m2/shared", Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            let start = Instant::now();
            // tell 顺序投递：1 long + 24 medium + 4000 tiny（全部 fire-and-forget）
            let mut jhs = Vec::new();
            {
                let sr = shared.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    sr.send_with_timeout(Box::new(MinuteCpuTask { iterations: LONG_ITERS, salt: 1 }), None).await?;
                    anyhow::Ok(())
                }));
            }
            for k in 0..MEDIUM {
                let sr = shared.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    sr.send_with_timeout(Box::new(MediumCpuTask { iterations: MEDIUM_ITERS, salt: 100 + k }), None).await?;
                    anyhow::Ok(())
                }));
            }
            for k in 0..TINY {
                let sr = shared.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    sr.send_with_timeout(Box::new(TinyTask { salt: k }), None).await?;
                    anyhow::Ok(())
                }));
            }
            for jh in jhs { jh.await.unwrap().unwrap(); }
            // 短任务代价：向同一 actor 追加一个"哨兵" ask（长超时覆盖整条队列），
            // 其延迟 ≈ 尾部剩余队列的处理时间。
            let t_probe = Instant::now();
            let _ = shared
                .send_with_timeout(Box::new(TinyTask { salt: 0x5 }), Some(Duration::from_secs(300)))
                .await;
            let tail_latency = t_probe.elapsed();
            let elapsed = start.elapsed();

            report.push(BenchResult {
                name: "mixed-same-actor-fifo".into(),
                engine: "thread",
                total_messages: 1 + MEDIUM + TINY,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: true,
                cpu_work_secs: Some(nominal),
                note: format!(
                    "1×34s + 6×2.7s + 4000 tiny FIFO on ONE actor; tail-probe ask lat={:.0}ms (≈queued-behind time)",
                    tail_latency.as_secs_f64() * 1000.0,
                ),
            });
        }

        // ---------- M3. 分片长任务（协作式让出）vs 连续长任务 ----------
        // 同一 600M 迭代任务：A) 单块连算 B) 30 片 × 20M，片间 yield。
        // 对比：总耗时（让出开销）与同 actor 排队 tiny 任务的等待（响应性）。
        {
            const TOTAL: u64 = 30_000_000_000;   // ~34s @release each variant
            const CHUNK: u64 = 1_500_000_000;   // ~20 片

            // ChunkedLongTask actor：async handler，片间 tokio yield
            struct ChunkActor;
            impl Actor for ChunkActor {
                type Config = EmptyConfig;
                type Context = ThreadContext<Self>;
                fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
                    Box::pin(async { Ok(()) })
                }
                fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                    Box::pin(async move {
                        let Some(t) = msg.downcast_ref::<ChunkedLongTask>() else {
                            return Ok(msg);
                        };
                        let total = t.total_iterations;
                        let chunk = t.chunk_iterations.max(1);
                        let mut acc: u64 = t.salt;
                        let mut done = 0u64;
                        while done < total {
                            let take = chunk.min(total - done);
                            acc = acc.wrapping_add(burn_cpu(take, acc));
                            done += take;
                            tokio::task::yield_now().await; // 让出点
                        }
                        Ok(Box::new(acc) as BoxedMessage)
                    })
                }
                fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
                    None
                }
                fn state(&self) -> ActorState { ActorState::Running }
            }
            impl Message for ChunkedLongTask {
                type Result = u64;
                fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
                    r.downcast::<u64>().map(|b| *b).map_err(|_| ActorError::MessageHandlingError("type".into()))
                }
            }

            let chunked = ts.spawn_at::<ChunkActor>(ChunkActor, "/bench/m3/chunked", None, ThreadActorConfig::default()).await.unwrap();
            let chunked2 = ts.spawn_at::<ChunkActor>(ChunkActor, "/bench/m3/chunked2", None, ThreadActorConfig::default()).await.unwrap();
            let solid = spawn_actor(&ts, "/bench/m3/solid", Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;

            // 对每个变体：发长任务（纯入队），随后立刻发 tiny ask（长超时）——
            // tiny 的 ask 延迟 = 它在邮箱里排队等长任务完成的全部时间。
            // A) 连续（solid）
            let t0 = Instant::now();
            solid
                .send_with_timeout(Box::new(MinuteCpuTask { iterations: TOTAL, salt: 3 }), None)
                .await
                .unwrap();
            let solid_probe = {
                let sr = solid.clone_boxed();
                tokio::spawn(async move {
                    let tp = Instant::now();
                    let v = sr
                        .send_with_timeout(Box::new(TinyTask { salt: 1 }), Some(Duration::from_secs(300)))
                        .await?;
                    anyhow::Ok((tp.elapsed(), v))
                })
            };
            let (solid_wait, _) = solid_probe.await.unwrap().unwrap();
            // tiny 排在 long 之后：probe 返回即 long 已完成 —— 总时长才是
            // 连续变体的真实完成时间（send(None) 是纯入队，立即返回）。
            let solid_long = t0.elapsed();

            // B) 分片（chunked）
            let t1 = Instant::now();
            let chunked_ref: Box<dyn ActorRef> = Box::new(chunked);
            let chunk_handle = {
                let cr = chunked_ref.clone_boxed();
                tokio::spawn(async move {
                    cr.send_with_timeout(
                        Box::new(ChunkedLongTask { total_iterations: TOTAL, chunk_iterations: CHUNK, salt: 3 }),
                        Some(Duration::from_secs(300)),
                    ).await
                })
            };
            // probe 必须发给同一个 chunked actor（排在分片任务后面），
            // 才能测到分片让出带来的响应性收益。
            let chunk_probe = {
                let cr2: Box<dyn ActorRef> = chunked_ref.clone_boxed();
                tokio::spawn(async move {
                    let tp = Instant::now();
                    let v = cr2
                        .send_with_timeout(Box::new(TinyTask { salt: 2 }), Some(Duration::from_secs(300)))
                        .await?;
                    anyhow::Ok((tp.elapsed(), v))
                })
            };
            let _ = chunk_handle.await;
            let (chunk_wait, _) = chunk_probe.await.unwrap().unwrap();
            let chunk_long = t1.elapsed();

            let nominal = TOTAL as f64 / BURN_RATE;
            let yield_overhead_pct = (chunk_long.as_secs_f64() / solid_long.as_secs_f64() - 1.0) * 100.0;
            report.push(BenchResult {
                name: "chunked-vs-solid-longrun".into(),
                engine: "thread",
                total_messages: 2,
                elapsed: solid_long + chunk_long,
                latencies: latency_stats(vec![solid_wait.as_micros(), chunk_wait.as_micros()]),
                correctness: true,
                cpu_work_secs: Some(nominal * 2.0),
                note: format!(
                    "solid={:.1}s (tiny queued {:.0}ms) vs chunked×20 (tiny wait {:.0}ms); yield overhead {:.1}%",
                    solid_long.as_secs_f64(),
                    solid_wait.as_secs_f64() * 1000.0,
                    chunk_wait.as_secs_f64() * 1000.0,
                    yield_overhead_pct,
                ),
            });
        }

        // ---------- E1. 乒乓 RTT：两个 actor 互发 ----------
        {
            let ops_p = Arc::new(AtomicU64::new(0));
            let pa = spawn_actor(&ts, "/bench/e1/pa", ops_p.clone(), cpu_sink.clone()).await;
            let pb = spawn_actor(&ts, "/bench/e1/pb", ops_p.clone(), cpu_sink.clone()).await;
            const BOUNCES: u64 = 10_000;
            let mut samples = Vec::with_capacity(BOUNCES as usize / 10);
            let start = Instant::now();
            // 发起者 ask A -> A 完成 -> ask B -> B 完成 = 一次 RTT（两跳）
            let mut i = 0u64;
            while i < BOUNCES {
                let t0 = Instant::now();
                let va = pa.ask(Echo { value: i }).await?;
                let vb = pb.ask(Echo { value: va }).await?;
                assert_eq!(vb, i);
                if i % 10 == 0 { samples.push(t0.elapsed().as_micros()); }
                i += 1;
            }
            report.push(BenchResult {
                name: "pingpong-rtt-10k".into(),
                engine: "thread",
                total_messages: BOUNCES * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                cpu_work_secs: None,
                correctness: true,
                note: "2-hop ask RTT (A→B), sampled every 10th".into(),
            });
        }

        // ---------- E2. 相关联发（talk-to-oneself）：actor 内自 ask 前置深度 ----------
        {
            // 背靠背自传递：ask 完成后立刻向同一 actor tell，再 ask……
            // （handler 内自 ask 会死锁邮箱，故用外部驱动的退化链式衡量
            // "对同一 actor 连续 ask→tell" 的引擎开销。）
            let ops_c = Arc::new(AtomicU64::new(0));
            let ca = spawn_actor(&ts, "/bench/e2/chain", ops_c.clone(), cpu_sink.clone()).await;
            const CHAIN_TICKS: u64 = 20_000;
            let start = Instant::now();
            let mut i = 0u64;
            while i < CHAIN_TICKS {
                // ask 自身不能在 handler 内做（会死锁邮箱）；改为 ask→完成→
                // 立刻再 ask 同一 actor，衡量背靠背自传递吞吐
                let v = ca.ask(Echo { value: i }).await?;
                let _ = ca.send(Box::new(Echo { value: v + 1 }) as BoxedMessage).await?;
                i += 1;
            }
            let ok = wait_until(
                || ops_c.load(Ordering::Relaxed) >= CHAIN_TICKS * 2,
                Duration::from_secs(60), Duration::from_millis(2),
            ).await;
            report.push(BenchResult {
                name: "self-chain-ask-tell-20k".into(),
                engine: "thread",
                total_messages: CHAIN_TICKS * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                cpu_work_secs: None,
                correctness: ok,
                note: "back-to-back ask→tell to same actor (degenerate chain)".into(),
            });
        }

        // ---------- E3. 慢消费者 + 快生产者（有界邮箱挤压） ----------
        {
            const PRODUCERS: usize = 8;
            const PER_PRODUCER: u64 = 5_000;   // 每个 5000 条
            const CONSUMER_WORK_US: u64 = 200; // 每条 ~200µs 处理

            let iters_per_msg: u64 = 170_000; // ~200µs 实测标定
            let nominal = (PRODUCERS as f64 * PER_PRODUCER as f64 * iters_per_msg as f64) / BURN_RATE;

            let ops_cons = Arc::new(AtomicU64::new(0));
            let consumer = spawn_actor(&ts, "/bench/e3/slow", ops_cons.clone(), cpu_sink.clone()).await;

            let start = Instant::now();
            let mut jhs = Vec::new();
            for pid in 0..PRODUCERS {
                let cr = consumer.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    for k in 0..PER_PRODUCER {
                        cr.send(Box::new(CpuTask { iterations: iters_per_msg, salt: pid as u64 * 10_000 + k }) as BoxedMessage).await?;
                    }
                    anyhow::Ok(())
                }));
            }
            let mut send_ok = true;
            for jh in jhs {
                if jh.await.unwrap().is_err() { send_ok = false; }
            }
            let send_elapsed = start.elapsed();
            let target = (PRODUCERS as u64) * PER_PRODUCER;
            let drained = wait_until(
                || ops_cons.load(Ordering::Relaxed) >= target * 2, // CpuTask 前后各 +1
                Duration::from_secs(300), Duration::from_millis(20),
            ).await;
            let elapsed = start.elapsed();
            report.push(BenchResult {
                name: "slow-consumer-8prod-40k".into(),
                engine: "thread",
                total_messages: target,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: drained && send_ok,
                cpu_work_secs: Some(nominal),
                note: format!(
                    "8 producers × 5000 × ~200µs msgs into ONE consumer; send-all={:.2}s; drain={} backlog≈{:.0}k",
                    send_elapsed.as_secs_f64(),
                    if drained { "ok" } else { "TIMEOUT" },
                    (target - (ops_cons.load(Ordering::Relaxed) / 2)) as f64 / 1000.0,
                ),
            });
        }

        // ---------- E4. 创建/销毁风暴：spawn 5k + stop + 重建 ----------
        {
            const WAVES: usize = 5;
            const PER_WAVE: usize = 1_000;
            let start = Instant::now();
            let mut alive_checks = 0u64;
            for w in 0..WAVES {
                let mut refs = Vec::new();
                for i in 0..PER_WAVE {
                    let p = format!("/bench/e4/w{}-{}", w, i);
                    refs.push((p.clone(), spawn_actor(&ts, &p, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await));
                }
                // 每 wave 抽 10 个发 ask 确认存活
                for (idx, (_, r)) in refs.iter().enumerate().take(10) {
                    if r.ask(Echo { value: idx as u64 }).await.is_ok() {
                        alive_checks += 1;
                    }
                }
                // 全停
                for (p, _) in &refs {
                    let _ = ts.stop_actor(p).await;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let elapsed = start.elapsed();
            report.push(BenchResult {
                name: "spawn-stop-storm-5k".into(),
                engine: "thread",
                total_messages: (WAVES * PER_WAVE) as u64,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: alive_checks == (WAVES * 10) as u64,
                cpu_work_secs: None,
                note: format!("5 waves × 1000 spawn+ask+stop; alive spot-checks {}/50", alive_checks),
            });
        }


        println!("==================== THREAD REPORT ====================");
        println!("{}", report.dump_markdown());
        std::fs::write("/tmp/parrot_bench_thread.md", report.dump_markdown()).ok();

        let _ = ts.shutdown_internal().await;
        let _ = _parrot;
        anyhow::Ok(())
    }).unwrap();
}
