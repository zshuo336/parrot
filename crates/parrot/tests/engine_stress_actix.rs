//! Actix 引擎压测套件（双引擎采用相同固定计算量；调度与准入分别测量）。
//!
//! 运行：`cargo test --test engine_stress_actix -- --nocapture --ignored`

mod engine_stress_common;
use futures::FutureExt;

use engine_stress_common::contracts::*;
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
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 测试 actor（逻辑与 thread 版一致）
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

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// M6: actix 同步快路径移至引擎侧扩展 trait（ActixEngineExt）。
impl parrot_api::actor::ActixEngineExt for BenchActor {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        let out: BoxedMessage = if let Some(pause) = msg.downcast_ref::<PauseForTimeout>() {
            if let Err(error) = pause.execute() {
                return Some(Err(error));
            }
            Box::new(())
        } else if let Some(t) = msg.downcast_ref::<GatedWork>() {
            self.ops.fetch_add(1, Ordering::Relaxed);
            let result = match t.solid() {
                Ok(Some(value)) => value,
                Ok(None) => return Some(Ok(Box::new(()) as BoxedMessage)),
                Err(error) => return Some(Err(error)),
            };
            self.cpu_sink.fetch_add(result.value, Ordering::Relaxed);
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(result)
        } else if let Some(probe) = msg.downcast_ref::<ProbeMessage>() {
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(probe.execute())
        } else if let Some(t) = msg.downcast_ref::<CpuTask>() {
            // Pre-mark so tests can observe the task has entered the handler.
            self.ops.fetch_add(1, Ordering::Relaxed);
            let r = burn_cpu(t.iterations, t.salt);
            self.cpu_sink.fetch_add(r, Ordering::Relaxed);
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(r)
        } else if let Some(t) = msg.downcast_ref::<IoTask>() {
            // 同步 handler 中不能 sleep；记为延迟值（延迟模拟见 bench 内 tokio::spawn）
            Box::new(t.duration_ms)
        } else if let Some(t) = msg.downcast_ref::<TimedLongTask>() {
            self.ops.fetch_add(1, Ordering::Relaxed);
            let measured = timed_cpu(t.total_iterations);
            self.cpu_sink.fetch_add(measured.value, Ordering::Relaxed);
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(measured)
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
        } else if let Some(t) = msg.downcast_ref::<MinuteCpuTask>() {
            self.ops.fetch_add(1, Ordering::Relaxed);
            let r = burn_cpu(t.iterations, t.salt);
            self.cpu_sink.fetch_add(r, Ordering::Relaxed);
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(r)
        } else if let Some(t) = msg.downcast_ref::<MediumCpuTask>() {
            self.ops.fetch_add(1, Ordering::Relaxed);
            let r = burn_cpu(t.iterations, t.salt);
            self.cpu_sink.fetch_add(r, Ordering::Relaxed);
            self.ops.fetch_add(1, Ordering::Relaxed);
            Box::new(r)
        } else if let Some(t) = msg.downcast_ref::<TinyTask>() {
            self.ops.fetch_add(1, Ordering::Relaxed);
            let r = burn_cpu(1_000, t.salt);
            self.cpu_sink.fetch_add(r, Ordering::Relaxed);
            Box::new(r)
        } else if let Some(b) = msg.downcast_ref::<BatchEcho>() {
            self.ops.fetch_add(1, Ordering::Relaxed);
            let mut acc = b.value;
            for k in 0..b.batch {
                acc = acc.wrapping_add(burn_cpu(20_000, b.value ^ k));
            }
            self.cpu_sink.fetch_add(acc, Ordering::Relaxed);
            Box::new(acc)
        } else if msg.is::<GetCount>() {
            Box::new(self.ops.load(Ordering::Relaxed))
        } else {
            return Some(Err(ActorError::MessageHandlingError("unknown msg".into())));
        };
        self.messages.fetch_add(1, Ordering::Relaxed);
        Some(Ok(out))
    }
}

// Message impls（与 thread 版相同）
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
impl Message for ChunkedLongTask {
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

async fn setup() -> anyhow::Result<(ParrotActorSystem, ActixActorSystem)> {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await?;
    let actix_sys = ActixActorSystem::new().await?;
    parrot
        .register_actix_system("bench-actix".into(), actix_sys.clone(), true)
        .await?;
    Ok((parrot, actix_sys))
}

async fn spawn_actor(
    scope: &ActorScope,
    sys: &ActixActorSystem,
    ops: Arc<AtomicU64>,
    cpu_sink: Arc<AtomicU64>,
) -> Box<dyn ActorRef> {
    let actor = sys
        .spawn_root_typed(BenchActor::new(ops, cpu_sink, scope.owner()), EmptyConfig)
        .await
        .expect("spawn ok");
    scope.track(actor.as_ref());
    actor
}

// ---------------------------------------------------------------------------
// 异步 handler actor：验证 actix 适配层的 async IO 能力
// ---------------------------------------------------------------------------

/// `use_async_handler() == true`：所有消息走 `receive_message`（async fn），
/// handler 内部做真实异步等待（模拟 IO）。与 thread 版 IoActor 逻辑对齐。
pub struct IoAsyncActor {
    _owner: ActorOwner,
    pub done: Arc<AtomicU64>,
}

// M6: async opt-in actor——同步快路径默认 None。
impl parrot_api::actor::ActixEngineExt for IoAsyncActor {}

impl Actor for IoAsyncActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

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
            // 真实异步等待（IO 模拟）：await 期间释放 arbiter 线程
            tokio::time::sleep(Duration::from_millis(10)).await;
            done.fetch_add(1, Ordering::Relaxed);
            let value = msg
                .downcast_ref::<Echo>()
                .ok_or_else(|| ActorError::MessageHandlingError("expected IO Echo".into()))?
                .value;
            Ok(Box::new(value) as BoxedMessage)
        })
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

async fn bench_sequential_ask(
    report: &Report,
    name: &str,
    actor: &dyn ActorRef,
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
    report.push(BenchResult {
        unit: "messages",
        name: name.into(),
        engine: "actix",
        total_messages: n,
        elapsed: start.elapsed(),
        latencies: latency_stats(samples),
        correctness: true,
        note: String::new(),
    });
    Ok(())
}

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
    report.push(BenchResult {
        unit: "messages",
        name: name.into(),
        engine: "actix",
        total_messages: (concurrency * per_task as usize) as u64,
        elapsed: start.elapsed(),
        latencies: latency_stats(samples),
        correctness: correct,
        note: format!("concurrency={}", concurrency),
    });
    Ok(())
}

async fn bench_request_throughput(
    report: &Report,
    name: &str,
    sys: &ActixActorSystem,
    actor: Box<dyn ActorRef>,
    n: u64,
    ops: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let start = Instant::now();
    for i in 0..n {
        checked_u64(actor.as_ref(), Box::new(Echo { value: i }), i).await?;
    }
    let ok = wait_until(
        || ops.load(Ordering::Relaxed) >= n,
        Duration::from_secs(120),
        Duration::from_millis(5),
    )
    .await;
    report.push(BenchResult {
        unit: "messages",
        name: name.into(),
        engine: "actix",
        total_messages: n,
        elapsed: start.elapsed(),
        latencies: latency_stats(vec![]),
        correctness: ok,
        note: if ok {
            String::new()
        } else {
            "DRAIN TIMEOUT".into()
        },
    });
    let _ = sys;
    Ok(())
}

pub struct ChunkActor {
    _owner: ActorOwner,
}
impl parrot_api::actor::ActixEngineExt for ChunkActor {}
impl Actor for ChunkActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
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
    fn use_async_handler(&self) -> bool {
        true
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
fn actix_engine_full_suite() {
    // Actix 需要 System 上下文；在其内运行 tokio multi-thread 不冲突（actix System 自带 arbiter）
    actix::System::new().block_on(async {
        // tokio IO/timer 能力：actix System 基于 tokio，可直接使用
        let report = Report::new().with_runtime(format!("Actix System caller; default settings={:?}", parrot::actix::system::ActixSystemSettings::default()));
        println!("==================== ACTIX ENGINE STRESS ====================");

        let expected = oracle::prepare(std::thread::available_parallelism().unwrap().get());
        let (parrot, sys) = setup().await.expect("setup");
        let scope = ActorScope::default();
        let result = std::panic::AssertUnwindSafe(async {

        // ---------- 1. Echo ask 基线 ----------
        let ops = Arc::new(AtomicU64::new(0));
        let cpu_sink = Arc::new(AtomicU64::new(0));
        let a1 = spawn_actor(&scope, &sys, ops.clone(), cpu_sink.clone()).await;
        bench_sequential_ask(&report, "seq-ask-echo-1k", a1.as_ref(), 1_000).await.unwrap();
        scope.cleanup().await?;

        let ops2 = Arc::new(AtomicU64::new(0));
        let a2 = spawn_actor(&scope, &sys, ops2.clone(), cpu_sink.clone()).await;
        bench_concurrent_ask(&report, "conc-ask-echo-c8-m1000", a2, 8, 1_000).await.unwrap();
        scope.cleanup().await?;

        let ops3 = Arc::new(AtomicU64::new(0));
        let a3 = spawn_actor(&scope, &sys, ops3.clone(), cpu_sink.clone()).await;
        bench_concurrent_ask(&report, "conc-ask-echo-c64-m200", a3, 64, 200).await.unwrap();
        scope.cleanup().await?;
        // ---------- 2. request/reply throughput ----------
        let ops4 = Arc::new(AtomicU64::new(0));
        let a4 = spawn_actor(&scope, &sys, ops4.clone(), cpu_sink.clone()).await;
        bench_request_throughput(&report, "request-echo-100k", &sys, a4, 100_000, ops4.clone()).await.unwrap();

        scope.cleanup().await?;
        // ---------- 3. CPU 密集 ----------
        {
            let ops5 = Arc::new(AtomicU64::new(0));
            let a5 = spawn_actor(&scope, &sys, ops5.clone(), cpu_sink.clone()).await;
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
                name: "cpu-serial-200x200k".into(), engine: "actix",
                total_messages: 200, elapsed: start.elapsed(),
                latencies: latency_stats(samples), correctness: true,
                note: "200000 fixed iterations per message".into(),
            });
        }
        {
            // 并行 CPU：actor 分散到 arbiter 池（默认 CPU 数量个 OS 线程）
            scope.cleanup().await?;
            let ops_multi = Arc::new(AtomicU64::new(0));
            let mut refs = Vec::new();
            for _ in 0..64 {
                refs.push(spawn_actor(&scope, &sys, ops_multi.clone(), cpu_sink.clone()).await);
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
            for h in handles { all.extend(h.await.unwrap().unwrap()); }
            report.push(BenchResult {
                unit: "messages",
                name: "cpu-parallel-64actors-200k".into(), engine: "actix",
                total_messages: 64 * 25, elapsed: start.elapsed(),
                latencies: latency_stats(all), correctness: true,
                note: "64 actors × 25 msgs × 200k iters (pooled arbiters)".into(),
            });
        }

        scope.cleanup().await?;
        // ---------- 4. 洪泛 ----------
        {
            let ops_flood = Arc::new(AtomicU64::new(0));
            let af = spawn_actor(&scope, &sys, ops_flood.clone(), cpu_sink.clone()).await;
            let n = 500_000u64;
            let start = Instant::now();
            let mut jhs = Vec::new();
            for p in 0..4 {
                let ar = af.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    for i in 0..(n / 4) {
                        checked_u64(ar.as_ref(), Box::new(Echo { value: i + p }), i + p).await?;
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
                unit: "messages",
                name: "flood-500k-requests".into(), engine: "actix",
                total_messages: n, elapsed: start.elapsed(),
                latencies: latency_stats(vec![]), correctness: drained && ok,
                note: format!("sent in {:.3}s; {}", sent_elapsed.as_secs_f64(), if drained { "drained" } else { "TIMEOUT" }),
            });
        }

        scope.cleanup().await?;
        // ---------- 5. timeout after confirmed handler entry ----------
        {
            let at = spawn_actor(&scope, &sys, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            let start = Instant::now();
            let correct = exercise_request_timeout(at).await?;
            report.push(BenchResult {
                unit: "messages",
                name: "ask-timeout-short".into(), engine: "actix",
                total_messages: 2, elapsed: start.elapsed(),
                latencies: latency_stats(vec![]), correctness: correct,
                note: "1ms request deadline while an acknowledged handler is gated".into(),
            });
        }

        scope.cleanup().await?;
        // ---------- 6. 停止后发送 ----------
        {
            let ops_s = Arc::new(AtomicU64::new(0));
            let asref = spawn_actor(&scope, &sys, ops_s.clone(), cpu_sink.clone()).await;
            let alive_before = asref.is_alive().await;
            scope.cleanup().await?;
            let alive_after = asref.is_alive().await;
            let r = asref.send(Box::new(Echo { value: 1 }) as BoxedMessage).await;
            report.push(BenchResult {
                unit: "messages",
                name: "send-after-stop".into(), engine: "actix",
                total_messages: 1, elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]), correctness: matches!(r, Err(ActorError::Stopped)) && !asref.is_alive().await,
                note: format!(
                    "alive before={}, after={}; send => {:?}",
                    alive_before, alive_after,
                    r.err().map(|e| e.to_string())
                ),
            });
        }

        scope.cleanup().await?;
        // ---------- 7. 大量 actor ----------
        {
            let start = Instant::now();
            let mut refs = Vec::new();
            let mut ok = 0;
            for i in 0..20_000 {
                let ops_h = Arc::new(AtomicU64::new(0));
                let r = spawn_actor(&scope, &sys, ops_h, cpu_sink.clone()).await;
                // 唯一路径后注册表不再互相覆盖
                let _ = i;
                refs.push(r);
            }
            let spawn_elapsed = start.elapsed();
            let mut jhs = Vec::new();
            for r in refs {
                let a = r.clone_boxed();
                jhs.push(tokio::spawn(async move {
                    checked_u64(a.as_ref(), Box::new(Echo { value: 1 }), 1).await.is_ok()
                }));
            }
            for jh in jhs { if jh.await.unwrap() { ok += 1; } }
            report.push(BenchResult {
                unit: "messages",
                name: "herd-20000-actors".into(), engine: "actix",
                total_messages: 20_000, elapsed: start.elapsed(),
                latencies: latency_stats(vec![]), correctness: ok == 20_000,
                note: format!("spawned 20000 in {:.3}s; all requests replied", spawn_elapsed.as_secs_f64()),
            });
        }

        scope.cleanup().await?;
        // ---------- 8. 长时程 ----------
        {
            let ops_l = Arc::new(AtomicU64::new(0));
            let al = spawn_actor(&scope, &sys, ops_l.clone(), cpu_sink.clone()).await;
            let start = Instant::now();
            let r = al.send_with_timeout(Box::new(LongRunningTask { total_iterations: 2_000_000_000 }), Some(STRESS_REQUEST_TIMEOUT)).await?;
            let r = *r.downcast::<u64>().map_err(|_| anyhow::anyhow!("long work reply type"))?;
            let elapsed = start.elapsed();
            report.push(BenchResult {
                unit: "messages",
                name: "longrun-2G-iters".into(), engine: "actix",
                total_messages: 1, elapsed,
                latencies: latency_stats(vec![elapsed.as_micros()]), correctness: r == expected[&(2_000_000_000, 7)],
                note: "single 2G-iteration compute".into(),
            });
        }


        scope.cleanup().await?;
        // ---------- 9. Cross-actor probes overlapping the actual CPU interval ----------
        {
            let ops_a = Arc::new(AtomicU64::new(0));
            let ops_b = Arc::new(AtomicU64::new(0));
            let aa = spawn_actor(&scope, &sys, ops_a, cpu_sink.clone()).await;
            let ab = spawn_actor(&scope, &sys, ops_b, cpu_sink.clone()).await;
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
                engine: "actix",
                total_messages: window.completed,
                elapsed: measured.completed.duration_since(measured.started),
                latencies: window.overlapping.clone(),
                correctness: window.baseline.count > 0 && measured.value == expected[&(1_000_000_000, 7)],
                note: format!("CPU interval measured inside handler; {} probe completions within interval; {} overlapping request round trips (may finish after CPU work); baseline n={} p99={:.2}ms. Zero progress is a measured outcome, not a harness failure.", window.completed, window.overlapping.count, window.baseline.count, window.baseline.p99_us as f64 / 1000.0),
            });
        }

        scope.cleanup().await?;
        // ---------- 10. IO 密集：async handler 内真实 await ----------
        // `use_async_handler() == true` 的 actor：消息走 receive_message
        // （async fn），handler 内 tokio::time::sleep(10ms) 模拟 IO 等待。
        // 64 actor × 各 20 条：await 期间 arbiter 线程被释放去跑其他
        // actor 的 handler，总墙钟时间应远小于串行时间。
        {
            let io_done = Arc::new(AtomicU64::new(0));
            let mut refs = Vec::new();
            for _ in 0..64 {
                let actor = sys.spawn_root_typed(IoAsyncActor { done: io_done.clone(), _owner: scope.owner() }, EmptyConfig).await.expect("spawn ok");
                scope.track(actor.as_ref());
                refs.push(actor);
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for r in refs {
                handles.push(tokio::spawn(async move {
                    for _ in 0..20u64 {
                        checked_u64(r.as_ref(), Box::new(Echo { value: 1 }), 1).await?;
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


        // =====================================================================
        // 第二轮扩展场景（2026-10-02）：分钟级 CPU + 混合负载 + 极端场景
        // （与 thread 版逐字节对齐的负载参数）
        // =====================================================================

        scope.cleanup().await?;
        // ---------- M1. 分钟级任务 + 持续加入新长任务/中任务 + 短探测 ----------
        {
            const LONG_ACTORS: usize = 8;
            const BACKUP_ACTORS: usize = 4;

            let mut long_refs = Vec::new();
            for i in 0..LONG_ACTORS {
                let ops_i = Arc::new(AtomicU64::new(0));
                let r = spawn_actor(&scope, &sys, ops_i, cpu_sink.clone()).await;
                let _ = i;
                long_refs.push(r);
            }
            let mut backup_refs = Vec::new();
            for i in 0..BACKUP_ACTORS {
                let ops_i = Arc::new(AtomicU64::new(0));
                let r = spawn_actor(&scope, &sys, ops_i, cpu_sink.clone()).await;
                let _ = i;
                backup_refs.push(r);
            }
            let probe = spawn_actor(&scope, &sys, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;

            report.push(mixed_workload("actix", long_refs, backup_refs, probe, expected.clone()).await?);
        }

        scope.cleanup().await?;
        // ---------- M2. Confirmed backlog probe ----------
        {
            let shared = spawn_actor(&scope, &sys, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            report.push(mixed_backlog("actix", shared, expected.clone()).await?);
        }

        scope.cleanup().await?;
        // ---------- M3. Same computation and admitted probes ----------
        {
            let chunked = sys.spawn_root_typed(ChunkActor {_owner:scope.owner()}, EmptyConfig).await?;
            scope.track(chunked.as_ref());
            let solid = spawn_actor(&scope, &sys, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            report.push(compare_chunked("actix", solid.as_ref(), chunked.as_ref(), 30_000_000_000, 1_500_000_000, expected[&(30_000_000_000,3)]).await?);
        }

        scope.cleanup().await?;
        // ---------- E1. Caller-mediated pair of requests ----------
        {
            let pa = spawn_actor(&scope, &sys, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            let pb = spawn_actor(&scope, &sys, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
            const BOUNCES: u64 = 10_000;
            let mut samples = Vec::with_capacity(BOUNCES as usize / 10);
            let start = Instant::now();
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
                engine: "actix",
                total_messages: BOUNCES * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: true,
                note: "caller→A then caller→B sequential request pair, sampled every 10th".into(),
            });
        }

        scope.cleanup().await?;
        // ---------- E2. Back-to-back caller requests ----------
        {
            let ops_c = Arc::new(AtomicU64::new(0));
            let ca = spawn_actor(&scope, &sys, ops_c.clone(), cpu_sink.clone()).await;
            const CHAIN_TICKS: u64 = 20_000;
            let start = Instant::now();
            let mut i = 0u64;
            while i < CHAIN_TICKS {
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
                engine: "actix",
                total_messages: CHAIN_TICKS * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok,
                note: "back-to-back request/reply to same actor".into(),
            });
        }

        scope.cleanup().await?;
        // ---------- E3. 慢消费者 + 快生产者 ----------
        {
            const PRODUCERS: usize = 8;
            const PER_PRODUCER: u64 = 5_000;
            let iters_per_msg: u64 = 170_000; // fixed iterations, not a duration guarantee

            let ops_cons = Arc::new(AtomicU64::new(0));
            let consumer = spawn_actor(&scope, &sys, ops_cons.clone(), cpu_sink.clone()).await;

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
                || ops_cons.load(Ordering::Relaxed) >= target * 2,
                Duration::from_secs(300), Duration::from_millis(20),
            ).await;
            let elapsed = start.elapsed();
            report.push(BenchResult {
                unit: "messages",
                name: "slow-consumer-8prod-40k-requests".into(),
                engine: "actix",
                total_messages: target,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: drained && send_ok,
                note: format!(
                    "8 producers × 5000 × 170000-iteration requests into ONE consumer; send-all={:.2}s; drain={}",
                    send_elapsed.as_secs_f64(),
                    if drained { "ok" } else { "TIMEOUT" },
                ),
            });
        }

        scope.cleanup().await?;
        // ---------- E4. 创建/销毁风暴 ----------
        {
            const WAVES: usize = 5;
            const PER_WAVE: usize = 1_000;
            let start = Instant::now();
            let mut alive_checks = 0u64;
            for _w in 0..WAVES {
                let mut refs = Vec::new();
                for _i in 0..PER_WAVE {
                    let r = spawn_actor(&scope, &sys, Arc::new(AtomicU64::new(0)), cpu_sink.clone()).await;
                    refs.push(r);
                }
                for (idx, r) in refs.iter().take(10).enumerate() {
                    if checked_u64(r.as_ref(), Box::new(Echo { value: idx as u64 }), idx as u64).await.is_ok() {
                        alive_checks += 1;
                    }
                }
                scope.cleanup().await?;
            }
            let elapsed = start.elapsed();
            report.push(BenchResult {
                unit: "lifecycle operations",
                name: "spawn-stop-storm-5k".into(),
                engine: "actix",
                total_messages: (WAVES * (PER_WAVE + 10)) as u64,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: alive_checks == (WAVES * 10) as u64,
                note: format!("5000 spawn/stop cycles; 5050 operations = 5000 stop calls + 50 echo replies; exact spot-checks {}/50; every actor destroyed", alive_checks),
            });
        }


        println!("==================== ACTIX REPORT ====================");
        println!("{}", report.dump_markdown());


        scope.cleanup().await?;
        anyhow::ensure!(report.results.lock().unwrap().len() == 20, "all scenarios must execute");
        println!("[stress-owners] actix created={} destroyed={}", scope.counts().0, scope.counts().1);
        report.verify()
        }).catch_unwind().await;
        let shutdown = parrot.internal_shutdown().await;
        match result {
            Ok(result) => { shutdown?; result?; report.save(std::env::var_os("PARROT_STRESS_REPORT").unwrap_or_else(|| "/tmp/parrot_bench_actix.md".into())) },
            Err(panic) => { shutdown.expect("cleanup after failed stress run"); std::panic::resume_unwind(panic) },
        }
    }).unwrap();
}

#[test]
fn timeout_probe_observes_actual_handler_entry_and_release() {
    actix::System::new().block_on(async {
        let (parrot, sys) = setup().await.unwrap();
        let scope = ActorScope::default();
        let actor = spawn_actor(
            &scope,
            &sys,
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
        &'a ActixActorSystem,
        &'a ActorScope,
    ) -> futures::future::LocalBoxFuture<'a, anyhow::Result<()>>,
) {
    actix::System::new().block_on(async {
        let (parrot, sys) = setup().await.unwrap();
        let scope = ActorScope::default();
        let result = std::panic::AssertUnwindSafe(test(&sys, &scope))
            .catch_unwind()
            .await;
        let cleanup = scope.cleanup().await;
        let shutdown = parrot.internal_shutdown().await;
        match result {
            Ok(result) => {
                if let Err(error) = result {
                    panic!("contract: {error:?}; cleanup: {cleanup:?}; shutdown: {shutdown:?}");
                }
                cleanup.unwrap();
                shutdown.unwrap();
            }
            Err(panic) => {
                eprintln!("panic cleanup: {cleanup:?}; shutdown: {shutdown:?}");
                std::panic::resume_unwind(panic)
            }
        }
    });
}

#[test]
fn actual_cpu_echo_and_async_io_replies_have_exact_values() {
    run_runtime_contract(|sys, scope| {
        Box::pin(async move {
            let sink = Arc::new(AtomicU64::new(0));
            let actor = spawn_actor(scope, sys, Arc::new(AtomicU64::new(0)), sink.clone()).await;
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
                .spawn_root_typed(
                    IoAsyncActor {
                        done: done.clone(),
                        _owner: scope.owner(),
                    },
                    EmptyConfig,
                )
                .await?;
            scope.track(io.as_ref());
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
            let solid = spawn_actor(scope, sys, Arc::new(AtomicU64::new(0)), sink.clone()).await;
            let chunked = sys
                .spawn_root_typed(
                    ChunkActor {
                        _owner: scope.owner(),
                    },
                    EmptyConfig,
                )
                .await?;
            scope.track(chunked.as_ref());
            for (total, chunk) in [(107, 8), (200_003, 8_000), (0, 0)] {
                compare_chunked(
                    "actix",
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
