//! Transport identity and control authorization. Claimed HELLO names are not credentials.
use crate::{error::RemoteError, tls::TlsConfig, transport::FrameSender};
use std::{collections::BTreeSet, net::SocketAddr};

#[derive(Clone, Default)]
pub enum RemoteSecurity {
    /// Plain TCP only on loopback; privileged network commands are denied.
    /// Memory connections are trusted within the owning process.
    #[default]
    Local,
    /// Verified peer certificates plus node CN binding. Administrative access
    /// additionally requires membership in the explicit node allowlist.
    MutualTls {
        credentials: TlsConfig,
        admin_nodes: BTreeSet<String>,
    },
    /// Explicit development opt-in: unauthenticated network and control access.
    InsecureDevelopment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAuthentication {
    Unauthenticated,
    InProcess,
    MutualTls,
}

#[derive(Debug, Clone, Default)]
pub enum ControlPolicy {
    #[default]
    LocalOnly,
    AuthenticatedNodes(BTreeSet<String>),
    InsecureDevelopment,
}
impl ControlPolicy {
    /// A verified cluster certificate grants cluster participation; administrative
    /// actions still need the separate node allowlist. Memory stays process-local.
    pub fn allows_cluster(&self, peer: &FrameSender) -> bool {
        match self {
            Self::LocalOnly => peer.authentication() == PeerAuthentication::InProcess,
            Self::AuthenticatedNodes(_) => peer.authentication() == PeerAuthentication::MutualTls,
            Self::InsecureDevelopment => true,
        }
    }

    pub fn allows(&self, peer: &FrameSender) -> bool {
        match self {
            Self::LocalOnly => peer.authentication() == PeerAuthentication::InProcess,
            Self::AuthenticatedNodes(nodes) => {
                peer.authentication() == PeerAuthentication::MutualTls
                    && nodes.contains(peer.node_id())
            }
            Self::InsecureDevelopment => true,
        }
    }
}
impl RemoteSecurity {
    pub(crate) fn control_policy(&self) -> ControlPolicy {
        match self {
            Self::Local => ControlPolicy::LocalOnly,
            Self::MutualTls { admin_nodes, .. } => {
                ControlPolicy::AuthenticatedNodes(admin_nodes.clone())
            }
            Self::InsecureDevelopment => ControlPolicy::InsecureDevelopment,
        }
    }
    pub(crate) fn credentials(&self) -> Option<&TlsConfig> {
        match self {
            Self::MutualTls { credentials, .. } => Some(credentials),
            _ => None,
        }
    }
    pub(crate) fn validate(
        &self,
        node: &str,
        scheme: &str,
        bind: Option<SocketAddr>,
    ) -> Result<(), RemoteError> {
        if let Some(bind) = bind {
            self.check_address(bind)?;
        }
        match self {
            Self::Local if scheme == "quic" => Err(RemoteError::Transport(
                "QUIC requires mTLS credentials or explicit insecure development mode".into(),
            )),
            Self::MutualTls {
                credentials,
                admin_nodes,
            } => {
                if scheme == "mem" {
                    return Err(RemoteError::Transport(
                        "mTLS is not applicable to the in-process memory transport".into(),
                    ));
                }
                crate::tls::validate_local_identity(credentials, node)?;
                for node in admin_nodes {
                    parrot_config::endpoint::validate_node_id(node)
                        .map_err(RemoteError::Transport)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub(crate) fn check_address(&self, address: SocketAddr) -> Result<(), RemoteError> {
        if matches!(self, Self::Local) && !address.ip().is_loopback() {
            Err(RemoteError::Transport(
                "non-loopback networking requires mTLS or explicit insecure development mode"
                    .into(),
            ))
        } else {
            Ok(())
        }
    }
}

// Only transports that have already verified the TLS chain may supply certs.
// This type is private to the crate; wire input cannot select its trust class.
pub(crate) enum PeerTrust {
    Unauthenticated,
    InProcess,
    Expected(Box<PeerTrust>, String),
    Certificates(Vec<rustls::pki_types::CertificateDer<'static>>),
}
impl PeerTrust {
    pub(crate) fn expecting(self, node: &str) -> Self {
        Self::Expected(Box::new(self), node.to_owned())
    }
    pub(crate) fn verify(&self, claim: &str) -> Result<(), RemoteError> {
        parrot_config::endpoint::validate_node_id(claim).map_err(|error| {
            RemoteError::Handshake(format!("invalid peer node identity: {error}"))
        })?;
        match self {
            Self::Expected(trust, expected) => {
                if claim != expected {
                    return Err(RemoteError::Handshake(format!(
                        "node identity mismatch: expected node {expected:?}, received {claim:?}"
                    )));
                }
                trust.verify(claim)
            }
            Self::Certificates(certs) => crate::tls::verify_node_identity(certs, claim),
            _ => Ok(()),
        }
    }
    pub(crate) fn authentication(&self) -> PeerAuthentication {
        match self {
            Self::Unauthenticated => PeerAuthentication::Unauthenticated,
            Self::InProcess => PeerAuthentication::InProcess,
            Self::Certificates(_) => PeerAuthentication::MutualTls,
            Self::Expected(trust, _) => trust.authentication(),
        }
    }
}

/// Bound TCP/QUIC establishment, including TLS and the application handshake,
/// by one deadline. The transport owner independently cancels this on shutdown.
pub(crate) async fn handshake_deadline<T>(
    future: impl std::future::Future<Output = Result<T, RemoteError>>,
) -> Result<T, RemoteError> {
    tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .map_err(|_| RemoteError::Handshake("connection establishment timeout 5s".into()))?
}
