use super::{MeasuredMetrics, NodeSpec, RemoteActorSystem};
use parrot_remote::metrics::field;
use std::{sync::Arc, time::Instant};

pub(crate) struct Sample {
    pub at: Instant,
    pub metrics: MeasuredMetrics,
}
pub(crate) async fn collect(
    client: &Arc<RemoteActorSystem>,
    nodes: &[NodeSpec],
) -> Vec<(String, Result<Sample, String>)> {
    let mut out = Vec::new();
    for node in nodes {
        let result = client.measured_metrics(&node.name).await;
        let at = Instant::now();
        out.push((
            node.name.clone(),
            result
                .map(|metrics| Sample { at, metrics })
                .map_err(|e| e.to_string()),
        ));
    }
    out
}
fn rate(before: Option<&Sample>, after: &Sample, field: u64) -> Option<f64> {
    let before = before?;
    if before.metrics.epoch.is_empty() || before.metrics.epoch != after.metrics.epoch {
        return None;
    }
    let elapsed = after.at.checked_duration_since(before.at)?.as_secs_f64();
    if elapsed == 0.0 {
        return None;
    }
    let delta = after
        .metrics
        .value(field)?
        .checked_sub(before.metrics.value(field)?)?;
    Some(delta as f64 / elapsed)
}
pub(crate) fn line(name: &str, before: Option<&Sample>, after: &Sample) -> String {
    let text = |field| {
        rate(before, after, field)
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "n/a".into())
    };
    format!(
        "{}: asks/s={} tells/s={} rx_bytes/s={} tx_bytes/s={} heartbeats/s={}",
        super::terminal_text(name),
        text(field::ASKS_RX),
        text(field::TELLS_RX),
        text(field::BYTES_RX),
        text(field::BYTES_TX),
        text(field::HEARTBEATS_RX)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn sample(at: Instant, count: u64) -> Sample {
        Sample {
            at,
            metrics: MeasuredMetrics {
                snapshot: parrot_remote::admin_v2::MetricsSnapshot {
                    asks_rx: count,
                    ..Default::default()
                },
                available: field::ASKS_RX,
                epoch: "owner-a".into(),
                ..Default::default()
            },
        }
    }
    #[test]
    fn rates_require_available_counters_same_epoch_and_actual_receipt_intervals() {
        let at = Instant::now();
        let before = sample(at, 10);
        let mut after = sample(at + Duration::from_secs(5), 20);
        assert_eq!(rate(Some(&before), &after, field::ASKS_RX), Some(2.0));
        assert!(rate(None, &after, field::ASKS_RX).is_none());
        after.metrics.snapshot.asks_rx = 9;
        assert!(rate(Some(&before), &after, field::ASKS_RX).is_none());
        after.metrics.snapshot.asks_rx = 20;
        after.metrics.epoch = "owner-b".into();
        assert!(rate(Some(&before), &after, field::ASKS_RX).is_none());
        after.metrics.epoch = "owner-a".into();
        after.metrics.available = 0;
        assert!(rate(Some(&before), &after, field::ASKS_RX).is_none());
        after.metrics.available = field::ASKS_RX;
        after.at = at;
        assert!(rate(Some(&before), &after, field::ASKS_RX).is_none());
        after.at = at - Duration::from_secs(1);
        assert!(rate(Some(&before), &after, field::ASKS_RX).is_none());
    }
    #[test]
    fn unknown_baselines_do_not_turn_into_zero_rates_and_u64_delta_stays_exact() {
        let at = Instant::now();
        let mut before = sample(at, u64::MAX - 10);
        let after = sample(at + Duration::from_secs(5), u64::MAX);
        assert_eq!(rate(Some(&before), &after, field::ASKS_RX), Some(2.0));
        before.metrics.available = 0;
        assert!(rate(Some(&before), &after, field::ASKS_RX).is_none());
        before.metrics.available = field::ASKS_RX;
        before.metrics.epoch.clear();
        assert!(rate(Some(&before), &after, field::ASKS_RX).is_none());
    }
}
