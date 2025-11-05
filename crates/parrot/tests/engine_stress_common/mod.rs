// 共享 bench 类型池：thread/actix 两侧按需取用（单侧未用属预期）
#![allow(dead_code)]

//! 引擎压测公共设施：双引擎共用的 actor 定义、计时器、结果收集。
//!
//! 设计原则：
//! - 两个引擎跑**完全相同的业务逻辑**，只有引擎绑定层不同
//! - 计时用 `Instant`，统计 p50/p90/p99/max/throughput
//! - 所有 actor 状态外置到 `Arc<AtomicU64>`，避免引擎差异影响观察

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub mod contracts;
#[path = "../../../../bench/actix-bench/src/oracle.rs"]
pub mod oracle;

// ---------------------------------------------------------------------------
// 消息类型（引擎无关）
// ---------------------------------------------------------------------------

/// CPU 密集任务：要求 actor 做 `iterations` 次平方迭代运算。
pub struct CpuTask {
    pub iterations: u64,
    /// 用于混淆优化器，防止计算被折叠
    pub salt: u64,
}

/// IO 密集任务：要求 actor sleep `duration`（模拟 IO 等待）。
pub struct IoTask {
    pub duration_ms: u64,
}

/// 长时程任务：actor 内部累计工作量（分片 CPU）。
pub struct LongRunningTask {
    pub total_iterations: u64,
}

/// 简单回声。
pub struct Echo {
    pub value: u64,
}

/// 分钟级 CPU 密集任务（约 N 秒的纯计算）。
pub struct MinuteCpuTask {
    pub iterations: u64,
    pub salt: u64,
}

/// 中等时长 CPU 任务（秒级），用于"新长任务持续加入"。
pub struct MediumCpuTask {
    pub iterations: u64,
    pub salt: u64,
}

/// 混合负载里的短小任务（微秒级）。
pub struct TinyTask {
    pub salt: u64,
}

/// 分片长任务：分 N 片执行，每片之间向系统让出（协作式）。
pub struct ChunkedLongTask {
    pub total_iterations: u64,
    pub chunk_iterations: u64,
    pub salt: u64,
}

/// 批量 echo（一次消息做 N 个小工作，降低测量噪声）。
pub struct BatchEcho {
    pub value: u64,
    pub batch: u64,
}

/// 读取计数。
pub struct GetCount;

// ---------------------------------------------------------------------------
// 统计
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LatencyStats {
    pub count: u64,
    pub p50_us: u128,
    pub p90_us: u128,
    pub p99_us: u128,
    pub max_us: u128,
    pub mean_us: f64,
}

/// 对一组延迟样本（微秒）做分位数统计。
pub fn latency_stats(mut samples: Vec<u128>) -> LatencyStats {
    if samples.is_empty() {
        return LatencyStats {
            count: 0,
            p50_us: 0,
            p90_us: 0,
            p99_us: 0,
            max_us: 0,
            mean_us: 0.0,
        };
    }
    samples.sort_unstable();
    let n = samples.len();
    let pick = |q: f64| -> u128 {
        let idx = (((q / 100.0) * n as f64).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
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

/// 一轮场景的测量结果。
#[derive(Debug, Clone)]
pub struct BenchResult {
    pub name: String,
    pub engine: &'static str,
    pub unit: &'static str,
    pub total_messages: u64,
    pub elapsed: Duration,
    pub latencies: LatencyStats,
    pub correctness: bool,
    pub note: String,
}

impl BenchResult {
    pub fn msg_per_sec(&self) -> Option<f64> {
        (!self.elapsed.is_zero()).then(|| self.total_messages as f64 / self.elapsed.as_secs_f64())
    }
    fn throughput_text(&self) -> String {
        self.msg_per_sec()
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "n/a".into())
    }
    fn latency_text(&self, micros: u128) -> String {
        if self.latencies.count == 0 {
            "n/a".into()
        } else {
            format!("{:.2}", micros as f64 / 1000.0)
        }
    }
    pub fn print(&self) {
        println!(
            "[{}] {} | count={} ({}) | wall={:.3}s | tput={}/s | samples={} | lat(ms) p50={} p90={} p99={} max={} | correct={}",
            self.engine,
            self.name,
            self.total_messages,
            self.unit,
            self.elapsed.as_secs_f64(),
            self.throughput_text(),
            self.latencies.count,
            self.latency_text(self.latencies.p50_us),
            self.latency_text(self.latencies.p90_us),
            self.latency_text(self.latencies.p99_us),
            self.latency_text(self.latencies.max_us),
            self.correctness
        );
        if !self.note.is_empty() {
            println!("        note: {}", self.note);
        }
    }
}

/// 简单进度防优化累加器。
pub struct Sink(pub AtomicU64);

impl Sink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(0)))
    }
    pub fn add(&self, v: u64) {
        self.0.fetch_add(v, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Explicit bound for the full fixed-work experiment, including saturated queues.
pub const STRESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

/// 时长校准：按目标秒数求 burn_cpu 迭代数（对 llvm-cov 插桩/慢机鲁棒）。
///
/// 标定策略：三轮取最小单次耗时（首轮缓存冷偏慢），乘 0.8 安全系数
/// （gate 宁可略短也不能超过 ask 默认 5s 超时）。
pub fn calibrated_iters(secs: f64) -> u64 {
    let probe = 8_000_000u64;
    let mut best = f64::MAX;
    let mut sink = 0u64;
    for _ in 0..3 {
        let t0 = Instant::now();
        sink = sink.wrapping_add(burn_cpu(probe, 1));
        best = best.min(t0.elapsed().as_secs_f64());
    }
    std::hint::black_box(&sink);
    (((secs * 0.8) / (best / probe as f64)) as u64).max(1_000)
}

/// CPU 工作负载：不可被优化器折叠的迭代混合运算。
#[inline]
pub fn burn_cpu(iterations: u64, salt: u64) -> u64 {
    burn_cpu_range(0, iterations, salt | 1)
}

/// Continue the same recurrence using global iteration indices. Splitting a
/// workload into cooperative chunks must not change its computation or result.
pub fn burn_cpu_range(start: u64, iterations: u64, mut state: u64) -> u64 {
    for i in start..start + iterations {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            ^ i;
    }
    state
}

// ---------------------------------------------------------------------------
// 结果收集器
// ---------------------------------------------------------------------------

use std::sync::Mutex;

pub struct Report {
    pub results: Mutex<Vec<BenchResult>>,
    run_id: uuid::Uuid,
    started_unix_ms: u128,
    runtime: String,
}

impl Report {
    pub fn new() -> Self {
        Self {
            results: Mutex::new(Vec::new()),
            run_id: uuid::Uuid::new_v4(),
            started_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            runtime: "unspecified (measurement fixture)".into(),
        }
    }

    pub fn with_runtime(mut self, runtime: String) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn push(&self, r: BenchResult) {
        r.print();
        self.results.lock().unwrap().push(r);
    }

    /// Every recorded correctness failure must fail the enclosing test, even
    /// when individual scenarios collected errors instead of returning early.
    pub fn verify(&self) -> anyhow::Result<()> {
        let results = self.results.lock().unwrap();
        anyhow::ensure!(!results.is_empty(), "stress suite recorded no scenarios");
        let failures: Vec<_> = results
            .iter()
            .filter(|result| !result.correctness)
            .map(|result| format!("{}: {}", result.name, result.note))
            .collect();
        anyhow::ensure!(
            failures.is_empty(),
            "stress correctness failures: {}",
            failures.join("; ")
        );
        Ok(())
    }

    /// 输出对比小结（markdown 表格行）。
    pub fn dump_markdown(&self) -> String {
        let mut out = format!(
            "# Parrot stress report\n\nschema: 2\n\nrun_id: {}\n\nstarted_unix_ms: {}\n\nos/arch: {}/{}\n\navailable_parallelism: {:?}\n\ndebug_assertions: {}\n\nruntime: {}\n\nrequest_timeout_seconds: {}\n\nSingle host run; no cross-runtime performance certification.\n\n",
            self.run_id,
            self.started_unix_ms,
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().ok(),
            cfg!(debug_assertions),
            self.runtime,
            STRESS_REQUEST_TIMEOUT.as_secs(),
        );
        out.push_str(
            "| 场景 | 引擎 | 数量 | 单位 | 耗时(s) | 吞吐(次/s) | p50(ms) | p90(ms) | p99(ms) | max(ms) | 样本数 | 正确 |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        let mut rows = self.results.lock().unwrap().clone();
        rows.sort_by(|a, b| (&a.name, a.engine).cmp(&(&b.name, b.engine)));
        for r in rows {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {:.3} | {} | {} | {} | {} | {} | {} | {} |\n",
                r.name,
                r.engine,
                r.total_messages,
                r.unit,
                r.elapsed.as_secs_f64(),
                r.throughput_text(),
                r.latency_text(r.latencies.p50_us),
                r.latency_text(r.latencies.p90_us),
                r.latency_text(r.latencies.p99_us),
                r.latency_text(r.latencies.max_us),
                r.latencies.count,
                r.correctness,
            ));
        }
        for r in self.results.lock().unwrap().iter() {
            if !r.note.is_empty() {
                out.push_str(&format!("\n- {}: {}\n", r.name, r.note));
            }
        }
        out
    }

    pub fn save(&self, path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
        self.verify()?;
        std::fs::write(path, self.dump_markdown())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 等待工具：轮询直到条件满足或超时
// ---------------------------------------------------------------------------

pub async fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration, poll: Duration) -> bool {
    let start = Instant::now();
    while !cond() {
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
    true
}

/// Deterministic timeout probe: the actor explicitly acknowledges entry and
/// remains blocked until the request deadline has been observed by the caller.
pub struct PauseForTimeout {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    released: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl PauseForTimeout {
    pub fn execute(&self) -> parrot_api::types::ActorResult<()> {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        match self
            .released
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
        {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(()),
            Err(_) => Err(parrot_api::errors::ActorError::TimeoutDetail(
                "test failed to release timeout probe".into(),
            )),
        }
    }
}
pub async fn exercise_request_timeout(
    actor: parrot_api::types::BoxedActorRef,
) -> anyhow::Result<bool> {
    let (entered, receipt) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let blocking = actor.clone_boxed();
    let task = tokio::spawn(async move {
        blocking
            .send_with_timeout(
                Box::new(PauseForTimeout {
                    entered: Mutex::new(Some(entered)),
                    released: Mutex::new(released),
                }),
                Some(Duration::from_secs(10)),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), receipt).await??;
    let reply = actor
        .send_with_timeout(Box::new(Echo { value: 1 }), Some(Duration::from_millis(1)))
        .await;
    // Release before asserting or propagating any error; joining checks that
    // the blocked request really completed and its resources were retired.
    drop(release);
    task.await??;
    Ok(matches!(
        reply,
        Err(parrot_api::errors::ActorError::Timeout
            | parrot_api::errors::ActorError::TimeoutDetail(_))
    ))
}

/// Captured on the actor execution thread, immediately around the actual CPU work.
pub struct TimedLongTask {
    pub total_iterations: u64,
}
pub struct TimedWorkResult {
    pub value: u64,
    pub started: Instant,
    pub completed: Instant,
}
pub fn timed_cpu(iterations: u64) -> TimedWorkResult {
    let started = Instant::now();
    let value = burn_cpu(iterations, 7);
    let completed = Instant::now();
    TimedWorkResult {
        value,
        started,
        completed,
    }
}
#[derive(Clone, Copy)]
pub struct ProbeSample {
    pub started: Instant,
    pub completed: Instant,
}
pub struct ProbeWindowStats {
    pub baseline: LatencyStats,
    /// Whole round-trip latency for requests with positive overlap, including
    /// requests stalled until after the heavy computation completed.
    pub overlapping: LatencyStats,
    /// Completion events in the half-open [start, end) computation interval.
    pub completed: u64,
}
pub fn probe_window(
    samples: &[ProbeSample],
    started: Instant,
    completed: Instant,
) -> anyhow::Result<ProbeWindowStats> {
    anyhow::ensure!(
        started < completed,
        "measurement window must have positive duration"
    );
    anyhow::ensure!(
        samples
            .iter()
            .all(|sample| sample.started <= sample.completed),
        "probe completion cannot precede its start"
    );
    let latency =
        |sample: &ProbeSample| sample.completed.duration_since(sample.started).as_micros();
    Ok(ProbeWindowStats {
        baseline: latency_stats(
            samples
                .iter()
                .filter(|sample| sample.completed <= started)
                .map(latency)
                .collect(),
        ),
        overlapping: latency_stats(
            samples
                .iter()
                .filter(|sample| sample.started < completed && sample.completed > started)
                .map(latency)
                .collect(),
        ),
        completed: samples
            .iter()
            .filter(|sample| sample.completed >= started && sample.completed < completed)
            .count() as u64,
    })
}
