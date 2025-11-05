//! Read, write and liveness progress must not share an awaited IO operation.
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use parrot_remote::{
    frame::Frame,
    handshake::HandshakeBody,
    transport::{
        run_connection, ConnParams, ConnSide, ConnectionHandle, FrameCodec, FrameSender,
        RuntimeKnobs,
    },
};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::{mpsc, Notify},
};

type Inbound = (Frame, FrameSender, String);
struct IoState {
    blocked: AtomicBool,
    dropped: AtomicBool,
    observe_read: AtomicBool,
    write_seen: Notify,
    read_seen: Notify,
    write_waker: futures::task::AtomicWaker,
}
struct ControlledIo {
    io: DuplexStream,
    state: Arc<IoState>,
}
impl Drop for ControlledIo {
    fn drop(&mut self) {
        self.state.dropped.store(true, Ordering::SeqCst);
    }
}
impl AsyncRead for ControlledIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.io).poll_read(cx, buf);
        if self.state.observe_read.load(Ordering::SeqCst) && buf.filled().len() > before {
            self.state.read_seen.notify_one();
        }
        result
    }
}
impl AsyncWrite for ControlledIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.state.write_waker.register(cx.waker());
        if self.state.blocked.load(Ordering::SeqCst) {
            self.state.write_seen.notify_one();
            return Poll::Pending;
        }
        Pin::new(&mut self.io).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
struct Fixture {
    handle: ConnectionHandle,
    peer: tokio_util::codec::Framed<DuplexStream, FrameCodec>,
    inbound: mpsc::Receiver<Inbound>,
    state: Arc<IoState>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.sender.close();
    }
}
impl Fixture {
    async fn new(heartbeat: bool, full_inbound: bool) -> Self {
        let (io, peer) = tokio::io::duplex(4096);
        let state = Arc::new(IoState {
            blocked: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            observe_read: AtomicBool::new(false),
            write_seen: Notify::new(),
            read_seen: Notify::new(),
            write_waker: futures::task::AtomicWaker::new(),
        });
        let io = ControlledIo {
            io,
            state: state.clone(),
        };
        let (inbound, receiver) = mpsc::channel(1);
        if full_inbound {
            inbound
                .send((
                    Frame::heartbeat(),
                    FrameSender::detached(),
                    "prefilled".into(),
                ))
                .await
                .unwrap();
        }
        let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let peer_handshake = async {
            let mut peer = tokio_util::codec::Framed::new(peer, FrameCodec);
            let mut payload = BytesMut::new();
            HandshakeBody {
                node_id: "progress-peer".into(),
                ..Default::default()
            }
            .encode_tlv(&mut payload)
            .unwrap();
            peer.send(Frame::handshake(payload.freeze())).await.unwrap();
            let ack = peer.next().await.unwrap().unwrap();
            assert_eq!(
                ack.header.frame_type,
                parrot_remote::frame::frame_type::HANDSHAKE_ACK
            );
            peer
        };
        let params = ConnParams {
            side: ConnSide::Accept,
            local_addr: None,
            peer_addr: None,
            scheme: "mem",
            local_handshake: HandshakeBody {
                node_id: "progress-local".into(),
                ..Default::default()
            },
            inbound,
            on_disconnect: Arc::new(|_| {}),
            shutdown: shutdown_rx,
            knobs: Some(RuntimeKnobs {
                heartbeat_enabled: heartbeat,
                heartbeat_interval: Duration::from_secs(10),
                heartbeat_max_loss: 2,
                ..Default::default()
            }),
        };
        let (handle, peer) = tokio::join!(run_connection(io, params), peer_handshake);
        Self {
            handle: handle.unwrap(),
            peer,
            inbound: receiver,
            state,
        }
    }
    async fn block_write(&self) {
        self.state.blocked.store(true, Ordering::SeqCst);
        self.handle
            .sender
            .send(Frame::tell("/outbound", "data", Bytes::from_static(b"out")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), self.state.write_seen.notified())
            .await
            .unwrap();
    }
    async fn close(&mut self) {
        self.handle.sender.close();
        tokio::time::timeout(Duration::from_secs(1), &mut self.handle.closed)
            .await
            .unwrap()
            .unwrap();
        assert!(
            self.state.dropped.load(Ordering::SeqCst),
            "closed preceded underlying IO destruction"
        );
    }
}

#[tokio::test]
async fn blocked_write_does_not_prevent_inbound_delivery() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.block_write().await;
    fixture
        .peer
        .send(Frame::tell("/inbound", "data", Bytes::from_static(b"in")))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_millis(500), fixture.inbound.recv()).await;
    fixture.close().await;
    let (frame, _, _) = result
        .expect("blocked outbound write stopped reading")
        .unwrap();
    assert_eq!(frame.path, "/inbound");
    assert_eq!(frame.payload.as_ref(), b"in");
}

#[tokio::test]
async fn full_inbound_queue_does_not_prevent_outbound_delivery() {
    let mut fixture = Fixture::new(false, true).await;
    fixture.state.observe_read.store(true, Ordering::SeqCst);
    fixture
        .peer
        .send(Frame::tell("/blocked-inbound", "data", Bytes::new()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), fixture.state.read_seen.notified())
        .await
        .unwrap();
    fixture
        .handle
        .sender
        .send(Frame::tell(
            "/outbound",
            "data",
            Bytes::from_static(b"progress"),
        ))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_millis(500), fixture.peer.next()).await;
    assert_eq!(fixture.inbound.try_recv().unwrap().2, "prefilled");
    fixture.close().await;
    let frame = result
        .expect("full inbound queue stopped writing")
        .unwrap()
        .unwrap();
    assert_eq!(frame.path, "/outbound");
    assert_eq!(frame.payload.as_ref(), b"progress");
}

#[tokio::test]
async fn full_inbound_queue_still_answers_heartbeat() {
    let mut fixture = Fixture::new(false, true).await;
    fixture
        .peer
        .send(Frame::tell("/dropped", "data", Bytes::new()))
        .await
        .unwrap();
    fixture.peer.send(Frame::heartbeat()).await.unwrap();
    let heartbeat_ack = tokio::time::timeout(Duration::from_millis(500), fixture.peer.next())
        .await
        .expect("inbound overload must not stop reading heartbeats")
        .unwrap()
        .unwrap();
    assert_eq!(
        heartbeat_ack.header.frame_type,
        parrot_remote::frame::frame_type::HEARTBEAT_ACK
    );
    fixture.close().await;
}

#[tokio::test]
async fn full_inbound_queue_rejects_ask_without_hanging_the_caller() {
    let mut fixture = Fixture::new(false, true).await;
    fixture
        .peer
        .send(Frame::ask(
            91,
            "/overloaded",
            "bin:Test#v1",
            Bytes::new(),
            None,
        ))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_millis(500), fixture.peer.next())
        .await
        .expect("overloaded ASK must receive a terminal reply")
        .unwrap()
        .unwrap();
    assert_eq!(
        reply.header.frame_type,
        parrot_remote::frame::frame_type::REPLY_ERR
    );
    let (code, detail) = parrot_remote::error::decode_err_payload(&reply.payload).unwrap();
    assert_eq!(code, parrot_remote::error::ErrCode::Overloaded);
    assert!(detail.contains("inbound dispatch queue"));
    fixture.close().await;
}

#[tokio::test(start_paused = true)]
async fn blocked_write_cannot_disable_heartbeat_deadline() {
    let mut fixture = Fixture::new(true, false).await;
    fixture.block_write().await;
    tokio::time::advance(Duration::from_secs(21)).await;
    let result = tokio::time::timeout(Duration::from_secs(1), &mut fixture.handle.closed).await;
    if result.is_err() {
        fixture.close().await;
    }
    assert!(
        matches!(result, Ok(Ok(()))),
        "write stall disabled the 20-second receive deadline or the driver panicked: {result:?}"
    );
    assert!(fixture.state.dropped.load(Ordering::SeqCst));
    assert!(fixture.handle.sender.is_closed());
}

#[tokio::test(start_paused = true)]
async fn received_ack_refreshes_deadline_while_write_is_blocked() {
    let mut fixture = Fixture::new(true, false).await;
    fixture.block_write().await;
    tokio::time::advance(Duration::from_secs(15)).await;
    fixture.peer.send(Frame::heartbeat_ack()).await.unwrap();
    fixture
        .peer
        .send(Frame::tell("/receipt-barrier", "data", Bytes::new()))
        .await
        .unwrap();
    let (frame, _, _) = tokio::time::timeout(Duration::from_secs(1), fixture.inbound.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.path, "/receipt-barrier");
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert!(
        matches!(
            fixture.handle.closed.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "fresh traffic did not extend liveness"
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::timeout(Duration::from_secs(1), &mut fixture.handle.closed)
        .await
        .unwrap()
        .unwrap();
    assert!(fixture.state.dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn heartbeat_flood_is_coalesced_and_business_fifo_is_preserved() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.block_write().await;
    let send_flood = async {
        for _ in 0..1000 {
            fixture.peer.send(Frame::heartbeat()).await.unwrap();
        }
        fixture
            .peer
            .send(Frame::tell("/all-heartbeats-read", "data", Bytes::new()))
            .await
            .unwrap();
    };
    tokio::time::timeout(Duration::from_secs(2), send_flood)
        .await
        .unwrap();
    let (frame, _, _) = tokio::time::timeout(Duration::from_secs(1), fixture.inbound.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.path, "/all-heartbeats-read");
    fixture.state.blocked.store(false, Ordering::SeqCst);
    fixture.state.write_waker.wake();
    for i in 0..16u8 {
        fixture
            .handle
            .sender
            .send(Frame::tell("/ordered", "data", Bytes::from(vec![i])))
            .await
            .unwrap();
    }
    let mut ordered = Vec::new();
    let mut acknowledgements = 0;
    let first = tokio::time::timeout(Duration::from_secs(1), fixture.peer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.path, "/outbound");
    while ordered.len() < 16 || acknowledgements == 0 {
        let frame = tokio::time::timeout(Duration::from_secs(1), fixture.peer.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if frame.header.frame_type == parrot_remote::frame::frame_type::HEARTBEAT_ACK {
            acknowledgements += 1;
        } else {
            assert_eq!(frame.path, "/ordered");
            ordered.push(frame.payload[0]);
        }
    }
    assert_eq!(ordered, (0..16u8).collect::<Vec<_>>());
    assert_eq!(acknowledgements, 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), fixture.peer.next())
            .await
            .is_err(),
        "unbounded heartbeat ACK backlog"
    );
    fixture.close().await;
}

#[tokio::test]
async fn protocol_violation_flushes_error_before_io_is_closed() {
    let mut fixture = Fixture::new(false, false).await;
    fixture
        .peer
        .send(Frame::handshake(Bytes::new()))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), fixture.peer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        frame.header.frame_type,
        parrot_remote::frame::frame_type::ERROR
    );
    assert_eq!(
        u16::from_le_bytes(frame.payload[..2].try_into().unwrap()),
        parrot_remote::error::ErrCode::ProtocolViolation as u16
    );
    tokio::time::timeout(Duration::from_secs(1), &mut fixture.handle.closed)
        .await
        .unwrap()
        .unwrap();
    assert!(fixture.state.dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn terminal_error_has_a_deadline_even_with_heartbeat_disabled() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.block_write().await;
    fixture.state.observe_read.store(true, Ordering::SeqCst);
    fixture
        .peer
        .send(Frame::handshake(Bytes::new()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), fixture.state.read_seen.notified())
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::timeout(Duration::from_secs(1), &mut fixture.handle.closed)
        .await
        .unwrap()
        .unwrap();
    assert!(fixture.state.dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn closed_application_ingress_terminates_connection() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.inbound.close();
    fixture
        .peer
        .send(Frame::tell("/closed", "data", Bytes::new()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), &mut fixture.handle.closed)
        .await
        .unwrap()
        .unwrap();
    assert!(fixture.state.dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn direct_connection_rejects_invalid_runtime_limits_without_handshake_or_panic() {
    use tokio::io::AsyncReadExt;
    for knobs in [
        RuntimeKnobs {
            outbound_queue: 0,
            ..Default::default()
        },
        RuntimeKnobs {
            heartbeat_interval: Duration::ZERO,
            ..Default::default()
        },
        RuntimeKnobs {
            heartbeat_max_loss: 0,
            ..Default::default()
        },
        RuntimeKnobs {
            heartbeat_interval: Duration::MAX,
            ..Default::default()
        },
    ] {
        let (io, mut peer) = tokio::io::duplex(4096);
        let (inbound, _receiver) = mpsc::channel(1);
        let (_shutdown, shutdown) = tokio::sync::watch::channel(false);
        let result = run_connection(
            io,
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: HandshakeBody {
                    node_id: "limits-test".into(),
                    ..Default::default()
                },
                inbound,
                on_disconnect: Arc::new(|_| {}),
                shutdown,
                knobs: Some(knobs),
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(parrot_remote::RemoteError::Transport(_))
        ));
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(
            bytes.is_empty(),
            "invalid configuration started a handshake"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn inbound_backpressure_cannot_disable_receive_progress_deadline() {
    let mut fixture = Fixture::new(true, true).await;
    fixture.state.observe_read.store(true, Ordering::SeqCst);
    fixture
        .peer
        .send(Frame::tell("/waiting-for-ingress", "data", Bytes::new()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), fixture.state.read_seen.notified())
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(21)).await;
    tokio::time::timeout(Duration::from_secs(1), &mut fixture.handle.closed)
        .await
        .unwrap()
        .unwrap();
    assert!(fixture.state.dropped.load(Ordering::SeqCst));
    assert_eq!(fixture.inbound.try_recv().unwrap().2, "prefilled");
    assert!(
        matches!(
            fixture.inbound.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ),
        "closed driver retained or delivered the blocked frame"
    );
}

#[tokio::test(start_paused = true)]
async fn disabled_heartbeat_has_no_receive_deadline_but_close_still_joins() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.block_write().await;
    tokio::time::advance(Duration::from_secs(1000)).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        fixture.handle.closed.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    fixture.close().await;
}
