use super::*;
use futures::{SinkExt, StreamExt};
use parrot_remote::{
    frame::{frame_type, Frame, FrameHeader, PROTOCOL_VERSION, SEQ_NONE},
    handshake::{caps, HandshakeBody},
    receptionist::{decode_sync, encode_sync, ReceptionistEntry, ReceptionistSync},
    transport::FrameCodec,
};
use tokio_util::codec::Framed;

// Independent wire DTOs ensure the new body remains usable across module/API boundaries.
#[derive(serde::Deserialize)]
struct View {
    path: Vec<String>,
    entries: Vec<ReceptionistEntry>,
}
#[derive(serde::Deserialize)]
struct Routed {
    from: String,
    views: Vec<View>,
}
struct Peer {
    wire: Framed<tokio::net::TcpStream, FrameCodec>,
    pending: Option<(u64, usize, Vec<u8>)>,
    segments: usize,
    routed: bool,
}
impl Peer {
    async fn connect(system: &Arc<RemoteActorSystem>, node: &str, routed: bool) -> Self {
        let stream = tokio::net::TcpStream::connect(system.local_addr().unwrap())
            .await
            .unwrap();
        let mut wire = Framed::new(stream, FrameCodec);
        let mut body = bytes::BytesMut::new();
        HandshakeBody {
            node_id: node.into(),
            capabilities: caps::BIN
                | caps::RECEPTIONIST_SYNC
                | if routed {
                    caps::RECEPTIONIST_ROUTES
                        | caps::RECEPTIONIST_NATIVE_PATHS
                        | caps::RECEPTIONIST_SEGMENTS
                } else {
                    0
                },
            max_frame_len: 512,
            ..Default::default()
        }
        .encode_tlv(&mut body)
        .unwrap();
        wire.send(Frame::handshake(body.freeze())).await.unwrap();
        let ack = tokio::time::timeout(LIMIT, wire.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(ack.header.frame_type, frame_type::HANDSHAKE_ACK);
        tokio::time::timeout(LIMIT, async {
            while !system.membership.lock().await.members.contains_key(node) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Self {
            wire,
            pending: None,
            segments: 0,
            routed,
        }
    }
    async fn publish(&mut self, sync: &ReceptionistSync) {
        self.wire
            .send(Frame {
                header: FrameHeader {
                    frame_len: 0,
                    version: PROTOCOL_VERSION,
                    frame_type: frame_type::SYSTEM_EVENT,
                    flags: 0,
                    correlation_id: 0,
                    hop_count: 0,
                    hop_limit: 1,
                    seq: SEQ_NONE,
                },
                path: String::new(),
                type_key: String::new(),
                payload: encode_sync(sync),
            })
            .await
            .unwrap();
    }
    async fn next_snapshot(&mut self) -> bytes::Bytes {
        tokio::time::timeout(LIMIT, async {
            loop {
                let frame = self.wire.next().await.expect("peer closed").unwrap();
                assert!(frame.header.frame_len <= 512);
                if frame.header.frame_type == frame_type::HEARTBEAT {
                    self.wire.send(Frame::heartbeat_ack()).await.unwrap();
                    continue;
                }
                if frame.header.frame_type != frame_type::SYSTEM_EVENT {
                    continue;
                }
                let payload = frame.payload;
                let tag = payload.first().copied().unwrap();
                if !self.routed {
                    assert_eq!(
                        tag, 0x20,
                        "a bit-17-only peer must never receive routed or segmented snapshots"
                    );
                    return payload.slice(1..);
                }
                if tag == 0x22 {
                    return payload.slice(1..);
                }
                assert_eq!(tag, 0x23);
                assert!(payload.len() > 17);
                let id = u64::from_le_bytes(payload[1..9].try_into().unwrap());
                let total = u32::from_le_bytes(payload[9..13].try_into().unwrap()) as usize;
                let offset = u32::from_le_bytes(payload[13..17].try_into().unwrap()) as usize;
                if offset == 0 {
                    assert!(
                        self.pending.is_none(),
                        "unfinished snapshot must keep its identity"
                    );
                    self.pending = Some((id, total, Vec::new()));
                }
                let pending = self
                    .pending
                    .as_mut()
                    .expect("first segment must arrive first");
                assert_eq!((pending.0, pending.1, pending.2.len()), (id, total, offset));
                pending.2.extend_from_slice(&payload[17..]);
                assert!(pending.2.len() <= total);
                self.segments += 1;
                if pending.2.len() == total {
                    return self.pending.take().unwrap().2.into();
                }
            }
        })
        .await
        .expect("complete snapshot deadline")
    }
}

#[tokio::test]
async fn small_frame_relay_finishes_segmented_snapshot_and_withdrawal_with_queue_two() {
    let relay = node_with("sparse-small-b", AclRules::permissive(), 2).await;
    let owner = node("sparse-small-c").await;
    let mut learned = relay.receptionist_gateway().subscribe(key()).await.unwrap();
    for i in 0..64 {
        owner
            .receptionist_gateway()
            .register(key(), format!("/user/bulk/{i:04}"))
            .await
            .unwrap();
    }
    connect(&relay, &owner).await;
    tokio::time::timeout(LIMIT, async {
        for _ in 0..64 {
            assert!(matches!(
                learned.recv().await,
                Some(ReceptionistEvent::Registered { .. })
            ));
        }
    })
    .await
    .unwrap();
    let mut peer = Peer::connect(&relay, "sparse-small-observer", true).await;
    let sync: Routed =
        parrot_api::message::serde_remote_deserialize(&peer.next_snapshot().await).unwrap();
    assert_eq!(sync.from, relay.config.node_id);
    let selected: Vec<_> = sync
        .views
        .iter()
        .filter(|view| view.path.first() == Some(&owner.config.node_id))
        .collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].path,
        [owner.config.node_id.clone(), relay.config.node_id.clone()]
    );
    assert_eq!(selected[0].entries.len(), 64);
    assert!(
        peer.segments > 2,
        "payload must actually exceed both frame and queue capacity"
    );
    for i in 0..64 {
        owner
            .receptionist_gateway()
            .deregister(key(), format!("/user/bulk/{i:04}"))
            .await
            .unwrap();
    }
    tokio::time::timeout(LIMIT, async {
        loop {
            let sync: Routed =
                parrot_api::message::serde_remote_deserialize(&peer.next_snapshot().await).unwrap();
            if !sync.views.iter().any(|view| {
                view.entries
                    .iter()
                    .any(|entry| entry.node == owner.config.node_id)
            }) {
                break;
            }
        }
    })
    .await
    .expect("routed withdrawal must follow an in-flight complete snapshot");
    drop(peer);
    owner.shutdown().await.unwrap();
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn bit17_only_peer_publishes_through_new_relay_and_receives_only_legacy_owner_view() {
    let observer = node("sparse-mixed-a").await;
    let relay = node("sparse-mixed-b").await;
    connect(&observer, &relay).await;
    let old = "sparse-mixed-old";
    let mut events = observer
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    let mut peer = Peer::connect(&relay, old, false).await;
    let path = format!("parrot://{old}/user/echo");
    peer.publish(&ReceptionistSync {
        from: old.into(),
        entries: vec![ReceptionistEntry {
            node: old.into(),
            key: key(),
            remote_path: path.clone(),
        }],
    })
    .await;
    registered(&mut events, old, key()).await;
    let marker = service("edge/mixed-marker");
    relay
        .receptionist_gateway()
        .register(marker.clone(), "/user/echo".into())
        .await
        .unwrap();
    tokio::time::timeout(LIMIT, async {
        loop {
            let sync = decode_sync(&peer.next_snapshot().await).unwrap();
            assert_eq!(sync.from, relay.config.node_id);
            assert!(sync
                .entries
                .iter()
                .all(|entry| entry.node == relay.config.node_id));
            if sync.entries.iter().any(|entry| entry.key == marker) {
                break;
            }
        }
    })
    .await
    .unwrap();
    echo(&observer, &relay.config.node_id, 720).await;
    peer.publish(&ReceptionistSync {
        from: old.into(),
        entries: Vec::new(),
    })
    .await;
    assert_eq!(
        tokio::time::timeout(LIMIT, events.recv()).await.unwrap(),
        Some(ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: path
        })
    );
    drop(peer);
    relay.shutdown().await.unwrap();
    observer.shutdown().await.unwrap();
}
