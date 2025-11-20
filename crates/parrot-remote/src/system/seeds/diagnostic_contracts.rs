use super::*;
use crate::system::lifecycle_diagnostic_contracts::{echo, isolated, Lookup, Probe};
use std::sync::{atomic::Ordering, Arc};

async fn dns(probe: Arc<Probe>) {
    let mut group = SeedGroup {
        identity: "diagnostic-seed".into(),
        ..Default::default()
    };
    group.numeric.push(NodeAddr::tcp(
        "diagnostic-seed",
        "127.0.0.1:1".parse().unwrap(),
    ));
    group.dns = [
        "tcp://diagnostic-seed@bad.test:2",
        "tcp://diagnostic-seed@good.test:3",
    ]
    .map(|source| SeedEndpoint::parse(source, "tcp").unwrap())
    .to_vec();
    probe.armed.store(true, Ordering::SeqCst);
    let answers = group
        .candidates(|host, port| async move {
            if host == "bad.test" {
                Err(io::Error::other("controlled DNS error"))
            } else {
                Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
            }
        })
        .await;
    assert_eq!(answers.len(), 2);
    assert!(answers
        .iter()
        .any(|(address, retained)| address.addr.port() == 1 && *retained));
    assert!(answers
        .iter()
        .any(|(address, retained)| address.addr.port() == 3 && !retained));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    probe.armed.store(false, Ordering::SeqCst);
}

async fn candidates(probe: Arc<Probe>, timeout: bool) {
    let server = crate::RemoteActorSystem::new(
        RemoteConfig::tcp("diagnostic-seed", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup),
    )
    .unwrap();
    let client = crate::RemoteActorSystem::new(
        RemoteConfig::tcp("diagnostic-client", None),
        Arc::new(Lookup),
    )
    .unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad = if timeout {
        blackhole.local_addr().unwrap()
    } else {
        "127.0.0.1:0".parse().unwrap()
    };
    let good = server.local_addr().unwrap();
    let mut group = SeedGroup {
        identity: "diagnostic-seed".into(),
        explicit: true,
        numeric: vec![
            NodeAddr::tcp("diagnostic-seed", bad),
            NodeAddr::tcp("diagnostic-seed", good),
        ],
        ..Default::default()
    };
    // candidates sorts addresses; force the known failing candidate to run first.
    group.next_address = usize::from(bad > good);
    probe.armed.store(true, Ordering::SeqCst);
    let identity = tokio::time::timeout(
        Duration::from_secs(5),
        group.attempt(&Arc::downgrade(&client)),
    )
    .await
    .expect("seed candidate fallback stopped making progress");
    assert_eq!(identity.as_deref(), Some("diagnostic-seed"));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert!(client.direct_sender_of("diagnostic-seed").is_some());
    echo(&client, "diagnostic-seed").await;
    probe.armed.store(false, Ordering::SeqCst);
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

macro_rules! contract {
    ($name:ident, $message:literal, $payload:literal, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            let probe = Probe::new($message);
            isolated(
                concat!("system::seeds::diagnostic_contracts::", stringify!($name)),
                &["parrot_remote::system::seeds"],
                $payload,
                probe.clone(),
                $body(probe),
            )
            .await;
        }
    };
}
contract!(
    dns_failure_diagnostic_preserves_completed_candidates,
    "seed DNS refresh failed",
    false,
    dns
);
contract!(
    dns_failure_diagnostic_payload_preserves_completed_candidates,
    "seed DNS refresh failed",
    true,
    dns
);
contract!(
    connection_failure_diagnostic_preserves_next_candidate,
    "seed connection retry",
    false,
    |probe| candidates(probe, false)
);
contract!(
    connection_failure_diagnostic_payload_preserves_next_candidate,
    "seed connection retry",
    true,
    |probe| candidates(probe, false)
);
contract!(
    candidate_timeout_diagnostic_preserves_next_candidate,
    "seed candidate timed out",
    false,
    |probe| candidates(probe, true)
);
contract!(
    candidate_timeout_diagnostic_payload_preserves_next_candidate,
    "seed candidate timed out",
    true,
    |probe| candidates(probe, true)
);
