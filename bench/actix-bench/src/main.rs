//! 纯 Actix 基准（零 parrot 依赖）。
//!
//! 目的：量化 parrot 包装层对 actix 的性能损耗（wrapper tax）。与
//! `parrot/tests/engine_stress_actix.rs`（经 parrot 适配层的 actix 引擎）
//! 场景**逐字节对等**：
//!   - 相同消息语义/数量/每条 CPU 迭代数（burn_cpu 同 LCG 常数）
//!   - 相同分位数算法（ceil 索引）
//!   - 相同 arbiter 拓扑（num_cpus 个 worker arbiter，round-robin 分配）
//!   - 相同调用语义：parrot `send()`/`send_with_timeout(None)` = `addr.send().await`
//!     （ask 往返）；parrot `deliver()`/`tell()` = `addr.do_send`。本基准逐一对齐。
//!
//! 唯一差异：**不经过 parrot**——原生 `actix::Actor`/`Handler<M>` 静态分发，
//! 无 BoxedMessage 装箱、无 MessageEnvelope/Uuid、无 AtomicResponse 回复装箱、
//! 无 downcast 链、无注册表写锁。
//!
//! 运行（位于 bench/ 基准目录，与 akka-bench 平级）：
//! ```sh
//! cd bench/actix-bench && cargo run --release   # 或 ./run.sh
//! ```
//! 报告写入 /tmp/parrot_bench_actix_raw.md，与 /tmp/parrot_bench_actix.md
//! （parrot 包装版）逐行相减即为包装税。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use actix::prelude::*;

// ===========================================================================
// 负载内核（与 parrot 版 engine_stress_common::burn_cpu 逐字节一致）
// ===========================================================================

#[inline]
fn burn_cpu(iterations: u64, salt: u64) -> u64 {
    let mut x: u64 = salt | 1;
    for i in 0..iterations {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) ^ i;
        if x == 42 {
            return x;
        }
    }
    x
}

/// 实测 burn_cpu 速率（iters/sec，与 parrot 版一致）。
const BURN_RATE: f64 = 880_000_000.0;

// ===========================================================================
// 统计（与 parrot 版 latency_stats 一致：ceil 索引分位）
// ===========================================================================

#[derive(Debug, Clone)]
struct LatencyStats {
    count: u64,
    p50_us: u128,
    p90_us: u128,
    p99_us: u128,
    max_us: u128,
    mean_us: f64,
}

fn latency_stats(mut samples: Vec<u128>) -> LatencyStats {
    if samples.is_empty() {
        return LatencyStats { count: 0, p50_us: 0, p90_us: 0, p99_us: 0, max_us: 0, mean_us: 0.0 };
    }
    samples.sort_unstable();
    let n = samples.len();
    let pick = |q: f64| -> u128 {
        let idx = (((q / 100.0) * n as f64).ceil() as usize).saturating_sub(1).min(n - 1);
        samples[idx]
    };
    let sum: u128 = samples.iter().sum();
    LatencyStats {
        count: n as u64,
        p50_us: pick(50.0),
        p90_us: pick(90.0),
        p99_us: pick(99.0),
        max_us: *samples.last().unwrap(),
        mean_us: sum as f64 / n as f64,
    }
}

// ===========================================================================
// 报告（与 parrot 版 Report 对齐；engine 列 = "actix-raw"）
// ===========================================================================

#[derive(Clone)]
struct BenchResult {
    name: String,
    engine: &'static str, // "actix-raw"
    total_messages: u64,
    elapsed: Duration,
    latencies: LatencyStats,
    correctness: bool,
    note: String,
    cpu_work_secs: Option<f64>,
}

impl BenchResult {
    fn msg_per_sec(&self) -> f64 {
        self.total_messages as f64 / self.elapsed.as_secs_f64().max(f64::EPSILON)
    }

    fn utilization(&self) -> f64 {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8) as f64;
        self.cpu_work_secs
            .map(|w| w / (self.elapsed.as_secs_f64().max(f64::EPSILON) * cores))
            .unwrap_or(0.0)
    }
}

struct Report {
    results: Mutex<Vec<BenchResult>>,
}

impl Report {
    fn new() -> Self {
        Self { results: Mutex::new(Vec::new()) }
    }

    fn push(&self, r: BenchResult) {
        let util = r
            .cpu_work_secs
            .map(|_| format!(" | util={:.0}%", r.utilization() * 100.0))
            .unwrap_or_default();
        println!(
            "[{}] {} | msgs={} | wall={:.3}s | tput={:.0}/s | lat p50={:.1}ms p90={:.1}ms p99={:.1}ms max={:.1}ms{} | correct={}",
            r.engine,
            r.name,
            r.total_messages,
            r.elapsed.as_secs_f64(),
            r.msg_per_sec(),
            r.latencies.p50_us as f64 / 1000.0,
            r.latencies.p90_us as f64 / 1000.0,
            r.latencies.p99_us as f64 / 1000.0,
            r.latencies.max_us as f64 / 1000.0,
            util,
            r.correctness,
        );
        if !r.note.is_empty() {
            println!("        note: {}", r.note);
        }
        self.results.lock().unwrap().push(r);
    }

    fn dump_markdown(&self) -> String {
        let mut out = String::from(
            "| 场景 | 引擎 | 消息数 | 耗时(s) | 吞吐(/s) | p50(ms) | p90(ms) | p99(ms) | max(ms) | util | 正确 |\n|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        let mut rows = self.results.lock().unwrap().clone();
        rows.sort_by(|a, b| (&a.name, a.engine).cmp(&(&b.name, b.engine)));
        for r in rows {
            let util = r
                .cpu_work_secs
                .map(|_| format!("{:.0}%", r.utilization() * 100.0))
                .unwrap_or_else(|| "-".into());
            out.push_str(&format!(
                "| {} | {} | {} | {:.3} | {:.0} | {:.2} | {:.2} | {:.2} | {:.2} | {} | {} |\n",
                r.name,
                r.engine,
                r.total_messages,
                r.elapsed.as_secs_f64(),
                r.msg_per_sec(),
                r.latencies.p50_us as f64 / 1000.0,
                r.latencies.p90_us as f64 / 1000.0,
                r.latencies.p99_us as f64 / 1000.0,
                r.latencies.max_us as f64 / 1000.0,
                util,
                r.correctness,
            ));
        }
        out
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

// ===========================================================================
// 原生 actix 消息（强类型；#[rtype] 与 parrot 版回复类型 u64 一致）
// ===========================================================================

#[derive(Message)]
#[rtype(u64)]
struct Echo {
    v: u64,
}

#[derive(Message)]
#[rtype(u64)]
struct CpuTask {
    iters: u64,
    salt: u64,
}

#[derive(Message)]
#[rtype(u64)]
struct TinyTask {
    salt: u64,
}

#[derive(Message)]
#[rtype(u64)]
struct MediumCpu {
    iters: u64,
    salt: u64,
}

#[derive(Message)]
#[rtype(u64)]
struct MinuteCpu {
    iters: u64,
    salt: u64,
}

#[derive(Message)]
#[rtype(u64)]
struct LongRun {
    iters: u64,
}

#[derive(Message)]
#[rtype(u64)]
struct GetCount;

/// 停止指令（对等 parrot StopMessage 拦截 → ctx.stop()）
#[derive(Message)]
#[rtype(result = "()")]
struct StopNow;

/// 分片长任务（对等 parrot ChunkedLongTask：async handler + 片间让出）
#[derive(Message)]
#[rtype(u64)]
struct ChunkedLong {
    total: u64,
    chunk: u64,
    salt: u64,
}

// ===========================================================================
// 同步路径 BenchActor（对等 parrot 版 BenchActor 的负载逻辑；
// ops 计数点位与 parrot 版完全一致）
// ===========================================================================

struct RawBenchActor {
    ops: Arc<AtomicU64>,
    cpu_sink: Arc<AtomicU64>,
}

impl Actor for RawBenchActor {
    type Context = actix::Context<Self>;
}

impl Handler<Echo> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, m: Echo, _ctx: &mut Self::Context) -> Self::Result {
        self.ops.fetch_add(1, Ordering::Relaxed);
        m.v
    }
}

impl Handler<CpuTask> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, m: CpuTask, _ctx: &mut Self::Context) -> Self::Result {
        self.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(m.iters, m.salt);
        self.cpu_sink.fetch_add(r, Ordering::Relaxed);
        self.ops.fetch_add(1, Ordering::Relaxed);
        r
    }
}

impl Handler<TinyTask> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, m: TinyTask, _ctx: &mut Self::Context) -> Self::Result {
        self.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(1_000, m.salt);
        self.cpu_sink.fetch_add(r, Ordering::Relaxed);
        r
    }
}

impl Handler<MediumCpu> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, m: MediumCpu, _ctx: &mut Self::Context) -> Self::Result {
        self.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(m.iters, m.salt);
        self.cpu_sink.fetch_add(r, Ordering::Relaxed);
        self.ops.fetch_add(1, Ordering::Relaxed);
        r
    }
}

impl Handler<MinuteCpu> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, m: MinuteCpu, _ctx: &mut Self::Context) -> Self::Result {
        self.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(m.iters, m.salt);
        self.cpu_sink.fetch_add(r, Ordering::Relaxed);
        self.ops.fetch_add(1, Ordering::Relaxed);
        r
    }
}

impl Handler<LongRun> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, m: LongRun, _ctx: &mut Self::Context) -> Self::Result {
        self.ops.fetch_add(1, Ordering::Relaxed);
        let r = burn_cpu(m.iters, 7);
        self.cpu_sink.fetch_add(r, Ordering::Relaxed);
        self.ops.fetch_add(1, Ordering::Relaxed);
        r
    }
}

impl Handler<GetCount> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, _m: GetCount, _ctx: &mut Self::Context) -> Self::Result {
        self.ops.load(Ordering::Relaxed)
    }
}

impl Handler<StopNow> for RawBenchActor {
    type Result = ();
    fn handle(&mut self, _m: StopNow, ctx: &mut Self::Context) -> Self::Result {
        ctx.stop();
    }
}

// ===========================================================================
// IO actor（对等 parrot IoAsyncActor：handler 内真实异步 sleep(10ms)，
// await 期间释放 arbiter 线程）
// ===========================================================================

struct RawIoActor {
    done: Arc<AtomicU64>,
}

impl Actor for RawIoActor {
    type Context = actix::Context<Self>;
}

impl Handler<Echo> for RawIoActor {
    type Result = ResponseActFuture<Self, u64>;
    fn handle(&mut self, _m: Echo, _ctx: &mut Self::Context) -> Self::Result {
        let done = self.done.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            done.fetch_add(1, Ordering::Relaxed);
            1u64
        }
        .into_actor(self))
    }
}

// ===========================================================================
// 分片长任务 actor（对等 parrot ChunkAsyncActor：片间 tokio yield）
// ===========================================================================

struct RawChunkActor;

impl Actor for RawChunkActor {
    type Context = actix::Context<Self>;
}

impl Handler<ChunkedLong> for RawChunkActor {
    type Result = ResponseActFuture<Self, u64>;
    fn handle(&mut self, m: ChunkedLong, _ctx: &mut Self::Context) -> Self::Result {
        Box::pin(
            async move {
                let total = m.total;
                let chunk = m.chunk.max(1);
                let mut acc: u64 = m.salt;
                let mut done = 0u64;
                while done < total {
                    let take = chunk.min(total - done);
                    acc = acc.wrapping_add(burn_cpu(take, acc));
                    done += take;
                    tokio::task::yield_now().await;
                }
                acc
            }
            .into_actor(self),
        )
    }
}

impl Handler<TinyTask> for RawChunkActor {
    type Result = u64;
    fn handle(&mut self, m: TinyTask, _ctx: &mut Self::Context) -> Self::Result {
        burn_cpu(1_000, m.salt)
    }
}

// ===========================================================================
// Arbiter 池：复刻 parrot ActixActorSystem::ArbiterPool（round-robin），
// 保证拓扑对等（否则包装税会混入拓扑差异）。
// ===========================================================================

struct ArbiterPool {
    handles: Vec<ArbiterHandle>,
    next: AtomicU64,
}

impl ArbiterPool {
    fn new(size: usize) -> Self {
        let handles = (0..size).map(|_| Arbiter::new().handle()).collect();
        Self { handles, next: AtomicU64::new(0) }
    }

    fn next_arbiter(&self) -> ArbiterHandle {
        let i = self.next.fetch_add(1, Ordering::Relaxed) as usize % self.handles.len();
        self.handles[i].clone()
    }
}

fn spawn_in_pool(pool: &ArbiterPool, ops: Arc<AtomicU64>, cpu_sink: Arc<AtomicU64>) -> Addr<RawBenchActor> {
    actix::Actor::start_in_arbiter(&pool.next_arbiter(), move |_| RawBenchActor { ops, cpu_sink })
}

// ===========================================================================
// 主流程：与 engine_stress_actix.rs 场景逐一对应（编号一致）
// ===========================================================================

fn main() {
    System::new().block_on(async {
        let report = Report::new();
        println!("==================== ACTIX RAW (no parrot wrapper) ====================");

        let cpu_sink = Arc::new(AtomicU64::new(0));
        // 与 parrot ActixActorSystem::new() 默认一致：arbiter 数 = CPU 数
        let pool = ArbiterPool::new(num_cpus());

        // ---------- 1. Echo ask 基线 ----------
        {
            let ops = Arc::new(AtomicU64::new(0));
            let a1 = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let mut samples = Vec::with_capacity(1_000);
            let start = Instant::now();
            for i in 0..1_000u64 {
                let t0 = Instant::now();
                let r = a1.send(Echo { v: i }).await.unwrap();
                assert_eq!(r, i);
                samples.push(t0.elapsed().as_micros());
            }
            report.push(BenchResult {
                name: "seq-ask-echo-1k".into(),
                engine: "actix-raw",
                total_messages: 1_000,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: true,
                note: String::new(),
                cpu_work_secs: None,
            });
        }
        for (name, conc, per) in [
            ("conc-ask-echo-c8-m1000", 8usize, 1_000u64),
            ("conc-ask-echo-c64-m200", 64, 200),
        ] {
            let ops = Arc::new(AtomicU64::new(0));
            let addr = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let mut samples = Vec::with_capacity(conc * per as usize);
            let start = Instant::now();
            let mut handles = Vec::with_capacity(conc);
            for c in 0..conc {
                let ar = addr.clone();
                handles.push(tokio::spawn(async move {
                    let mut local = Vec::with_capacity(per as usize);
                    for i in 0..per {
                        let t0 = Instant::now();
                        let r = ar.send(Echo { v: i + c as u64 }).await.unwrap();
                        assert_eq!(r, i + c as u64);
                        local.push(t0.elapsed().as_micros());
                    }
                    local
                }));
            }
            let mut correct = true;
            for h in handles {
                match h.await {
                    Ok(mut s) => samples.append(&mut s),
                    Err(_) => correct = false,
                }
            }
            report.push(BenchResult {
                name: name.into(),
                engine: "actix-raw",
                total_messages: (conc * per as usize) as u64,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: correct,
                note: format!("concurrency={}", conc),
                cpu_work_secs: None,
            });
        }

        // ---------- 2. tell 吞透（对齐 parrot 语义：send().await 逐条等待）----------
        {
            let ops = Arc::new(AtomicU64::new(0));
            let a4 = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let n = 100_000u64;
            let start = Instant::now();
            for i in 0..n {
                let _ = a4.send(Echo { v: i }).await;
            }
            let ok = wait_until(
                || ops.load(Ordering::Relaxed) >= n,
                Duration::from_secs(120),
                Duration::from_millis(5),
            )
            .await;
            report.push(BenchResult {
                name: "tell-echo-100k".into(),
                engine: "actix-raw",
                total_messages: n,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok,
                note: if ok { String::new() } else { "DRAIN TIMEOUT".into() },
                cpu_work_secs: None,
            });
        }
        // 附加：原生 do_send 纯投递（actix 原生 fire-and-forget 上限参考）
        {
            let ops = Arc::new(AtomicU64::new(0));
            let a4 = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let n = 100_000u64;
            let start = Instant::now();
            for i in 0..n {
                a4.do_send(Echo { v: i });
            }
            let sent_elapsed = start.elapsed();
            let ok = wait_until(
                || ops.load(Ordering::Relaxed) >= n,
                Duration::from_secs(120),
                Duration::from_millis(5),
            )
            .await;
            report.push(BenchResult {
                name: "tell-echo-100k-dosend".into(),
                engine: "actix-raw",
                total_messages: n,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok,
                note: format!("do_send fired in {:.3}s (native fire-and-forget)", sent_elapsed.as_secs_f64()),
                cpu_work_secs: None,
            });
        }

        // ---------- 3. CPU 密集 ----------
        {
            let ops = Arc::new(AtomicU64::new(0));
            let a5 = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let mut samples = Vec::new();
            let start = Instant::now();
            for i in 0..200u64 {
                let t0 = Instant::now();
                let r = a5.send(CpuTask { iters: 200_000, salt: i }).await.unwrap();
                assert!(r > 0);
                samples.push(t0.elapsed().as_micros());
            }
            report.push(BenchResult {
                name: "cpu-serial-200x200k".into(),
                engine: "actix-raw",
                total_messages: 200,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: true,
                note: "~200µs work/msg".into(),
                cpu_work_secs: None,
            });
        }
        {
            let ops_multi = Arc::new(AtomicU64::new(0));
            let mut addrs = Vec::new();
            for _ in 0..64 {
                addrs.push(spawn_in_pool(&pool, ops_multi.clone(), cpu_sink.clone()));
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for (i, r) in addrs.into_iter().enumerate() {
                handles.push(tokio::spawn(async move {
                    let mut samples = Vec::new();
                    for k in 0..25u64 {
                        let t0 = Instant::now();
                        let v = r.send(CpuTask { iters: 200_000, salt: i as u64 * 1000 + k }).await.unwrap();
                        samples.push(t0.elapsed().as_micros());
                        assert!(v > 0);
                    }
                    samples
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.await.unwrap());
            }
            report.push(BenchResult {
                name: "cpu-parallel-8actors-200k".into(),
                engine: "actix-raw",
                total_messages: 64 * 25,
                elapsed: start.elapsed(),
                latencies: latency_stats(all),
                correctness: true,
                note: "64 actors × 25 msgs × 200k iters (pooled arbiters)".into(),
                cpu_work_secs: None,
            });
        }

        // ---------- 4. 洪泛（对齐 parrot：4 任务并发 send().await 往返）----------
        {
            let ops_flood = Arc::new(AtomicU64::new(0));
            let af = spawn_in_pool(&pool, ops_flood.clone(), cpu_sink.clone());
            let n = 500_000u64;
            let start = Instant::now();
            let mut jhs = Vec::new();
            for p in 0..4u64 {
                let ar = af.clone();
                jhs.push(tokio::spawn(async move {
                    for i in 0..(n / 4) {
                        let _ = ar.send(Echo { v: i + p }).await;
                    }
                }));
            }
            let mut ok = true;
            for jh in jhs {
                if jh.await.is_err() {
                    ok = false;
                }
            }
            let sent_elapsed = start.elapsed();
            let drained = wait_until(
                || ops_flood.load(Ordering::Relaxed) >= n,
                Duration::from_secs(300),
                Duration::from_millis(20),
            )
            .await;
            report.push(BenchResult {
                name: "flood-500k-tell".into(),
                engine: "actix-raw",
                total_messages: n,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: drained && ok,
                note: format!(
                    "sent in {:.3}s; {}",
                    sent_elapsed.as_secs_f64(),
                    if drained { "drained" } else { "TIMEOUT" }
                ),
                cpu_work_secs: None,
            });
        }
        // 附加：原生 do_send 洪泛
        {
            let ops_flood = Arc::new(AtomicU64::new(0));
            let af = spawn_in_pool(&pool, ops_flood.clone(), cpu_sink.clone());
            let n = 500_000u64;
            let start = Instant::now();
            let mut jhs = Vec::new();
            for p in 0..4u64 {
                let ar = af.clone();
                jhs.push(tokio::spawn(async move {
                    for i in 0..(n / 4) {
                        ar.do_send(Echo { v: i + p });
                    }
                }));
            }
            let mut ok = true;
            for jh in jhs {
                if jh.await.is_err() {
                    ok = false;
                }
            }
            let sent_elapsed = start.elapsed();
            let drained = wait_until(
                || ops_flood.load(Ordering::Relaxed) >= n,
                Duration::from_secs(300),
                Duration::from_millis(20),
            )
            .await;
            report.push(BenchResult {
                name: "flood-500k-dosend".into(),
                engine: "actix-raw",
                total_messages: n,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: drained && ok,
                note: format!(
                    "do_send sent in {:.3}s; {}",
                    sent_elapsed.as_secs_f64(),
                    if drained { "drained" } else { "TIMEOUT" }
                ),
                cpu_work_secs: None,
            });
        }

        // ---------- 5. ask 超时边界 ----------
        {
            let ops_t = Arc::new(AtomicU64::new(0));
            let at = spawn_in_pool(&pool, ops_t.clone(), cpu_sink.clone());
            for s in 1..=2u64 {
                at.do_send(CpuTask { iters: 50_000_000, salt: s });
            }
            let started = wait_until(
                || ops_t.load(Ordering::Relaxed) >= 1,
                Duration::from_secs(10),
                Duration::from_millis(1),
            )
            .await;
            assert!(started, "heavy task must start");
            // 独立 OS 线程 + current-thread runtime 探测 1ms 超时
            let probe_addr = at.clone();
            let probe = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    probe_addr
                        .send(Echo { v: 1 })
                        .timeout(Duration::from_millis(1))
                        .await
                })
            });
            let r = probe.join().expect("probe thread");
            let correct = r.is_err();
            report.push(BenchResult {
                name: "ask-timeout-short".into(),
                engine: "actix-raw",
                total_messages: 2,
                elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]),
                correctness: correct,
                note: format!("short-timeout ask result err: {:?}", r.err().map(|e| e.to_string())),
                cpu_work_secs: None,
            });
        }

        // ---------- 6. 停止后发送 ----------
        {
            let ops_s = Arc::new(AtomicU64::new(0));
            let asref = spawn_in_pool(&pool, ops_s.clone(), cpu_sink.clone());
            let alive_before = asref.connected();
            asref.do_send(StopNow);
            tokio::time::sleep(Duration::from_millis(50)).await;
            let alive_after = asref.connected();
            let r = asref.send(Echo { v: 1 }).await;
            report.push(BenchResult {
                name: "send-after-stop".into(),
                engine: "actix-raw",
                total_messages: 1,
                elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]),
                correctness: r.is_err(),
                note: format!(
                    "alive before={}, after={}; send => {:?}",
                    alive_before,
                    alive_after,
                    r.err().map(|e| e.to_string())
                ),
                cpu_work_secs: None,
            });
        }

        // ---------- 7. 大量 actor ----------
        {
            let start = Instant::now();
            let mut addrs = Vec::new();
            for _ in 0..20_000 {
                addrs.push(spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone()));
            }
            let spawn_elapsed = start.elapsed();
            let mut jhs = Vec::new();
            for r in addrs {
                let a = r.clone();
                jhs.push(tokio::spawn(async move {
                    a.send(Echo { v: 1 }).await.is_ok()
                }));
            }
            let mut ok = 0u64;
            for jh in jhs {
                if jh.await.unwrap() {
                    ok += 1;
                }
            }
            report.push(BenchResult {
                name: "herd-20000-actors".into(),
                engine: "actix-raw",
                total_messages: 20_000,
                elapsed: spawn_elapsed,
                latencies: latency_stats(vec![]),
                correctness: ok == 20_000,
                note: format!(
                    "spawned 20000 in {:.3}s; all accepted send",
                    spawn_elapsed.as_secs_f64()
                ),
                cpu_work_secs: None,
            });
        }

        // ---------- 8. 长时程 ----------
        {
            let ops_l = Arc::new(AtomicU64::new(0));
            let al = spawn_in_pool(&pool, ops_l.clone(), cpu_sink.clone());
            let start = Instant::now();
            let r = al.send(LongRun { iters: 2_000_000_000 }).await.unwrap();
            let elapsed = start.elapsed();
            report.push(BenchResult {
                name: "longrun-2G-iters".into(),
                engine: "actix-raw",
                total_messages: 1,
                elapsed,
                latencies: latency_stats(vec![elapsed.as_micros()]),
                correctness: r > 0,
                note: "single 2G-iteration compute".into(),
                cpu_work_secs: None,
            });
        }

        // ---------- 9. 饥饿交叉验证（ticker 探测法） ----------
        {
            let ops_a = Arc::new(AtomicU64::new(0));
            let ops_b = Arc::new(AtomicU64::new(0));
            let aa = spawn_in_pool(&pool, ops_a.clone(), cpu_sink.clone());
            let ab = spawn_in_pool(&pool, ops_b.clone(), cpu_sink.clone());

            let stop = Arc::new(AtomicU64::new(0));
            let t_origin = Instant::now();
            let samples = Arc::new(Mutex::new(Vec::<(u128, u128)>::new()));
            let ticker = {
                let ab = ab.clone();
                let stop = stop.clone();
                let samples = samples.clone();
                tokio::spawn(async move {
                    let mut i = 0u64;
                    while stop.load(Ordering::Relaxed) == 0 {
                        let t0 = Instant::now();
                        if let Ok(v) = ab.send(Echo { v: i }).await {
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
                let ar = aa.clone();
                async move {
                    let _ = ar.send(LongRun { iters: 1_000_000_000 }).await;
                }
            });
            let started = wait_until(
                || ops_a.load(Ordering::Relaxed) > 0,
                Duration::from_secs(10),
                Duration::from_millis(1),
            )
            .await;
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
            } else {
                during.clone()
            };
            let during_stats = latency_stats(during_new);

            let _ = heavy.await;
            let compute_elapsed = heavy_start.elapsed();
            stop.store(1, Ordering::Relaxed);
            let _ = ticker.await;

            report.push(BenchResult {
                name: "starve-echo-during-longrun".into(),
                engine: "actix-raw",
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
                cpu_work_secs: None,
            });
        }

        // ---------- 10. IO 密集：handler 内真实 await sleep(10ms) ----------
        {
            let io_done = Arc::new(AtomicU64::new(0));
            let mut addrs = Vec::new();
            for _ in 0..64 {
                let done = io_done.clone();
                addrs.push(actix::Actor::start_in_arbiter(&pool.next_arbiter(), move |_| RawIoActor { done }));
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for r in addrs {
                handles.push(tokio::spawn(async move {
                    for _ in 0..20u64 {
                        let _ = r.send(Echo { v: 1 }).await;
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }
            let drained = wait_until(
                || io_done.load(Ordering::Relaxed) >= 64 * 20,
                Duration::from_secs(120),
                Duration::from_millis(5),
            )
            .await;
            let elapsed = start.elapsed();
            let total = 64 * 20;
            let serial_expected = total as f64 * 10.0 / 1000.0;
            report.push(BenchResult {
                name: "io-async-64actors-10ms".into(),
                engine: "actix-raw",
                total_messages: total as u64,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: drained,
                note: format!(
                    "async sleep(10ms) in handler; {} tasks wall={:.2}s (serial would be {:.2}s; speedup {:.1}x)",
                    total,
                    elapsed.as_secs_f64(),
                    serial_expected,
                    serial_expected / elapsed.as_secs_f64(),
                ),
                cpu_work_secs: None,
            });
        }

        // ---------- M1. 分钟级任务 + 持续加入 + 短探测 ----------
        {
            const LONG_ACTORS: usize = 8;
            const BACKUP_ACTORS: usize = 4;
            const LONG_ITERS: u64 = 57_200_000_000;   // ~65s @release
            const MEDIUM_ITERS: u64 = 2_400_000_000;  // ~2.7s @release
            const MEDIUM_PER_BACKUP: u64 = 10;
            let nominal = (LONG_ACTORS as f64 * LONG_ITERS as f64
                + BACKUP_ACTORS as f64 * MEDIUM_PER_BACKUP as f64 * MEDIUM_ITERS as f64)
                / BURN_RATE;

            let mut long_refs = Vec::new();
            for _ in 0..LONG_ACTORS {
                long_refs.push(spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone()));
            }
            let mut backup_refs = Vec::new();
            for _ in 0..BACKUP_ACTORS {
                backup_refs.push(spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone()));
            }
            let probe = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());

            let start = Instant::now();
            let mut handles = Vec::new();
            for (i, r) in long_refs.into_iter().enumerate() {
                handles.push(tokio::spawn(async move {
                    let t0 = Instant::now();
                    let v = r.send(MinuteCpu { iters: LONG_ITERS, salt: 0xD00D + i as u64 }).await.unwrap();
                    vec![t0.elapsed().as_micros(), v as u128]
                }));
            }
            for (i, r) in backup_refs.into_iter().enumerate() {
                handles.push(tokio::spawn(async move {
                    let mut lats = Vec::new();
                    for k in 0..MEDIUM_PER_BACKUP {
                        let t0 = Instant::now();
                        let _ = r.send(MediumCpu { iters: MEDIUM_ITERS, salt: 0xBACC + i as u64 * 100 + k }).await.unwrap();
                        lats.push(t0.elapsed().as_micros());
                    }
                    lats
                }));
            }
            let probe_stop = Arc::new(AtomicU64::new(0));
            let probe_handle = {
                let pr = probe.clone();
                let st = probe_stop.clone();
                tokio::spawn(async move {
                    let mut lats = Vec::new();
                    while st.load(Ordering::Relaxed) == 0 {
                        let t0 = Instant::now();
                        let _ = pr.send(TinyTask { salt: 0xE }).await;
                        lats.push(t0.elapsed().as_micros());
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    lats
                })
            };

            let mut correct = true;
            let mut all_lats: Vec<u128> = Vec::new();
            for h in handles {
                match h.await {
                    Ok(mut v) => all_lats.append(&mut v),
                    Err(_) => correct = false,
                }
            }
            probe_stop.store(1, Ordering::Relaxed);
            let probe_lats = probe_handle.await.unwrap_or_default();
            let elapsed = start.elapsed();
            let ps = latency_stats(probe_lats.clone());
            report.push(BenchResult {
                name: "mixed-minute-cpu-plus-incoming".into(),
                engine: "actix-raw",
                total_messages: (LONG_ACTORS + BACKUP_ACTORS * MEDIUM_PER_BACKUP as usize) as u64,
                elapsed,
                latencies: ps.clone(),
                correctness: correct,
                cpu_work_secs: Some(nominal),
                note: format!(
                    "8×65s long + 40×2.7s medium concurrent; probe(tiny ask) p99={:.1}ms max={:.1}ms over {} probes",
                    ps.p99_us as f64 / 1000.0,
                    ps.max_us as f64 / 1000.0,
                    ps.count,
                ),
            });
        }

        // ---------- M2. 同一 actor 混合负载 FIFO ----------
        {
            const LONG_ITERS: u64 = 30_000_000_000;  // ~34s @release
            const MEDIUM: u64 = 6;
            const MEDIUM_ITERS: u64 = 2_400_000_000; // ~2.7s
            const TINY: u64 = 4_000;
            let nominal = (LONG_ITERS as f64 + MEDIUM as f64 * MEDIUM_ITERS as f64
                + TINY as f64 * 1_000.0) / BURN_RATE;

            let shared = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            let start = Instant::now();
            let mut jhs = Vec::new();
            {
                let sr = shared.clone();
                jhs.push(tokio::spawn(async move {
                    let _ = sr.send(MinuteCpu { iters: LONG_ITERS, salt: 1 }).await;
                }));
            }
            for k in 0..MEDIUM {
                let sr = shared.clone();
                jhs.push(tokio::spawn(async move {
                    let _ = sr.send(MediumCpu { iters: MEDIUM_ITERS, salt: 100 + k }).await;
                }));
            }
            for k in 0..TINY {
                let sr = shared.clone();
                jhs.push(tokio::spawn(async move {
                    let _ = sr.send(TinyTask { salt: k }).await;
                }));
            }
            for jh in jhs {
                let _ = jh.await;
            }
            // 哨兵 ask：长超时覆盖整条队列
            let t_probe = Instant::now();
            let _ = shared.send(TinyTask { salt: 0x5 }).await;
            let tail_latency = t_probe.elapsed();
            let elapsed = start.elapsed();

            report.push(BenchResult {
                name: "mixed-same-actor-fifo".into(),
                engine: "actix-raw",
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

        // ---------- M3. 分片长任务 vs 连续长任务 ----------
        {
            const TOTAL: u64 = 30_000_000_000;   // ~34s @release each variant
            const CHUNK: u64 = 1_500_000_000;    // ~20 片

            let solid = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            let chunked: Addr<RawChunkActor> =
                actix::Actor::start_in_arbiter(&pool.next_arbiter(), |_| RawChunkActor);

            // A) 连续（solid，同步 handler）
            let t0 = Instant::now();
            solid.send(MinuteCpu { iters: TOTAL, salt: 3 }).await.unwrap();
            let solid_probe = {
                let sr = solid.clone();
                tokio::spawn(async move {
                    let tp = Instant::now();
                    let v = sr.send(TinyTask { salt: 1 }).await.unwrap();
                    (tp.elapsed(), v)
                })
            };
            let (solid_wait, _) = solid_probe.await.unwrap();
            let solid_long = t0.elapsed();

            // B) 分片（async + yield）
            let t1 = Instant::now();
            let chunk_handle = {
                let cr = chunked.clone();
                tokio::spawn(async move {
                    let _ = cr.send(ChunkedLong { total: TOTAL, chunk: CHUNK, salt: 3 }).await;
                })
            };
            let chunk_probe = {
                let cr2 = chunked.clone();
                tokio::spawn(async move {
                    let tp = Instant::now();
                    let v = cr2.send(TinyTask { salt: 2 }).await.unwrap();
                    (tp.elapsed(), v)
                })
            };
            let _ = chunk_handle.await;
            let (chunk_wait, _) = chunk_probe.await.unwrap();
            let chunk_long = t1.elapsed();

            let nominal = TOTAL as f64 / BURN_RATE;
            let yield_overhead_pct = (chunk_long.as_secs_f64() / solid_long.as_secs_f64() - 1.0) * 100.0;
            report.push(BenchResult {
                name: "chunked-vs-solid-longrun".into(),
                engine: "actix-raw",
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

        // ---------- E1. 乒乓 RTT ----------
        {
            let pa = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            let pb = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            const BOUNCES: u64 = 10_000;
            let mut samples = Vec::with_capacity(BOUNCES as usize / 10);
            let start = Instant::now();
            let mut i = 0u64;
            while i < BOUNCES {
                let t0 = Instant::now();
                let va = pa.send(Echo { v: i }).await.unwrap();
                let vb = pb.send(Echo { v: va }).await.unwrap();
                assert_eq!(vb, i);
                if i % 10 == 0 {
                    samples.push(t0.elapsed().as_micros());
                }
                i += 1;
            }
            report.push(BenchResult {
                name: "pingpong-rtt-10k".into(),
                engine: "actix-raw",
                total_messages: BOUNCES * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: true,
                note: "2-hop ask RTT (A→B), sampled every 10th".into(),
                cpu_work_secs: None,
            });
        }

        // ---------- E2. 背靠背自传递 ----------
        {
            let ops_c = Arc::new(AtomicU64::new(0));
            let ca = spawn_in_pool(&pool, ops_c.clone(), cpu_sink.clone());
            const CHAIN_TICKS: u64 = 20_000;
            let start = Instant::now();
            let mut i = 0u64;
            while i < CHAIN_TICKS {
                let v = ca.send(Echo { v: i }).await.unwrap();
                let _ = ca.send(Echo { v: v + 1 }).await;
                i += 1;
            }
            let ok = wait_until(
                || ops_c.load(Ordering::Relaxed) >= CHAIN_TICKS * 2,
                Duration::from_secs(60),
                Duration::from_millis(2),
            )
            .await;
            report.push(BenchResult {
                name: "self-chain-ask-tell-20k".into(),
                engine: "actix-raw",
                total_messages: CHAIN_TICKS * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok,
                note: "back-to-back ask→tell to same actor (degenerate chain)".into(),
                cpu_work_secs: None,
            });
        }

        // ---------- E3. 慢消费者 + 快生产者 ----------
        {
            const PRODUCERS: usize = 8;
            const PER_PRODUCER: u64 = 5_000;
            let iters_per_msg: u64 = 170_000;
            let nominal = (PRODUCERS as f64 * PER_PRODUCER as f64 * iters_per_msg as f64) / BURN_RATE;

            let ops_cons = Arc::new(AtomicU64::new(0));
            let consumer = spawn_in_pool(&pool, ops_cons.clone(), cpu_sink.clone());

            let start = Instant::now();
            let mut jhs = Vec::new();
            for pid in 0..PRODUCERS {
                let cr = consumer.clone();
                jhs.push(tokio::spawn(async move {
                    for k in 0..PER_PRODUCER {
                        if cr.send(CpuTask { iters: iters_per_msg, salt: pid as u64 * 10_000 + k }).await.is_err() {
                            return Err(());
                        }
                    }
                    Ok(())
                }));
            }
            let mut send_ok = true;
            for jh in jhs {
                if jh.await.unwrap().is_err() {
                    send_ok = false;
                }
            }
            let send_elapsed = start.elapsed();
            let target = (PRODUCERS as u64) * PER_PRODUCER;
            let drained = wait_until(
                || ops_cons.load(Ordering::Relaxed) >= target * 2,
                Duration::from_secs(300),
                Duration::from_millis(20),
            )
            .await;
            let elapsed = start.elapsed();
            report.push(BenchResult {
                name: "slow-consumer-8prod-40k".into(),
                engine: "actix-raw",
                total_messages: target,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: drained && send_ok,
                cpu_work_secs: Some(nominal),
                note: format!(
                    "8 producers × 5000 × ~200µs msgs into ONE consumer; send-all={:.2}s; drain={}",
                    send_elapsed.as_secs_f64(),
                    if drained { "ok" } else { "TIMEOUT" },
                ),
            });
        }

        // ---------- E4. 创建/销毁风暴 ----------
        {
            const WAVES: usize = 5;
            const PER_WAVE: usize = 1_000;
            let start = Instant::now();
            let mut alive_checks = 0u64;
            for _w in 0..WAVES {
                let mut addrs = Vec::new();
                for _ in 0..PER_WAVE {
                    addrs.push(spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone()));
                }
                for (idx, r) in addrs.iter().take(10).enumerate() {
                    if r.send(Echo { v: idx as u64 }).await.is_ok() {
                        alive_checks += 1;
                    }
                }
                for r in &addrs {
                    r.do_send(StopNow);
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let elapsed = start.elapsed();
            report.push(BenchResult {
                name: "spawn-stop-storm-5k".into(),
                engine: "actix-raw",
                total_messages: (WAVES * PER_WAVE) as u64,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: alive_checks == (WAVES * 10) as u64,
                note: format!("5 waves × 1000 spawn+ask+stop; alive spot-checks {}/50", alive_checks),
                cpu_work_secs: None,
            });
        }

        println!("==================== ACTIX RAW REPORT ====================");
        println!("{}", report.dump_markdown());
        std::fs::write("/tmp/parrot_bench_actix_raw.md", report.dump_markdown()).ok();

        System::current().stop();
    });
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8)
}
