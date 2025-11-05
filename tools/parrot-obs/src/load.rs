//! Bounded latency histogram and owned load tasks. Quantiles are bucket upper bounds.
use super::{probe_once, NodeSpec};
use parrot_remote::RemoteActorSystem;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(crate) struct Latencies {
    buckets: [u64; 65],
    count: u64,
    max: u64,
}
impl Default for Latencies {
    fn default() -> Self {
        Self {
            buckets: [0; 65],
            count: 0,
            max: 0,
        }
    }
}
impl Latencies {
    pub fn record(&mut self, micros: u64) {
        self.buckets[(64 - micros.leading_zeros()) as usize] += 1;
        self.count += 1;
        self.max = self.max.max(micros);
    }
    pub fn merge(&mut self, other: &Self) {
        for (a, b) in self.buckets.iter_mut().zip(&other.buckets) {
            *a += b;
        }
        self.count += other.count;
        self.max = self.max.max(other.max);
    }
    pub fn percentile(&self, percent: u64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = (u128::from(self.count) * u128::from(percent)).div_ceil(100);
        let mut seen = 0u128;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += u128::from(*n);
            if seen >= rank {
                return Some(if i == 64 { u64::MAX } else { (1u64 << i) - 1 }.min(self.max));
            }
        }
        None
    }
    pub fn text(&self, percent: u64) -> String {
        self.percentile(percent)
            .map(|us| format!("≤{:.3}ms", us as f64 / 1000.0))
            .unwrap_or_else(|| "n/a".into())
    }
}
#[derive(Default)]
pub(crate) struct LoadReport {
    pub ok: u64,
    pub errors: u64,
    pub latencies: Latencies,
    pub elapsed: Duration,
}
impl LoadReport {
    pub fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.ok > 0, "load produced no valid replies");
        anyhow::ensure!(
            self.errors == 0,
            "load received {} failed or invalid replies",
            self.errors
        );
        Ok(())
    }
}

pub(crate) async fn run_load(
    client: Arc<RemoteActorSystem>,
    nodes: Vec<NodeSpec>,
    duration: Duration,
    concurrency: usize,
) -> anyhow::Result<LoadReport> {
    anyhow::ensure!(
        !duration.is_zero() && concurrency > 0 && concurrency <= 128 && !nodes.is_empty(),
        "load requires a positive duration, nodes and 1..=128 workers"
    );
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + duration;
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..concurrency {
        let client = client.clone();
        let nodes = nodes.clone();
        workers.spawn(async move {
            let mut report = LoadReport::default();
            let mut sequence = 0;
            'work: loop {
                for node in &nodes {
                    if tokio::time::Instant::now() >= deadline {
                        break 'work;
                    }
                    let at = Instant::now();
                    match probe_once(&client, node, sequence).await {
                        Ok(()) => {
                            report.ok += 1;
                            report
                                .latencies
                                .record(at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
                        }
                        Err(_) => {
                            report.errors += 1;
                        }
                    }
                    sequence += 1;
                    // Failed immediate requests must not monopolize the executor,
                    // including on current-thread test/application runtimes.
                    tokio::task::yield_now().await;
                }
            }
            report
        });
    }
    let mut report = LoadReport::default();
    let mut task_error = None;
    while let Some(result) = workers.join_next().await {
        match result {
            Ok(r) => {
                report.ok += r.ok;
                report.errors += r.errors;
                report.latencies.merge(&r.latencies);
            }
            Err(e) => task_error = Some(e),
        }
    }
    report.elapsed = started.elapsed();
    if let Some(error) = task_error {
        return Err(error.into());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn histogram_retains_tail_without_storing_per_request_samples() {
        let mut a = Latencies::default();
        for _ in 0..98 {
            a.record(1);
        }
        let mut b = Latencies::default();
        b.record(500);
        b.record(1000);
        a.merge(&b);
        assert_eq!(a.count, 100);
        assert_eq!(a.percentile(50), Some(1));
        assert_eq!(a.percentile(99), Some(511));
        assert_eq!(a.percentile(100), Some(1000));
        assert_eq!(a.buckets.len(), 65);
        let mut max = Latencies::default();
        max.record(u64::MAX);
        assert_eq!(max.percentile(99), Some(u64::MAX));
        assert_eq!(Latencies::default().percentile(99), None);
    }
    #[test]
    fn empty_or_partially_failed_load_is_not_a_success() {
        let mut r = LoadReport::default();
        assert!(r.verify().is_err());
        r.ok = 5;
        assert!(r.verify().is_ok());
        r.errors = 1;
        assert!(r.verify().is_err());
    }
}
