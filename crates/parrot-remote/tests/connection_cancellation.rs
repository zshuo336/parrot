//! Cancellation must interrupt awaited IO/queue sends inside the driver loop.
use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use parrot_remote::{
    frame::Frame,
    handshake::HandshakeBody,
    transport::{run_connection, ConnParams, ConnSide, FrameCodec, FrameSender},
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
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};

struct ObservedIo {
    io: DuplexStream,
    dropped: Arc<AtomicBool>,
    block_write: Arc<AtomicBool>,
    observe_read: Arc<AtomicBool>,
    event: Arc<tokio::sync::Notify>,
}
impl Drop for ObservedIo {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}
impl AsyncRead for ObservedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.io).poll_read(cx, buf);
        if self.observe_read.load(Ordering::SeqCst) && buf.filled().len() > before {
            self.event.notify_one();
        }
        result
    }
}
impl AsyncWrite for ObservedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.block_write.load(Ordering::SeqCst) {
            self.event.notify_one();
            return Poll::Pending; // Deliberately never writable; only cancellation can finish.
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

async fn stalled_driver_is_cancellable(stall_write: bool, global_shutdown: bool) {
    let (local, peer) = tokio::io::duplex(4096);
    let block_write = Arc::new(AtomicBool::new(false));
    let observe_read = Arc::new(AtomicBool::new(false));
    let event = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let io = ObservedIo {
        io: local,
        dropped: dropped.clone(),
        block_write: block_write.clone(),
        observe_read: observe_read.clone(),
        event: event.clone(),
    };
    let (inbound, _rx) = tokio::sync::mpsc::channel(1);
    // Force driver ingress send to wait on a receiver that remains alive.
    inbound
        .send((
            Frame::heartbeat(),
            FrameSender::detached(),
            "prefilled".into(),
        ))
        .await
        .unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let peer = tokio::spawn(async move {
        let mut peer = tokio_util::codec::Framed::new(peer, FrameCodec);
        let mut payload = BytesMut::new();
        HandshakeBody {
            node_id: "blocked-peer".into(),
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
    });
    let mut handle = run_connection(
        io,
        ConnParams {
            side: ConnSide::Accept,
            local_addr: None,
            peer_addr: None,
            scheme: "mem",
            local_handshake: HandshakeBody {
                node_id: "blocked-local".into(),
                ..Default::default()
            },
            inbound,
            on_disconnect: Arc::new(|_| {}),
            shutdown: shutdown_rx,
            knobs: None,
        },
    )
    .await
    .unwrap();
    let mut peer = peer.await.unwrap();
    if stall_write {
        block_write.store(true, Ordering::SeqCst);
        handle.sender.send(Frame::heartbeat()).await.unwrap();
    } else {
        observe_read.store(true, Ordering::SeqCst);
        peer.send(Frame::ask(
            7,
            "/user/test",
            "bin:Test#v1",
            bytes::Bytes::new(),
            None,
        ))
        .await
        .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), event.notified())
        .await
        .unwrap();
    if stall_write {
        assert!(matches!(
            handle.closed.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        if global_shutdown {
            shutdown.send_replace(true);
        } else {
            handle.sender.close();
        }
        tokio::time::timeout(Duration::from_secs(1), &mut handle.closed)
            .await
            .expect("blocked driver must stop")
            .unwrap();
    } else {
        // A full global inbound queue is load-shed at the frame boundary so
        // the read loop cannot starve heartbeat processing. The connection
        // remains usable for outbound traffic until its owner closes it.
        assert!(matches!(
            handle.closed.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        if global_shutdown {
            shutdown.send_replace(true);
        } else {
            handle.sender.close();
        }
        tokio::time::timeout(Duration::from_secs(1), &mut handle.closed)
            .await
            .expect("closed owner must still stop the connection")
            .unwrap();
    }
    assert!(
        dropped.load(Ordering::SeqCst),
        "closed notification preceded IO destruction"
    );
    assert!(handle.sender.is_closed());
    assert!(handle.sender.send(Frame::heartbeat()).await.is_err());
    assert!(handle.sender.send_frame_sync(Frame::heartbeat()).is_err());
}

#[tokio::test]
async fn connection_close_interrupts_stalled_write() {
    stalled_driver_is_cancellable(true, false).await;
}
#[tokio::test]
async fn shutdown_interrupts_stalled_write() {
    stalled_driver_is_cancellable(true, true).await;
}
#[tokio::test]
async fn connection_close_interrupts_full_inbound_queue() {
    stalled_driver_is_cancellable(false, false).await;
}
#[tokio::test]
async fn shutdown_interrupts_full_inbound_queue() {
    stalled_driver_is_cancellable(false, true).await;
}
