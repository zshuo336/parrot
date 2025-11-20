//! Real TCP negotiated-frame counterexamples. Heartbeats prove transport liveness.
use futures::{SinkExt, StreamExt};
use parrot_api::{
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey},
    types::BoxedActorRef,
};
use parrot_remote::{
    acl::AclRules,
    frame::{frame_type, Frame, BODY_FIXED_OVERHEAD},
    handshake::{caps, HandshakeBody},
    receptionist::{decode_sync, encode_sync, ReceptionistEntry, ReceptionistSync},
    swim::SwimConfig,
    transport::FrameCodec,
    LocalLookup, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};
use tokio_util::codec::Framed;

const FRAME_LIMIT: u32 = 512;
const LIMIT: Duration = Duration::from_secs(5);
const SEGMENTS: u32 = 1 << 19;
const SEGMENT_TAG: u8 = 0x21;
struct Peer {
    wire: Framed<tokio::net::TcpStream, FrameCodec>,
    pending: Option<(u64, usize, Vec<u8>)>,
    segments: usize,
}

impl Peer {
    fn snapshot(&mut self, payload: &[u8]) -> Option<ReceptionistSync> {
        if payload.first() == Some(&parrot_remote::admin::sys_event_tag::RECEPTIONIST_SYNC) {
            self.pending = None;
            return Some(decode_sync(&payload[1..]).unwrap());
        }
        if payload.first() != Some(&SEGMENT_TAG) {
            return None;
        }
        assert!(payload.len() > 17);
        let id = u64::from_le_bytes(payload[1..9].try_into().unwrap());
        let total = u32::from_le_bytes(payload[9..13].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(payload[13..17].try_into().unwrap()) as usize;
        if offset == 0 {
            assert!(
                self.pending.is_none(),
                "a snapshot must finish before a new one starts"
            );
            self.pending = Some((id, total, Vec::new()));
        }
        let pending = self
            .pending
            .as_mut()
            .expect("first segment must precede its continuation");
        assert_eq!(
            (pending.0, pending.1, pending.2.len()),
            (id, total, offset),
            "segment cursor must match all previously delivered bytes"
        );
        pending.2.extend_from_slice(&payload[17..]);
        assert!(pending.2.len() <= total);
        self.segments += 1;
        if pending.2.len() == total {
            Some(decode_sync(&self.pending.take().unwrap().2).unwrap())
        } else {
            None
        }
    }
}
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
fn key() -> ReceptionistKey {
    ReceptionistKey::new("budget/service").unwrap()
}
async fn node() -> Arc<RemoteActorSystem> {
    node_with(Duration::from_millis(10), 1024).await
}
async fn node_with(period: Duration, queue: usize) -> Arc<RemoteActorSystem> {
    node_with_registration_limits(period, queue, Default::default()).await
}
async fn node_with_registration_limits(
    period: Duration,
    queue: usize,
    limits: parrot_config::receptionist::RegistrationLimits,
) -> Arc<RemoteActorSystem> {
    let config = RemoteConfig::tcp("budget-owner", Some("127.0.0.1:0".parse().unwrap()))
        .with_receptionist_registrations(limits)
        .with_insecure_development()
        .with_receptionist_acl(AclRules::permissive())
        .with_knobs(parrot_remote::transport::RuntimeKnobs {
            outbound_queue: queue,
            ..Default::default()
        })
        .with_swim_config(SwimConfig {
            gossip_interval: period,
            ..Default::default()
        });
    let system = RemoteActorSystem::new(config, Arc::new(Empty)).unwrap();
    system.start().await.unwrap();
    system
}
async fn connect(system: &Arc<RemoteActorSystem>) -> Peer {
    connect_with_caps(
        system,
        caps::BIN | caps::RECEPTIONIST_SYNC | caps::RECEPTIONIST_NATIVE_PATHS | SEGMENTS,
    )
    .await
}
async fn connect_with_caps(system: &Arc<RemoteActorSystem>, capabilities: u32) -> Peer {
    connect_with_frame_limit(system, capabilities, FRAME_LIMIT).await
}
async fn connect_with_frame_limit(
    system: &Arc<RemoteActorSystem>,
    capabilities: u32,
    limit: u32,
) -> Peer {
    let stream = tokio::net::TcpStream::connect(system.local_addr().unwrap())
        .await
        .unwrap();
    let mut peer = Framed::new(stream, FrameCodec);
    let mut body = bytes::BytesMut::new();
    HandshakeBody {
        node_id: "budget-peer".into(),
        capabilities,
        max_frame_len: limit,
        ..Default::default()
    }
    .encode_tlv(&mut body)
    .unwrap();
    peer.send(Frame::handshake(body.freeze())).await.unwrap();
    let ack = tokio::time::timeout(LIMIT, peer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(ack.header.frame_type, frame_type::HANDSHAKE_ACK);
    tokio::time::timeout(LIMIT, async {
        while !system
            .membership
            .lock()
            .await
            .members
            .contains_key("budget-peer")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    Peer {
        wire: peer,
        pending: None,
        segments: 0,
    }
}
async fn bulk(system: &Arc<RemoteActorSystem>) -> ReceptionistSync {
    let gateway = system.receptionist_gateway();
    for index in 0..64 {
        gateway
            .register(key(), format!("/user/bulk/{index:04}"))
            .await
            .unwrap();
    }
    let sync = ReceptionistSync {
        from: system.config.node_id.clone(),
        entries: (0..64)
            .map(|index| ReceptionistEntry {
                key: key(),
                remote_path: format!("parrot://budget-owner/user/bulk/{index:04}"),
                node: "budget-owner".into(),
            })
            .collect(),
    };
    let body_len = BODY_FIXED_OVERHEAD + encode_sync(&sync).len();
    assert!(body_len > FRAME_LIMIT as usize);
    println!("snapshot_body_bytes={body_len} negotiated_frame_limit={FRAME_LIMIT}");
    sync
}
async fn observe(peer: &mut Peer, latest: &mut Option<ReceptionistSync>) {
    peer.wire.send(Frame::heartbeat()).await.unwrap();
    let mut acknowledged = false;
    let until = tokio::time::Instant::now() + Duration::from_millis(400);
    while let Ok(frame) = tokio::time::timeout_at(until, peer.wire.next()).await {
        let frame = frame.expect("discovery peer remains connected").unwrap();
        assert!(frame.header.frame_len <= FRAME_LIMIT);
        match frame.header.frame_type {
            frame_type::HEARTBEAT_ACK => acknowledged = true,
            frame_type::HEARTBEAT => peer.wire.send(Frame::heartbeat_ack()).await.unwrap(),
            frame_type::SYSTEM_EVENT => {
                if let Some(snapshot) = peer.snapshot(&frame.payload) {
                    *latest = Some(snapshot);
                }
            }
            _ => {}
        }
    }
    assert!(
        acknowledged,
        "real TCP heartbeat acknowledgement must survive"
    );
    println!("heartbeat_acknowledged=true discovery_observation_ms=400");
}

#[tokio::test]
async fn oversized_initial_table_must_be_discoverable_on_a_live_small_frame_connection() {
    let system = node().await;
    let expected = bulk(&system).await;
    let mut peer = connect(&system).await;
    let mut latest = None;
    observe(&mut peer, &mut latest).await;
    system.shutdown().await.unwrap();
    let actual = latest.expect("oversized initial snapshot must reach the live negotiated peer");
    assert_eq!(actual.entries.len(), expected.entries.len());
    assert!(expected
        .entries
        .iter()
        .all(|entry| actual.entries.contains(entry)));
}

#[tokio::test]
async fn growing_table_must_not_prevent_withdrawal_on_a_live_small_frame_connection() {
    let system = node().await;
    let gateway = system.receptionist_gateway();
    gateway
        .register(key(), "/user/original".into())
        .await
        .unwrap();
    let mut peer = connect(&system).await;
    let mut latest = None;
    observe(&mut peer, &mut latest).await;
    assert_eq!(latest.as_ref().unwrap().entries.len(), 1);
    let mut expected = bulk(&system).await;
    gateway
        .deregister(key(), "/user/original".into())
        .await
        .unwrap();
    expected
        .entries
        .retain(|entry| !entry.remote_path.ends_with("/original"));
    observe(&mut peer, &mut latest).await;
    system.shutdown().await.unwrap();
    let actual = latest.unwrap();
    assert!(
        actual
            .entries
            .iter()
            .all(|entry| !entry.remote_path.ends_with("/original")),
        "withdrawal must reach the live peer while the remaining table exceeds its frame limit"
    );
    assert_eq!(actual.entries.len(), expected.entries.len());
}

fn owned(paths: &[&str]) -> ReceptionistSync {
    ReceptionistSync {
        from: "budget-peer".into(),
        entries: paths
            .iter()
            .map(|path| ReceptionistEntry {
                key: key(),
                node: "budget-peer".into(),
                remote_path: format!("parrot://budget-peer{path}"),
            })
            .collect(),
    }
}
fn segment(id: u64, total: usize, offset: usize, data: &[u8]) -> Frame {
    let mut payload = bytes::BytesMut::new();
    payload.extend_from_slice(&[SEGMENT_TAG]);
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(&(total as u32).to_le_bytes());
    payload.extend_from_slice(&(offset as u32).to_le_bytes());
    payload.extend_from_slice(data);
    let mut frame = Frame::heartbeat();
    frame.header.frame_type = frame_type::SYSTEM_EVENT;
    frame.header.hop_limit = 1;
    frame.payload = payload.freeze();
    frame
}
async fn push(peer: &mut Peer, id: u64, sync: &ReceptionistSync) {
    let payload = encode_sync(sync);
    let body = &payload[1..];
    for offset in (0..body.len()).step_by(100) {
        peer.wire
            .send(segment(
                id,
                body.len(),
                offset,
                &body[offset..body.len().min(offset + 100)],
            ))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn segmented_incoming_snapshots_commit_atomically_and_replay_cannot_revive_withdrawals() {
    let system = node().await;
    let mut events = system
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    let mut peer = connect(&system).await;
    push(&mut peer, 1, &owned(&["/user/original"])).await;
    assert_eq!(
        tokio::time::timeout(LIMIT, events.recv())
            .await
            .unwrap()
            .unwrap(),
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://budget-peer/user/original".into()
        }
    );
    let next = owned(&["/user/replacement"]);
    let payload = encode_sync(&next);
    let body = &payload[1..];
    let split = body.len() / 2;
    peer.wire
        .send(segment(2, body.len(), 0, &body[..split]))
        .await
        .unwrap();
    observe(&mut peer, &mut None).await;
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty),
        "a partial snapshot must publish no changes"
    );
    peer.wire
        .send(segment(2, body.len(), split, &body[split..]))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(LIMIT, events.recv())
            .await
            .unwrap()
            .unwrap(),
        ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: "parrot://budget-peer/user/original".into()
        }
    );
    assert_eq!(
        tokio::time::timeout(LIMIT, events.recv())
            .await
            .unwrap()
            .unwrap(),
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://budget-peer/user/replacement".into()
        }
    );
    push(&mut peer, 1, &owned(&["/user/original"])).await;
    observe(&mut peer, &mut None).await;
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    let mut snapshot = system
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    assert_eq!(
        snapshot.recv().await,
        Some(ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://budget-peer/user/replacement".into(),
        })
    );
    assert_eq!(
        snapshot.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    system.shutdown().await.unwrap();
}

async fn closed(peer: &mut Peer) {
    tokio::time::timeout(LIMIT, async {
        while let Some(frame) = peer.wire.next().await {
            if frame.is_err() {
                break;
            }
        }
    })
    .await
    .expect("invalid segmented peer must be disconnected");
}

#[tokio::test]
async fn segments_require_mutual_capability_and_retain_legacy_full_snapshots() {
    let system = node().await;
    system
        .receptionist_gateway()
        .register(key(), "/user/original".into())
        .await
        .unwrap();
    let mut peer = connect_with_caps(&system, caps::BIN | caps::RECEPTIONIST_SYNC).await;
    let mut latest = None;
    observe(&mut peer, &mut latest).await;
    assert_eq!(latest.unwrap().entries.len(), 1);
    assert_eq!(peer.segments, 0);
    push(&mut peer, 1, &owned(&["/user/forbidden"])).await;
    closed(&mut peer).await;
    system.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_or_foreign_segmented_snapshots_close_without_partial_publication() {
    for case in 0..4 {
        let system = node().await;
        let mut events = system
            .receptionist_gateway()
            .subscribe(key())
            .await
            .unwrap();
        let mut peer = connect(&system).await;
        match case {
            0 => peer
                .wire
                .send(segment(1, 16 * 1024 * 1024 + 1, 0, &[1]))
                .await
                .unwrap(),
            1 => peer.wire.send(segment(1, 4, 1, &[1])).await.unwrap(),
            2 => {
                peer.wire.send(segment(1, 4, 0, &[1])).await.unwrap();
                peer.wire.send(segment(1, 5, 1, &[2])).await.unwrap();
            }
            _ => {
                let mut sync = owned(&["/user/forged"]);
                sync.from = "somebody-else".into();
                push(&mut peer, 1, &sync).await;
            }
        }
        closed(&mut peer).await;
        assert_eq!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        assert_eq!(
            system
                .receptionist_gateway()
                .subscribe(key())
                .await
                .unwrap()
                .try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        system.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn snapshot_larger_than_outbound_queue_resumes_without_restarting_its_prefix() {
    let system = node_with(Duration::from_millis(1), 2).await;
    let gateway = system.receptionist_gateway();
    let tail = "x".repeat(600);
    let paths: Vec<_> = (0..1024)
        .map(|index| format!("/user/bulk/{index:04}/{tail}"))
        .collect();
    for path in &paths {
        gateway.register(key(), path.clone()).await.unwrap();
    }
    let mut peer = connect(&system).await;
    let actual = tokio::time::timeout(LIMIT, async {
        loop {
            let frame = peer
                .wire
                .next()
                .await
                .expect("bounded queue must not disconnect")
                .unwrap();
            assert!(frame.header.frame_len <= FRAME_LIMIT);
            if frame.header.frame_type == frame_type::HEARTBEAT {
                peer.wire.send(Frame::heartbeat_ack()).await.unwrap();
            } else if frame.header.frame_type == frame_type::SYSTEM_EVENT {
                if let Some(snapshot) = peer.snapshot(&frame.payload) {
                    break snapshot;
                }
            }
        }
    })
    .await
    .expect("a snapshot must progress through a two-frame writer queue");
    assert!(
        peer.segments > 1024,
        "snapshot must exceed even the default frame queue"
    );
    assert_eq!(actual.entries.len(), paths.len());
    let actual: std::collections::HashSet<_> = actual
        .entries
        .iter()
        .map(|entry| entry.remote_path.as_str())
        .collect();
    assert!(paths
        .iter()
        .all(|path| actual.contains(format!("parrot://budget-owner{path}").as_str())));
    println!(
        "queued_snapshot_segments={} writer_capacity=2",
        peer.segments
    );
    system.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_segmented_event_rejects_entire_snapshot_before_any_publication() {
    oversized_event_rejected(true).await;
}

#[tokio::test]
async fn oversized_legacy_event_rejects_entire_snapshot_before_any_publication() {
    oversized_event_rejected(false).await;
}

async fn oversized_event_rejected(segmented: bool) {
    let system = node().await;
    let mut events = system
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    let capabilities = caps::BIN
        | caps::RECEPTIONIST_SYNC
        | caps::RECEPTIONIST_NATIVE_PATHS
        | if segmented { SEGMENTS } else { 0 };
    let mut peer = connect_with_frame_limit(&system, capabilities, 128 * 1024).await;
    // The first entry is valid and must not be published when the later one is rejected.
    let mut snapshot = owned(&["/user/must-not-publish", "/user/oversized"]);
    snapshot.entries[1].key = ReceptionistKey::new(format!(
        "budget/{}",
        "x".repeat(parrot_remote::receptionist::MAX_EVENT_BYTES)
    ))
    .unwrap();
    if segmented {
        push(&mut peer, 1, &snapshot).await;
    } else {
        let mut frame = Frame::heartbeat();
        frame.header.frame_type = frame_type::SYSTEM_EVENT;
        frame.header.hop_limit = 1;
        frame.payload = encode_sync(&snapshot);
        peer.wire.send(frame).await.unwrap();
    }
    tokio::time::timeout(LIMIT, async {
        loop {
            tokio::select! {
                event = events.recv() => panic!("oversized peer event must reject the entire snapshot atomically: {event:?}"),
                frame = peer.wire.next() => {
                    let Some(Ok(frame)) = frame else { break; };
                    if frame.header.frame_type == frame_type::HEARTBEAT {
                        let _ = peer.wire.send(Frame::heartbeat_ack()).await;
                    }
                }
            }
        }
    }).await.expect("oversized event peer must be disconnected");
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty),
        "oversized peer event must reject the entire snapshot atomically"
    );
    let mut replay = system
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    assert_eq!(
        replay.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    system.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_snapshot_registration_budget_rejection_keeps_connection_and_old_view() {
    registration_budget_recovery(false).await;
}

#[tokio::test]
async fn segmented_snapshot_registration_budget_rejection_keeps_connection_and_old_view() {
    registration_budget_recovery(true).await;
}

async fn send_snapshot(peer: &mut Peer, id: u64, sync: &ReceptionistSync, segmented: bool) {
    if segmented {
        push(peer, id, sync).await;
    } else {
        let mut frame = Frame::heartbeat();
        frame.header.frame_type = frame_type::SYSTEM_EVENT;
        frame.header.hop_limit = 1;
        frame.payload = encode_sync(sync);
        peer.wire.send(frame).await.unwrap();
    }
}

async fn registration_budget_recovery(segmented: bool) {
    // One local plus one learned registration fills the shared two-entry table.
    let system = node_with_registration_limits(
        Duration::from_millis(10),
        1024,
        parrot_config::receptionist::RegistrationLimits {
            max_registrations: 2,
            max_reserved_bytes: usize::MAX,
        },
    )
    .await;
    let gateway = system.receptionist_gateway();
    gateway.register(key(), "/user/local".into()).await.unwrap();
    let mut events = gateway.subscribe(key()).await.unwrap();
    events.try_recv().unwrap();
    let mut peer = connect_with_caps(
        &system,
        caps::BIN
            | caps::RECEPTIONIST_SYNC
            | caps::RECEPTIONIST_NATIVE_PATHS
            | if segmented { SEGMENTS } else { 0 },
    )
    .await;
    send_snapshot(&mut peer, 1, &owned(&["/user/old"]), segmented).await;
    assert_eq!(
        tokio::time::timeout(LIMIT, events.recv()).await.unwrap(),
        Some(ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://budget-peer/user/old".into(),
        })
    );
    send_snapshot(
        &mut peer,
        2,
        &owned(&["/user/new", "/user/extra"]),
        segmented,
    )
    .await;
    observe(&mut peer, &mut None).await;
    assert_eq!(events.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty),
        "over-budget snapshot must retain old view without partial events on the same live connection");
    let mut replay = gateway.subscribe(key()).await.unwrap();
    let mut actual = Vec::new();
    while let Ok(ReceptionistEvent::Registered { remote_path, .. }) = replay.try_recv() {
        actual.push(remote_path);
    }
    actual.sort();
    assert_eq!(
        actual,
        [
            "parrot://budget-owner/user/local",
            "parrot://budget-peer/user/old"
        ]
    );
    // Same connection: smaller replacement uses credit for the old node.
    send_snapshot(&mut peer, 3, &owned(&["/user/new"]), segmented).await;
    let mut replacement = Vec::new();
    for _ in 0..2 {
        replacement.push(
            tokio::time::timeout(LIMIT, events.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(
        replacement,
        [
            ReceptionistEvent::Unregistered {
                key: key(),
                remote_path: "parrot://budget-peer/user/old".into()
            },
            ReceptionistEvent::Registered {
                key: key(),
                remote_path: "parrot://budget-peer/user/new".into()
            },
        ]
    );
    gateway
        .deregister(key(), "/user/local".into())
        .await
        .unwrap();
    events.try_recv().unwrap();
    // A fresh synchronization can use capacity freed locally, without reconnecting.
    send_snapshot(
        &mut peer,
        4,
        &owned(&["/user/new", "/user/extra"]),
        segmented,
    )
    .await;
    assert_eq!(
        tokio::time::timeout(LIMIT, events.recv()).await.unwrap(),
        Some(ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://budget-peer/user/extra".into(),
        })
    );
    send_snapshot(&mut peer, 5, &owned(&[]), segmented).await;
    for _ in 0..2 {
        assert!(matches!(
            tokio::time::timeout(LIMIT, events.recv()).await.unwrap(),
            Some(ReceptionistEvent::Unregistered { .. })
        ));
    }
    observe(&mut peer, &mut None).await;
    assert!(gateway.subscribe(key()).await.unwrap().is_empty());
    system.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_snapshot_registration_bytes_are_atomic_and_recover_on_same_connection() {
    registration_byte_recovery(false).await;
}

#[tokio::test]
async fn segmented_snapshot_registration_bytes_are_atomic_and_recover_on_same_connection() {
    registration_byte_recovery(true).await;
}

async fn registration_byte_recovery(segmented: bool) {
    let cost = parrot_remote::receptionist::registration_bytes(
        "budget-peer",
        &key(),
        "parrot://budget-peer/user/old",
    );
    let system = node_with_registration_limits(
        Duration::from_millis(10),
        1024,
        parrot_config::receptionist::RegistrationLimits {
            max_registrations: 10,
            max_reserved_bytes: cost as usize,
        },
    )
    .await;
    let gateway = system.receptionist_gateway();
    let mut events = gateway.subscribe(key()).await.unwrap();
    let mut peer = connect_with_caps(
        &system,
        caps::BIN
            | caps::RECEPTIONIST_SYNC
            | caps::RECEPTIONIST_NATIVE_PATHS
            | if segmented { SEGMENTS } else { 0 },
    )
    .await;
    send_snapshot(&mut peer, 1, &owned(&["/user/old"]), segmented).await;
    assert_eq!(
        tokio::time::timeout(LIMIT, events.recv()).await.unwrap(),
        Some(ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://budget-peer/user/old".into(),
        })
    );
    // Count remains one. Only declared bytes exceed the configured exact cap.
    send_snapshot(&mut peer, 2, &owned(&["/user/longer"]), segmented).await;
    observe(&mut peer, &mut None).await;
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty),
        "snapshot byte growth must retain old view and the same live connection"
    );
    let mut replay = gateway.subscribe(key()).await.unwrap();
    assert_eq!(
        replay.try_recv().unwrap(),
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://budget-peer/user/old".into()
        }
    );
    assert!(replay.is_empty());
    send_snapshot(&mut peer, 3, &owned(&["/user/new", "/user/new"]), segmented).await;
    for expected in [
        ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: "parrot://budget-peer/user/old".into(),
        },
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://budget-peer/user/new".into(),
        },
    ] {
        assert_eq!(
            tokio::time::timeout(LIMIT, events.recv()).await.unwrap(),
            Some(expected)
        );
    }
    send_snapshot(&mut peer, 4, &owned(&[]), segmented).await;
    assert!(matches!(
        tokio::time::timeout(LIMIT, events.recv()).await.unwrap(),
        Some(ReceptionistEvent::Unregistered { .. })
    ));
    observe(&mut peer, &mut None).await;
    assert!(gateway.subscribe(key()).await.unwrap().is_empty());
    system.shutdown().await.unwrap();
}
