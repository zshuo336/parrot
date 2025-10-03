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
    pub total_messages: u64,
    pub elapsed: Duration,
    pub latencies: LatencyStats,
    pub correctness: bool,
    pub note: String,
    /// 场景的标称 CPU 工作量（秒）。用于计算 worker 利用率 =
    /// 标称总量 / (墙钟 × 可用核数)。None = 不适用。
    pub cpu_work_secs: Option<f64>,
}

impl BenchResult {
    pub fn throughput(&self) -> f64 {
        self.elapsed.as_secs_f64().max(f64::EPSILON)
    }

    pub fn msg_per_sec(&self) -> f64 {
        self.total_messages as f64 / self.throughput()
    }

    pub fn print(&self) {
        let util = self
            .cpu_work_secs
            .map(|_w| format!(" | util={:.0}%", self.utilization() * 100.0))
            .unwrap_or_default();
        println!(
            "[{}] {} | msgs={} | wall={:.3}s | tput={:.0}/s | lat p50={:.1}ms p90={:.1}ms p99={:.1}ms max={:.1}ms{} | correct={}",
            self.engine,
            self.name,
            self.total_messages,
            self.elapsed.as_secs_f64(),
            self.msg_per_sec(),
            self.latencies.p50_us as f64 / 1000.0,
            self.latencies.p90_us as f64 / 1000.0,
            self.latencies.p99_us as f64 / 1000.0,
            self.latencies.max_us as f64 / 1000.0,
            util,
            self.correctness,
        );
        if !self.note.is_empty() {
            println!("        note: {}", self.note);
        }
    }

    /// 有效 CPU 利用率 = 标称工作量 / (墙钟 × 核数)。
    pub fn utilization(&self) -> f64 {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8) as f64;
        self.cpu_work_secs
            .map(|w| w / (self.elapsed.as_secs_f64().max(f64::EPSILON) * cores))
            .unwrap_or(0.0)
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

/// 实测 burn_cpu 速率（iters/sec）。
///
/// 标定环境：**release test profile**（`cargo test --release`），
/// Apple Silicon。2G iters 实测 ~2.27s → ~0.88 G/s。
/// （debug profile 约 0.194 G/s，若用 debug 跑请自行换算：场景时长 ×4.5。）
pub const BURN_RATE: f64 = 880_000_000.0;

/// CPU 工作负载：不可被优化器折叠的迭代混合运算。
#[inline]
pub fn burn_cpu(iterations: u64, salt: u64) -> u64 {
    let mut x: u64 = salt | 1;
    for i in 0..iterations {
        // 乘加 + 异或混合，无除法（除法太慢导致单次时间不可控）
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            ^ i;
        if x == 42 {
            // 概率近乎为零，仅防分支消除
            return x;
        }
    }
    x
}

// ---------------------------------------------------------------------------
// 结果收集器
// ---------------------------------------------------------------------------

use std::sync::Mutex;

pub struct Report {
    pub results: Mutex<Vec<BenchResult>>,
}

impl Report {
    pub fn new() -> Self {
        Self {
            results: Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, r: BenchResult) {
        r.print();
        self.results.lock().unwrap().push(r);
    }

    /// 输出对比小结（markdown 表格行）。
    pub fn dump_markdown(&self) -> String {
        let mut out = String::from(
            "| 场景 | 引擎 | 消息数 | 耗时(s) | 吞吐(/s) | p50(ms) | p90(ms) | p99(ms) | max(ms) | util | 正确 |\n|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        let mut rows = self.results.lock().unwrap().clone();
        rows.sort_by(|a, b| (&a.name, a.engine).cmp(&(&b.name, b.engine)));
        for r in rows {
            let util = r
                .cpu_work_secs
                .map(|_w| format!("{:.0}%", r.utilization() * 100.0))
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
