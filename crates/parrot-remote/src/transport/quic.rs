//! 职责：QuicTransport——quinn 0.11 + ALPN "parrot/1"（DEV_02 §4 / 06 §2.3）。
//!
//! 流模型：每连接一条 bi 流承载 Wire 1.0（控制+数据同流；帧格式不变）；
//! uni 流每 ask 一条是 P3 反压优化项——预留扩展点。
//!
//! TLS：quinn 自带 rustls。证书经 `QuicCrypto` 注入（server 必需；client
//! 验证策略可换）；系统 mTLS 使用配置 CA，开发跳过校验必须显式选择。

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::error::RemoteError;
use crate::handshake::HandshakeBody;
use crate::node::NodeAddr;
use crate::transport::{
    run_connection_captured, ConnParams, ConnSide, ConnectionHandle, ConnectionTasks, FrameSender,
    OnDisconnect, Transport,
};

type InboundItem = (crate::frame::Frame, FrameSender, String);
type ClientEndpoints =
    Arc<std::sync::Mutex<Option<std::collections::HashMap<SocketAddr, quinn::Endpoint>>>>;

pub const ALPN_PARROT_1: &[u8] = b"parrot/1";

/// TLS 材料包（server 配置 + client 验证策略）——测试用 rcgen 生成注入；
/// K4 mTLS 后由 tls.rs 的 CA 体系构造。
#[derive(Clone)]
pub struct QuicCrypto {
    pub server: quinn::ServerConfig,
    pub client: quinn::ClientConfig,
}

pub struct QuicTransport {
    tasks: ConnectionTasks,
    capture: Option<crate::capture::FrameCapture>,
    verified_mtls: bool,
    handshake: HandshakeBody,
    inbound: mpsc::Sender<InboundItem>,
    on_disconnect: OnDisconnect,
    shutdown: tokio::sync::watch::Sender<bool>,
    knobs: Option<crate::transport::RuntimeKnobs>,
    endpoint: std::sync::Mutex<Option<quinn::Endpoint>>,
    address: SocketAddr,
    runtime_tasks: tokio_util::task::TaskTracker,
    clients: ClientEndpoints,
    client_cfg: quinn::ClientConfig,
}

impl Drop for QuicTransport {
    fn drop(&mut self) {
        // Explicitly close Quinn endpoints as well as cancelling Parrot's
        // drivers; external signal/sender clones must not extend IO ownership.
        self.close();
    }
}

impl QuicTransport {
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
        inbound: mpsc::Sender<InboundItem>,
        on_disconnect: OnDisconnect,
        shutdown: tokio::sync::watch::Sender<bool>,
        bind: SocketAddr,
        crypto: QuicCrypto,
        knobs: Option<crate::transport::RuntimeKnobs>,
    ) -> Result<Self, RemoteError> {
        let client = crypto.client; // ALPN 已在构造侧设置
        let runtime_tasks = tokio_util::task::TaskTracker::new();
        let socket = std::net::UdpSocket::bind(bind)
            .map_err(|e| RemoteError::Transport(format!("quic bind {bind}: {e}")))?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(crypto.server),
            socket,
            Arc::new(OwnedRuntime(runtime_tasks.clone())),
        )
        .map_err(|e| RemoteError::Transport(format!("quic endpoint: {e}")))?;
        let address = endpoint
            .local_addr()
            .map_err(|e| RemoteError::Transport(format!("quic local address: {e}")))?;
        Ok(Self {
            tasks: ConnectionTasks::default(),
            capture: None,
            verified_mtls: false,
            handshake,
            inbound,
            on_disconnect,
            shutdown,
            knobs,
            endpoint: std::sync::Mutex::new(Some(endpoint)),
            address,
            runtime_tasks,
            clients: Arc::new(std::sync::Mutex::new(Some(Default::default()))),
            client_cfg: client,
        })
    }

    /// Construct authenticated transport from credentials, or explicitly opt in
    /// to development crypto. Arbitrary injected QuicCrypto is never tagged mTLS.
    pub fn with_security(
        handshake: HandshakeBody,
        inbound: mpsc::Sender<InboundItem>,
        on_disconnect: OnDisconnect,
        shutdown: tokio::sync::watch::Sender<bool>,
        bind: SocketAddr,
        security: &crate::security::RemoteSecurity,
        knobs: Option<crate::transport::RuntimeKnobs>,
    ) -> Result<Self, RemoteError> {
        security.validate(&handshake.node_id, "quic", Some(bind))?;
        let (crypto, verified) = match security.credentials() {
            Some(cfg) => (mtls_crypto(cfg)?, true),
            None => (dev_crypto_insecure(), false),
        };
        let mut transport = Self::new(
            handshake,
            inbound,
            on_disconnect,
            shutdown,
            bind,
            crypto,
            knobs,
        )?;
        transport.verified_mtls = verified;
        Ok(transport)
    }

    fn peer_trust(
        &self,
        conn: &quinn::Connection,
    ) -> Result<crate::security::PeerTrust, RemoteError> {
        if !self.verified_mtls {
            return Ok(crate::security::PeerTrust::Unauthenticated);
        }
        let certs = conn
            .peer_identity()
            .and_then(|identity| {
                identity
                    .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
                    .ok()
            })
            .ok_or_else(|| RemoteError::Transport("QUIC peer certificate missing".into()))?;
        Ok(crate::security::PeerTrust::Certificates(*certs))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// uni 流扩展点（P3 反压优化：每 ask 一条 uni 流）。
    #[allow(dead_code)]
    async fn open_uni(&self, _conn: &quinn::Connection) {}
}

#[async_trait::async_trait]
impl Transport for QuicTransport {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError> {
        if self.verified_mtls && !addr.identity_is_explicit {
            return Err(RemoteError::Transport(
                "mTLS requires an explicit node ID in the endpoint".into(),
            ));
        }
        self.tasks
            .run(crate::security::handshake_deadline(async {
                // Outbound routing must not inherit a loopback-only listener
                // bind. Own the client endpoint through handshake and driver.
                let bind = if addr.addr.is_ipv6() {
                    "[::]:0"
                } else {
                    "0.0.0.0:0"
                };
                let socket = std::net::UdpSocket::bind(bind)
                    .map_err(|e| RemoteError::Transport(format!("quic client bind: {e}")))?;
                let endpoint = quinn::Endpoint::new(
                    quinn::EndpointConfig::default(),
                    None,
                    socket,
                    Arc::new(OwnedRuntime(self.runtime_tasks.clone())),
                )
                .map_err(|e| RemoteError::Transport(format!("quic client endpoint: {e}")))?;
                let local = endpoint.local_addr().map_err(|e| {
                    RemoteError::Transport(format!("quic client local address: {e}"))
                })?;
                {
                    let mut clients = self.clients.lock().unwrap();
                    let clients = clients.as_mut().ok_or_else(ConnectionTasks::error)?;
                    clients.insert(local, endpoint.clone());
                }
                let client = ClientEndpoint {
                    endpoint,
                    local,
                    clients: self.clients.clone(),
                };
                let conn = client
                    .endpoint
                    .connect_with(
                        self.client_cfg.clone(),
                        addr.addr,
                        if self.verified_mtls {
                            &addr.node_id
                        } else {
                            "localhost"
                        },
                    )
                    .map_err(|e| {
                        RemoteError::Transport(format!("quic connect {}: {e}", addr.addr))
                    })?
                    .await
                    .map_err(|e| {
                        RemoteError::Transport(format!("quic handshake {}: {e}", addr.addr))
                    })?;
                let trust = self.peer_trust(&conn)?;
                let trust = if addr.identity_is_explicit {
                    trust.expecting(&addr.node_id)
                } else {
                    trust
                };
                let (send, recv) = conn
                    .open_bi()
                    .await
                    .map_err(|e| RemoteError::Transport(format!("quic open_bi: {e}")))?;
                run_connection_captured(
                    QuicIo {
                        io: tokio::io::join(recv, send),
                        connection: conn,
                        client: Some(client),
                    },
                    ConnParams {
                        side: ConnSide::Connect,
                        local_addr: Some(local),
                        peer_addr: Some(addr.addr),
                        scheme: "quic",
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
            }))
            .await
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<(), RemoteError> {
        self.tasks
            .run(async {
                Ok(()) // endpoint 已在 new() 绑定
            })
            .await
    }

    async fn accept(&self) -> Result<ConnectionHandle, RemoteError> {
        self.tasks
            .run(async {
                let endpoint = self
                    .endpoint
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(ConnectionTasks::error)?;
                let Some(incoming) = endpoint.accept().await else {
                    return Err(RemoteError::Transport("quic endpoint closed".into()));
                };
                crate::security::handshake_deadline(async {
                    let conn = incoming
                        .await
                        .map_err(|e| RemoteError::Transport(format!("quic accept: {e}")))?;
                    let trust = self.peer_trust(&conn)?;
                    let (send, recv) = conn
                        .accept_bi()
                        .await
                        .map_err(|e| RemoteError::Transport(format!("quic accept_bi: {e}")))?;
                    let peer = conn.remote_address();
                    run_connection_captured(
                        QuicIo {
                            io: tokio::io::join(recv, send),
                            connection: conn,
                            client: None,
                        },
                        ConnParams {
                            side: ConnSide::Accept,
                            local_addr: Some(self.local_addr()),
                            peer_addr: Some(peer),
                            scheme: "quic",
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
                })
                .await
            })
            .await
    }

    fn close(&self) {
        self.tasks.close();
        if let Some(endpoint) = self.endpoint.lock().unwrap().take() {
            endpoint.close(0u32.into(), b"transport shutdown");
        }
        if let Some(clients) = self.clients.lock().unwrap().take() {
            for endpoint in clients.into_values() {
                endpoint.close(0u32.into(), b"transport shutdown");
            }
        }
        self.runtime_tasks.close();
    }

    async fn shutdown(&self) {
        self.close();
        self.tasks.wait().await;
        // Quinn's endpoint and connection drivers own the UDP socket even
        // after Endpoint::close. Observe their actual destruction.
        self.runtime_tasks.wait().await;
    }

    fn scheme(&self) -> &'static str {
        "quic"
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.endpoint.lock().unwrap().as_ref().map(|_| self.address)
    }
}

/// Track Quinn's own endpoint/connection drivers, including UDP ownership.
#[derive(Debug)]
struct OwnedRuntime(tokio_util::task::TaskTracker);
impl quinn::Runtime for OwnedRuntime {
    fn new_timer(&self, at: std::time::Instant) -> std::pin::Pin<Box<dyn quinn::AsyncTimer>> {
        quinn::TokioRuntime.new_timer(at)
    }
    fn spawn(&self, future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>) {
        self.0.spawn(future);
    }
    fn wrap_udp_socket(
        &self,
        socket: std::net::UdpSocket,
    ) -> std::io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
        quinn::TokioRuntime.wrap_udp_socket(socket)
    }
}

/// The client endpoint stays owned until its handshake/driver IO is dropped.
struct ClientEndpoint {
    endpoint: quinn::Endpoint,
    local: SocketAddr,
    clients: ClientEndpoints,
}
impl Drop for ClientEndpoint {
    fn drop(&mut self) {
        if let Some(clients) = self.clients.lock().unwrap().as_mut() {
            clients.remove(&self.local);
        }
        self.endpoint.close(0u32.into(), b"connection closed");
    }
}

struct QuicIo {
    io: tokio::io::Join<quinn::RecvStream, quinn::SendStream>,
    connection: quinn::Connection,
    // Retained for outbound endpoints; server endpoint belongs to transport.
    client: Option<ClientEndpoint>,
}
impl Drop for QuicIo {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"connection closed");
        self.client.take();
    }
}
impl tokio::io::AsyncRead for QuicIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for QuicIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// dev 跳过证书校验（K4 替换为 CA 体系）。
#[derive(Debug)]
struct DevSkipVerify;
impl rustls::client::danger::ServerCertVerifier for DevSkipVerify {
    fn verify_server_cert(
        &self,
        _e: &rustls::pki_types::CertificateDer<'_>,
        _i: &[rustls::pki_types::CertificateDer<'_>],
        _n: &rustls::pki_types::ServerName<'_>,
        _o: &[u8],
        _t: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
        ]
    }
}

/// CA-verified mutual TLS for QUIC; identity is bound to Wire HELLO separately.
fn mtls_crypto(cfg: &crate::tls::TlsConfig) -> Result<QuicCrypto, RemoteError> {
    let mut server = crate::tls::server_config(cfg)?;
    let mut client = crate::tls::client_config(cfg)?;
    server.alpn_protocols = vec![ALPN_PARROT_1.to_vec()];
    client.alpn_protocols = vec![ALPN_PARROT_1.to_vec()];
    Ok(QuicCrypto {
        server: quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server)
                .map_err(|e| RemoteError::Transport(format!("QUIC server TLS: {e}")))?,
        )),
        client: quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client)
                .map_err(|e| RemoteError::Transport(format!("QUIC client TLS: {e}")))?,
        )),
    })
}

/// dev 自签 crypto 包（rcgen 进程内生成 + client 跳过校验）——
/// Only for explicit insecure development mode; never the default configuration.
pub fn dev_crypto_insecure() -> QuicCrypto {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let mut server_crypto =
        rustls::ServerConfig::builder_with_provider(crate::tls::crypto_provider())
            .with_safe_default_protocol_versions()
            .expect("supported TLS versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key)
            .unwrap();
    server_crypto.alpn_protocols = vec![ALPN_PARROT_1.to_vec()];
    let server = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap(),
    ));
    let mut client_crypto =
        rustls::ClientConfig::builder_with_provider(crate::tls::crypto_provider())
            .with_safe_default_protocol_versions()
            .expect("supported TLS versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(DevSkipVerify))
            .with_no_client_auth();
    client_crypto.alpn_protocols = vec![ALPN_PARROT_1.to_vec()];
    QuicCrypto {
        server,
        client: quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto).unwrap(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{frame_type, Frame};
    use std::time::Duration;

    fn dev_crypto() -> QuicCrypto {
        dev_crypto_insecure()
    }

    #[tokio::test]
    async fn shutdown_joins_client_endpoint_and_reports_its_actual_port() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let make = |name: &str| {
            let (inbound, _) = mpsc::channel(16);
            let (shutdown, _) = tokio::sync::watch::channel(false);
            QuicTransport::new(
                HandshakeBody {
                    node_id: name.into(),
                    ..Default::default()
                },
                inbound,
                Arc::new(|_| {}),
                shutdown,
                "127.0.0.1:0".parse().unwrap(),
                dev_crypto(),
                None,
            )
            .unwrap()
        };
        let server = make("owned-server");
        let client = make("owned-client");
        let server_addr = server.local_addr();
        let client_listener = client.local_addr();
        let node = NodeAddr::quic("owned-server", server_addr);
        let (accepted, connected) = tokio::join!(server.accept(), client.connect(&node));
        let mut accepted = accepted.unwrap();
        let mut connected = connected.unwrap();
        let outgoing = connected.info.local.unwrap();
        assert_eq!(outgoing.port(), accepted.info.peer.unwrap().port());
        assert_ne!(outgoing.port(), client_listener.port());
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(client.shutdown(), server.shutdown());
        })
        .await
        .unwrap();
        assert!(accepted.closed.try_recv().is_ok());
        assert!(connected.closed.try_recv().is_ok());
        assert!(client.runtime_tasks.is_empty());
        assert!(server.runtime_tasks.is_empty());
        let _outgoing = tokio::net::UdpSocket::bind(outgoing)
            .await
            .expect("outbound client socket retained");
        let _client = tokio::net::UdpSocket::bind(client_listener)
            .await
            .expect("client listener retained");
        let _server = tokio::net::UdpSocket::bind(server_addr)
            .await
            .expect("server listener retained");
    }

    // quic_connect_handshake：与 TCP 同断言集（Transport 抽象的意义）
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quic_connect_handshake_roundtrip() {
        // rustls CryptoProvider 进程级安装（ring——quinn 默认族）
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (in_a, _rx_a) = mpsc::channel::<InboundItem>(64);
        let (in_b, mut rx_b) = mpsc::channel::<InboundItem>(64);
        let noop: OnDisconnect = Arc::new(|_| {});
        let (sd_a, _) = tokio::sync::watch::channel(false);
        let (sd_b, _) = tokio::sync::watch::channel(false);
        let tb = QuicTransport::new(
            HandshakeBody {
                node_id: "quic-b".into(),
                ..Default::default()
            },
            in_b,
            noop.clone(),
            sd_b,
            "127.0.0.1:0".parse().unwrap(),
            dev_crypto(),
            None,
        )
        .unwrap();
        let ta = QuicTransport::new(
            HandshakeBody {
                node_id: "quic-a".into(),
                ..Default::default()
            },
            in_a,
            noop,
            sd_a,
            "127.0.0.1:0".parse().unwrap(),
            dev_crypto(),
            None,
        )
        .unwrap();
        let addr = NodeAddr::quic("quic-b", tb.local_addr());
        let (conn_b, conn_a) = tokio::try_join!(tb.accept(), ta.connect(&addr)).unwrap();
        assert_eq!(conn_a.node_id, "quic-b");
        assert_eq!(conn_b.node_id, "quic-a");

        conn_a
            .sender
            .send(Frame::ask(
                1,
                "/u/echo",
                "bin:x::P",
                bytes::Bytes::from_static(b"P"),
                None,
            ))
            .await
            .unwrap();
        let (got, _, _) = tokio::time::timeout(Duration::from_secs(5), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.header.frame_type, frame_type::ASK);
        assert_eq!(got.path, "/u/echo");
    }
}
