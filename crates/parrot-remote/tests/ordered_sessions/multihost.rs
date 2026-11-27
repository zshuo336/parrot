// SPDX-License-Identifier: AGPL-3.0-only
use super::{start, state, Empty, Target, Work};
use parrot_api::sender::ActorSenderOwner;
use parrot_remote::{handshake::TopologyRole, tls::TlsConfig, NodeAddr, RemoteConfig};
use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration, time::Instant};

const FLOWS: usize = 4;
const ROUNDS: usize = 6;
const COUNT: usize = 256;
const TIMEOUT: Duration = Duration::from_secs(10);
const SOURCE: &str = "flow-source";
const TARGET: &str = "flow-target";
const HUB: &str = "flow-hub";
const FINISH: u64 = u64::MAX;

fn config(name: &str, bind: Option<SocketAddr>) -> RemoteConfig {
    let dir = PathBuf::from(env::var_os("PARROT_FLOW_TLS").expect("PARROT_FLOW_TLS"));
    let read = |file| std::fs::read(dir.join(file)).unwrap();
    let tls = TlsConfig {
        cert_pem: read(format!("{name}.pem")),
        key_pem: read(format!("{name}.key")),
        ca_pem: read("ca.pem".into()),
    };
    RemoteConfig::tcp(name, bind).with_mtls(tls, [])
}

fn count(round: usize) -> usize {
    if round == 0 {
        64
    } else {
        COUNT
    }
}

fn value(round: usize, flow: usize, seq: usize) -> u64 {
    (1 << 48) | (round as u64) << 32 | (flow as u64) << 16 | seq as u64
}

fn observe(next: &mut [usize; FLOWS], round: usize, message: u64) -> Result<(), &'static str> {
    let flow = ((message >> 16) & 0xffff) as usize;
    let seq = next.get_mut(flow).ok_or("unknown flow")?;
    if *seq >= count(round) || message != value(round, flow, *seq) {
        return Err("duplicate, gap, stale round, or invalid payload");
    }
    *seq += 1;
    Ok(())
}

#[test]
fn receiver_requires_exact_sequence_per_flow_and_round() {
    let mut next = [0; FLOWS];
    for (flow, seq) in [(2, 0), (0, 0), (2, 1)] {
        observe(&mut next, 1, value(1, flow, seq)).unwrap();
    }
    for wrong in [
        value(1, 0, 0),
        value(1, 0, 2),
        value(0, 0, 1),
        value(1, FLOWS, 0),
        FINISH,
    ] {
        assert!(observe(&mut next, 1, wrong).is_err());
    }
    assert_eq!(next, [1, 0, 2, 0]);
}

async fn receive(bind: SocketAddr, routed: bool) {
    let (target_state, mut received) = state();
    let hub = if routed {
        Some(
            start(
                config(HUB, Some(bind)).with_role(TopologyRole::Hub),
                Arc::new(Empty),
            )
            .await,
        )
    } else {
        None
    };
    let target = start(
        config(TARGET, (!routed).then_some(bind)).with_ordered_relays([HUB.into()]),
        Arc::new(Target(target_state)),
    )
    .await;
    let entry = hub.as_ref().unwrap_or(&target);
    let addr = entry.local_addr().unwrap();
    if routed {
        target.connect(&NodeAddr::tcp(HUB, addr)).await.unwrap();
    }
    println!("FLOW_READY {addr}");
    for round in 0..=ROUNDS {
        let mut next = [0; FLOWS];
        for _ in 0..FLOWS * count(round) {
            let message = received.recv().await.unwrap();
            observe(&mut next, round, message).unwrap_or_else(|why| {
                panic!("{why}: round={round} message={message} next={next:?}")
            });
        }
        assert_eq!(next, [count(round); FLOWS]);
        println!(
            "FLOW_RECEIVED {}",
            serde_json::json!({"round": round, "counts": next})
        );
        target
            .remote_ref("parrot://flow-source/target")
            .unwrap()
            .deliver_with_ack(Box::new(Work(round as u64)), TIMEOUT)
            .await
            .unwrap();
    }
    assert_eq!(received.recv().await, Some(FINISH));
    // Keep the return path alive until the source has its final ACK and disconnects.
    while entry
        .links_snapshot()
        .await
        .iter()
        .any(|(node, _, _)| node == SOURCE)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    target.shutdown().await.unwrap();
    if let Some(hub) = hub {
        hub.shutdown().await.unwrap();
    }
    assert!(received.try_recv().is_err(), "extra receiver delivery");
}

async fn send(addr: SocketAddr, routed: bool) {
    let (ack_state, mut acks) = state();
    let source = start(config(SOURCE, None), Arc::new(Target(ack_state))).await;
    let peer = if routed { HUB } else { TARGET };
    source.connect(&NodeAddr::tcp(peer, addr)).await.unwrap();
    let reference = source.remote_ref("parrot://flow-target/target").unwrap();
    let owners: Vec<_> = (0..FLOWS)
        .map(|f| ActorSenderOwner::new(&format!("/user/flow-{f}")))
        .collect();
    for round in 0..=ROUNDS {
        let mut tasks = tokio::task::JoinSet::new();
        let started = Instant::now();
        for (flow, owner) in owners.iter().enumerate() {
            let target = reference.clone();
            tasks.spawn(owner.sender().scope(async move {
                let mut samples = Vec::with_capacity(count(round));
                for seq in 0..count(round) {
                    let sent = Instant::now();
                    target
                        .deliver_ordered(
                            Box::new(Work(value(round, flow, seq))),
                            Default::default(),
                        )
                        .await
                        .unwrap();
                    samples.push(sent.elapsed().as_nanos() as u64);
                }
                samples
            }));
        }
        let mut samples = Vec::with_capacity(FLOWS * count(round));
        while let Some(done) = tasks.join_next().await {
            samples.extend(done.unwrap());
        }
        assert_eq!(acks.recv().await, Some(round as u64));
        let verified_ns = started.elapsed().as_nanos() as u64;
        let links = source.links_snapshot().await;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, peer);
        samples.sort_unstable();
        let n = samples.len();
        println!(
            "FLOW_TRIAL {}",
            serde_json::json!({"routed":routed, "peer":peer, "round":round,
            "messages":n, "verified_ns":verified_ns, "p50_ns":samples[n.div_ceil(2)-1],
            "p99_ns":samples[(n*99).div_ceil(100)-1]})
        );
    }
    for owner in &owners {
        owner
            .sender()
            .in_scope(|| reference.close_ordered_session(TIMEOUT))
            .await
            .unwrap();
    }
    reference
        .deliver_with_ack(Box::new(Work(FINISH)), TIMEOUT)
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    assert!(acks.try_recv().is_err(), "extra round receipt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "two-host mTLS measurement; configure PARROT_FLOW_ROLE, TOPOLOGY, ADDR and TLS"]
async fn mtls_multihost() {
    let addr = env::var("PARROT_FLOW_ADDR")
        .expect("PARROT_FLOW_ADDR")
        .parse()
        .unwrap();
    let routed = match env::var("PARROT_FLOW_TOPOLOGY").as_deref() {
        Ok("direct") => false,
        Ok("hub") => true,
        _ => panic!("expected direct or hub"),
    };
    tokio::time::timeout(Duration::from_secs(120), async {
        match env::var("PARROT_FLOW_ROLE").as_deref() {
            Ok("receiver") => receive(addr, routed).await,
            Ok("sender") => send(addr, routed).await,
            _ => panic!("expected sender or receiver"),
        }
    })
    .await
    .expect("two-host flow deadline");
}
