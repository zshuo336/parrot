// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use serde_json::{json, Value};

fn write_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("write_bytes: "))
        .expect("Linux process storage accounting")
        .parse()
        .unwrap()
}

async fn measure(operation: impl Future<Output = ()>) -> Value {
    let before = write_bytes();
    let start = Instant::now();
    operation.await;
    let elapsed = start.elapsed();
    json!({"ns": elapsed.as_nanos() as u64, "write_bytes": write_bytes() - before})
}

// Inspect the published ticket outside the timing window to distinguish a real
// receiver rollover from a reconnect that reused the same ticket.
fn ticket(directory: &Path, enabled: bool, active: bool) -> Option<Value> {
    let path = directory.join("sender.state");
    if !enabled {
        assert_eq!(
            std::fs::metadata(path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        return None;
    }
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..8], b"POSND01\0");
    let snapshot: Value = serde_json::from_slice(&bytes[40..]).unwrap();
    let tickets = snapshot["tickets"].as_array().unwrap();
    assert_eq!(tickets.len(), usize::from(active));
    tickets.first().cloned()
}

async fn trial(enabled: bool, round: usize, pki: &pki::Pki) -> Value {
    let sender = tempfile::tempdir().unwrap();
    let receiver = tempfile::tempdir().unwrap();
    let mut cfg = sender_config(sender.path()).with_mtls(pki.node("recovery-source"), []);
    cfg.persist_sender_sessions = enabled;
    let source = start(cfg, state(None).0).await;
    let cfg = config(receiver.path()).with_mtls(pki.node("recovery-target"), []);
    let (first_state, mut first_rx) = state(None);
    let first = start(cfg.clone(), first_state).await;
    connect(&source, &first).await;
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();

    let open = measure(send(&reference, 1)).await;
    seen(&mut first_rx, 1).await;
    let previous = ticket(sender.path(), enabled, true);
    let steady = measure(send(&reference, 2)).await;
    seen(&mut first_rx, 2).await;
    assert_eq!(ticket(sender.path(), enabled, true), previous);

    first.shutdown().await.unwrap();
    assert!(first_rx.try_recv().is_err());
    let (next_state, mut next_rx) = state(None);
    let next = start(cfg, next_state).await;
    connect(&source, &next).await;
    let rotate = measure(send(&reference, 3)).await;
    seen(&mut next_rx, 3).await;
    let new = ticket(sender.path(), enabled, true);
    if let (Some(old), Some(new)) = (previous, new) {
        assert_ne!(old["receiver"], new["receiver"]);
        assert_ne!(old["ticket"], new["ticket"]);
        assert_eq!(
            old["epoch"].as_u64().unwrap() + 1,
            new["epoch"].as_u64().unwrap()
        );
    }
    let close = measure(async {
        reference
            .close_ordered_session(Duration::from_secs(5))
            .await
            .unwrap();
    })
    .await;
    assert!(ticket(sender.path(), enabled, false).is_none());
    source.shutdown().await.unwrap();
    next.shutdown().await.unwrap();
    assert!(next_rx.try_recv().is_err());
    json!({"round": round, "enabled": enabled, "open": open, "steady": steady,
           "rotate": rotate, "close": close})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run alone on a disk-backed Linux TMPDIR to measure durable sender ticket costs"]
async fn sender_journal_cost() {
    let pki = pki::Pki::new();
    let mut rows = Vec::new();
    for round in 0..=12 {
        for enabled in [round % 2 == 0, round % 2 != 0] {
            let row = trial(enabled, round, &pki).await;
            // Round zero warms both paths. Print after sampling to keep output
            // redirected to a file out of the process-wide write counters.
            if round > 0 {
                rows.push(row);
            }
        }
    }
    for row in rows {
        println!("JOURNAL_TRIAL {row}");
    }
}
