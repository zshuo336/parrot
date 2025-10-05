//! 职责：QuicTransport——quinn 0.11 + ALPN "parrot/1"（DEV_02 §4 / 06 §2.3）。
//!
//! 流模型：每连接一条 bi 流承载 Wire 1.0（控制+数据同流；帧格式不变）；
//! uni 流每 ask 一条是 P3 反压优化项——预留扩展点。
//!
//! TLS：quinn 自带 rustls。证书经 `QuicCrypto` 注入（server 必需；client
//! 验证策略可换）——dev 自签由测试侧生成（rcgen dev-dep），生产 CA 在 K4。

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::error::RemoteError;
use crate::handshake::HandshakeBody;
use crate::node::NodeAddr;
use crate::transport::{
    run_connection, ConnParams, ConnSide, ConnectionHandle, FrameSender, OnDisconnect, Transport,
};

type InboundItem = (crate::frame::Frame, FrameSender, String);

pub const ALPN_PARROT_1: &[u8] = b"parrot/1";

/// TLS 材料包（server 配置 + client 验证策略）——测试用 rcgen 生成注入；
/// K4 mTLS 后由 tls.rs 的 CA 体系构造。
#[derive(Clone)]
pub struct QuicCrypto {
    pub server: quinn::ServerConfig,
    pub client: quinn::ClientConfig,
}

pub struct QuicTransport {
    handshake: HandshakeBody,
    inbound: mpsc::Sender<InboundItem>,
    on_disconnect: OnDisconnect,
    shutdown: tokio::sync::watch::Sender<bool>,
    knobs: Option<crate::transport::RuntimeKnobs>,
    endpoint: quinn::Endpoint,
    client_cfg: quinn::ClientConfig,
}

impl QuicTransport {
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
        let endpoint = quinn::Endpoint::server(crypto.server, bind)
            .map_err(|e| RemoteError::Transport(format!("quic bind {bind}: {e}")))?;
        Ok(Self {
            handshake,
            inbound,
            on_disconnect,
            shutdown,
            knobs,
            endpoint,
            client_cfg: client,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap())
    }

    /// uni 流扩展点（P3 反压优化：每 ask 一条 uni 流）。
    #[allow(dead_code)]
    async fn open_uni(&self, _conn: &quinn::Connection) {}
}

#[async_trait::async_trait]
impl Transport for QuicTransport {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError> {
        let mut ep = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| RemoteError::Transport(format!("quic client ep: {e}")))?;
        ep.set_default_client_config(self.client_cfg.clone());
        let conn = ep
            .connect(addr.addr, "localhost")
            .map_err(|e| RemoteError::Transport(format!("quic connect {}: {e}", addr.addr)))?
            .await
            .map_err(|e| RemoteError::Transport(format!("quic handshake {}: {e}", addr.addr)))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| RemoteError::Transport(format!("quic open_bi: {e}")))?;
        run_connection(
            quic_io(recv, send),
            ConnParams {
                side: ConnSide::Connect,
                local_addr: Some(self.local_addr()),
                peer_addr: Some(addr.addr),
                scheme: "quic",
                local_handshake: self.handshake.clone(),
                inbound: self.inbound.clone(),
                on_disconnect: self.on_disconnect.clone(),
                shutdown: self.shutdown.subscribe(),
                knobs: self.knobs,
            },
        )
        .await
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<(), RemoteError> {
        Ok(()) // endpoint 已在 new() 绑定
    }

    async fn accept(&self) -> Result<ConnectionHandle, RemoteError> {
        let Some(incoming) = self.endpoint.accept().await else {
            return Err(RemoteError::Transport("quic endpoint closed".into()));
        };
        let conn = incoming
            .await
            .map_err(|e| RemoteError::Transport(format!("quic accept: {e}")))?;
        let (send, recv) = conn
            .accept_bi()
            .await
            .map_err(|e| RemoteError::Transport(format!("quic accept_bi: {e}")))?;
        run_connection(
            quic_io(recv, send),
            ConnParams {
                side: ConnSide::Accept,
                local_addr: Some(self.local_addr()),
                peer_addr: Some(conn.remote_address()),
                scheme: "quic",
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
        "quic"
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.endpoint.local_addr().ok()
    }
}

/// RecvStream(读) + SendStream(写) 组合为单 IO 对象。
fn quic_io(
    recv: quinn::RecvStream,
    send: quinn::SendStream,
) -> impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {
    tokio::io::join(recv, send)
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

/// dev 自签 crypto 包（rcgen 进程内生成 + client 跳过校验）——
/// `RemoteConfig::quic` 默认注入；生产换 `QuicCrypto`（K4 CA 体系）。
pub fn dev_crypto_insecure() -> QuicCrypto {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
    server_crypto.alpn_protocols = vec![ALPN_PARROT_1.to_vec()];
    let server = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap(),
    ));
    let mut client_crypto = rustls::ClientConfig::builder()
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
        let addr = NodeAddr::tcp("quic-b", tb.local_addr());
        let accept_fut = tokio::spawn(async move { tb.accept().await });
        let conn_a = ta.connect(&addr).await.expect("quic connect");
        let conn_b = accept_fut.await.unwrap().expect("quic accept");
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
