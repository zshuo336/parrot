//! 职责：TcpTransport——tokio TcpStream + Framed（05 §2.2）。
//!
//! 要点：set_nodelay(true) 必须（LAN µs 级 RTT 前提）；心跳/重连不在
//! Transport（机制与策略分离——ConnectionTask 驱动）。

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::error::RemoteError;
use crate::handshake::HandshakeBody;
use crate::node::NodeAddr;
use crate::transport::{
    run_connection, ConnParams, ConnSide, ConnectionHandle, OnDisconnect, Transport,
};

pub struct TcpTransport {
    handshake: HandshakeBody,
    inbound:
        tokio::sync::mpsc::Sender<(crate::frame::Frame, crate::transport::FrameSender, String)>,
    on_disconnect: OnDisconnect,
    shutdown: tokio::sync::watch::Sender<bool>,
    knobs: Option<crate::transport::RuntimeKnobs>,
    listener: Mutex<Option<Arc<TcpListener>>>,
}

impl TcpTransport {
    pub fn new(
        handshake: HandshakeBody,
        inbound: tokio::sync::mpsc::Sender<(
            crate::frame::Frame,
            crate::transport::FrameSender,
            String,
        )>,
        on_disconnect: OnDisconnect,
        shutdown: tokio::sync::watch::Sender<bool>,
        knobs: Option<crate::transport::RuntimeKnobs>,
    ) -> Self {
        Self {
            handshake,
            inbound,
            on_disconnect,
            shutdown,
            knobs,
            listener: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl Transport for TcpTransport {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError> {
        let stream = TcpStream::connect(addr.addr)
            .await
            .map_err(|e| RemoteError::Transport(format!("tcp connect {}: {e}", addr.addr)))?;
        stream.set_nodelay(true).ok();
        let local = stream.local_addr().ok();
        let peer = stream.peer_addr().ok();
        run_connection(
            stream,
            ConnParams {
                side: ConnSide::Connect,
                local_addr: local,
                peer_addr: peer,
                scheme: "tcp",
                local_handshake: self.handshake.clone(),
                inbound: self.inbound.clone(),
                on_disconnect: self.on_disconnect.clone(),
                shutdown: self.shutdown.subscribe(),
                knobs: self.knobs,
            },
        )
        .await
    }

    async fn listen(&self, bind: SocketAddr) -> Result<(), RemoteError> {
        let l = TcpListener::bind(bind)
            .await
            .map_err(|e| RemoteError::Transport(format!("tcp bind {bind}: {e}")))?;
        *self.listener.lock().await = Some(Arc::new(l));
        Ok(())
    }

    async fn accept(&self) -> Result<ConnectionHandle, RemoteError> {
        let l = {
            let g = self.listener.lock().await;
            g.clone()
                .ok_or_else(|| RemoteError::Transport("listen() not called".into()))?
        };
        let (stream, peer) = l
            .accept()
            .await
            .map_err(|e| RemoteError::Transport(format!("tcp accept: {e}")))?;
        stream.set_nodelay(true).ok();
        let local = stream.local_addr().ok();
        run_connection(
            stream,
            ConnParams {
                side: ConnSide::Accept,
                local_addr: local,
                peer_addr: Some(peer),
                scheme: "tcp",
                local_handshake: self.handshake.clone(),
                inbound: self.inbound.clone(),
                on_disconnect: self.on_disconnect.clone(),
                shutdown: self.shutdown.subscribe(),
                knobs: self.knobs,
            },
        )
        .await
    }

    fn scheme(&self) -> &'static str {
        "tcp"
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.listener
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|l| l.local_addr().ok()))
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{frame_type, Frame};
    use std::time::Duration;

    // 两个 TcpTransport 经真实 TCP 握手并交换一帧（CI 用 127.0.0.1 随机端口）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tcp_connect_handshake_roundtrip() {
        let (in_a, rx_a) = tokio::sync::mpsc::channel::<(
            crate::frame::Frame,
            crate::transport::FrameSender,
            String,
        )>(64);
        let (in_b, mut rx_b) = tokio::sync::mpsc::channel::<(
            crate::frame::Frame,
            crate::transport::FrameSender,
            String,
        )>(64);
        let noop: OnDisconnect = Arc::new(|_| {});
        let (sd_a, _) = tokio::sync::watch::channel(false);
        let (sd_b, _) = tokio::sync::watch::channel(false);
        let ta = TcpTransport::new(
            HandshakeBody {
                node_id: "tcp-a".into(),
                ..Default::default()
            },
            in_a,
            noop.clone(),
            sd_a,
            None,
        );
        let tb = TcpTransport::new(
            HandshakeBody {
                node_id: "tcp-b".into(),
                ..Default::default()
            },
            in_b,
            noop,
            sd_b,
            None,
        );
        // bind → 公布端口 → 并行 accept/connect（POC 教训：勿串行阻塞）
        tb.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = {
            let g = tb.listener.lock().await;
            g.as_ref().unwrap().local_addr().unwrap()
        };
        let accept_fut = tokio::spawn(async move { tb.accept().await });
        let mut connected = None;
        for _ in 0..50 {
            if let Ok(c) = ta.connect(&crate::node::NodeAddr::tcp("tcp-b", addr)).await {
                connected = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let conn_a = connected.expect("connect retries exhausted");
        let conn_b = accept_fut.await.unwrap().unwrap();
        assert_eq!(conn_a.node_id, "tcp-b");
        assert_eq!(conn_b.node_id, "tcp-a");

        // a → b ask 帧
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
        let _ = rx_a;
    }
}
