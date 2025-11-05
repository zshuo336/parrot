//! 职责：MemoryTransport——进程内 duplex 双工对载体（测试/嵌入用，05 §2.3）。
//!
//! 全局端点表按 node_id 注册：connect 侧查表把 server 半边投递给目标节点，
//! 目标节点 accept() 取出后走与 tcp 完全一致的握手时序（run_connection）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::io::DuplexStream;
use tokio::sync::mpsc;

use crate::error::RemoteError;
use crate::frame::Frame;
use crate::handshake::HandshakeBody;
use crate::node::NodeAddr;
use crate::transport::{
    run_connection_captured, ConnParams, ConnSide, ConnectionHandle, ConnectionTasks, FrameSender,
    OnDisconnect, Transport,
};

type InboundItem = (Frame, FrameSender, String);
#[derive(Clone)]
struct Offer {
    tx: mpsc::Sender<DuplexStream>,
    closed: Arc<Mutex<bool>>,
}

impl Offer {
    async fn send(&self, stream: DuplexStream) -> Result<(), RemoteError> {
        let permit = self
            .tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| RemoteError::Transport("mem accept side closed".into()))?;
        self.publish(permit, stream)
    }

    fn publish(
        &self,
        permit: mpsc::OwnedPermit<DuplexStream>,
        stream: DuplexStream,
    ) -> Result<(), RemoteError> {
        // Receiver::close/drop cannot revoke previously issued permits. Serialize
        // the actual publication with endpoint closure, after waiting for space.
        let closed = self.closed.lock().unwrap();
        if *closed {
            return Err(RemoteError::Transport("mem accept side closed".into()));
        }
        permit.send(stream);
        Ok(())
    }

    fn same_channel(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }
}

/// 进程内单例端点表：node_id → accept 侧投递通道。
fn hub() -> &'static Mutex<HashMap<String, Offer>> {
    static HUB: OnceLock<Mutex<HashMap<String, Offer>>> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct MemoryTransport {
    tasks: ConnectionTasks,
    capture: Option<crate::capture::FrameCapture>,
    handshake: HandshakeBody,
    inbound: mpsc::Sender<InboundItem>,
    on_disconnect: OnDisconnect,
    shutdown: tokio::sync::watch::Sender<bool>,
    knobs: Option<crate::transport::RuntimeKnobs>,
    node_id: String,
    accept_tx: Offer,
    accept_rx: Mutex<Option<mpsc::Receiver<DuplexStream>>>,
    accept_gate: tokio::sync::Mutex<()>,
}

impl MemoryTransport {
    pub(crate) fn with_metrics(mut self, metrics: Arc<crate::metrics::TransportMetrics>) -> Self {
        self.tasks.metrics = metrics;
        self
    }
    pub fn with_capture(mut self, capture: Option<crate::capture::FrameCapture>) -> Self {
        self.capture = capture;
        self
    }

    /// Reserve this process-local node address until close or Drop. Duplicate
    /// live identities fail without replacing the existing endpoint, including
    /// when neither owner has started its accept loop yet.
    pub fn new(
        handshake: HandshakeBody,
        inbound: mpsc::Sender<InboundItem>,
        on_disconnect: OnDisconnect,
        shutdown: tokio::sync::watch::Sender<bool>,
        knobs: Option<crate::transport::RuntimeKnobs>,
    ) -> Result<Self, RemoteError> {
        parrot_config::endpoint::validate_node_id(&handshake.node_id)
            .map_err(RemoteError::Transport)?;
        let (accept_tx, accept_rx) = mpsc::channel(16);
        let accept_tx = Offer {
            tx: accept_tx,
            closed: Arc::new(Mutex::new(false)),
        };
        let node_id = handshake.node_id.clone();
        // Check and publish under one lock: a failed constructor never owns a
        // reservation and therefore cannot unregister the successful owner.
        match hub().lock().unwrap().entry(node_id.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => {
                return Err(RemoteError::Transport(format!(
                    "memory endpoint {node_id} is already owned"
                )));
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(accept_tx.clone());
            }
        }
        Ok(Self {
            tasks: ConnectionTasks::default(),
            capture: None,
            handshake,
            inbound,
            on_disconnect,
            shutdown,
            knobs,
            node_id,
            accept_tx,
            accept_rx: Mutex::new(Some(accept_rx)),
            accept_gate: tokio::sync::Mutex::new(()),
        })
    }
}

impl MemoryTransport {
    fn close_endpoint(&self) {
        {
            // Revoke previously obtained offers before allowing a new owner to
            // reserve this name. Keep the hub -> offer lock order consistent.
            let mut g = hub().lock().unwrap();
            *self.accept_tx.closed.lock().unwrap() = true;
            if let Some(tx) = g.get(&self.node_id) {
                if tx.same_channel(&self.accept_tx) {
                    g.remove(&self.node_id);
                }
            }
        }
        self.accept_rx.lock().unwrap().take();
    }
}

impl Drop for MemoryTransport {
    fn drop(&mut self) {
        // Senders and watch signals are observers, not owners of the drivers.
        self.close();
    }
}

#[async_trait::async_trait]
impl Transport for MemoryTransport {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError> {
        self.tasks
            .run(async {
                let target = {
                    let g = hub().lock().unwrap();
                    g.get(&addr.node_id).cloned().ok_or_else(|| {
                        RemoteError::Transport(format!("mem endpoint {} not found", addr.node_id))
                    })?
                };
                let (client, server) = tokio::io::duplex(64 * 1024);
                target.send(server).await.map_err(|_| {
                    RemoteError::Transport(format!("mem peer {} not accepting", addr.node_id))
                })?;
                run_connection_captured(
                    client,
                    ConnParams {
                        side: ConnSide::Connect,
                        local_addr: None,
                        peer_addr: None,
                        scheme: "mem",
                        local_handshake: self.handshake.clone(),
                        inbound: self.inbound.clone(),
                        on_disconnect: self.on_disconnect.clone(),
                        shutdown: self.shutdown.subscribe(),
                        knobs: self.knobs,
                    },
                    Some(&self.tasks),
                    crate::security::PeerTrust::InProcess.expecting(&addr.node_id),
                    self.capture.as_ref(),
                )
                .await
            })
            .await
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<(), RemoteError> {
        self.tasks
            .run(async {
                Ok(()) // mem 载体无绑定语义（端点随 new() 注册）
            })
            .await
    }

    async fn accept(&self) -> Result<ConnectionHandle, RemoteError> {
        self.tasks
            .run(async {
                let server = {
                    let _accept = self.accept_gate.lock().await;
                    let offer =
                        std::future::poll_fn(|cx| match self.accept_rx.lock().unwrap().as_mut() {
                            Some(rx) => rx.poll_recv(cx),
                            None => std::task::Poll::Ready(None),
                        })
                        .await;
                    match offer {
                        Some(s) => s,
                        None => {
                            return Err(RemoteError::Transport("mem accept side closed".into()))
                        }
                    }
                };
                run_connection_captured(
                    server,
                    ConnParams {
                        side: ConnSide::Accept,
                        local_addr: None,
                        peer_addr: None,
                        scheme: "mem",
                        local_handshake: self.handshake.clone(),
                        inbound: self.inbound.clone(),
                        on_disconnect: self.on_disconnect.clone(),
                        shutdown: self.shutdown.subscribe(),
                        knobs: self.knobs,
                    },
                    Some(&self.tasks),
                    crate::security::PeerTrust::InProcess,
                    self.capture.as_ref(),
                )
                .await
            })
            .await
    }

    fn close(&self) {
        self.tasks.close();
        self.close_endpoint();
    }

    async fn shutdown(&self) {
        self.close();
        self.tasks.wait().await;
    }

    fn scheme(&self) -> &'static str {
        "mem"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::frame_type;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn invalid_direct_memory_identities_fail_before_reserving_an_address() {
        for id in ["", "bad/id", "bad@id", "bad\0id", "bad id"] {
            let (inbound, _) = mpsc::channel(1);
            let (shutdown, _) = tokio::sync::watch::channel(false);
            assert!(MemoryTransport::new(
                HandshakeBody {
                    node_id: id.into(),
                    ..Default::default()
                },
                inbound,
                Arc::new(|_| {}),
                shutdown,
                None,
            )
            .is_err());
            assert!(!hub().lock().unwrap().contains_key(id));
        }
    }

    #[tokio::test]
    async fn close_drains_queued_offers_and_rejects_cached_offer_sender() {
        use tokio::io::AsyncReadExt;
        let (inbound, _) = mpsc::channel(1);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let transport = MemoryTransport::new(
            HandshakeBody {
                node_id: "close-queued-offer".into(),
                ..Default::default()
            },
            inbound,
            Arc::new(|_| {}),
            shutdown,
            None,
        )
        .unwrap();
        let cached = hub()
            .lock()
            .unwrap()
            .get("close-queued-offer")
            .unwrap()
            .clone();
        let (mut client, server) = tokio::io::duplex(16);
        cached.send(server).await.unwrap();
        let reserved = cached.tx.clone().reserve_owned().await.unwrap();
        transport.close();
        let (mut late_client, late_server) = tokio::io::duplex(16);
        assert!(cached.publish(reserved, late_server).is_err());
        let late_read =
            tokio::time::timeout(Duration::from_secs(1), late_client.read(&mut [0])).await;
        assert_eq!(
            late_read
                .expect("reserved offer after close retained a server half")
                .unwrap(),
            0
        );
        assert_eq!(
            client.read(&mut [0]).await.unwrap(),
            0,
            "queued server half was retained"
        );
        let (_, late) = tokio::io::duplex(16);
        assert!(cached.send(late).await.is_err());
        transport.shutdown().await;
        assert!(transport.accept().await.is_err());
        assert!(transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .is_err());
    }

    // 两个 MemoryTransport 经 duplex 握手并交换一帧（与 tcp 测试同构）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mem_connect_handshake_roundtrip() {
        let (in_a, _rx_a) = mpsc::channel::<InboundItem>(64);
        let (in_b, mut rx_b) = mpsc::channel::<InboundItem>(64);
        let noop: OnDisconnect = Arc::new(|_| {});
        let (sd_a, _) = tokio::sync::watch::channel(false);
        let (sd_b, _) = tokio::sync::watch::channel(false);
        let ma = MemoryTransport::new(
            HandshakeBody {
                node_id: "mem-a-t1".into(),
                ..Default::default()
            },
            in_a,
            noop.clone(),
            sd_a,
            None,
        )
        .unwrap();
        let mb = MemoryTransport::new(
            HandshakeBody {
                node_id: "mem-b-t1".into(),
                ..Default::default()
            },
            in_b,
            noop,
            sd_b,
            None,
        )
        .unwrap();
        // 并行 accept/connect（串行 await 会死锁：accept 等投递）
        let address = NodeAddr::mem("mem-b-t1");
        let (conn_b, conn_a) = tokio::try_join!(mb.accept(), ma.connect(&address)).unwrap();
        assert_eq!(conn_a.node_id, "mem-b-t1");
        assert_eq!(conn_b.node_id, "mem-a-t1");

        // a → b 一帧
        conn_a
            .sender
            .send(Frame::ask(
                1,
                "/u/echo",
                "bin:x::Ping",
                bytes::Bytes::from_static(b"P"),
                None,
            ))
            .await
            .unwrap();
        let (got, _, _) = tokio::time::timeout(Duration::from_secs(3), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.header.frame_type, frame_type::ASK);
        assert_eq!(got.path, "/u/echo");
    }
}
