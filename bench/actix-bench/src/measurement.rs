use std::sync::Mutex;
use std::time::{Duration, Instant};

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
            "[{}] {} | msgs={} | wall={:.3}s | tput={}/s | samples={} | lat(ms) p50={} p90={} p99={} max={} | mean(ms)={} | correct={}",
            self.engine,
            self.name,
            self.total_messages,
            self.elapsed.as_secs_f64(),
            self.throughput_text(),
            self.latencies.count,
            self.latency_text(self.latencies.p50_us),
            self.latency_text(self.latencies.p90_us),
            self.latency_text(self.latencies.p99_us),
            self.latency_text(self.latencies.max_us),
            if self.latencies.count == 0 { "n/a".into() } else { format!("{:.2}", self.latencies.mean_us / 1000.0) },
            self.correctness
        );
        if !self.note.is_empty() {
            println!("        note: {}", self.note);
        }
    }
}

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

    /// Every recorded correctness failure must fail the enclosing test, even
    /// when individual scenarios collected errors instead of returning early.
    pub fn verify(&self) -> Result<(), String> {
        let results = self.results.lock().unwrap();
        if results.is_empty() {
            return Err("stress suite recorded no scenarios".into());
        }
        let failures: Vec<_> = results
            .iter()
            .filter(|result| !result.correctness)
            .map(|result| format!("{}: {}", result.name, result.note))
            .collect();
        if !failures.is_empty() {
            return Err(format!(
                "stress correctness failures: {}",
                failures.join("; ")
            ));
        }
        Ok(())
    }

    /// 输出对比小结（markdown 表格行）。
    pub fn dump_markdown(&self) -> String {
        let mut out = String::from(
            "| 场景 | 引擎 | 消息数 | 耗时(s) | 吞吐(/s) | p50(ms) | p90(ms) | p99(ms) | max(ms) | 样本数 | 正确 |\n|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        let mut rows = self.results.lock().unwrap().clone();
        rows.sort_by(|a, b| (&a.name, a.engine).cmp(&(&b.name, b.engine)));
        for r in rows {
            out.push_str(&format!(
                "| {} | {} | {} | {:.3} | {} | {} | {} | {} | {} | {} | {} |\n",
                r.name,
                r.engine,
                r.total_messages,
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
}

#[inline]
pub fn burn_cpu(iterations: u64, salt: u64) -> u64 {
    burn_cpu_range(0, iterations, salt | 1)
}
pub fn burn_cpu_range(start: u64, iterations: u64, mut state: u64) -> u64 {
    for i in start..start + iterations {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            ^ i;
    }
    state
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
) -> Result<ProbeWindowStats, String> {
    if started >= completed {
        return Err("measurement window must have positive duration".into());
    }
    if samples
        .iter()
        .any(|sample| sample.started > sample.completed)
    {
        return Err("probe completion cannot precede its start".into());
    }
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
