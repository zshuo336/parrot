//! 职责：mTLS——TCP 路径 tokio-rustls 包裹 + node_id↔证书 CN 强绑定（DEV_02 §5 / 06 §2.4）。
//!
//! 证书体系（P2）：自签 CA + 节点证书（CN=node_id）；`parrot-remote-cli cert gen`
//! 生成（开发环境 10 分钟可用）。P3 完整证书轮换——P2 只做双证书容忍（CA 链
//! 同时信任新旧）。

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsConnector;

use crate::error::RemoteError;

/// TLS 材料配置（RemoteConfig.tls 注入）。
#[derive(Clone)]
pub struct TlsConfig {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub ca_pem: Vec<u8>,
}

impl TlsConfig {
    /// 文件路径加载。
    pub fn from_paths(
        cert_path: &str,
        key_path: &str,
        ca_path: &str,
    ) -> Result<Self, RemoteError> {
        Ok(Self {
            cert_pem: std::fs::read(cert_path)
                .map_err(|e| RemoteError::Transport(format!("read {cert_path}: {e}")))?,
            key_pem: std::fs::read(key_path)
                .map_err(|e| RemoteError::Transport(format!("read {key_path}: {e}")))?,
            ca_pem: std::fs::read(ca_path)
                .map_err(|e| RemoteError::Transport(format!("read {ca_path}: {e}")))?,
        })
    }
}

fn load_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, RemoteError> {
    let mut rd = std::io::BufReader::new(pem);
    rustls_pemfile::certs(&mut rd)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| RemoteError::Transport(format!("cert parse: {e}")))
}

fn load_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, RemoteError> {
    let mut rd = std::io::BufReader::new(pem);
    rustls_pemfile::private_key(&mut rd)
        .map_err(|e| RemoteError::Transport(format!("key parse: {e}")))?
        .ok_or_else(|| RemoteError::Transport("no private key in pem".into()))
}

/// server 侧 TLS acceptor（mTLS：要求客户端证书）。
pub fn server_acceptor(cfg: &TlsConfig) -> Result<tokio_rustls::TlsAcceptor, RemoteError> {
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
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| RemoteError::Transport(format!("verifier: {e}")))?;
    let sc = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| RemoteError::Transport(format!("server cert: {e}")))?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(sc)))
}

/// client 侧 connector（验证 server 证书链；本端证书 mTLS 呈递）。
pub fn client_connector(cfg: &TlsConfig) -> Result<TlsConnector, RemoteError> {
    let certs = load_certs(&cfg.cert_pem)?;
    let key = load_key(&cfg.key_pem)?;
    let mut ca = load_certs(&cfg.ca_pem)?;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for c in ca.drain(..) {
        roots
            .add(c)
            .map_err(|e| RemoteError::Transport(format!("ca add: {e}")))?;
    }
    let cc = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| RemoteError::Transport(format!("client cert: {e}")))?;
    Ok(TlsConnector::from(Arc::new(cc)))
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
        return Err(RemoteError::Transport("mTLS: peer presented no certificate".into()));
    };
    // 提取 CN（OU 简化解析——x509-parser 不进白名单；CN 在 Subject DN 可打印串）
    let cn = extract_cn(cert).ok_or_else(|| {
        RemoteError::Transport("mTLS: certificate has no CN".into())
    })?;
    if cn != claimed_node_id {
        return Err(RemoteError::Transport(format!(
            "mTLS: node_id mismatch (cert CN={cn:?}, handshake={claimed_node_id:?})"
        )));
    }
    Ok(())
}

/// 最小 CN 提取：Subject DN 中 CN 的值是 UTF8String/PrintableString——
/// DER 里以可打印字节直存。策略：定位声称 node_id 的字节串并要求其
/// 紧邻 OID 上下文（2.5.4.3 CN 的 DER `06 03 55 04 03` + 长度 + 串）。
fn extract_cn(cert: &CertificateDer<'_>) -> Option<String> {
    let der = cert.as_ref();
    // CN OID DER 编码：06 03 55 04 03（1.2.840.113549.1.9.1 之外的常用位）
    const CN_OID: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];
    let mut pos = 0;
    while let Some(off) = find(der, CN_OID, pos) {
        // OID 后：[A0/13/0C len] 值（0C=UTF8String, 13=PrintableString）
        // 跳过可选 tag（context [4] 0xA4 等包装）——直接扫可打印串 tag
        let scan_end = (off + CN_OID.len() + 4).min(der.len());
        for i in (off + CN_OID.len())..scan_end {
            let (tag, len) = (der[i], der[i + 1] as usize);
            if tag == 0x0C || tag == 0x13 {
                let start = i + 2;
                let end = start + len;
                if end <= der.len() {
                    return Some(String::from_utf8_lossy(&der[start..end]).into_owned());
                }
                return None;
            }
        }
        pos = off + 1;
    }
    None
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    // dev 材料：rcgen 自签 CA + 两节点证书（测试进程内生成）
    struct DevPki {
        node_a: Vec<u8>, // PEM cert
        node_a_key: Vec<u8>,
        node_b: Vec<u8>,
        node_b_key: Vec<u8>,
    }

    fn dev_pki() -> DevPki {
        // 简化：直接自签两份（互为 CA 的 dev 形态——分离 CA 链在 cli cert gen）
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
        let acc = acceptor.clone();
        let server_task = tokio::spawn(async move { wrap_server(&acc, server_io).await });
        // client 用 B 自己的证书（dev 自签自证——链通过）
        let cfg_c = TlsConfig {
            cert_pem: pki.node_b.clone(),
            key_pem: pki.node_b_key.clone(),
            ca_pem: pki.node_b.clone(),
        };
        let connector = client_connector(&cfg_c).unwrap();
        let _tls = wrap_client(&connector, client_io, "node-b").await.unwrap();
        let _tls_server = server_task.await.unwrap().unwrap();
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
        let acc = acceptor.clone();
        let _server_task = tokio::spawn(async move {
            let _ = wrap_server(&acc, server_io).await;
        });
        let bad_ca_cfg = TlsConfig {
            cert_pem: pki.node_a.clone(),
            key_pem: pki.node_a_key.clone(),
            ca_pem: pki.node_a.clone(), // 只信 node-a 链
        };
        let connector = client_connector(&bad_ca_cfg).unwrap();
        let r = wrap_client(&connector, client_io, "node-b").await;
        assert!(r.is_err(), "bad CA must fail handshake");
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
