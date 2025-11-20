//! Public connection notifications preserve sibling progress and driver retirement.
use super::*;
use futures::FutureExt;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Poll, Wake, Waker},
};
use tokio::io::ReadBuf;

const WAITERS: usize = 40;
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("controlled connection notification payload disposal");
    }
}
fn throw(mode: u8, retired: &Arc<AtomicUsize>) {
    match mode {
        1 => panic!("controlled connection notification panic"),
        2 => std::panic::panic_any(Payload(retired.clone())),
        _ => {}
    }
}
struct Probe {
    sender: FrameSender,
    calls: AtomicUsize,
    premature: AtomicBool,
    closed: bool,
    throwing: u8,
    retired: Arc<AtomicUsize>,
    io_dropped: Option<Arc<AtomicBool>>,
    disconnected: Option<Arc<AtomicUsize>>,
}
impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.closed {
            self.premature
                .fetch_or(!self.sender.is_closed(), Ordering::SeqCst);
            self.sender.close();
        }
        if let Some(dropped) = &self.io_dropped {
            self.premature
                .fetch_or(!dropped.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        if let Some(disconnected) = &self.disconnected {
            self.premature
                .fetch_or(disconnected.load(Ordering::SeqCst) != 1, Ordering::SeqCst);
        }
        throw(self.throwing, &self.retired);
    }
}
fn probe(sender: &FrameSender, closed: bool, throwing: u8) -> Arc<Probe> {
    Arc::new(Probe {
        sender: sender.clone(),
        calls: 0.into(),
        premature: false.into(),
        closed,
        throwing,
        retired: Arc::new(AtomicUsize::new(0)),
        io_dropped: None,
        disconnected: None,
    })
}
fn park<T>(future: Pin<&mut impl Future<Output = T>>, probe: &Arc<Probe>) {
    assert!(future
        .poll(&mut Context::from_waker(&Waker::from(probe.clone())))
        .is_pending());
}
fn check(probe: &Probe) {
    let calls = probe.calls.load(Ordering::SeqCst);
    assert!(calls > 0, "a selected connection waiter lost its wake");
    assert!(
        !probe.premature.load(Ordering::SeqCst),
        "notification preceded committed close or resource retirement"
    );
    assert_eq!(
        probe.retired.load(Ordering::SeqCst),
        calls * usize::from(probe.throwing == 2)
    );
}
fn frame(value: u8) -> Frame {
    Frame::tell(
        "/target",
        "notification",
        bytes::Bytes::copy_from_slice(&[value]),
    )
}
async fn cohort(mode: &str, receiver_close: bool, throwing: u8) {
    let cleanup = crate::ingress::unwind::Cleanup::default();
    let (tx, mut rx) = mpsc::channel(1);
    let sender = FrameSender::new(tx, "notification-peer");
    sender.send_frame_sync(frame(1)).unwrap();
    let probes: Vec<_> = (0..WAITERS)
        .map(|_| probe(&sender, true, throwing))
        .collect();
    let mut futures: Vec<_> = (0..WAITERS)
        .map(|_| {
            Box::pin(async {
                match mode {
                    "closed" => {
                        sender.closed().await;
                        Ok(())
                    }
                    "send" => sender.send(frame(2)).await,
                    "reserve" => {
                        let frame = frame(2);
                        let permit = sender.reserve(&frame).await?;
                        permit.send(frame);
                        Ok(())
                    }
                    _ => unreachable!(),
                }
            })
        })
        .collect();
    for (future, probe) in futures.iter_mut().zip(&probes) {
        park(future.as_mut(), probe);
    }
    let result = cleanup.call(|| {
        if receiver_close {
            rx.close();
        } else {
            sender.close();
        }
    });
    let mut outcomes = Vec::new();
    for mut future in futures {
        outcomes.push(cleanup.call(|| match future.as_mut().now_or_never() {
            Some(Ok(())) => mode == "closed",
            Some(Err(RemoteError::Transport(_))) => mode != "closed",
            _ => false,
        }));
        cleanup.dispose(future);
    }
    assert_eq!(result, Ok(()), "connection closure was interrupted");
    for (outcome, probe) in outcomes.into_iter().zip(&probes) {
        assert_eq!(outcome, Ok(true));
        check(probe);
    }
    assert_eq!(rx.try_recv().unwrap().payload.as_ref(), [1]);
    assert!(
        rx.try_recv().is_err(),
        "cancelled work leaked into the outbound queue"
    );
    assert!(sender.send(frame(3)).await.is_err());
    sender.close();
}
async fn capacity(reserve: bool, throwing: u8) {
    let cleanup = crate::ingress::unwind::Cleanup::default();
    let (tx, mut rx) = mpsc::channel(1);
    let sender = FrameSender::new(tx, "capacity-peer");
    sender.send_frame_sync(frame(1)).unwrap();
    let probe = probe(&sender, false, throwing);
    let mut future = Box::pin(async {
        if reserve {
            let frame = frame(2);
            sender.reserve(&frame).await?.send(frame);
            Ok(())
        } else {
            sender.send(frame(2)).await
        }
    });
    park(future.as_mut(), &probe);
    let released = cleanup.call(|| rx.try_recv());
    let outcome = cleanup.call(|| future.as_mut().now_or_never());
    cleanup.dispose(future);
    assert!(
        matches!(released, Ok(Ok(frame)) if frame.payload.as_ref() == [1]),
        "capacity release was interrupted"
    );
    assert!(
        matches!(outcome, Ok(Some(Ok(())))),
        "admitted frame did not complete"
    );
    check(&probe);
    assert!(!sender.is_closed());
    assert_eq!(rx.try_recv().unwrap().payload.as_ref(), [2]);
    assert!(rx.try_recv().is_err());
    sender.close();
}

struct TrackedIo<S> {
    inner: S,
    dropped: Arc<AtomicBool>,
}
struct DisconnectOwner {
    unwinding: Arc<AtomicBool>,
    retired: Arc<AtomicUsize>,
}
impl DisconnectOwner {
    fn observe(&self) {}
}
impl Drop for DisconnectOwner {
    fn drop(&mut self) {
        self.unwinding
            .store(std::thread::panicking(), Ordering::SeqCst);
        self.retired.fetch_add(1, Ordering::SeqCst);
    }
}
impl<S> Drop for TrackedIo<S> {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for TrackedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for TrackedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
async fn driver(peer_closes: bool, throwing: u8, disconnect_throws: bool) {
    let cleanup = crate::ingress::unwind::Cleanup::default();
    let owner = ConnectionTasks::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (accepted, connected) =
        tokio::join!(listener.accept(), tokio::net::TcpStream::connect(addr));
    let (server, _) = accepted.unwrap();
    let mut peer = tokio_util::codec::Framed::new(connected.unwrap(), FrameCodec);
    let dropped = Arc::new(AtomicBool::new(false));
    let disconnected = Arc::new(AtomicUsize::new(0));
    let dis_retired = Arc::new(AtomicUsize::new(0));
    let owner_unwinding = Arc::new(AtomicBool::new(false));
    let owner_retired = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicBool::new(false));
    let on_disconnect: OnDisconnect = {
        let owner = DisconnectOwner {
            unwinding: owner_unwinding.clone(),
            retired: owner_retired.clone(),
        };
        let (dropped, disconnected, retired, observed) = (
            dropped.clone(),
            disconnected.clone(),
            dis_retired.clone(),
            observed.clone(),
        );
        Arc::new(move |sender| {
            owner.observe();
            disconnected.fetch_add(1, Ordering::SeqCst);
            observed.store(
                dropped.load(Ordering::SeqCst)
                    && sender.is_closed()
                    && sender.node_id() == "notification-tcp-peer",
                Ordering::SeqCst,
            );
            sender.close();
            if disconnect_throws {
                throw(throwing, &retired);
            }
        })
    };
    let (inbound, mut received) = mpsc::channel(4);
    let (_shutdown, shutdown) = tokio::sync::watch::channel(false);
    let server = run_connection_owned(
        TrackedIo {
            inner: server,
            dropped: dropped.clone(),
        },
        ConnParams {
            side: ConnSide::Accept,
            local_addr: None,
            peer_addr: None,
            scheme: "tcp",
            local_handshake: HandshakeBody {
                node_id: "notification-tcp-server".into(),
                ..Default::default()
            },
            inbound,
            on_disconnect,
            shutdown,
            knobs: Some(RuntimeKnobs {
                heartbeat_enabled: false,
                ..Default::default()
            }),
        },
        Some(&owner),
    );
    let handshake = async {
        let mut body = BytesMut::new();
        HandshakeBody {
            node_id: "notification-tcp-peer".into(),
            ..Default::default()
        }
        .encode_tlv(&mut body)
        .unwrap();
        peer.send(Frame::handshake(body.freeze())).await.unwrap();
        assert_eq!(
            peer.next().await.unwrap().unwrap().header.frame_type,
            crate::frame::frame_type::HANDSHAKE_ACK
        );
    };
    let (handle, ()) = tokio::join!(server, handshake);
    let mut handle = handle.unwrap();
    peer.send(frame(71)).await.unwrap();
    let (message, source, from) = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.payload.as_ref(), [71]);
    assert_eq!(from, "notification-tcp-peer");
    assert!(source.same_connection(&handle.sender));
    source.send(frame(72)).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), peer.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .payload
            .as_ref(),
        [72]
    );
    let close_probe = probe(&handle.sender, true, throwing);
    let mut receipt_probe = probe(&handle.sender, true, throwing);
    Arc::get_mut(&mut receipt_probe).unwrap().io_dropped = Some(dropped.clone());
    Arc::get_mut(&mut receipt_probe).unwrap().disconnected = Some(disconnected.clone());
    let mut closed = Box::pin(handle.sender.closed());
    park(closed.as_mut(), &close_probe);
    park(Pin::new(&mut handle.closed), &receipt_probe);
    let mut peer = Some(peer);
    let trigger = cleanup.call(|| {
        if peer_closes {
            drop(peer.take());
        } else {
            handle.sender.close();
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while disconnected.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection retirement did not reach its disconnect callback");
    owner.close();
    tokio::time::timeout(Duration::from_secs(2), owner.wait())
        .await
        .unwrap();
    let receipt = cleanup.call(|| handle.closed.try_recv());
    let closed_outcome = cleanup.call(|| closed.as_mut().now_or_never());
    cleanup.dispose(closed);
    assert_eq!(trigger, Ok(()), "local close was interrupted");
    assert!(
        matches!(receipt, Ok(Ok(()))),
        "driver lost successful close receipt: {receipt:?}"
    );
    assert_eq!(closed_outcome, Ok(Some(())));
    assert_eq!(owner_retired.load(Ordering::SeqCst), 1);
    assert!(
        !owner_unwinding.load(Ordering::SeqCst),
        "connection driver unwound through its callback owner"
    );
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(disconnected.load(Ordering::SeqCst), 1);
    assert_eq!(
        dis_retired.load(Ordering::SeqCst),
        usize::from(disconnect_throws && throwing == 2)
    );
    check(&close_probe);
    check(&receipt_probe);
    if let Some(mut peer) = peer {
        assert!(
            tokio::time::timeout(Duration::from_secs(2), peer.next())
                .await
                .unwrap()
                .is_none(),
            "local close did not retire its TCP socket"
        );
    }
}

macro_rules! contract {
    ($name:ident, $exercise:expr) => {
        #[tokio::test]
        async fn $name() {
            crate::system::diagnostic_contracts::isolated(
                concat!("transport::notification_contracts::", stringify!($name)),
                &[],
                false,
                0,
                async { $exercise },
            )
            .await;
        }
    };
}
contract!(
    close_notifies_every_reentrant_observer,
    cohort("closed", false, 0).await
);
contract!(
    close_isolates_every_throwing_observer,
    cohort("closed", false, 1).await
);
contract!(
    close_isolates_every_throwing_payload,
    cohort("closed", false, 2).await
);
contract!(
    receiver_close_notifies_every_reentrant_observer,
    cohort("closed", true, 0).await
);
contract!(
    receiver_close_isolates_every_throwing_observer,
    cohort("closed", true, 1).await
);
contract!(
    receiver_close_isolates_every_throwing_payload,
    cohort("closed", true, 2).await
);
contract!(
    receiver_close_releases_every_reentrant_send,
    cohort("send", true, 0).await
);
contract!(
    receiver_close_isolates_every_throwing_send,
    cohort("send", true, 1).await
);
contract!(
    receiver_close_isolates_every_throwing_send_payload,
    cohort("send", true, 2).await
);
contract!(
    receiver_close_releases_every_reentrant_reservation,
    cohort("reserve", true, 0).await
);
contract!(
    receiver_close_isolates_every_throwing_reservation,
    cohort("reserve", true, 1).await
);
contract!(
    receiver_close_isolates_every_throwing_reservation_payload,
    cohort("reserve", true, 2).await
);
contract!(
    local_close_isolates_every_throwing_send,
    cohort("send", false, 1).await
);
contract!(
    local_close_isolates_every_throwing_send_payload,
    cohort("send", false, 2).await
);
contract!(
    local_close_isolates_every_throwing_reservation,
    cohort("reserve", false, 1).await
);
contract!(
    local_close_isolates_every_throwing_reservation_payload,
    cohort("reserve", false, 2).await
);
contract!(
    capacity_release_preserves_send_after_throwing_wake,
    capacity(false, 1).await
);
contract!(
    capacity_release_preserves_send_after_throwing_payload,
    capacity(false, 2).await
);
contract!(
    capacity_release_preserves_reservation_after_throwing_wake,
    capacity(true, 1).await
);
contract!(
    capacity_release_preserves_reservation_after_throwing_payload,
    capacity(true, 2).await
);
contract!(
    tcp_local_close_preserves_driver_receipt_after_throwing_wakes,
    driver(false, 1, false).await
);
contract!(
    tcp_local_close_preserves_driver_receipt_after_throwing_payloads,
    driver(false, 2, false).await
);
contract!(
    tcp_peer_close_preserves_driver_receipt_after_throwing_wakes,
    driver(true, 1, false).await
);
contract!(
    tcp_peer_close_preserves_driver_receipt_after_throwing_payloads,
    driver(true, 2, false).await
);
contract!(
    tcp_disconnect_and_receipt_isolate_independent_throwing_callbacks,
    driver(true, 1, true).await
);
contract!(
    tcp_disconnect_and_receipt_isolate_independent_throwing_payloads,
    driver(true, 2, true).await
);
