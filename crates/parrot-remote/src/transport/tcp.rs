//! 职责：TcpTransport——tokio TcpStream + Framed（05 §2.2）。
//!
//! 要点：set_nodelay(true) 必须（LAN µs 级 RTT 前提）；心跳/重连不在
//! Transport（机制与策略分离——ConnectionTask 驱动）。

use std::net::SocketAddr;
use std::sync::Arc;

use std::sync::Mutex;
use tokio::net::{TcpListener, TcpStream};

use crate::error::RemoteError;
use crate::handshake::HandshakeBody;
use crate::node::NodeAddr;
use crate::transport::{
    run_connection_captured, ConnParams, ConnSide, ConnectionHandle, ConnectionTasks, OnDisconnect,
    Transport,
};

pub struct TcpTransport {
    tasks: ConnectionTasks,
    capture: Option<crate::capture::FrameCapture>,
    security: crate::security::RemoteSecurity,
    tls: Option<(tokio_rustls::TlsAcceptor, tokio_rustls::TlsConnector)>,
    handshake: HandshakeBody,
    inbound:
        tokio::sync::mpsc::Sender<(crate::frame::Frame, crate::transport::FrameSender, String)>,
    on_disconnect: OnDisconnect,
    shutdown: tokio::sync::watch::Sender<bool>,
    knobs: Option<crate::transport::RuntimeKnobs>,
    listener: Mutex<Option<Arc<TcpListener>>>,
}

impl Drop for TcpTransport {
    fn drop(&mut self) {
        self.close();
    }
}

impl TcpTransport {
    pub(crate) fn with_metrics(mut self, metrics: Arc<crate::metrics::TransportMetrics>) -> Self {
        self.tasks.metrics = metrics;
        self
    }
    pub fn with_capture(mut self, capture: Option<crate::capture::FrameCapture>) -> Self {
        self.capture = capture;
        self
    }

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
            tasks: ConnectionTasks::default(),
            capture: None,
            security: Default::default(),
            tls: None,
            handshake,
            inbound,
            on_disconnect,
            shutdown,
            knobs,
            listener: Mutex::new(None),
        }
    }
}

trait TcpIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> TcpIo for T {}

impl TcpTransport {
    pub fn with_security(
        mut self,
        security: crate::security::RemoteSecurity,
    ) -> Result<Self, RemoteError> {
        security.validate(&self.handshake.node_id, "tcp", None)?;
        self.tls = security
            .credentials()
            .map(|cfg| {
                Ok::<_, RemoteError>((
                    crate::tls::server_acceptor(cfg)?,
                    crate::tls::client_connector(cfg)?,
                ))
            })
            .transpose()?;
        self.security = security;
        Ok(self)
    }

    async fn establish(
        &self,
        stream: TcpStream,
        side: ConnSide,
        expected: Option<&str>,
    ) -> Result<ConnectionHandle, RemoteError> {
        stream.set_nodelay(true).ok();
        let local = stream.local_addr().ok();
        let address = stream
            .peer_addr()
            .map_err(|e| RemoteError::Transport(e.to_string()))?;
        self.security.check_address(address)?;
        let (io, trust): (Box<dyn TcpIo>, crate::security::PeerTrust) = match (&self.tls, side) {
            (Some((_, connector)), ConnSide::Connect) => {
                let name = rustls::pki_types::ServerName::try_from(expected.unwrap().to_owned())
                    .map_err(|e| RemoteError::Transport(format!("TLS server name: {e}")))?;
                let stream = connector
                    .connect(name, stream)
                    .await
                    .map_err(|e| RemoteError::Transport(format!("TLS connect: {e}")))?;
                let certs = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .unwrap_or_default()
                    .to_vec();
                (
                    Box::new(stream),
                    crate::security::PeerTrust::Certificates(certs),
                )
            }
            (Some((acceptor, _)), ConnSide::Accept) => {
                let stream = acceptor
                    .accept(stream)
                    .await
                    .map_err(|e| RemoteError::Transport(format!("TLS accept: {e}")))?;
                let certs = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .unwrap_or_default()
                    .to_vec();
                (
                    Box::new(stream),
                    crate::security::PeerTrust::Certificates(certs),
                )
            }
            (None, _) => (
                Box::new(stream),
                crate::security::PeerTrust::Unauthenticated,
            ),
        };
        let trust = if let Some(node) = expected {
            trust.expecting(node)
        } else {
            trust
        };
        run_connection_captured(
            io,
            ConnParams {
                side,
                local_addr: local,
                peer_addr: Some(address),
                scheme: "tcp",
                local_handshake: self.handshake.clone(),
                inbound: self.inbound.clone(),
                on_disconnect: self.on_disconnect.clone(),
                shutdown: self.shutdown.subscribe(),
                knobs: self.knobs,
            },
            Some(&self.tasks),
            trust,
            self.capture.as_ref(),
        )
        .await
    }
}

#[async_trait::async_trait]
impl Transport for TcpTransport {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError> {
        self.security.check_address(addr.addr)?;
        if self.tls.is_some() && !addr.identity_is_explicit {
            return Err(RemoteError::Transport(
                "mTLS requires an explicit node ID in the endpoint".into(),
            ));
        }
        self.tasks
            .run(crate::security::handshake_deadline(async {
                let stream = TcpStream::connect(addr.addr).await.map_err(|e| {
                    RemoteError::Transport(format!("tcp connect {}: {e}", addr.addr))
                })?;
                self.establish(
                    stream,
                    ConnSide::Connect,
                    addr.identity_is_explicit.then_some(addr.node_id.as_str()),
                )
                .await
            }))
            .await
    }

    async fn listen(&self, bind: SocketAddr) -> Result<(), RemoteError> {
        self.security.check_address(bind)?;
        self.tasks
            .run(async {
                let l = TcpListener::bind(bind)
                    .await
                    .map_err(|e| RemoteError::Transport(format!("tcp bind {bind}: {e}")))?;
                let mut listener = self.listener.lock().unwrap();
                if self.tasks.is_closed() {
                    return Err(ConnectionTasks::error());
                }
                *listener = Some(Arc::new(l));
                Ok(())
            })
            .await
    }

    async fn accept(&self) -> Result<ConnectionHandle, RemoteError> {
        self.tasks
            .run(async {
                let l = self
                    .listener
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| RemoteError::Transport("listen() not called".into()))?;
                let (stream, _) = l
                    .accept()
                    .await
                    .map_err(|e| RemoteError::Transport(format!("tcp accept: {e}")))?;
                crate::security::handshake_deadline(self.establish(stream, ConnSide::Accept, None))
                    .await
            })
            .await
    }

    fn close(&self) {
        self.tasks.close();
        self.listener.lock().unwrap().take();
    }

    async fn shutdown(&self) {
        self.close();
        self.tasks.wait().await;
    }

    fn scheme(&self) -> &'static str {
        "tcp"
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.listener
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|l| l.local_addr().ok())
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
            let g = tb.listener.lock().unwrap();
            g.as_ref().unwrap().local_addr().unwrap()
        };
        let address = crate::node::NodeAddr::tcp("tcp-b", addr);
        let (conn_b, conn_a) = tokio::try_join!(tb.accept(), ta.connect(&address)).unwrap();
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
