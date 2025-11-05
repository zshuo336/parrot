//! Thread 引擎压测套件。
//!
//! 运行：`cargo test --test engine_stress_thread -- --nocapture --ignored`

mod engine_stress_common;
use futures::FutureExt;

use engine_stress_common::contracts::*;
use engine_stress_common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::{ActorRef, ActorRefExt};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 测试 actor：与 actix 版本逻辑完全一致
// ---------------------------------------------------------------------------

pub struct BenchActor {
    _owner: ActorOwner,
    pub ops: Arc<AtomicU64>,
    pub cpu_sink: Arc<AtomicU64>,
    pub messages: Arc<AtomicU64>,
}

impl BenchActor {
    pub fn new(ops: Arc<AtomicU64>, cpu_sink: Arc<AtomicU64>, owner: ActorOwner) -> Self {
        Self {
            _owner: owner,
            ops,
            cpu_sink,
            messages: Arc::new(AtomicU64::new(0)),
        }
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

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// 共享 dispatch：thread 引擎两条路径（receive_message / receive_message_with_engine）走同一逻辑。
fn dispatch_bench_message(actor: &mut BenchActor, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
    let out: BoxedMessage = if let Some(pause) = msg.downcast_ref::<PauseForTimeout>() {
        pause.execute()?;
        Box::new(())
    } else if let Some(t) = msg.downcast_ref::<GatedWork>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let Some(result) = t.solid()? else {
            return Ok(Box::new(()) as BoxedMessage);
        };
        actor.cpu_sink.fetch_add(result.value, Ordering::Relaxed);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(result)
    } else if let Some(probe) = msg.downcast_ref::<ProbeMessage>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(probe.execute())
    } else if let Some(t) = msg.downcast_ref::<CpuTask>() {
        // Pre-mark so tests can observe the task has entered the handler.
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(t.iterations, t.salt);
        actor.cpu_sink.fetch_add(r, Ordering::Relaxed);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(r)
    } else if let Some(t) = msg.downcast_ref::<IoTask>() {
        // 无法在同步 handler 内 sleep；用返回延迟值记录（IO 延迟模拟在 bench 层完成）
        Box::new(t.duration_ms)
    } else if let Some(t) = msg.downcast_ref::<TimedLongTask>() {
        actor.ops.fetch_add(1, Ordering::Relaxed);
        let measured = timed_cpu(t.total_iterations);
        actor.cpu_sink.fetch_add(measured.value, Ordering::Relaxed);
        actor.ops.fetch_add(1, Ordering::Relaxed);
        Box::new(measured)
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
        let r = burn_cpu(1_000, t.salt); // fixed 1000 iterations
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
#[allow(dead_code)] // bench 辅助 trait
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
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for IoTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for LongRunningTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for Echo {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for GetCount {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for MinuteCpuTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for MediumCpuTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for TinyTask {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for BatchEcho {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}

// ---------------------------------------------------------------------------
// 环境搭建
// ---------------------------------------------------------------------------

async fn setup() -> anyhow::Result<(ParrotActorSystem, Arc<ThreadActorSystem>)> {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await?;
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    parrot
        .register_thread_system("bench-thread".into(), ts.clone(), true)
        .await?;
    Ok((parrot, ts))
}

async fn spawn_actor(
    scope: &ActorScope,
    ts: &Arc<ThreadActorSystem>,
    path: &str,
    ops: Arc<AtomicU64>,
    cpu_sink: Arc<AtomicU64>,
) -> Box<dyn ActorRef> {
    let r = ts
        .spawn_at::<BenchActor>(
            BenchActor::new(ops, cpu_sink, scope.owner()),
            path,
            None,
            ThreadActorConfig::default(),
        )
        .await
        .expect("spawn ok");
    scope.track(&r);
    Box::new(r)
}

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
        unit: "messages",
        name: name.to_string(),
        engine: "thread",
        total_messages: n,
        elapsed,
        latencies: latency_stats(samples),
        correctness: true,
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
                let r = ar
                    .ask(Echo {
                        value: i + c as u64,
                    })
                    .await?;
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
        unit: "messages",
        name: name.to_string(),
        engine: "thread",
        total_messages: (concurrency * per_task as usize) as u64,
        elapsed,
        latencies: latency_stats(samples),
        correctness: correct,
        note: format!("concurrency={}", concurrency),
    });
    Ok(())
}

/// Sequential request/reply throughput; each reply confirms processing.
async fn bench_request_throughput(
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
        checked_u64(actor.as_ref(), Box::new(Echo { value: i }), i).await?;
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
        unit: "messages",
        name: name.to_string(),
        engine: "thread",
        total_messages: n,
        elapsed,
        latencies: latency_stats(vec![]),
        correctness: ok,
        note: if ok {
            String::new()
        } else {
            "DRAIN TIMEOUT".into()
        },
    });
    Ok(())
}

struct IoActor {
    _owner: ActorOwner,
    done: Arc<AtomicU64>,
}
impl Actor for IoActor {
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
        let done = self.done.clone();
        Box::pin(async move {
            // 真实异步等待（IO 模拟）
            tokio::time::sleep(Duration::from_millis(10)).await;
            done.fetch_add(1, Ordering::Relaxed);
            let value = msg
                .downcast_ref::<Echo>()
                .ok_or_else(|| ActorError::MessageHandlingError("expected IO Echo".into()))?
                .value;
            Ok(Box::new(value) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

pub struct ChunkActor {
    _owner: ActorOwner,
}

impl Actor for ChunkActor {
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
        Box::pin(async move {
            if let Some(work) = msg.downcast_ref::<GatedWork>() {
                return Ok(match work.chunked().await? {
                    Some(result) => Box::new(result) as BoxedMessage,
                    None => Box::new(()) as BoxedMessage,
                });
            }
            if let Some(probe) = msg.downcast_ref::<ProbeMessage>() {
                return Ok(Box::new(probe.execute()) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError(
                "unknown chunk message".into(),
            ))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ---------------------------------------------------------------------------
// 测试入口
// ---------------------------------------------------------------------------

// Fixed-work CPU experiments can take several minutes; duration depends on the host.
//   cargo test -p parrot --release            # 跳过压测
//   cargo test -p parrot --release -- --ignored --nocapture   # 只跑压测
#[test]
#[ignore]
#[allow(non_local_definitions)] // bench 场景内聚：类型与 impl 同点定义
fn thread_engine_full_suite() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let report = Report::new().with_runtime(format!("Tokio caller workers=8; shared Thread system defaults={:?}; actor defaults={:?}", ThreadActorSystemConfig::default(), ThreadActorConfig::default()));
        println!("==================== THREAD ENGINE STRESS ====================");

        let expected = oracle::prepare(std::thread::available_parallelism().unwrap().get());
        let (_parrot, ts) = setup().await.expect("setup");
        let scope = ActorScope::default();
        let result = std::panic::AssertUnwindSafe(async {

        // ---------- 1. Echo ask 基线 ----------
        let ops = Arc::new(AtomicU64::new(0));
        let cpu_sink = Arc::new(AtomicU64::new(0));
        let a1 = spawn_actor(&scope, &ts, "/bench/echo", ops.clone(), cpu_sink.clone()).await;
        bench_sequential_ask(&report, "seq-ask-echo-1k", a1.as_ref(), Echo { value: 0 }, 1_000).await.unwrap();
        scope.cleanup().await?;

        let ops2 = Arc::new(AtomicU64::new(0));
        let a2 = spawn_actor(&scope, &ts, "/bench/echo2", ops2.clone(), cpu_sink.clone()).await;
        bench_concurrent_ask(&report, "conc-ask-echo-c8-m1000", a2, 8, 1_000).await.unwrap();
        scope.cleanup().await?;

        let ops3 = Arc::new(AtomicU64::new(0));
        let a3 = spawn_actor(&scope, &ts, "/bench/echo3", ops3.clone(), cpu_sink.clone()).await;
        bench_concurrent_ask(&report, "conc-ask-echo-c64-m200", a3, 64, 200).await.unwrap();
        scope.cleanup().await?;
        // ---------- 2. request/reply throughput ----------
        let ops4 = Arc::new(AtomicU64::new(0));
        spawn_actor(&scope, &ts, "/bench/tell", ops4.clone(), cpu_sink.clone()).await;
        bench_request_throughput(&report, "request-echo-100k", &ts, "/bench/tell", 100_000, ops4.clone()).await.unwrap();

        scope.cleanup().await?;
        // ---------- 3. CPU 密集 ----------
        // 单 actor 串行 CPU：每次200000迭代
        let ops5 = Arc::new(AtomicU64::new(0));
        let a5 = spawn_actor(&scope, &ts, "/bench/cpu1", ops5.clone(), cpu_sink.clone()).await;
        {
            let mut samples = Vec::new();
            let start = Instant::now();
            for i in 0..200u64 {
                let t0 = Instant::now();
                let r = a5.ask(CpuTask { iterations: 200_000, salt: i }).await.unwrap();
                let completed = Instant::now();
                assert_eq!(r, expected[&(200_000, i)]);
                samples.push(completed.duration_since(t0).as_micros());
            }
            report.push(BenchResult {
                unit: "messages",
                name: "cpu-serial-200x200k".into(),
                engine: "thread",
                total_messages: 200,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: true,
                note: "200000 fixed iterations per message".into(),
            });
        }

        // 多 actor 并行 CPU：64 actor × 各 200k 迭代 × 25 条 = 并行度收益验证
        {
            scope.cleanup().await?;
            let ops_multi = Arc::new(AtomicU64::new(0));
            let mut refs = Vec::new();
            for i in 0..64 {
                let p = format!("/bench/cpu-parallel-{}", i);
                refs.push(spawn_actor(&scope, &ts, &p, ops_multi.clone(), cpu_sink.clone()).await);
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for (i, r) in refs.into_iter().enumerate() {
                let expected = expected.clone();
                handles.push(tokio::spawn(async move {
                    let mut samples = Vec::new();
                    for k in 0..25u64 {
                        let t0 = Instant::now();
                        let v = r.ask(CpuTask { iterations: 200_000, salt: i as u64 * 1000 + k }).await?;
                        samples.push(t0.elapsed().as_micros());
                        assert_eq!(v, expected[&(200_000, i as u64 * 1000 + k)]);
                    }
                    anyhow::Ok(samples)
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.await.unwrap().unwrap());
            }
            report.push(BenchResult {
                unit: "messages",
                name: "cpu-parallel-64actors-200k".into(),
                engine: "thread",
                total_messages: 64 * 25,
                elapsed: start.elapsed(),
                latencies: latency_stats(all),
                correctness: true,
                note: "64 actors × 25 msgs × 200k iters".into(),
            });
        }

        scope.cleanup().await?;
        // ---------- 4. 高压边界：邮箱洪泛 ----------
        {
            let ops_flood = Arc::new(AtomicU64::new(0));
            spawn_actor(&scope, &ts, "/bench/flood", ops_flood.clone(), cpu_sink.clone()).await;
            let actor = ts.get_actor_ref("/bench/flood").unwrap();
            let n = 500_000u64;
            let start = Instant::now();
            // Concurrent request/reply producers; every send waits for its reply.
            let mut jhs = Vec::new();
            for p in 0..4 {
                let ar = actor.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    for i in 0..(n / 4) {
                        checked_u64(ar.as_ref(), Box::new(Echo { value: i + p }), i + p).await?;
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
                unit: "messages",
                name: "flood-500k-requests".into(),
                engine: "thread",
                total_messages: n,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: drained && ok,
                note: format!("sent in {:.3}s; {}", sent_elapsed.as_secs_f64(), if drained { "drained" } else { "TIMEOUT" }),
            });
        }

        scope.cleanup().await?;
        // ---------- 5. timeout after confirmed handler entry ----------
        {
            let at = spawn_actor(&scope, &ts, "/bench/timeout", Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            let start = Instant::now();
            let correct = exercise_request_timeout(at).await?;
            report.push(BenchResult {
                unit: "messages",
                name: "ask-timeout-short".into(), engine: "thread",
                total_messages: 2, elapsed: start.elapsed(),
                latencies: latency_stats(vec![]), correctness: correct,
                note: "1ms request deadline while an acknowledged handler is gated".into(),
            });
        }

        scope.cleanup().await?;
        // ---------- 6. 停止后发送边界 ----------
        {
            let ops_s = Arc::new(AtomicU64::new(0));
            let asref = spawn_actor(&scope, &ts, "/bench/stop", ops_s.clone(), cpu_sink.clone()).await;
            scope.cleanup().await?;
            let r = asref.send(Box::new(Echo { value: 1 }) as BoxedMessage).await;
            report.push(BenchResult {
                unit: "messages",
                name: "send-after-stop".into(),
                engine: "thread",
                total_messages: 1,
                elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]),
                correctness: matches!(r, Err(ActorError::Stopped)) && !asref.is_alive().await,
                note: format!("send after stop => {:?}", r.err().map(|e| e.to_string())),
            });
        }

        scope.cleanup().await?;
        // ---------- 7. 大量 actor 存活性 ----------
        {
            let start = Instant::now();
            let mut paths = Vec::new();
            for i in 0..20_000 {
                let p = format!("/bench/herd-{}", i);
                let ops_h = Arc::new(AtomicU64::new(0));
                spawn_actor(&scope, &ts, &p, ops_h, cpu_sink.clone()).await;
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
                        checked_u64(a.as_ref(), Box::new(Echo { value: 1 }), 1).await.is_ok()
                    }));
                }
            }
            for jh in jhs {
                if jh.await.unwrap() { ok += 1; }
            }
            report.push(BenchResult {
                unit: "messages",
                name: "herd-20000-actors".into(),
                engine: "thread",
                total_messages: 20_000,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok == 20_000,
                note: format!("spawned 20000 in {:.3}s; all requests replied", spawn_elapsed.as_secs_f64()),
            });
        }

        scope.cleanup().await?;
        // ---------- 8. 长时程任务（分片 CPU） ----------
        {
            let ops_l = Arc::new(AtomicU64::new(0));
            let al = spawn_actor(&scope, &ts, "/bench/longrun", ops_l.clone(), cpu_sink.clone()).await;
            let start = Instant::now();
            // 一次 2 秒级别的计算（约 2G 迭代）；显式放宽超时以覆盖长时程
            let r = al
                .send_with_timeout(Box::new(LongRunningTask { total_iterations: 2_000_000_000 }), Some(STRESS_REQUEST_TIMEOUT))
                .await?;
            let elapsed = start.elapsed();
            let val = *r.downcast::<u64>().map_err(|_| anyhow::anyhow!("long work reply type"))?;
            report.push(BenchResult {
                unit: "messages",
                name: "longrun-2G-iters".into(),
                engine: "thread",
                total_messages: 1,
                elapsed,
                latencies: latency_stats(vec![elapsed.as_micros()]),
                correctness: val == expected[&(2_000_000_000, 7)],
                note: "single 2G-iteration compute".into(),
            });
        }


        scope.cleanup().await?;
        // ---------- 9. Cross-actor probes overlapping the actual CPU interval ----------
        {
            let ops_a = Arc::new(AtomicU64::new(0));
            let ops_b = Arc::new(AtomicU64::new(0));
            let aa = spawn_actor(&scope, &ts, "/bench/starve-a", ops_a, cpu_sink.clone()).await;
            let ab = spawn_actor(&scope, &ts, "/bench/starve-b", ops_b, cpu_sink.clone()).await;
            let stop = Arc::new(AtomicU64::new(0));
            let samples = Arc::new(std::sync::Mutex::new(Vec::<ProbeSample>::new()));
            let (baseline_ready, baseline_received) = tokio::sync::oneshot::channel();
            let ticker = {
                let ab = ab.clone_boxed();
                let stop = stop.clone();
                let samples = samples.clone();
                tokio::spawn(async move {
                    let mut i = 0u64;
                    let mut baseline_ready = Some(baseline_ready);
                    while stop.load(Ordering::Relaxed) == 0 {
                        let started = Instant::now();
                        let value = Echo::extract_result(ab.send_with_timeout(Box::new(Echo { value: i }), Some(STRESS_REQUEST_TIMEOUT)).await?)?;
                        let completed = Instant::now();
                        assert_eq!(value, i);
                        samples.lock().unwrap().push(ProbeSample { started, completed });
                        if let Some(ready) = baseline_ready.take() {
                            let _ = ready.send(());
                        }
                        i += 1;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    anyhow::Ok(())
                })
            };
            // A real response proves the ticker has started; executor scheduling
            // is not inferred from a sleep.
            let baseline_result = baseline_received.await;
            if baseline_result.is_err() {
                ticker.await??;
                anyhow::bail!("probe ended without a baseline response");
            }
            // This is a deliberate baseline collection period, not a proxy for
            // where the heavy handler begins or ends.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let work = aa.send_with_timeout(Box::new(TimedLongTask { total_iterations: 1_000_000_000 }), Some(STRESS_REQUEST_TIMEOUT)).await;
            stop.store(1, Ordering::Relaxed);
            let ticker_result = ticker.await;
            let measured = *work?.downcast::<TimedWorkResult>().map_err(|_| anyhow::anyhow!("heavy handler returned the wrong result type"))?;
            ticker_result??;
            let window = probe_window(&samples.lock().unwrap(), measured.started, measured.completed)?;
            report.push(BenchResult {
                unit: "messages",
                name: "starve-echo-during-longrun".into(),
                engine: "thread",
                total_messages: window.completed,
                elapsed: measured.completed.duration_since(measured.started),
                latencies: window.overlapping.clone(),
                correctness: window.baseline.count > 0 && measured.value == expected[&(1_000_000_000, 7)],
                note: format!("CPU interval measured inside handler; {} probe completions within interval; {} overlapping request round trips (may finish after CPU work); baseline n={} p99={:.2}ms. Zero progress is a measured outcome, not a harness failure.", window.completed, window.overlapping.count, window.baseline.count, window.baseline.p99_us as f64 / 1000.0),
            });
        }

        scope.cleanup().await?;
        // ---------- 10. IO 密集：async handler 内真实 await ----------
        // Thread 引擎的 receive_message 是 async fn，可以在 handler 内做真实 IO 等待。
        {
            let io_done = Arc::new(AtomicU64::new(0));
            // 独立的 IO actor 类型：handler 内 sleep 10ms（模拟 IO 等待）
            // 64 actors × 20 requests × 10ms async IO; observe actual elapsed time.
            let mut refs = Vec::new();
            for i in 0..64 {
                let p = format!("/bench/io-{}", i);
                let r = ts.spawn_at::<IoActor>(IoActor { done: io_done.clone(), _owner: scope.owner() }, &p, None, ThreadActorConfig::default()).await.expect("io spawn");
                scope.track(&r);
                refs.push(r);
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for r in refs {
                handles.push(tokio::spawn(async move {
                    for _ in 0..20u64 {
                        checked_u64(&r, Box::new(Echo { value: 1 }), 1).await?;
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
                unit: "messages",
                name: "io-async-64actors-10ms".into(),
                engine: "thread",
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


        // =====================================================================
        // 第二轮扩展场景（2026-10-02）：分钟级 CPU + 混合负载 + 极端场景
        // =====================================================================

        scope.cleanup().await?;
        // ---------- M1. 分钟级任务 + 持续加入新长任务/中任务 + 短探测 ----------
        // Eight actors execute 57.2G iterations each.
        // 同时 4 个后备 actor 持续接秒级新任务；独立探测器测短任务延迟。
        {
            const LONG_ACTORS: usize = 8;
            const BACKUP_ACTORS: usize = 4;

            let mut long_refs = Vec::new();
            for i in 0..LONG_ACTORS {
                let p = format!("/bench/m1/long-{}", i);
                long_refs.push(spawn_actor(&scope, &ts, &p, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await);
            }
            let mut backup_refs = Vec::new();
            for i in 0..BACKUP_ACTORS {
                let p = format!("/bench/m1/backup-{}", i);
                backup_refs.push(spawn_actor(&scope, &ts, &p, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await);
            }
            let probe = spawn_actor(&scope, &ts, "/bench/m1/probe", Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;

            report.push(mixed_workload("thread", long_refs, backup_refs, probe, expected.clone()).await?);
        }

        scope.cleanup().await?;
        // ---------- M2. Confirmed backlog probe ----------
        {
            let shared = spawn_actor(&scope, &ts, "/bench/m2/shared", Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            report.push(mixed_backlog("thread", shared, expected.clone()).await?);
        }

        scope.cleanup().await?;
        // ---------- M3. Same computation and admitted probes ----------
        {
            let chunked = ts.spawn_at::<ChunkActor>(ChunkActor {_owner:scope.owner()}, "/bench/m3/chunked", None, ThreadActorConfig::default()).await?;
            scope.track(&chunked);
            let chunked:Box<dyn ActorRef> = Box::new(chunked);
            let solid = spawn_actor(&scope, &ts, "/bench/m3/solid", Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            report.push(compare_chunked("thread", solid.as_ref(), chunked.as_ref(), 30_000_000_000, 1_500_000_000, expected[&(30_000_000_000,3)]).await?);
        }

        scope.cleanup().await?;
        // ---------- E1. Caller-mediated pair of requests ----------
        {
            let ops_p = Arc::new(AtomicU64::new(0));
            let pa = spawn_actor(&scope, &ts, "/bench/e1/pa", ops_p.clone(), cpu_sink.clone()).await;
            let pb = spawn_actor(&scope, &ts, "/bench/e1/pb", ops_p.clone(), cpu_sink.clone()).await;
            const BOUNCES: u64 = 10_000;
            let mut samples = Vec::with_capacity(BOUNCES as usize / 10);
            let start = Instant::now();
            // Both requests are initiated by the caller.
            let mut i = 0u64;
            while i < BOUNCES {
                let t0 = Instant::now();
                let va = pa.ask(Echo { value: i }).await?;
                let vb = pb.ask(Echo { value: va }).await?;
                assert_eq!(vb, i);
                if i.is_multiple_of(10) { samples.push(t0.elapsed().as_micros()); }
                i += 1;
            }
            report.push(BenchResult {
                unit: "messages",
                name: "caller-two-actor-requests-10k".into(),
                engine: "thread",
                total_messages: BOUNCES * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: true,
                note: "caller→A then caller→B sequential request pair, sampled every 10th".into(),
            });
        }

        scope.cleanup().await?;
            // Both requests are initiated by the caller.
        {
            // Back-to-back request/reply calls to the same actor.
            // （handler 内自 ask 会死锁邮箱，故用外部驱动的退化链式衡量
            // "对同一 actor 连续 request/reply" 的引擎开销。）
            let ops_c = Arc::new(AtomicU64::new(0));
            let ca = spawn_actor(&scope, &ts, "/bench/e2/chain", ops_c.clone(), cpu_sink.clone()).await;
            const CHAIN_TICKS: u64 = 20_000;
            let start = Instant::now();
            let mut i = 0u64;
            while i < CHAIN_TICKS {
                // ask 自身不能在 handler 内做（会死锁邮箱）；改为 ask→完成→
                // 立刻再 ask 同一 actor，衡量背靠背自传递吞吐
                let v = ca.ask(Echo { value: i }).await?;
                anyhow::ensure!(v == i, "first chained reply");
                checked_u64(ca.as_ref(), Box::new(Echo { value: v + 1 }), i + 1).await?;
                i += 1;
            }
            let ok = wait_until(
                || ops_c.load(Ordering::Relaxed) >= CHAIN_TICKS * 2,
                Duration::from_secs(60), Duration::from_millis(2),
            ).await;
            report.push(BenchResult {
                unit: "messages",
                name: "back-to-back-requests-20k".into(),
                engine: "thread",
                total_messages: CHAIN_TICKS * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok,
                note: "back-to-back request/reply to same actor".into(),
            });
        }

        scope.cleanup().await?;
        // ---------- E3. 慢消费者 + 快生产者（有界邮箱挤压） ----------
        {
            const PRODUCERS: usize = 8;
            const PER_PRODUCER: u64 = 5_000;   // 每个 5000 条

            let iters_per_msg: u64 = 170_000; // fixed iterations, not a duration guarantee

            let ops_cons = Arc::new(AtomicU64::new(0));
            let consumer = spawn_actor(&scope, &ts, "/bench/e3/slow", ops_cons.clone(), cpu_sink.clone()).await;

            let start = Instant::now();
            let mut jhs = Vec::new();
            for pid in 0..PRODUCERS {
                let cr = consumer.clone_boxed();
                let expected = expected.clone();
                jhs.push(tokio::spawn(async move {
                    for k in 0..PER_PRODUCER {
                        checked_u64(cr.as_ref(), Box::new(CpuTask { iterations: iters_per_msg, salt: pid as u64 * 10_000 + k }), expected[&(iters_per_msg, pid as u64 * 10_000 + k)]).await?;
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
                unit: "messages",
                name: "slow-consumer-8prod-40k-requests".into(),
                engine: "thread",
                total_messages: target,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: drained && send_ok,
                note: format!(
                    "8 producers × 5000 × 170000-iteration requests into ONE consumer; send-all={:.2}s; drain={} backlog≈{:.0}k",
                    send_elapsed.as_secs_f64(),
                    if drained { "ok" } else { "TIMEOUT" },
                    (target - (ops_cons.load(Ordering::Relaxed) / 2)) as f64 / 1000.0,
                ),
            });
        }

        scope.cleanup().await?;
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
                    refs.push((p.clone(), spawn_actor(&scope, &ts, &p, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await));
                }
                // 每 wave 抽 10 个发 ask 确认存活
                for (idx, (_, r)) in refs.iter().enumerate().take(10) {
                    if checked_u64(r.as_ref(), Box::new(Echo { value: idx as u64 }), idx as u64).await.is_ok() {
                        alive_checks += 1;
                    }
                }
                // 全停
                scope.cleanup().await?;
            }
            let elapsed = start.elapsed();
            report.push(BenchResult {
                unit: "lifecycle operations",
                name: "spawn-stop-storm-5k".into(),
                engine: "thread",
                total_messages: (WAVES * (PER_WAVE + 10)) as u64,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: alive_checks == (WAVES * 10) as u64,
                note: format!("5000 spawn/stop cycles; 5050 operations = 5000 stop calls + 50 echo replies; exact spot-checks {}/50; every actor destroyed", alive_checks),
            });
        }


        println!("==================== THREAD REPORT ====================");
        println!("{}", report.dump_markdown());


        scope.cleanup().await?;
        anyhow::ensure!(report.results.lock().unwrap().len() == 20, "all scenarios must execute");
        println!("[stress-owners] thread created={} destroyed={}", scope.counts().0, scope.counts().1);
        report.verify()
        }).catch_unwind().await;
        let shutdown = _parrot.internal_shutdown().await;
        match result {
            Ok(result) => { shutdown?; result?; report.save(std::env::var_os("PARROT_STRESS_REPORT").unwrap_or_else(|| "/tmp/parrot_bench_thread.md".into())) },
            Err(panic) => { shutdown.expect("cleanup after failed stress run"); std::panic::resume_unwind(panic) },
        }
    }).unwrap();
}

#[test]
fn timeout_probe_observes_actual_handler_entry_and_release() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (parrot, sys) = setup().await.unwrap();
        let scope = ActorScope::default();
        let actor = spawn_actor(
            &scope,
            &sys,
            "/timeout-probe",
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
        .await;
        let result = exercise_request_timeout(actor).await;
        scope.cleanup().await.unwrap();
        parrot.internal_shutdown().await.unwrap();
        assert!(
            result.unwrap(),
            "the gated actor must return a timeout, not another error"
        );
    });
}

fn run_runtime_contract(
    test: impl for<'a> FnOnce(
        &'a Arc<ThreadActorSystem>,
        &'a ActorScope,
    ) -> futures::future::LocalBoxFuture<'a, anyhow::Result<()>>,
) {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (parrot, sys) = setup().await.unwrap();
            let scope = ActorScope::default();
            let result = std::panic::AssertUnwindSafe(test(&sys, &scope))
                .catch_unwind()
                .await;
            let cleanup = scope.cleanup().await;
            let shutdown = parrot.internal_shutdown().await;
            cleanup.unwrap();
            shutdown.unwrap();
            match result {
                Ok(result) => result.unwrap(),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        });
}

#[test]
fn actual_cpu_echo_and_async_io_replies_have_exact_values() {
    run_runtime_contract(|sys, scope| {
        Box::pin(async move {
            let sink = Arc::new(AtomicU64::new(0));
            let actor = spawn_actor(
                scope,
                sys,
                "/contract/solid",
                Arc::new(AtomicU64::new(0)),
                sink.clone(),
            )
            .await;
            for message in [
                Box::new(CpuTask {
                    iterations: 107,
                    salt: 3,
                }) as BoxedMessage,
                Box::new(MediumCpuTask {
                    iterations: 107,
                    salt: 3,
                }),
                Box::new(MinuteCpuTask {
                    iterations: 107,
                    salt: 3,
                }),
            ] {
                checked_u64(actor.as_ref(), message, 4046730678074085073).await?;
            }
            anyhow::ensure!(
                sink.load(Ordering::Relaxed) == 4046730678074085073u64.wrapping_mul(3),
                "CPU completion checksum"
            );
            checked_u64(actor.as_ref(), Box::new(Echo { value: 42 }), 42).await?;
            checked_u64(
                actor.as_ref(),
                Box::new(TinyTask { salt: 3 }),
                oracle::reference(1000, 3),
            )
            .await?;
            checked_u64(
                actor.as_ref(),
                Box::new(LongRunningTask {
                    total_iterations: 107,
                }),
                oracle::reference(107, 7),
            )
            .await?;
            let done = Arc::new(AtomicU64::new(0));
            let io = sys
                .spawn_at::<IoActor>(
                    IoActor {
                        done: done.clone(),
                        _owner: scope.owner(),
                    },
                    "/contract/io",
                    None,
                    ThreadActorConfig::default(),
                )
                .await?;
            scope.track(&io);
            let io: Box<dyn ActorRef> = Box::new(io);
            checked_u64(io.as_ref(), Box::new(Echo { value: 42 }), 42).await?;
            anyhow::ensure!(done.load(Ordering::Relaxed) == 1, "IO completion receipt");
            Ok(())
        })
    });
}

#[test]
fn queued_probes_measure_real_busy_actors_and_cancel_unreleased_work() {
    run_runtime_contract(|sys, scope| {
        Box::pin(async move {
            let sink = Arc::new(AtomicU64::new(0));
            let solid = spawn_actor(
                scope,
                sys,
                "/contract/solid",
                Arc::new(AtomicU64::new(0)),
                sink.clone(),
            )
            .await;
            let chunked = sys
                .spawn_at::<ChunkActor>(
                    ChunkActor {
                        _owner: scope.owner(),
                    },
                    "/contract/chunk",
                    None,
                    ThreadActorConfig::default(),
                )
                .await?;
            scope.track(&chunked);
            let chunked: Box<dyn ActorRef> = Box::new(chunked);
            for (total, chunk) in [(107, 8), (200_003, 8_000), (0, 0)] {
                compare_chunked(
                    "thread",
                    solid.as_ref(),
                    chunked.as_ref(),
                    total,
                    chunk,
                    oracle::reference(total, 3),
                )
                .await?;
            }
            let before = sink.load(Ordering::Relaxed);
            let pending = QueuedProbe::start(solid.as_ref(), 1_000_000_000, 3, None, 5).await?;
            drop(pending);
            checked_u64(solid.as_ref(), Box::new(Echo { value: 7 }), 7).await?;
            anyhow::ensure!(
                sink.load(Ordering::Relaxed) == before,
                "abandoned gated work must not start CPU computation"
            );
            Ok(())
        })
    });
}

#[test]
fn stage_cleanup_destroys_every_actor_even_while_caller_addresses_survive() {
    run_runtime_contract(|sys, scope| {
        Box::pin(async move {
            for wave in 0..2u64 {
                let mut addresses = Vec::new();
                for i in 0..32u64 {
                    let actor = spawn_actor(
                        scope,
                        sys,
                        &format!("/contract/w{wave}/{i}"),
                        Arc::new(AtomicU64::new(0)),
                        Arc::new(AtomicU64::new(0)),
                    )
                    .await;
                    checked_u64(actor.as_ref(), Box::new(Echo { value: i }), i).await?;
                    addresses.push(actor);
                }
                scope.cleanup().await?;
                anyhow::ensure!(
                    scope.counts() == ((wave + 1) * 32, (wave + 1) * 32),
                    "actual owner destruction count"
                );
                for actor in addresses {
                    anyhow::ensure!(!actor.is_alive().await, "old caller address remained live");
                }
            }
            Ok(())
        })
    });
}
