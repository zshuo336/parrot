//! mTLS helpers: tokio-rustls streams and leaf-subject CN identity checks.
//! Used by RemoteActorSystem TCP and QUIC when mutual TLS is configured.
//!
//! Callers supply a trusted CA bundle and node certificates (subject CN=node_id).
//! Multiple CA roots can be trusted together; certificate issuance, rotation,
//! revocation and automatic rotation are separate, unfinished capabilities.

use rustls_pki_types::pem::{Error as PemError, PemObject};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsConnector;

use crate::error::RemoteError;

/// Maximum encoded bytes per certificate chain, private key or CA bundle.
pub const MAX_TLS_PEM_BYTES: usize = 1024 * 1024;
/// Maximum certificates per chain or trust bundle, before root-store building.
pub const MAX_TLS_CERTIFICATES: usize = 64;

/// PEM material for the explicit TLS helper API.
#[derive(Clone)]
pub struct TlsConfig {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub ca_pem: Vec<u8>,
}

impl TlsConfig {
    /// Read regular files with bounded input. Symlinks to regular files are
    /// accepted so deployment tools can rotate credentials by replacing links.
    pub fn from_paths(cert_path: &str, key_path: &str, ca_path: &str) -> Result<Self, RemoteError> {
        Ok(Self {
            cert_pem: read_material(cert_path)?,
            key_pem: read_material(key_path)?,
            ca_pem: read_material(ca_path)?,
        })
    }

    fn validate_size(&self) -> Result<(), RemoteError> {
        for pem in [&self.cert_pem, &self.key_pem, &self.ca_pem] {
            check_material_size(pem.len() as u64)?;
        }
        Ok(())
    }
}

fn check_material_size(bytes: u64) -> Result<(), RemoteError> {
    if bytes > MAX_TLS_PEM_BYTES as u64 {
        return Err(RemoteError::Transport(format!(
            "TLS material limit: each PEM input must be at most {MAX_TLS_PEM_BYTES} bytes"
        )));
    }
    Ok(())
}

fn read_material(path: &str) -> Result<Vec<u8>, RemoteError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Inspect the opened descriptor, without blocking on a FIFO writer or
        // introducing a stat/open race for symlink-based deployments.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = options
        .open(path)
        .map_err(|e| RemoteError::Transport(format!("open TLS material {path}: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| RemoteError::Transport(format!("TLS material metadata {path}: {e}")))?;
    if !metadata.is_file() {
        return Err(RemoteError::Transport(
            "TLS material must be a regular file".into(),
        ));
    }
    check_material_size(metadata.len())?;
    read_material_bytes(file)
}

fn read_material_bytes(reader: impl std::io::Read) -> Result<Vec<u8>, RemoteError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    reader
        .take(MAX_TLS_PEM_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RemoteError::Transport(format!("read TLS material: {e}")))?;
    // A file growing after metadata inspection cannot bypass the read budget.
    check_material_size(bytes.len() as u64)?;
    Ok(bytes)
}

fn load_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, RemoteError> {
    check_material_size(pem.len() as u64)?;
    let mut certs = Vec::new();
    for cert in CertificateDer::pem_slice_iter(pem) {
        if certs.len() == MAX_TLS_CERTIFICATES {
            return Err(RemoteError::Transport(format!(
                "TLS material limit: at most {MAX_TLS_CERTIFICATES} certificates per PEM bundle"
            )));
        }
        certs.push(cert.map_err(|e| RemoteError::Transport(format!("cert parse: {e}")))?);
    }
    if certs.is_empty() {
        return Err(RemoteError::Transport(
            "no certificates in PEM material".into(),
        ));
    }
    Ok(certs)
}

fn load_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, RemoteError> {
    check_material_size(pem.len() as u64)?;
    // Preserve first-key selection across PKCS1, PKCS8 and SEC1. Invalid key
    // material must not fall through to another key that happens to match.
    PrivateKeyDer::from_pem_slice(pem).map_err(|error| match error {
        PemError::NoItemsFound => RemoteError::Transport("no private key in pem".into()),
        other => RemoteError::Transport(format!("key parse: {other}")),
    })
}

// Honor an explicitly installed provider. Otherwise select the already-enabled
// ring implementation locally; feature unification can enable multiple rustls
// backends, so relying on its implicit global selection would panic.
pub(crate) fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// server 侧 TLS acceptor（mTLS：要求客户端证书）。
pub fn server_config(cfg: &TlsConfig) -> Result<ServerConfig, RemoteError> {
    cfg.validate_size()?;
    let certs = load_certs(&cfg.cert_pem)?;
    let key = load_key(&cfg.key_pem)?;
    let mut ca = load_certs(&cfg.ca_pem)?;
    // 双证书容忍（P2）：CA 链可含多张根（新旧并存）
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for c in ca.drain(..) {
        roots
            .add(c)
            .map_err(|e| RemoteError::Transport(format!("ca add: {e}")))?;
    }
    let provider = crypto_provider();
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| RemoteError::Transport(format!("verifier: {e}")))?;
    let sc = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| RemoteError::Transport(format!("server TLS versions: {e}")))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| RemoteError::Transport(format!("server cert: {e}")))?;
    Ok(sc)
}

/// client 侧 connector（验证 server 证书链；本端证书 mTLS 呈递）。
pub fn client_config(cfg: &TlsConfig) -> Result<ClientConfig, RemoteError> {
    cfg.validate_size()?;
    let certs = load_certs(&cfg.cert_pem)?;
    let key = load_key(&cfg.key_pem)?;
    let mut ca = load_certs(&cfg.ca_pem)?;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for c in ca.drain(..) {
        roots
            .add(c)
            .map_err(|e| RemoteError::Transport(format!("ca add: {e}")))?;
    }
    let cc = ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| RemoteError::Transport(format!("client TLS versions: {e}")))?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| RemoteError::Transport(format!("client cert: {e}")))?;
    Ok(cc)
}

/// Build a TCP acceptor that requires verified client certificates.
pub fn server_acceptor(cfg: &TlsConfig) -> Result<tokio_rustls::TlsAcceptor, RemoteError> {
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(server_config(
        cfg,
    )?)))
}
/// Build a TCP connector with CA verification and client authentication.
pub fn client_connector(cfg: &TlsConfig) -> Result<TlsConnector, RemoteError> {
    Ok(TlsConnector::from(Arc::new(client_config(cfg)?)))
}
/// Node IDs in mTLS mode must also be usable as the TLS server name/SAN.
pub(crate) fn validate_local_identity(cfg: &TlsConfig, node: &str) -> Result<(), RemoteError> {
    cfg.validate_size()?;
    ServerName::try_from(node.to_string()).map_err(|e| {
        RemoteError::Transport(format!("mTLS node ID must be a valid TLS server name: {e}"))
    })?;
    verify_node_identity(&load_certs(&cfg.cert_pem)?, node)
}

/// TCP 流 → TLS 流（connect 侧）。
pub async fn wrap_client<S>(
    connector: &TlsConnector,
    io: S,
    node_id: &str,
) -> Result<impl AsyncRead + AsyncWrite + Unpin, RemoteError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let name = ServerName::try_from(node_id.to_string())
        .map_err(|e| RemoteError::Transport(format!("node_id as ServerName: {e}")))?;
    let stream = connector
        .connect(name, io)
        .await
        .map_err(|e| RemoteError::Transport(format!("tls connect: {e}")))?;
    Ok(stream)
}

/// TCP 流 → TLS 流（accept 侧）。
pub async fn wrap_server<S>(
    acceptor: &tokio_rustls::TlsAcceptor,
    io: S,
) -> Result<impl AsyncRead + AsyncWrite + Unpin, RemoteError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let stream = acceptor
        .accept(io)
        .await
        .map_err(|e| RemoteError::Transport(format!("tls accept: {e}")))?;
    Ok(stream)
}

/// node_id ↔ 证书 CN 强绑定校验（握手完成后调；不匹配断连——防身份伪造）。
/// TLS 层已完成链验证；此处校验对端呈现证书的 CN == 握手 TLV node_id。
pub fn verify_node_identity(
    peer_certs: &[CertificateDer<'_>],
    claimed_node_id: &str,
) -> Result<(), RemoteError> {
    let Some(cert) = peer_certs.first() else {
        return Err(RemoteError::Transport(
            "mTLS: peer presented no certificate".into(),
        ));
    };
    // Parse the certificate structure, not OID-shaped bytes anywhere in DER:
    // issuer, extensions and signatures cannot supply a leaf subject identity.
    let (remaining, parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|e| RemoteError::Transport(format!("mTLS: invalid certificate DER: {e}")))?;
    if !remaining.is_empty() {
        return Err(RemoteError::Transport(
            "mTLS: trailing certificate DER".into(),
        ));
    }
    let mut names = parsed.subject().iter_common_name();
    let name = names
        .next()
        .ok_or_else(|| RemoteError::Transport("mTLS: leaf subject has no CN".into()))?;
    if names.next().is_some() {
        return Err(RemoteError::Transport(
            "mTLS: ambiguous leaf subject CN".into(),
        ));
    }
    let cn = name
        .as_str()
        .map_err(|_| RemoteError::Transport("mTLS: invalid leaf subject CN string".into()))?;
    parrot_config::endpoint::validate_node_id(claimed_node_id)
        .map_err(|e| RemoteError::Transport(format!("mTLS: invalid claimed identity: {e}")))?;
    if cn != claimed_node_id {
        return Err(RemoteError::Transport(format!(
            "mTLS: node_id mismatch (cert CN={cn:?}, handshake={claimed_node_id:?})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_read_budget_also_bounds_growing_inputs() {
        use std::io::Read;
        struct Infinite {
            consumed: usize,
        }
        impl Read for Infinite {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                out.fill(b'x');
                self.consumed += out.len();
                Ok(out.len())
            }
        }
        let mut source = Infinite { consumed: 0 };
        let error = read_material_bytes(&mut source).unwrap_err().to_string();
        assert!(error.contains("TLS material limit"));
        assert_eq!(source.consumed, MAX_TLS_PEM_BYTES + 1);
        let exact = vec![b'x'; MAX_TLS_PEM_BYTES];
        assert_eq!(read_material_bytes(exact.as_slice()).unwrap(), exact);
    }

    // dev 材料：rcgen 自签 CA + 两节点证书（测试进程内生成）
    struct DevPki {
        node_a: Vec<u8>, // PEM cert
        node_a_key: Vec<u8>,
        node_b: Vec<u8>,
        node_b_key: Vec<u8>,
    }

    fn dev_pki() -> DevPki {
        // Self-signed fixtures here; tls_identity_contract also covers CA-issued leaves.
        let a = rcgen::generate_simple_self_signed(vec!["node-a".into()]).unwrap();
        let b = rcgen::generate_simple_self_signed(vec!["node-b".into()]).unwrap();
        DevPki {
            node_a: a.cert.pem().into_bytes(),
            node_a_key: a.signing_key.serialize_pem().into_bytes(),
            node_b: b.cert.pem().into_bytes(),
            node_b_key: b.signing_key.serialize_pem().into_bytes(),
        }
    }

    // tls_handshake_success：包 TCP 流 + 双向证书
    #[tokio::test]
    async fn tls_handshake_success() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let pki = dev_pki();
        // A→B：A 的 client 证书是 node-a；B 的 CA 信任…… dev 形态下 B 信任自己
        // （自签直连——验证 wrap 双路径通畅 + CN 提取）
        let cfg_b = TlsConfig {
            cert_pem: pki.node_b.clone(),
            key_pem: pki.node_b_key.clone(),
            ca_pem: pki.node_b.clone(), // 信任自签
        };
        let acceptor = server_acceptor(&cfg_b).unwrap();
        let (client_io, server_io) = tokio::io::duplex(8 * 1024);
        // client 用 B 自己的证书（dev 自签自证——链通过）
        let cfg_c = TlsConfig {
            cert_pem: pki.node_b.clone(),
            key_pem: pki.node_b_key.clone(),
            ca_pem: pki.node_b.clone(),
        };
        let connector = client_connector(&cfg_c).unwrap();
        let (server, client) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                wrap_server(&acceptor, server_io),
                wrap_client(&connector, client_io, "node-b")
            )
        })
        .await
        .expect("valid TLS handshake did not complete");
        server.unwrap();
        client.unwrap();
    }

    // tls_reject_bad_ca：client 不信任 server 证书 → 握手失败
    #[tokio::test]
    async fn tls_reject_bad_ca() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let pki = dev_pki();
        // server 用 node-b 证书；client 的 CA 是 node-a（不匹配）
        let cfg_b = TlsConfig {
            cert_pem: pki.node_b.clone(),
            key_pem: pki.node_b_key.clone(),
            ca_pem: pki.node_b.clone(),
        };
        let acceptor = server_acceptor(&cfg_b).unwrap();
        let (client_io, server_io) = tokio::io::duplex(8 * 1024);
        let bad_ca_cfg = TlsConfig {
            cert_pem: pki.node_a.clone(),
            key_pem: pki.node_a_key.clone(),
            ca_pem: pki.node_a.clone(), // 只信 node-a 链
        };
        let connector = client_connector(&bad_ca_cfg).unwrap();
        let (server, client) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                wrap_server(&acceptor, server_io),
                wrap_client(&connector, client_io, "node-b")
            )
        })
        .await
        .expect("bad CA handshake did not retire");
        assert!(client.is_err(), "bad CA must fail handshake");
        assert!(server.is_err(), "server must observe failed handshake");
    }

    /// 生成 CN=指定值的自签证书（rcgen param 显式 Subject）。
    fn cert_with_cn(cn: &str) -> CertificateDer<'static> {
        use rcgen::{CertificateParams, KeyPair};
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        cert.der().clone()
    }

    // tls_node_id_cn_mismatch：CN ≠ 握手 node_id → 断连（identity 校验函数）
    #[test]
    fn tls_node_id_cn_mismatch() {
        let der = cert_with_cn("node-real");
        // CN=node-real，声称 node-fake → 拒绝
        let err = verify_node_identity(&[der], "node-fake").unwrap_err();
        assert!(err.to_string().contains("mismatch"), "got {err:?}");
        // 匹配 → 通过
        let der2 = cert_with_cn("node-real");
        verify_node_identity(&[der2], "node-real").unwrap();
    }

    // 无证书呈现 → 拒绝
    #[test]
    fn tls_no_peer_cert_rejected() {
        assert!(verify_node_identity(&[], "x").is_err());
    }
}
