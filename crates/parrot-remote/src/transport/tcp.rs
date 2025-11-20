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
    pub(crate) fn with_admission(mut self, admission: super::ConnectionAdmission) -> Self {
        self.tasks.admission = admission;
        self
    }
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
            tasks: ConnectionTasks::with_knobs(knobs),
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
        connection: tokio::sync::OwnedSemaphorePermit,
        side: ConnSide,
        expected: Option<&str>,
    ) -> Result<ConnectionHandle, RemoteError> {
        stream.set_nodelay(true).ok();
        let local = stream.local_addr().ok();
        let address = stream
            .peer_addr()
            .map_err(|e| RemoteError::Transport(e.to_string()))?;
        self.security.check_address(address)?;
        let stream = super::AdmittedIo::new(stream, connection);
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

impl super::OwnedTransport for TcpTransport {
    fn close_admission(&self) {
        self.tasks.close_admission();
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
                let (connection, _handshake) = self.tasks.admit()?;
                let stream = TcpStream::connect(addr.addr).await.map_err(|e| {
                    RemoteError::Transport(format!("tcp connect {}: {e}", addr.addr))
                })?;
                self.establish(
                    stream,
                    connection,
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
                let (connection, _handshake) = self.tasks.admit()?;
                crate::security::handshake_deadline(self.establish(
                    stream,
                    connection,
                    ConnSide::Accept,
                    None,
                ))
                .await
            })
            .await
    }

    fn close(&self) {
        self.tasks.close_admission();
        let listener = self.listener.lock().unwrap().take();
        let cleanup = crate::ingress::unwind::Cleanup::default();
        self.tasks.close();
        cleanup.dispose(listener);
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

    #[tokio::test]
    async fn stalled_inbound_handshake_is_bounded_and_failed_peer_releases_capacity() {
        use tokio::io::AsyncReadExt;
        let (inbound, _receiver) = tokio::sync::mpsc::channel(8);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let server = Arc::new(TcpTransport::new(
            HandshakeBody {
                node_id: "inbound-admission-server".into(),
                ..Default::default()
            },
            inbound,
            Arc::new(|_| {}),
            shutdown,
            Some(super::super::RuntimeKnobs {
                connection_capacity: 2,
                handshake_capacity: 1,
                ..Default::default()
            }),
        ));
        server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr().unwrap();
        let stalled = TcpStream::connect(address).await.unwrap();
        let owner = server.clone();
        let attempt = tokio::spawn(async move { owner.accept().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match server.tasks.admit() {
                    Err(error) => {
                        assert!(error.to_string().contains("handshake capacity"), "{error}");
                        break;
                    }
                    Ok(permits) => drop(permits),
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first accept never started its handshake");
        let mut rejected = TcpStream::connect(address).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), server.accept())
            .await
            .unwrap()
            .err()
            .unwrap();
        assert!(error.to_string().contains("handshake capacity"), "{error}");
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), rejected.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        drop(stalled);
        assert!(attempt.await.unwrap().is_err());
        // Both the failed handshake and the rejected attempt must return their
        // connection permits, rather than permanently reducing node capacity.
        let first = server.tasks.admit().unwrap();
        drop(first.1);
        let second = server.tasks.admit().unwrap();
        drop((first.0, second));
        server.shutdown().await;
    }

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
