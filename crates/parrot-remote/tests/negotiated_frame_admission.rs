//! A rejected local frame must not tear down unrelated work on a shared link.
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use parrot_remote::{
    error::{decode_err_payload, ErrCode, RemoteError},
    frame::{Frame, FrameError},
    handshake::{HandshakeAckBody, HandshakeBody},
    ingress::{Ingress, LocalLookup, RelayRouter},
    registry::CallbackRegistry,
    transport::{run_connection, ConnParams, ConnSide, FrameCodec, FrameSender, RuntimeKnobs},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio_util::codec::Framed;

struct Relay(FrameSender);

#[tokio::test]
async fn minimum_negotiated_limit_applies_after_both_handshake_frames() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for (connect_limit, accept_limit) in [(28, 256), (256, 28)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let connect = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (accept, _) = listener.accept().await.unwrap();
            let (shutdown, _) = tokio::sync::watch::channel(false);
            let (connect_tx, mut connect_rx) = tokio::sync::mpsc::channel(8);
            let (accept_tx, mut accept_rx) = tokio::sync::mpsc::channel(8);
            let params = |side, name: &str, max_frame_len, inbound| ConnParams {
                side,
                local_addr: None,
                peer_addr: None,
                scheme: "tcp",
                local_handshake: HandshakeBody {
                    node_id: name.into(),
                    max_frame_len,
                    ..Default::default()
                },
                inbound,
                on_disconnect: Arc::new(|_| {}),
                shutdown: shutdown.subscribe(),
                knobs: Some(RuntimeKnobs {
                    heartbeat_enabled: false,
                    ..Default::default()
                }),
            };
            let (connecting, accepting) = tokio::join!(
                run_connection(
                    connect,
                    params(ConnSide::Connect, "connect", connect_limit, connect_tx)
                ),
                run_connection(
                    accept,
                    params(ConnSide::Accept, "accept", accept_limit, accept_tx)
                ),
            );
            let connecting = connecting.unwrap();
            let accepting = accepting.unwrap();
            for (sender, receiver) in [
                (&connecting.sender, &mut accept_rx),
                (&accepting.sender, &mut connect_rx),
            ] {
                let oversized = Frame::tell("", "", Bytes::from_static(b"x"));
                assert!(matches!(
                    sender.send(oversized).await,
                    Err(RemoteError::Frame(FrameError::TooLarge {
                        len: 29,
                        max: 28
                    }))
                ));
                sender
                    .send(Frame::tell("", "", Bytes::new()))
                    .await
                    .unwrap();
                let (frame, _, _) = receiver.recv().await.unwrap();
                assert!(frame.payload.is_empty());
                assert!(!sender.is_closed());
            }
            shutdown.send_replace(true);
            connecting.closed.await.unwrap();
            accepting.closed.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[async_trait::async_trait]
impl LocalLookup for Relay {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        panic!("remote request must not enter local dispatch")
    }
}
#[async_trait::async_trait]
impl RelayRouter for Relay {
    fn sender_of(&self, node: &str) -> Option<FrameSender> {
        (node == "peer").then(|| self.0.clone())
    }
    fn self_node(&self) -> &str {
        "local"
    }
    fn next_cid(&self) -> u64 {
        100
    }
    fn dial_addr_of(&self, _: &str) -> Option<String> {
        None
    }
    fn on_hint(&self, _: &str, _: &str) {}
}

async fn check_admission(side: ConnSide, local_limit: u32, peer_limit: u32, synchronous: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (remote, _) = listener.accept().await.unwrap();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let (inbound, mut received_inbound) = tokio::sync::mpsc::channel(8);
    let disconnected = Arc::new(AtomicUsize::new(0));
    let observed_disconnect = disconnected.clone();
    let params = ConnParams {
        side,
        local_addr: None,
        peer_addr: None,
        scheme: "tcp",
        local_handshake: HandshakeBody {
            node_id: "local".into(),
            max_frame_len: local_limit,
            ..Default::default()
        },
        inbound,
        on_disconnect: Arc::new(move |_| {
            observed_disconnect.fetch_add(1, Ordering::SeqCst);
        }),
        shutdown: shutdown.subscribe(),
        knobs: Some(RuntimeKnobs {
            heartbeat_enabled: false,
            ..Default::default()
        }),
    };
    let exchange = async {
        // Independent peer codec intentionally permits the global wire limit.
        let mut peer = Framed::new(remote, FrameCodec);
        let hello = HandshakeBody {
            node_id: "peer".into(),
            max_frame_len: peer_limit,
            ..Default::default()
        };
        let mut body = BytesMut::new();
        match side {
            ConnSide::Connect => {
                peer.next().await.unwrap().unwrap();
                let ack = HandshakeAckBody {
                    node_id: hello.node_id,
                    realm: None,
                    cluster: None,
                    capabilities: hello.capabilities,
                    max_frame_len: peer_limit,
                    topology_role: hello.topology_role,
                    hop_limit: hello.hop_limit,
                    direct_addr: None,
                    chosen_codec: "bin".into(),
                };
                ack.encode_tlv(&mut body).unwrap();
                peer.send(Frame::handshake_ack(body.freeze()))
                    .await
                    .unwrap();
            }
            ConnSide::Accept => {
                hello.encode_tlv(&mut body).unwrap();
                peer.send(Frame::handshake(body.freeze())).await.unwrap();
                peer.next().await.unwrap().unwrap();
            }
        }
        peer
    };
    let (connection, mut peer) = tokio::join!(run_connection(local, params), exchange);
    let connection = connection.unwrap();
    let limit = local_limit.min(peer_limit);
    // Non-ASCII identities force byte accounting, including both length fields.
    let exact = Frame::tell(
        "/甲",
        "键",
        Bytes::from(vec![7; limit as usize - 28 - 4 - 3]),
    );
    let mut oversized = exact.clone();
    oversized.payload = Bytes::from(vec![7; exact.payload.len() + 1]);
    let result = if synchronous {
        connection.sender.send_frame_sync(oversized.clone())
    } else {
        connection.sender.send(oversized.clone()).await
    };
    let rejected = matches!(result, Err(RemoteError::Frame(FrameError::TooLarge { len, max })) if len == limit + 1 && max == limit);
    // Always retire the actual TCP owner before reporting a failed assertion.
    if !rejected {
        shutdown.send_replace(true);
        connection.closed.await.unwrap();
        panic!("oversized frame must fail admission with its negotiated bound: {result:?}");
    }
    connection.sender.send(exact.clone()).await.unwrap();
    let received = peer.next().await.unwrap().unwrap();
    assert_eq!(received.path, exact.path);
    assert_eq!(received.type_key, exact.type_key);
    assert_eq!(received.payload, exact.payload);
    assert!(!connection.sender.is_closed());

    // The hub reservation path must reject before creating a correlation entry,
    // return the source CID, and keep this same next-hop connection usable.
    let relay = Arc::new(Relay(connection.sender.clone()));
    let ingress = Arc::new(Ingress::new(
        relay.clone(),
        Arc::new(CallbackRegistry::new(8)),
    ));
    *ingress.relay_slot() = Some(relay);
    let (reply_tx, mut replies) = tokio::sync::mpsc::channel(8);
    let back = FrameSender::new(reply_tx, "source");
    let request = Frame::ask(
        9,
        "parrot://peer/a",
        "opaque",
        Bytes::from(vec![1; limit as usize]),
        None,
    );
    ingress.dispatch(request, &back, "source").await;
    let rejection = replies.recv().await.unwrap();
    assert_eq!(rejection.header.correlation_id, 9);
    assert_eq!(
        decode_err_payload(&rejection.payload).unwrap().0,
        ErrCode::ProtocolViolation
    );
    assert!(ingress.relay_table.is_empty());
    let request = Frame::ask(
        10,
        "parrot://peer/a",
        "opaque",
        Bytes::from_static(b"ok"),
        None,
    );
    ingress.dispatch(request, &back, "source").await;
    let forwarded = peer.next().await.unwrap().unwrap();
    assert_eq!(forwarded.header.correlation_id, 100);
    assert_eq!(forwarded.header.hop_count, 1);
    ingress
        .dispatch(
            Frame::reply(100, "", "opaque", Bytes::from_static(b"reply")),
            &connection.sender,
            "peer",
        )
        .await;
    let reply = replies.recv().await.unwrap();
    assert_eq!(reply.header.correlation_id, 10);
    assert_eq!(reply.payload.as_ref(), b"reply");
    assert!(ingress.relay_table.is_empty());
    ingress.close();
    ingress.wait_closed().await;

    // A malicious *remote* oversize frame still closes the wire connection;
    // rejecting a local input must not weaken the inbound codec.
    peer.send(oversized).await.unwrap();
    connection.closed.await.unwrap();
    assert!(connection.sender.is_closed());
    assert_eq!(disconnected.load(Ordering::SeqCst), 1);
    assert!(received_inbound.try_recv().is_err());
}

#[tokio::test]
async fn oversize_local_frame_is_rejected_without_poisoning_tcp_connection() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for side in [ConnSide::Connect, ConnSide::Accept] {
            for (local, peer) in [(256, 512), (512, 256)] {
                for synchronous in [false, true] {
                    check_admission(side, local, peer, synchronous).await;
                }
            }
        }
    })
    .await
    .expect("TCP admission/retirement deadline");
}
