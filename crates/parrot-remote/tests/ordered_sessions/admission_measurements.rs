// SPDX-License-Identifier: AGPL-3.0-only
use super::{start, state, Empty, Pki, Target, Work};
use parrot_api::address::ActorRef;
use parrot_remote::{handshake::TopologyRole, NodeAddr, RemoteActorRef, RemoteConfig};
use std::{sync::Arc, time::Duration, time::Instant};
use tokio::sync::mpsc::UnboundedReceiver;

const SAMPLES: u64 = 1024;

async fn trial(
    reference: &RemoteActorRef,
    receiver: &mut UnboundedReceiver<u64>,
    first: u64,
    count: u64,
    strict: bool,
) -> serde_json::Value {
    let mut latencies = Vec::with_capacity(count as usize);
    let started = Instant::now();
    for value in first..first + count {
        let sent = Instant::now();
        if strict {
            reference
                .deliver_ordered(Box::new(Work(value)), Default::default())
                .await
                .unwrap();
        } else {
            reference.deliver(Box::new(Work(value))).await.unwrap();
        }
        latencies.push(sent.elapsed().as_nanos() as u64);
    }
    let send_elapsed = started.elapsed();
    for expected in first..first + count {
        assert_eq!(receiver.recv().await, Some(expected));
    }
    let checked_elapsed = started.elapsed();
    latencies.sort_unstable();
    serde_json::json!({
        "count": count, "first": first, "strict": strict,
        "p50_ns": latencies[latencies.len().div_ceil(2) - 1],
        "p99_ns": latencies[(latencies.len() * 99).div_ceil(100) - 1],
        "send_elapsed_ns": send_elapsed.as_nanos() as u64,
        "verified_elapsed_ns": checked_elapsed.as_nanos() as u64,
    })
}

async fn measure(routed: bool) {
    let pki = Pki::new();
    let config = |name: &str| {
        RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())).with_mtls(pki.node(name), [])
    };
    let (state, mut receiver) = state();
    let target = start(
        config("bench-target").with_ordered_relays(["bench-hub".into()]),
        Arc::new(Target(state)),
    )
    .await;
    let source = start(config("bench-source"), Arc::new(Empty)).await;
    let hub = if routed {
        let hub = start(
            config("bench-hub").with_role(TopologyRole::Hub),
            Arc::new(Empty),
        )
        .await;
        let addr = NodeAddr::tcp("bench-hub", hub.local_addr().unwrap());
        target.connect(&addr).await.unwrap();
        source.connect(&addr).await.unwrap();
        Some(hub)
    } else {
        source
            .connect(&NodeAddr::tcp("bench-target", target.local_addr().unwrap()))
            .await
            .unwrap();
        None
    };
    let reference = source.remote_ref("parrot://bench-target/target").unwrap();
    // Skip the fixture's two deliberately blocking work values.
    let mut first = 1000;
    for strict in [false, true] {
        trial(&reference, &mut receiver, first, 256, strict).await;
        first += 256;
    }
    for round in 0..6 {
        for strict in [round % 2 == 0, round % 2 != 0] {
            let measurement = trial(&reference, &mut receiver, first, SAMPLES, strict).await;
            println!(
                "ADMISSION_TRIAL {}",
                serde_json::json!({
                    "routed":routed, "round":round, "measurement":measurement
                })
            );
            first += SAMPLES;
        }
    }
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    if let Some(hub) = hub {
        hub.shutdown().await.unwrap();
    }
    target.shutdown().await.unwrap();
    assert!(receiver.try_recv().is_err(), "extra target delivery");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opt-in mTLS loopback latency and verified delivery throughput measurement"]
async fn mtls_loopback() {
    for routed in [false, true] {
        tokio::time::timeout(Duration::from_secs(120), measure(routed))
            .await
            .unwrap();
    }
}
