//! Native Actix fixed-work experiments, without a Parrot dependency.
//!
//! Run `./run.sh [report-path]` from this directory. Successful output requires
//! verified replies, completed workloads and joined arbiter threads. The native
//! and Parrot suites have different startup/registry/admission instrumentation;
//! their rows cannot simply be subtracted to infer a universal wrapper tax.
//! See README.md for the measurement boundaries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix::prelude::*;

mod contracts;
mod measurement;
mod oracle;
use contracts::*;
use measurement::*;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

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

#[derive(Message)]
#[rtype(u64)]
struct GetEchoSum;

/// 停止指令（对等 parrot StopMessage 拦截 → ctx.stop()）
#[derive(Message)]
#[rtype(result = "()")]
struct StopNow;

/// 分片长任务（对等 parrot ChunkedLongTask：async handler + 片间让出）
#[derive(Message)]
#[rtype(result = "WorkResult")]
struct ChunkedLong {
    gate: StartGate,
    total: u64,
    chunk: u64,
    salt: u64,
}

// ===========================================================================
// 同步路径 BenchActor（对等 parrot 版 BenchActor 的负载逻辑；
// ops increments at handler entry/completion, as in the Parrot workload）
// ===========================================================================

struct RawBenchActor {
    _life: ActorLifetime,
    echo_sum: u64,
    ops: Arc<AtomicU64>,
    cpu_sink: Arc<AtomicU64>,
}

impl Actor for RawBenchActor {
    type Context = actix::Context<Self>;
}

impl Handler<Echo> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, m: Echo, _ctx: &mut Self::Context) -> Self::Result {
        self.echo_sum = self.echo_sum.wrapping_add(m.v);
        self.ops.fetch_add(1, Ordering::Release);
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

impl Handler<GetEchoSum> for RawBenchActor {
    type Result = u64;
    fn handle(&mut self, _: GetEchoSum, _: &mut Self::Context) -> u64 {
        self.echo_sum
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
    _life: ActorLifetime,
    done: Arc<AtomicU64>,
}

impl Actor for RawIoActor {
    type Context = actix::Context<Self>;
}

impl Handler<Echo> for RawIoActor {
    type Result = AtomicResponse<Self, u64>;
    fn handle(&mut self, _m: Echo, _ctx: &mut Self::Context) -> Self::Result {
        let done = self.done.clone();
        AtomicResponse::new(Box::pin(
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                done.fetch_add(1, Ordering::Relaxed);
                1u64
            }
            .into_actor(self),
        ))
    }
}

// ===========================================================================
// 分片长任务 actor（对等 parrot ChunkAsyncActor：片间 tokio yield）
// ===========================================================================

struct RawChunkActor {
    _life: ActorLifetime,
}

impl Actor for RawChunkActor {
    type Context = actix::Context<Self>;
}

impl Handler<ChunkedLong> for RawChunkActor {
    type Result = AtomicResponse<Self, WorkResult>;
    fn handle(&mut self, m: ChunkedLong, _ctx: &mut Self::Context) -> Self::Result {
        m.gate.wait();
        AtomicResponse::new(Box::pin(
            async move {
                let total = m.total;
                let chunk = m.chunk.max(1);
                let started = Instant::now();
                let mut acc: u64 = m.salt | 1;
                let mut done = 0u64;
                while done < total {
                    let take = chunk.min(total - done);
                    acc = burn_cpu_range(done, take, acc);
                    done += take;
                    tokio::task::yield_now().await;
                }
                WorkResult {
                    value: acc,
                    started,
                    completed: Instant::now(),
                }
            }
            .into_actor(self),
        ))
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
// Pool size matches CPU parallelism; startup and registry costs still differ.
// ===========================================================================

struct ArbiterPool {
    handles: Vec<ArbiterHandle>,
    arbiters: Vec<Arbiter>,
    live: Arc<AtomicU64>,
    next: AtomicU64,
}

impl ArbiterPool {
    fn new(size: usize) -> Self {
        assert!(size > 0);
        let arbiters: Vec<_> = (0..size).map(|_| Arbiter::new()).collect();
        let handles = arbiters.iter().map(Arbiter::handle).collect();
        Self {
            handles,
            arbiters,
            live: Arc::new(AtomicU64::new(0)),
            next: AtomicU64::new(0),
        }
    }

    fn next_arbiter(&self) -> ArbiterHandle {
        let i = self.next.fetch_add(1, Ordering::Relaxed) as usize % self.handles.len();
        self.handles[i].clone()
    }
}

struct ActorLifetime(Arc<AtomicU64>);
impl Drop for ActorLifetime {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl ArbiterPool {
    fn lifetime(&self) -> ActorLifetime {
        self.live.fetch_add(1, Ordering::SeqCst);
        ActorLifetime(self.live.clone())
    }
    async fn drain(&self) {
        assert!(
            wait_until(
                || self.live.load(Ordering::SeqCst) == 0,
                Duration::from_secs(10),
                Duration::from_millis(1)
            )
            .await,
            "actors from the previous experiment were not destroyed"
        );
    }
    fn shutdown(&mut self) -> bool {
        for handle in &self.handles {
            handle.stop();
        }
        let mut ok = true;
        for arbiter in self.arbiters.drain(..) {
            ok &= arbiter.join().is_ok();
        }
        ok && self.live.load(Ordering::SeqCst) == 0
    }
}
impl Drop for ArbiterPool {
    fn drop(&mut self) {
        if !self.shutdown() {
            eprintln!("arbiter cleanup failed during unwind");
        }
    }
}

fn spawn_in_pool(
    pool: &ArbiterPool,
    ops: Arc<AtomicU64>,
    cpu_sink: Arc<AtomicU64>,
) -> Addr<RawBenchActor> {
    let life = pool.lifetime();
    actix::Actor::start_in_arbiter(&pool.next_arbiter(), move |_| RawBenchActor {
        ops,
        cpu_sink,
        _life: life,
        echo_sum: 0,
    })
}

// ===========================================================================
// 主流程：与 engine_stress_actix.rs 场景逐一对应（编号一致）
// ===========================================================================

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!("Usage: actix-bench [report-path]\nRuns the full fixed-work experiment (several minutes). Use a release build.\nDefault output: /tmp/parrot_bench_actix_raw.md");
        return;
    }
    assert!(
        args.len() <= 1,
        "expected at most one report path; use --help"
    );
    if cfg!(debug_assertions) {
        eprintln!("full benchmark requires --release");
        std::process::exit(2);
    }
    let output = args
        .into_iter()
        .next()
        .unwrap_or_else(|| "/tmp/parrot_bench_actix_raw.md".into());
    let run_unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    System::new().block_on(async {
        let report = Report::new();
        println!("==================== ACTIX RAW (no parrot wrapper) ====================");

        println!("Preparing independent CPU reference outputs (outside measurements)...");
        let expected = oracle::prepare(num_cpus());
        println!("Reference outputs ready: {} cases", expected.len());
        let cpu_sink = Arc::new(AtomicU64::new(0));
        // 与 parrot ActixActorSystem::new() 默认一致：arbiter 数 = CPU 数
        let mut pool = ArbiterPool::new(num_cpus());

        // ---------- 1. Echo ask 基线 ----------
        {
            let ops = Arc::new(AtomicU64::new(0));
            let a1 = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let mut samples = Vec::with_capacity(1_000);
            let start = Instant::now();
            for i in 0..1_000u64 {
                let t0 = Instant::now();
                let r = a1.send(Echo { v: i }).timeout(REQUEST_TIMEOUT).await.unwrap();
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
                        let r = ar.send(Echo { v: i + c as u64 }).timeout(REQUEST_TIMEOUT).await.unwrap();
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

            });
        }

        pool.drain().await;
        // ---------- 2. Sequential request/reply throughput----------
        {
            let ops = Arc::new(AtomicU64::new(0));
            let a4 = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let n = 100_000u64;
            let start = Instant::now();
            for i in 0..n {
                assert_eq!(a4.send(Echo { v: i }).timeout(REQUEST_TIMEOUT).await.unwrap(), i);
            }
            let ok = wait_until(
                || ops.load(Ordering::Relaxed) >= n,
                Duration::from_secs(120),
                Duration::from_millis(5),
            )
            .await;
            report.push(BenchResult {
                name: "request-echo-100k".into(),
                engine: "actix-raw",
                total_messages: n,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok,
                note: if ok { String::new() } else { "DRAIN TIMEOUT".into() },

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
            let elapsed = start.elapsed();
            let checksum = a4.send(GetEchoSum).timeout(REQUEST_TIMEOUT).await.unwrap();
            let ok = ok && ops.load(Ordering::Acquire) == n && checksum == n * (n - 1) / 2;
            report.push(BenchResult {
                name: "tell-echo-100k-dosend".into(),
                engine: "actix-raw",
                total_messages: n,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: ok,
                note: format!("do_send fired in {:.3}s (native fire-and-forget)", sent_elapsed.as_secs_f64()),

            });
        }

        pool.drain().await;
        // ---------- 3. CPU 密集 ----------
        {
            let ops = Arc::new(AtomicU64::new(0));
            let a5 = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let mut samples = Vec::new();
            let start = Instant::now();
            for i in 0..200u64 {
                let t0 = Instant::now();
                let r = a5.send(CpuTask { iters: 200_000, salt: i }).timeout(REQUEST_TIMEOUT).await.unwrap();
                let completed = Instant::now();
                assert_eq!(r, expected[&(200_000, i)]);
                samples.push(completed.duration_since(t0).as_micros());
            }
            report.push(BenchResult {
                name: "cpu-serial-200x200k".into(),
                engine: "actix-raw",
                total_messages: 200,
                elapsed: start.elapsed(),
                latencies: latency_stats(samples),
                correctness: true,
                note: "200000 fixed iterations per message".into(),

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
                let expected = expected.clone();
                handles.push(tokio::spawn(async move {
                    let mut samples = Vec::new();
                    for k in 0..25u64 {
                        let t0 = Instant::now();
                        let v = r.send(CpuTask { iters: 200_000, salt: i as u64 * 1000 + k }).timeout(REQUEST_TIMEOUT).await.unwrap();
                        samples.push(t0.elapsed().as_micros());
                        assert_eq!(v, expected[&(200_000, i as u64 * 1000 + k)]);
                    }
                    samples
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.await.unwrap());
            }
            report.push(BenchResult {
                name: "cpu-parallel-64actors-200k".into(),
                engine: "actix-raw",
                total_messages: 64 * 25,
                elapsed: start.elapsed(),
                latencies: latency_stats(all),
                correctness: true,
                note: "64 actors × 25 msgs × 200k iters (pooled arbiters)".into(),

            });
        }

        pool.drain().await;
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
                        assert_eq!(ar.send(Echo { v: i + p }).timeout(REQUEST_TIMEOUT).await.unwrap(), i + p);
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
                name: "flood-500k-requests".into(),
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
            let elapsed = start.elapsed();
            let checksum = af.send(GetEchoSum).timeout(REQUEST_TIMEOUT).await.unwrap();
            let per = n / 4;
            let drained = drained && ops_flood.load(Ordering::Acquire) == n
                && checksum == 4 * per * (per - 1) / 2 + 6 * per;
            report.push(BenchResult {
                name: "flood-500k-dosend".into(),
                engine: "actix-raw",
                total_messages: n,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: drained && ok,
                note: format!(
                    "do_send sent in {:.3}s; {}",
                    sent_elapsed.as_secs_f64(),
                    if drained { "drained" } else { "TIMEOUT" }
                ),

            });
        }

        pool.drain().await;
        // ---------- 5. Deadline after acknowledged handler entry ----------
        {
            let at = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            let start = Instant::now();
            let (gate, entered, release) = start_gate();
            let pending = at.send(GatedCpu { gate, iters: 1, salt: 1 });
            tokio::time::timeout(Duration::from_secs(5), entered).await.unwrap().unwrap();
            let r = at.send(Echo { v: 1 }).timeout(Duration::from_millis(1)).await;
            drop(release);
            pending.timeout(REQUEST_TIMEOUT).await.unwrap();
            report.push(BenchResult {
                name: "ask-timeout-short".into(), engine: "actix-raw", total_messages: 2,
                elapsed: start.elapsed(), latencies: latency_stats(vec![]),
                correctness: matches!(r, Err(MailboxError::Timeout)),
                note: "1ms deadline while the acknowledged handler is held by a gate".into(),
            });
        }

        pool.drain().await;
        // ---------- 6. 停止后发送 ----------
        {
            let ops_s = Arc::new(AtomicU64::new(0));
            let asref = spawn_in_pool(&pool, ops_s.clone(), cpu_sink.clone());
            let alive_before = asref.connected();
            asref.send(StopNow).timeout(REQUEST_TIMEOUT).await.unwrap();
            assert!(wait_until(|| !asref.connected(), Duration::from_secs(5), Duration::from_millis(1)).await);
            let alive_after = asref.connected();
            let r = asref.send(Echo { v: 1 }).timeout(REQUEST_TIMEOUT).await;
            report.push(BenchResult {
                name: "send-after-stop".into(),
                engine: "actix-raw",
                total_messages: 1,
                elapsed: Duration::from_millis(0),
                latencies: latency_stats(vec![]),
                correctness: matches!(r, Err(MailboxError::Closed)) && alive_before && !alive_after,
                note: format!(
                    "alive before={}, after={}; send => {:?}",
                    alive_before,
                    alive_after,
                    r.err().map(|e| e.to_string())
                ),

            });
        }

        pool.drain().await;
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
                    a.send(Echo { v: 1 }).timeout(REQUEST_TIMEOUT).await == Ok(1)
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
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok == 20_000,
                note: format!(
                    "spawned 20000 in {:.3}s; all accepted send",
                    spawn_elapsed.as_secs_f64()
                ),

            });
        }

        pool.drain().await;
        // ---------- 8. 长时程 ----------
        {
            let ops_l = Arc::new(AtomicU64::new(0));
            let al = spawn_in_pool(&pool, ops_l.clone(), cpu_sink.clone());
            let start = Instant::now();
            let r = al.send(LongRun { iters: 2_000_000_000 }).timeout(REQUEST_TIMEOUT).await.unwrap();
            let elapsed = start.elapsed();
            report.push(BenchResult {
                name: "longrun-2G-iters".into(),
                engine: "actix-raw",
                total_messages: 1,
                elapsed,
                latencies: latency_stats(vec![elapsed.as_micros()]),
                correctness: r == expected[&(2_000_000_000, 7)] && ops_l.load(Ordering::Relaxed) == 2,
                note: "single 2G-iteration compute".into(),

            });
        }

        pool.drain().await;
        // ---------- 9. Probes overlapping the actual CPU interval ----------
        {
            let aa = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            let ab = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            let stop = Arc::new(AtomicU64::new(0));
            let (ready, received) = tokio::sync::oneshot::channel();
            let ticker_stop = stop.clone();
            let ticker = tokio::spawn(async move {
                let mut ready = Some(ready);
                let mut samples = Vec::new();
                while ticker_stop.load(Ordering::Relaxed) == 0 {
                    let started = Instant::now();
                    assert_eq!(ab.send(Echo { v: 9 }).timeout(REQUEST_TIMEOUT).await.unwrap(), 9);
                    samples.push(ProbeSample { started, completed: Instant::now() });
                    if let Some(ready) = ready.take() { ready.send(()).unwrap(); }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                samples
            });
            received.await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            let (gate, entered, release) = start_gate();
            let work = aa.send(GatedCpu { gate, iters: 1_000_000_000, salt: 7 });
            entered.await.unwrap();
            drop(release);
            let work = work.timeout(REQUEST_TIMEOUT).await.unwrap();
            stop.store(1, Ordering::Relaxed);
            let samples = ticker.await.unwrap();
            let window = probe_window(&samples, work.started, work.completed).unwrap();
            report.push(BenchResult {
                name: "starve-echo-during-longrun".into(), engine: "actix-raw",
                total_messages: window.completed, elapsed: work.completed.duration_since(work.started),
                latencies: window.overlapping.clone(), correctness: window.baseline.count > 0 && work.value == expected[&(1_000_000_000, 7)],
                note: format!("CPU interval recorded inside handler; {} completions in interval; {} overlapping round trips, including late completions; baseline n={}. Zero progress is an outcome, not a harness failure.", window.completed, window.overlapping.count, window.baseline.count),
            });
        }

        pool.drain().await;
        // ---------- 10. IO 密集：handler 内真实 await sleep(10ms) ----------
        {
            let io_done = Arc::new(AtomicU64::new(0));
            let mut addrs = Vec::new();
            for _ in 0..64 {
                let done = io_done.clone();
                let life = pool.lifetime();
                addrs.push(actix::Actor::start_in_arbiter(&pool.next_arbiter(), move |_| RawIoActor { done, _life: life }));
            }
            let start = Instant::now();
            let mut handles = Vec::new();
            for r in addrs {
                handles.push(tokio::spawn(async move {
                    for _ in 0..20u64 {
                        assert_eq!(r.send(Echo { v: 1 }).timeout(REQUEST_TIMEOUT).await.unwrap(), 1);
                    }
                }));
            }
            for h in handles {
                h.await.unwrap();
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

            });
        }

        pool.drain().await;
        // ---------- M1. 分钟级任务 + 持续加入 + 短探测 ----------
        {
            const LONG_ACTORS: usize = 8;
            const BACKUP_ACTORS: usize = 4;
            const LONG_ITERS: u64 = 57_200_000_000;   // fixed iterations
            const MEDIUM_ITERS: u64 = 2_400_000_000;  // fixed iterations
            const MEDIUM_PER_BACKUP: u64 = 10;

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
                let expected = expected.clone();
                handles.push(tokio::spawn(async move {
                    let t0 = Instant::now();
                    let v = r.send(MinuteCpu { iters: LONG_ITERS, salt: 0xD00D + i as u64 }).timeout(REQUEST_TIMEOUT).await.unwrap();
                    { let elapsed = t0.elapsed().as_micros(); assert_eq!(v, expected[&(LONG_ITERS, 0xD00D + i as u64)]); vec![elapsed] }
                }));
            }
            for (i, r) in backup_refs.into_iter().enumerate() {
                let expected = expected.clone();
                handles.push(tokio::spawn(async move {
                    let mut lats = Vec::new();
                    for k in 0..MEDIUM_PER_BACKUP {
                        let t0 = Instant::now();
                        let value = r.send(MediumCpu { iters: MEDIUM_ITERS, salt: 0xBACC + i as u64 * 100 + k }).timeout(REQUEST_TIMEOUT).await.unwrap();
                        lats.push(t0.elapsed().as_micros());
                        assert_eq!(value, expected[&(MEDIUM_ITERS, 0xBACC + i as u64 * 100 + k)]);
                    }
                    lats
                }));
            }
            let probe_stop = Arc::new(AtomicU64::new(0));
            let probe_handle = {
                let pr = probe.clone();
                let st = probe_stop.clone();
                let expected_probe = expected[&(1_000, 0xE)];
                tokio::spawn(async move {
                    let mut lats = Vec::new();
                    while st.load(Ordering::Relaxed) == 0 {
                        let t0 = Instant::now();
                        let value = pr.send(TinyTask { salt: 0xE }).timeout(REQUEST_TIMEOUT).await.unwrap();
                        let completed = Instant::now();
                        assert_eq!(value, expected_probe);
                        lats.push(completed.duration_since(t0).as_micros());
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
            let probe_lats = probe_handle.await.unwrap();
            let elapsed = start.elapsed();
            correct &= all_lats.len() == LONG_ACTORS + BACKUP_ACTORS * MEDIUM_PER_BACKUP as usize;
            correct &= !probe_lats.is_empty();
            let ps = latency_stats(probe_lats);
            report.push(BenchResult {
                name: "mixed-minute-cpu-plus-incoming".into(),
                engine: "actix-raw",
                total_messages: (LONG_ACTORS + BACKUP_ACTORS * MEDIUM_PER_BACKUP as usize) as u64 + ps.count,
                elapsed,
                latencies: ps.clone(),
                correctness: correct,

                note: format!(
                    "48 work replies (8×57.2G iterations + 40×2.4G iterations) plus probes; probe(tiny ask) p99={:.1}ms max={:.1}ms over {} probes",
                    ps.p99_us as f64 / 1000.0,
                    ps.max_us as f64 / 1000.0,
                    ps.count,
                ),
            });
        }

        pool.drain().await;
        // ---------- M2. Same actor backlog, confirmed before release ----------
        {
            const LONG_ITERS: u64 = 30_000_000_000;
            const MEDIUM: u64 = 6;
            const MEDIUM_ITERS: u64 = 2_400_000_000;
            const TINY: u64 = 4_000;
            let ops = Arc::new(AtomicU64::new(0));
            let shared = spawn_in_pool(&pool, ops.clone(), cpu_sink.clone());
            let start = Instant::now();
            let (gate, entered, release) = start_gate();
            let long = shared.send(GatedCpu { gate, iters: LONG_ITERS, salt: 1 });
            entered.await.unwrap();
            let mut tasks = tokio::task::JoinSet::new();
            for k in 0..MEDIUM {
                let r = shared.clone();
                let expected = expected.clone();
                tasks.spawn(async move { assert_eq!(r.send(MediumCpu { iters: MEDIUM_ITERS, salt: 100 + k }).timeout(REQUEST_TIMEOUT).await.unwrap(), expected[&(MEDIUM_ITERS, 100 + k)]); });
            }
            for k in 0..TINY {
                let r = shared.clone();
                let expected_tiny = expected[&(1_000, k)];
                tasks.spawn(async move { assert_eq!(r.send(TinyTask { salt: k }).timeout(REQUEST_TIMEOUT).await.unwrap(), expected_tiny); });
            }
            // This request is enqueued while the first handler is gated. It is
            // not asserted to follow every concurrent producer in FIFO order.
            let probe_start = Instant::now();
            let probe = shared.send(TinyTask { salt: 5 });
            drop(release);
            let probe_value = probe.timeout(REQUEST_TIMEOUT).await.unwrap();
            let probe_latency = probe_start.elapsed();
            let work = long.timeout(REQUEST_TIMEOUT).await.unwrap();
            assert_eq!(work.value, expected[&(LONG_ITERS, 1)]);
            while let Some(result) = tasks.join_next().await { result.unwrap(); }
            assert_eq!(probe_value, expected[&(1_000, 5)]);
            report.push(BenchResult {
                name: "mixed-same-actor-backlog-probe".into(), engine: "actix-raw",
                total_messages: 2 + MEDIUM + TINY, elapsed: start.elapsed(),
                latencies: latency_stats(vec![probe_latency.as_micros()]),
                correctness: ops.load(Ordering::Relaxed) == 2 + MEDIUM * 2 + TINY + 1,
                note: format!("1×30G + 6×2.4G + 4000 tiny requests; probe submitted before long work released, not a FIFO-tail assertion; actual long CPU interval={:.3}s", work.completed.duration_since(work.started).as_secs_f64()),
            });
        }

        pool.drain().await;
        // ---------- M3. Same computation, serialized actor, queued probe ----------
        {
            const TOTAL: u64 = 30_000_000_000;
            const CHUNK: u64 = 1_500_000_000;
            let measured = compare_chunked(&pool, TOTAL, CHUNK).await;
            report.push(BenchResult {
                name: "chunked-vs-solid-longrun".into(), engine: "actix-raw", total_messages: 4,
                elapsed: measured.elapsed,
                latencies: latency_stats(vec![measured.solid_probe.as_micros(), measured.chunk_probe.as_micros()]),
                correctness: measured.solid_value == measured.chunk_value && measured.solid_value == expected[&(TOTAL, 3)],
                note: format!("same 30G recurrence; atomic async handler serializes messages; work intervals solid={:.3}s chunked={:.3}s, probes queued before computation; this does not measure reentrant message handling", measured.solid.as_secs_f64(), measured.chunked.as_secs_f64()),
            });
        }

        pool.drain().await;
        // ---------- E1. Caller-mediated sequential request pair ----------
        {
            let pa = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            let pb = spawn_in_pool(&pool, Arc::new(AtomicU64::new(0)), cpu_sink.clone());
            const BOUNCES: u64 = 10_000;
            let mut samples = Vec::with_capacity(BOUNCES as usize / 10);
            let start = Instant::now();
            let mut i = 0u64;
            while i < BOUNCES {
                let t0 = Instant::now();
                let va = pa.send(Echo { v: i }).timeout(REQUEST_TIMEOUT).await.unwrap();
                let vb = pb.send(Echo { v: va }).timeout(REQUEST_TIMEOUT).await.unwrap();
                assert_eq!(vb, i);
                if i.is_multiple_of(10) {
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
                note: "caller→A then caller→B sequential request pair, sampled every 10th".into(),

            });
        }

        pool.drain().await;
        // ---------- E2. Back-to-back requests to one actor ----------
        {
            let ops_c = Arc::new(AtomicU64::new(0));
            let ca = spawn_in_pool(&pool, ops_c.clone(), cpu_sink.clone());
            const CHAIN_TICKS: u64 = 20_000;
            let start = Instant::now();
            let mut i = 0u64;
            while i < CHAIN_TICKS {
                let v = ca.send(Echo { v: i }).timeout(REQUEST_TIMEOUT).await.unwrap();
                assert_eq!(v, i);
                assert_eq!(ca.send(Echo { v: v + 1 }).timeout(REQUEST_TIMEOUT).await.unwrap(), i + 1);
                i += 1;
            }
            let ok = wait_until(
                || ops_c.load(Ordering::Relaxed) >= CHAIN_TICKS * 2,
                Duration::from_secs(60),
                Duration::from_millis(2),
            )
            .await;
            report.push(BenchResult {
                name: "back-to-back-requests-20k".into(),
                engine: "actix-raw",
                total_messages: CHAIN_TICKS * 2,
                elapsed: start.elapsed(),
                latencies: latency_stats(vec![]),
                correctness: ok,
                note: "caller sends two sequential request/reply messages to the same actor".into(),

            });
        }

        pool.drain().await;
        // ---------- E3. 慢消费者 + 快生产者 ----------
        {
            const PRODUCERS: usize = 8;
            const PER_PRODUCER: u64 = 5_000;
            let iters_per_msg: u64 = 170_000;

            let ops_cons = Arc::new(AtomicU64::new(0));
            let consumer = spawn_in_pool(&pool, ops_cons.clone(), cpu_sink.clone());

            let start = Instant::now();
            let mut jhs = Vec::new();
            for pid in 0..PRODUCERS {
                let cr = consumer.clone();
                let expected = expected.clone();
                jhs.push(tokio::spawn(async move {
                    for k in 0..PER_PRODUCER {
                        if cr.send(CpuTask { iters: iters_per_msg, salt: pid as u64 * 10_000 + k }).timeout(REQUEST_TIMEOUT).await != Ok(expected[&(iters_per_msg, pid as u64 * 10_000 + k)]) {
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
                name: "slow-consumer-8prod-40k-requests".into(),
                engine: "actix-raw",
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

        pool.drain().await;
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
                    if r.send(Echo { v: idx as u64 }).timeout(REQUEST_TIMEOUT).await == Ok(idx as u64) {
                        alive_checks += 1;
                    }
                }
                for r in &addrs {
                    r.send(StopNow).timeout(REQUEST_TIMEOUT).await.unwrap();
                }
                assert!(wait_until(|| addrs.iter().all(|r| !r.connected()), Duration::from_secs(5), Duration::from_millis(1)).await);
                drop(addrs);
                pool.drain().await;
            }
            let elapsed = start.elapsed();
            report.push(BenchResult {
                name: "spawn-stop-storm-5k".into(),
                engine: "actix-raw",
                total_messages: (WAVES * (PER_WAVE + 10)) as u64,
                elapsed,
                latencies: latency_stats(vec![]),
                correctness: alive_checks == (WAVES * 10) as u64,
                note: format!("5000 spawn/stop cycles; 5050 messages = 5000 stop requests + 50 echo spot-checks; correct spot-checks {}/50", alive_checks),

            });
        }

        pool.drain().await;
        assert!(pool.shutdown(), "all arbiters must join and all actors must drop");
        assert_eq!(report.results.lock().unwrap().len(), 22, "all planned scenarios must run");
        report.verify().expect("benchmark correctness");
        println!("==================== ACTIX RAW REPORT ====================");
        println!("{}", report.dump_markdown());
        let metadata = format!("# Native Actix fixed-work measurement\n\nRun start (Unix seconds): {run_unix_seconds}. Schema: 231. OS: {}. Architecture: {}. Workers: {}. Build: {}.\n\nAll requests and joins checked; all actor owners retired. CPU references computed before measurement; validation lookups are included in scenario wall time. CPU utilization is not measured.\n\n", std::env::consts::OS, std::env::consts::ARCH, num_cpus(), if cfg!(debug_assertions) { "debug (not a performance result)" } else { "release" });
        std::fs::write(output, metadata + &report.dump_markdown()).expect("write benchmark report");

        System::current().stop();
    });
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
}
