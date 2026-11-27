// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_remote::capture::{CaptureDirection, CaptureLimits, FrameCapture};
use std::collections::BTreeMap;

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
pub(super) struct Sample([u8; 16]);
impl parrot_api::Message for Sample {
    type Result = ();
}
impl Sample {
    fn new(n: u64) -> Self {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&n.to_le_bytes());
        bytes[8..].copy_from_slice(&(!n).to_le_bytes());
        Self(bytes)
    }
    pub(super) fn number(&self) -> u64 {
        let n = u64::from_le_bytes(self.0[..8].try_into().unwrap());
        assert_eq!(self.0[8..], (!n).to_le_bytes());
        n
    }
}

#[tokio::test]
async fn paced_sensor_stream_measures_received_wire_bytes_and_application_order() {
    sensor(1).await;
    sensor(10).await;
}

async fn sensor(per_tick: usize) {
    let pki = Pki::new();
    let config = |name: &str| {
        RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())).with_mtls(pki.node(name), [])
    };
    let (activity, mut rx) = state();
    let (capture, mut records) = FrameCapture::channel(CaptureLimits {
        capacity: 1100,
        ..Default::default()
    })
    .unwrap();
    let target = start(
        config("sensor-target").with_capture(capture.clone()),
        Arc::new(Target(activity)),
    )
    .await;
    let source = start(config("sensor-source"), Arc::new(Empty)).await;
    source
        .connect(&NodeAddr::tcp(
            "sensor-target",
            target.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let reference = source.remote_ref("parrot://sensor-target/target").unwrap();
    // Open the ticket before starting the offered-load clock.
    strict(&reference, 1).await;
    seen(&mut rx, 1).await;
    let total = 1000 * per_tick;
    let start = std::time::Instant::now();
    let send = async {
        let mut timer = tokio::time::interval(Duration::from_millis(1));
        for n in 0..total {
            if n % per_tick == 0 {
                timer.tick().await;
            }
            reference
                .deliver(Box::new(Sample::new(5000 + n as u64)))
                .await
                .unwrap();
        }
        start.elapsed().as_micros()
    };
    let receive = async {
        for n in 0..total {
            seen(&mut rx, 5000 + n as u64).await;
        }
        start.elapsed().as_micros()
    };
    let (sent_us, received_us) = tokio::join!(send, receive);
    reference
        .close_ordered_session(Duration::from_secs(4))
        .await
        .unwrap();
    for node in [&source, &target] {
        node.shutdown().await.unwrap();
    }
    capture.close();
    tokio::task::spawn_blocking(move || {
        let mut sizes = BTreeMap::<usize,usize>::new();
        let mut wire = 0;
        let mut singles = bytes::BytesMut::new();
        let mut arrivals = Vec::new();
        while let Some(event) = records.blocking_recv() {
            if event.direction != CaptureDirection::Rx || event.frame.type_key != DATA_KEY { continue; }
            arrivals.push(event.wire.elapsed_micros);
            wire += 4 + event.frame.header.frame_len as usize;
            let mut count = 1;
            if event.frame.header.flags & parrot_remote::frame::flags::BATCH != 0 {
                let batch = event.frame.batch_frames().unwrap();
                count = batch.len();
                for frame in batch { frame.encode(&mut singles).unwrap(); }
            } else { event.frame.encode(&mut singles).unwrap(); }
            assert!((1..=64).contains(&count));
            *sizes.entry(count).or_default() += 1;
        }
        assert_eq!(records.final_stats().lost, 0);
        assert_eq!(sizes.iter().map(|(n, k)| n * k).sum::<usize>(), total);
        assert!(wire < singles.len());
        if per_tick == 10 { assert!(wire * 100 < singles.len() * 40); }
        let mut gaps: Vec<_> = arrivals.windows(2).map(|pair| pair[1] - pair[0]).collect();
        gaps.sort_unstable();
        println!("sensor members={total} sent_us={sent_us} received_us={received_us} frames={} wire_bytes={wire} single_bytes={} rx_gap_p50_us={} rx_gap_p99_us={} histogram={sizes:?}",
            arrivals.len(), singles.len(), gaps[gaps.len()/2], gaps[gaps.len()*99/100]);
    }).await.unwrap();
}

#[tokio::test]
async fn shared_hub_batches_independent_admissions_with_individual_receipts() {
    const SENDERS: usize = 12;
    const PER_SENDER: usize = 12;
    let pki = Pki::new();
    let config = |name: &str, listen| RemoteConfig::tcp(name, listen).with_mtls(pki.node(name), []);
    let (activity, mut rx) = state();
    let (capture, mut records) = FrameCapture::channel(CaptureLimits {
        capacity: 1024,
        ..Default::default()
    })
    .unwrap();
    let hub = start(
        config("batch-hub", Some("127.0.0.1:0".parse().unwrap())).with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let target = start(
        config("relay-batch-target", None)
            .with_ordered_relays(["batch-hub".into()])
            .with_capture(capture.clone()),
        Arc::new(Target(activity)),
    )
    .await;
    let address = NodeAddr::tcp("batch-hub", hub.local_addr().unwrap());
    target.connect(&address).await.unwrap();
    let mut sources = Vec::new();
    let mut references = Vec::new();
    for i in 0..SENDERS {
        let source = start(config(&format!("relay-batch-{i}"), None), Arc::new(Empty)).await;
        source.connect(&address).await.unwrap();
        let reference = source
            .remote_ref("parrot://relay-batch-target/target")
            .unwrap();
        strict(&reference, 1).await;
        seen(&mut rx, 1).await;
        sources.push(source);
        references.push(reference);
    }
    futures::future::join_all(
        references
            .iter()
            .enumerate()
            .map(|(i, reference)| async move {
                for n in 0..PER_SENDER {
                    strict(reference, (10000 + i * PER_SENDER + n) as u64).await;
                }
            }),
    )
    .await;
    let mut counts = [0; SENDERS];
    for _ in 0..SENDERS * PER_SENDER {
        let value = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap() as usize
            - 10000;
        let i = value / PER_SENDER;
        assert_eq!(value % PER_SENDER, counts[i]);
        counts[i] += 1;
    }
    assert_eq!(counts, [PER_SENDER; SENDERS]);
    for reference in &references {
        reference
            .close_ordered_session(Duration::from_secs(3))
            .await
            .unwrap();
    }
    for node in sources.iter().chain([&hub, &target]) {
        node.shutdown().await.unwrap();
    }
    capture.close();
    tokio::task::spawn_blocking(move || {
        let mut members = 0;
        let mut sizes = BTreeMap::<usize, usize>::new();
        let mut cids = std::collections::HashSet::new();
        while let Some(event) = records.blocking_recv() {
            if event.direction != CaptureDirection::Rx
                || event.frame.type_key != parrot_remote::ordered::ADMIT_KEY
            {
                continue;
            }
            assert_eq!(event.wire.peer_node.as_deref(), Some("batch-hub"));
            let frames = if event.frame.header.flags & parrot_remote::frame::flags::BATCH != 0 {
                event.frame.iter_batch().unwrap()
            } else {
                vec![event.frame.clone()]
            };
            *sizes.entry(frames.len()).or_default() += 1;
            for frame in frames {
                assert!(frame.header.hop_count > 0);
                assert_ne!(frame.header.correlation_id, 0);
                assert!(cids.insert(frame.header.correlation_id));
                members += 1;
            }
        }
        assert_eq!(records.final_stats().lost, 0);
        assert_eq!(members, SENDERS * (1 + PER_SENDER));
        assert!(sizes.keys().any(|&n| n > 1), "{sizes:?}");
        println!("hub admissions members={members} histogram={sizes:?}");
    })
    .await
    .unwrap();
}
