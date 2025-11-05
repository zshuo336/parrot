//! Real TCP, codec, transport owner, dispatcher and connection-bound callbacks.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use crate::{
    frame::{frame_type, Frame, FrameError},
    handshake::{HandshakeAckBody, HandshakeBody},
    ingress::{make_on_disconnect, Ingress, LocalLookup},
    node::{NodeState, NodeStatus},
    registry::{CallbackRegistry, ReplyPayload},
    transport::{
        run_connection_owned, ConnParams, ConnSide, ConnectionHandle, ConnectionTasks, FrameCodec,
        FrameSender, RuntimeKnobs,
    },
    ErrCode,
};

type Input = (Frame, FrameSender, String);

struct EmptyLookup;
#[async_trait::async_trait]
impl LocalLookup for EmptyLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        panic!("post-terminal business frame reached lookup")
    }
}

struct Harness {
    peer: Framed<TcpStream, FrameCodec>,
    connection: ConnectionHandle,
    owner: Arc<ConnectionTasks>,
    incoming: Option<mpsc::Sender<Input>>,
    receiver: Option<mpsc::Receiver<Input>>,
    dispatch: Option<JoinHandle<()>>,
    shutdown: watch::Sender<bool>,
    disconnects: Arc<AtomicUsize>,
    status: Arc<NodeStatus>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.owner.close();
        if let Some(task) = &self.dispatch {
            task.abort();
        }
    }
}

impl Harness {
    async fn start(side: ConnSide, capacity: usize, callbacks: Arc<CallbackRegistry>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (peer, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let mut peer = Framed::new(peer.unwrap(), FrameCodec);
        let (local, _) = accepted.unwrap();
        let owner = Arc::new(ConnectionTasks::default());
        let (incoming, receiver) = mpsc::channel(capacity);
        let (shutdown, _) = watch::channel(false);
        let disconnects = Arc::new(AtomicUsize::new(0));
        let status = Arc::new(NodeStatus::default());
        status.set(NodeState::Connected);
        let hook = make_on_disconnect(callbacks, vec![status.clone()]);
        let count = disconnects.clone();
        let params = ConnParams {
            side,
            local_addr: Some(addr),
            peer_addr: None,
            scheme: "tcp",
            local_handshake: HandshakeBody {
                node_id: "terminal-local".into(),
                ..Default::default()
            },
            inbound: incoming.clone(),
            on_disconnect: Arc::new(move |sender| {
                hook(sender);
                count.fetch_add(1, Ordering::SeqCst);
            }),
            shutdown: shutdown.subscribe(),
            knobs: Some(RuntimeKnobs {
                heartbeat_enabled: false,
                ..Default::default()
            }),
        };
        let handshake = async {
            let body = HandshakeBody {
                node_id: "terminal-peer".into(),
                ..Default::default()
            };
            let mut payload = BytesMut::new();
            match side {
                ConnSide::Accept => {
                    body.encode_tlv(&mut payload).unwrap();
                    peer.send(Frame::handshake(payload.freeze())).await.unwrap();
                    let ack = peer.next().await.unwrap().unwrap();
                    assert_eq!(ack.header.frame_type, frame_type::HANDSHAKE_ACK);
                }
                ConnSide::Connect => {
                    let hello = peer.next().await.unwrap().unwrap();
                    assert_eq!(hello.header.frame_type, frame_type::HANDSHAKE);
                    HandshakeAckBody {
                        node_id: body.node_id,
                        realm: body.realm,
                        cluster: body.cluster,
                        capabilities: body.capabilities,
                        max_frame_len: body.max_frame_len,
                        topology_role: body.topology_role,
                        hop_limit: body.hop_limit,
                        direct_addr: body.direct_addr,
                        chosen_codec: "bin".into(),
                    }
                    .encode_tlv(&mut payload)
                    .unwrap();
                    peer.send(Frame::handshake_ack(payload.freeze()))
                        .await
                        .unwrap();
                }
            }
        };
        let (connection, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(run_connection_owned(local, params, Some(&owner)), handshake)
        })
        .await
        .expect("TCP handshake did not finish");
        Self {
            peer,
            connection: connection.unwrap(),
            owner,
            incoming: Some(incoming),
            receiver: Some(receiver),
            dispatch: None,
            shutdown,
            disconnects,
            status,
        }
    }

    fn start_dispatch(&mut self, callbacks: Arc<CallbackRegistry>) {
        let ingress = Arc::new(Ingress::new(Arc::new(EmptyLookup), callbacks));
        let mut receiver = self.receiver.take().unwrap();
        self.dispatch = Some(tokio::spawn(async move {
            super::run(ingress, &mut receiver, RuntimeKnobs::default()).await;
        }));
    }

    async fn send(&mut self, frames: &[Frame]) -> u64 {
        let mut bytes = BytesMut::new();
        for frame in frames {
            frame.encode(&mut bytes).unwrap();
        }
        self.peer.get_mut().write_all(&bytes).await.unwrap();
        bytes.len() as u64
    }

    async fn observed(&self, minimum: u64) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.owner.metrics.snapshot().bytes_rx < minimum {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("transport did not observe test frame");
    }

    async fn retired(&mut self) {
        tokio::time::timeout(Duration::from_secs(2), &mut self.connection.closed)
            .await
            .expect("terminal ERROR did not retire driver")
            .expect("driver dropped its close receipt");
        assert!(
            self.connection.sender.is_closed(),
            "terminal sender remained open"
        );
        assert_eq!(
            self.disconnects.load(Ordering::SeqCst),
            1,
            "disconnect hook must run exactly once"
        );
        assert_eq!(self.status.get(), NodeState::Disconnected);
        let next = tokio::time::timeout(Duration::from_secs(2), self.peer.next())
            .await
            .expect("peer never observed retirement");
        assert!(
            matches!(next, None | Some(Err(FrameError::Io { .. }))),
            "terminal ERROR must not receive an ERROR response or later application traffic"
        );
    }

    async fn finish(mut self) {
        self.connection.sender.close();
        self.owner.close();
        tokio::time::timeout(Duration::from_secs(2), self.owner.wait())
            .await
            .unwrap();
        self.incoming.take();
        if let Some(task) = self.dispatch.take() {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            self.owner.metrics.snapshot().connections,
            0,
            "connection metric lease leaked"
        );
        assert_eq!(self.disconnects.load(Ordering::SeqCst), 1);
    }
}

fn pending(
    callbacks: &CallbackRegistry,
    cid: u64,
    connection: &FrameSender,
) -> oneshot::Receiver<ReplyPayload> {
    let (tx, rx) = oneshot::channel();
    callbacks
        .insert_on(cid, "terminal-peer", tx, connection)
        .unwrap();
    rx
}

async fn completion(rx: oneshot::Receiver<ReplyPayload>) -> ReplyPayload {
    tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("pending callback did not terminate")
        .expect("callback sender vanished")
}

fn lost(value: ReplyPayload) {
    assert!(
        matches!(value, ReplyPayload::Err(ErrCode::ConnectionLost, _)),
        "terminal ERROR must fail pending work with local connection loss"
    );
}

#[tokio::test]
async fn terminal_error_payload_vectors_retire_pending_connection() {
    let vectors = include_str!("../../../../../docs/vectors/error-payloads.tsv");
    let rows: Vec<_> = vectors
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(rows.len(), 30);
    for side in [ConnSide::Accept, ConnSide::Connect] {
        for row in &rows {
            let columns: Vec<_> = row.split('\t').collect();
            assert_eq!(columns.len(), 5, "invalid ERROR payload vector: {row}");
            let (pairs, remainder) = columns[2].as_bytes().as_chunks::<2>();
            assert!(remainder.is_empty(), "odd-length ERROR payload: {row}");
            let payload: Vec<u8> = pairs
                .iter()
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect();
            let callbacks = Arc::new(CallbackRegistry::default());
            let mut harness = Harness::start(side, 4, callbacks.clone()).await;
            let pending = pending(&callbacks, 1, &harness.connection.sender);
            harness.start_dispatch(callbacks.clone());
            let mut error = Frame::error_frame(ErrCode::Overloaded, "");
            error.payload = payload.into();
            harness
                .send(&[
                    error,
                    Frame::reply(1, "", "bin:test", Bytes::from_static(b"late")),
                ])
                .await;
            lost(completion(pending).await);
            harness.retired().await;
            assert!(callbacks.is_empty());
            harness.finish().await;
        }
    }
}

#[tokio::test]
async fn terminal_error_flags_close_without_error_response_loop() {
    for side in [ConnSide::Accept, ConnSide::Connect] {
        for flags in (0..16).map(|bit| 1u16 << bit).chain([u16::MAX]) {
            let callbacks = Arc::new(CallbackRegistry::default());
            let mut harness = Harness::start(side, 4, callbacks.clone()).await;
            harness.start_dispatch(callbacks);
            let mut error = Frame::error_frame(ErrCode::ProtocolViolation, "flags");
            error.header.flags = flags;
            harness.send(&[error]).await;
            harness.retired().await;
            harness.finish().await;
        }
    }
}

#[tokio::test]
async fn terminal_error_preserves_prior_reply_under_full_queue() {
    for side in [ConnSide::Accept, ConnSide::Connect] {
        for first in [
            Frame::reply(1, "", "bin:test", Bytes::from_static(b"before")),
            Frame::reply(1, "", "bin:test", Bytes::new()),
            Frame::reply_err(1, "", ErrCode::Stopped, "business"),
        ] {
            let callbacks = Arc::new(CallbackRegistry::default());
            let mut harness = Harness::start(side, 1, callbacks.clone()).await;
            let sibling = Harness::start(ConnSide::Accept, 4, callbacks.clone()).await;
            let before = pending(&callbacks, 1, &harness.connection.sender);
            let after = pending(&callbacks, 2, &harness.connection.sender);
            let mut other = pending(&callbacks, 3, &sibling.connection.sender);
            let initial = harness.owner.metrics.snapshot().bytes_rx;
            let mut encoded = BytesMut::new();
            first.encode(&mut encoded).unwrap();
            Frame::error_frame(ErrCode::ProtocolViolation, "stop")
                .encode(&mut encoded)
                .unwrap();
            let observed = initial + encoded.len() as u64;
            Frame::reply(2, "", "bin:test", Bytes::from_static(b"after"))
                .encode(&mut encoded)
                .unwrap();
            harness.peer.get_mut().write_all(&encoded).await.unwrap();
            harness.observed(observed).await;
            assert_eq!(
                harness.incoming.as_ref().unwrap().capacity(),
                0,
                "prior reply did not fill queue"
            );
            assert!(
                !harness.connection.sender.is_closed(),
                "transport discarded queued reply before dispatch"
            );
            harness.start_dispatch(callbacks.clone());
            match (first.header.frame_type, completion(before).await) {
                (frame_type::REPLY, ReplyPayload::Ok(payload, key)) => {
                    assert_eq!(payload, first.payload);
                    assert_eq!(key, first.type_key);
                }
                (frame_type::REPLY_ERR, ReplyPayload::Err(code, detail)) => {
                    assert_eq!(code, ErrCode::Stopped);
                    assert_eq!(detail, "business");
                }
                _ => panic!("terminal ERROR overwrote the preceding completed reply"),
            }
            lost(completion(after).await);
            harness.retired().await;
            assert_eq!(
                harness.owner.metrics.snapshot().bytes_rx,
                observed,
                "transport continued reading frames after terminal ERROR"
            );
            assert!(matches!(
                other.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(
                !sibling.connection.sender.is_closed(),
                "unrelated connection was retired"
            );
            assert_eq!(
                callbacks.len(),
                1,
                "disconnect cleaned an unrelated callback"
            );
            harness.finish().await;
            sibling.finish().await;
            lost(completion(other).await);
            assert!(callbacks.is_empty());
        }
    }
}

#[tokio::test]
async fn terminal_error_before_reply_cannot_be_overridden() {
    for side in [ConnSide::Accept, ConnSide::Connect] {
        let callbacks = Arc::new(CallbackRegistry::default());
        let mut harness = Harness::start(side, 4, callbacks.clone()).await;
        let result = pending(&callbacks, 9, &harness.connection.sender);
        harness.start_dispatch(callbacks);
        harness
            .send(&[
                Frame::error_frame(ErrCode::Overloaded, "terminal"),
                Frame::reply(9, "", "bin:test", Bytes::from_static(b"must not win")),
            ])
            .await;
        lost(completion(result).await);
        harness.retired().await;
        harness.finish().await;
    }
}

#[tokio::test]
async fn terminal_error_dispatch_stall_has_one_bounded_deadline() {
    for full in [false, true] {
        let callbacks = Arc::new(CallbackRegistry::default());
        let mut harness = Harness::start(ConnSide::Accept, 1, callbacks.clone()).await;
        if full {
            harness
                .incoming
                .as_ref()
                .unwrap()
                .try_send((
                    Frame::heartbeat(),
                    harness.connection.sender.clone(),
                    "terminal-peer".into(),
                ))
                .unwrap();
        }
        let result = pending(&callbacks, 1, &harness.connection.sender);
        let initial = harness.owner.metrics.snapshot().bytes_rx;
        let started = tokio::time::Instant::now();
        let sent = harness
            .send(&[Frame::error_frame(ErrCode::ProtocolViolation, "stalled")])
            .await;
        harness.observed(initial + sent).await;
        tokio::time::timeout(Duration::from_secs(7), &mut harness.connection.closed)
            .await
            .expect("stalled terminal dispatch retained connection indefinitely")
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(5),
            "terminal drain deadline fired early"
        );
        lost(completion(result).await);
        assert!(harness.connection.sender.is_closed());
        harness.finish().await;
    }
}

#[tokio::test]
async fn terminal_error_queue_progress_does_not_renew_deadline() {
    let callbacks = Arc::new(CallbackRegistry::default());
    let mut harness = Harness::start(ConnSide::Accept, 1, callbacks.clone()).await;
    harness
        .incoming
        .as_ref()
        .unwrap()
        .try_send((
            Frame::heartbeat(),
            harness.connection.sender.clone(),
            "terminal-peer".into(),
        ))
        .unwrap();
    let result = pending(&callbacks, 1, &harness.connection.sender);
    let initial = harness.owner.metrics.snapshot().bytes_rx;
    let sent = harness
        .send(&[Frame::error_frame(ErrCode::ProtocolViolation, "progress")])
        .await;
    harness.observed(initial + sent).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let prior = harness.receiver.as_mut().unwrap().recv().await.unwrap();
    assert_eq!(prior.0.header.frame_type, frame_type::HEARTBEAT);
    tokio::time::timeout(Duration::from_secs(3), &mut harness.connection.closed)
        .await
        .expect("terminal queue progress renewed the original deadline")
        .unwrap();
    lost(completion(result).await);
    harness.finish().await;
}

#[tokio::test]
async fn terminal_error_shutdown_interrupts_queued_drain() {
    let callbacks = Arc::new(CallbackRegistry::default());
    let mut harness = Harness::start(ConnSide::Accept, 1, callbacks.clone()).await;
    let result = pending(&callbacks, 1, &harness.connection.sender);
    let initial = harness.owner.metrics.snapshot().bytes_rx;
    let sent = harness
        .send(&[Frame::error_frame(ErrCode::ProtocolViolation, "shutdown")])
        .await;
    harness.observed(initial + sent).await;
    harness.shutdown.send(true).unwrap();
    harness.retired().await;
    lost(completion(result).await);
    harness.finish().await;
}

#[tokio::test]
async fn terminal_error_owner_cancellation_interrupts_queued_drain() {
    for side in [ConnSide::Accept, ConnSide::Connect] {
        let callbacks = Arc::new(CallbackRegistry::default());
        let mut harness = Harness::start(side, 1, callbacks.clone()).await;
        harness
            .incoming
            .as_ref()
            .unwrap()
            .try_send((
                Frame::heartbeat(),
                harness.connection.sender.clone(),
                "terminal-peer".into(),
            ))
            .unwrap();
        let result = pending(&callbacks, 1, &harness.connection.sender);
        let initial = harness.owner.metrics.snapshot().bytes_rx;
        let sent = harness
            .send(&[Frame::error_frame(
                ErrCode::ProtocolViolation,
                "owner cancelled",
            )])
            .await;
        harness.observed(initial + sent).await;
        assert_eq!(harness.incoming.as_ref().unwrap().capacity(), 0);
        harness.owner.close();
        harness.retired().await;
        lost(completion(result).await);
        harness.finish().await;
        assert!(callbacks.is_empty());
    }
}

#[tokio::test]
async fn terminal_error_with_dropped_dispatcher_retires() {
    let callbacks = Arc::new(CallbackRegistry::default());
    let mut harness = Harness::start(ConnSide::Accept, 1, callbacks.clone()).await;
    let result = pending(&callbacks, 1, &harness.connection.sender);
    harness.receiver.take();
    harness
        .send(&[Frame::error_frame(
            ErrCode::ProtocolViolation,
            "dispatcher dropped",
        )])
        .await;
    harness.retired().await;
    lost(completion(result).await);
    harness.finish().await;
}

#[tokio::test]
async fn terminal_error_business_reply_error_is_not_terminal() {
    for side in [ConnSide::Accept, ConnSide::Connect] {
        let callbacks = Arc::new(CallbackRegistry::default());
        let mut harness = Harness::start(side, 4, callbacks.clone()).await;
        let first = pending(&callbacks, 1, &harness.connection.sender);
        let second = pending(&callbacks, 2, &harness.connection.sender);
        harness.start_dispatch(callbacks.clone());
        harness
            .send(&[Frame::reply_err(1, "", ErrCode::Stopped, "business")])
            .await;
        assert!(matches!(
            completion(first).await,
            ReplyPayload::Err(ErrCode::Stopped, _)
        ));
        assert!(
            !harness.connection.sender.is_closed(),
            "REPLY_ERR incorrectly retired connection"
        );
        harness
            .send(&[
                Frame::reply(2, "", "bin:test", Bytes::from_static(b"recovered")),
                Frame::heartbeat(),
            ])
            .await;
        assert!(
            matches!(completion(second).await, ReplyPayload::Ok(payload, _) if payload == b"recovered"[..])
        );
        let ack = tokio::time::timeout(Duration::from_secs(2), harness.peer.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(ack.header.frame_type, frame_type::HEARTBEAT_ACK);
        assert!(callbacks.is_empty());
        harness.finish().await;
    }
}
