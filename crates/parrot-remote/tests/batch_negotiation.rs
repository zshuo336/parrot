use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use parrot_remote::{
    frame::{flags, frame_type, Frame, FrameError},
    handshake::{caps, HandshakeBody},
    transport::{run_connection, ConnParams, ConnSide, FrameCodec, FrameSender, RuntimeKnobs},
    LocalLookup, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};

// Assigned Wire capability, independent of the production constant under test.
const BATCH_CAP: u32 = 1 << 22;
type Inbound = mpsc::Sender<(Frame, FrameSender, String)>;

fn params(
    side: ConnSide,
    name: &str,
    batch: bool,
    inbound: Inbound,
    shutdown: &watch::Sender<bool>,
) -> ConnParams {
    ConnParams {
        side,
        local_addr: None,
        peer_addr: None,
        scheme: "mem",
        local_handshake: hello(name, batch),
        inbound,
        on_disconnect: Arc::new(|_| {}),
        shutdown: shutdown.subscribe(),
        knobs: Some(RuntimeKnobs {
            heartbeat_enabled: false,
            ..Default::default()
        }),
    }
}
fn hello(name: &str, batch: bool) -> HandshakeBody {
    HandshakeBody {
        node_id: name.into(),
        capabilities: caps::BIN | if batch { BATCH_CAP } else { 0 },
        ..Default::default()
    }
}
fn batch() -> Frame {
    Frame::batch(&[
        Frame::tell("/target", "key", Bytes::from_static(b"first")),
        Frame::tell("/target", "key", Bytes::from_static(b"second")),
    ])
    .unwrap()
}
fn unsupported(result: &Result<(), parrot_remote::error::RemoteError>) -> bool {
    matches!(result, Err(parrot_remote::error::RemoteError::Frame(FrameError::UnsupportedFlags { flags: value })) if *value & flags::BATCH != 0)
}

#[tokio::test]
async fn manual_senders_reject_batches_before_async_and_sync_admission() {
    for anonymous in [false, true] {
        let (tx, mut rx) = mpsc::channel(4);
        let sender = if anonymous {
            FrameSender::anon(tx)
        } else {
            FrameSender::new(tx, "manual-peer")
        };
        let asynchronous = sender.send(batch()).await;
        let synchronous = sender.send_frame_sync(batch());
        assert!(
            unsupported(&asynchronous),
            "manual sender has no negotiated batch capability"
        );
        assert!(unsupported(&synchronous));
        assert!(
            rx.try_recv().is_err(),
            "rejection must not consume queue capacity"
        );
        sender
            .send(Frame::tell("/target", "key", Bytes::from_static(b"single")))
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().payload, b"single"[..]);
        assert!(!sender.is_closed());
    }
}

#[tokio::test]
async fn both_handshake_sides_must_advertise_before_either_direction_sends_batches() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for (left, right) in [(false, false), (false, true), (true, false), (true, true)] {
            let (io_a, io_b) = tokio::io::duplex(4096);
            let (shutdown, _) = watch::channel(false);
            let (tx_a, mut rx_a) = mpsc::channel(8);
            let (tx_b, mut rx_b) = mpsc::channel(8);
            let (a, b) = tokio::join!(
                run_connection(io_a, params(ConnSide::Connect, "a", left, tx_a, &shutdown)),
                run_connection(io_b, params(ConnSide::Accept, "b", right, tx_b, &shutdown))
            );
            let (a, b) = (a.unwrap(), b.unwrap());
            let mut observations = Vec::new();
            for (sender, receiver) in [(&a.sender, &mut rx_b), (&b.sender, &mut rx_a)] {
                let asynchronous = sender.send(batch()).await;
                let synchronous = sender.send_frame_sync(batch());
                let expected_accepted = left && right;
                let rejected = unsupported(&asynchronous) && unsupported(&synchronous);
                for result in [&asynchronous, &synchronous] {
                    if result.is_ok() {
                        let (received, _, _) = receiver.recv().await.unwrap();
                        let members = received.iter_batch().unwrap();
                        assert_eq!(
                            members
                                .iter()
                                .map(|f| f.payload.clone())
                                .collect::<Vec<_>>(),
                            vec![Bytes::from_static(b"first"), Bytes::from_static(b"second")]
                        );
                    }
                }
                sender
                    .send(Frame::tell("/target", "key", Bytes::from_static(b"single")))
                    .await
                    .unwrap();
                let (single, _, _) = receiver.recv().await.unwrap();
                assert_eq!(single.payload, b"single"[..]);
                observations.push((
                    asynchronous.is_ok(),
                    synchronous.is_ok(),
                    rejected,
                    expected_accepted,
                ));
            }
            shutdown.send_replace(true);
            a.closed.await.unwrap();
            b.closed.await.unwrap();
            for (asynchronous, synchronous, rejected, expected) in observations {
                assert_eq!(
                    (asynchronous, synchronous),
                    (expected, expected),
                    "negotiation: left={left}, right={right}"
                );
                assert_eq!(rejected, !expected);
            }
        }
    })
    .await
    .unwrap();
}

async fn raw_unnegotiated(local_batch: bool, peer_batch: bool) {
    let (io, raw) = tokio::io::duplex(4096);
    let (shutdown, _) = watch::channel(false);
    let (tx, mut rx) = mpsc::channel(8);
    let server = tokio::spawn(run_connection(
        io,
        params(ConnSide::Accept, "receiver", local_batch, tx, &shutdown),
    ));
    let mut raw = tokio_util::codec::Framed::new(raw, FrameCodec);
    let mut hello_bytes = bytes::BytesMut::new();
    hello("raw-peer", peer_batch)
        .encode_tlv(&mut hello_bytes)
        .unwrap();
    raw.send(Frame::handshake(hello_bytes.freeze()))
        .await
        .unwrap();
    let ack = raw.next().await.unwrap().unwrap();
    assert_eq!(ack.header.frame_type, frame_type::HANDSHAKE_ACK);
    let connection = server.await.unwrap().unwrap();
    raw.send(batch()).await.unwrap();
    let terminal = tokio::time::timeout(Duration::from_secs(2), raw.next()).await;
    shutdown.send_replace(true);
    connection.closed.await.unwrap();
    let terminal = terminal
        .expect("unnegotiated batch must produce terminal protocol error")
        .unwrap()
        .unwrap();
    assert_eq!(terminal.header.frame_type, frame_type::ERROR);
    assert_eq!(
        &terminal.payload[..2],
        &(parrot_remote::ErrCode::ProtocolViolation as u16).to_le_bytes()
    );
    assert!(
        rx.try_recv().is_err(),
        "batch must not enter application dispatch"
    );
}

#[tokio::test]
async fn legacy_link_rejects_unsolicited_batch_before_application_dispatch() {
    raw_unnegotiated(false, false).await;
}

#[tokio::test]
async fn receiver_advertisement_alone_cannot_authorize_inbound_batch() {
    raw_unnegotiated(true, false).await;
}

#[tokio::test]
async fn sender_advertisement_alone_cannot_authorize_inbound_batch() {
    raw_unnegotiated(false, true).await;
}

struct NoActors;
#[async_trait::async_trait]
impl LocalLookup for NoActors {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}

#[tokio::test]
async fn native_systems_advertise_batch_support_on_the_actual_connection() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("batch-cap-a"), Arc::new(NoActors)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("batch-cap-b"), Arc::new(NoActors)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();
    let caps_a = a.links_snapshot().await[0].1.peer_capabilities();
    let caps_b = b.links_snapshot().await[0].1.peer_capabilities();
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    assert_ne!(
        caps_a & BATCH_CAP,
        0,
        "real native HELLO must advertise support"
    );
    assert_ne!(
        caps_b & BATCH_CAP,
        0,
        "real native ACK must advertise support"
    );
}
