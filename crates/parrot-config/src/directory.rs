//! Optional fixed-membership Directory host configuration. Discovery seeds do
//! not define voting membership; an explicit empty peer list selects a solo group.
use crate::{endpoint::SeedEndpoint, ConfigError, RemoteNodeSection, RemoteSecurityResolved};

/// Shared timing contract for configured and directly assembled Directory hosts.
#[derive(Debug, Clone, Copy)]
pub struct DirectoryTiming {
    pub election_ms: u64,
    pub heartbeat_ms: u64,
    pub rpc_timeout_ms: u64,
}
impl Default for DirectoryTiming {
    fn default() -> Self {
        Self {
            election_ms: 2000,
            heartbeat_ms: 150,
            rpc_timeout_ms: 5000,
        }
    }
}
impl DirectoryTiming {
    pub fn validate(self) -> Result<(), ConfigError> {
        if !(100..=60_000).contains(&self.election_ms)
            || !(20..=self.election_ms / 2).contains(&self.heartbeat_ms)
            || !(self.heartbeat_ms..=60_000).contains(&self.rpc_timeout_ms)
        {
            return Err(invalid("timing", "invalid election/heartbeat/RPC timing"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DirectorySection {
    pub storage_dir: Option<String>,
    pub group: Option<String>,
    pub peers: Option<Vec<String>>,
    pub election_ms: Option<u64>,
    pub heartbeat_ms: Option<u64>,
    pub rpc_timeout_ms: Option<u64>,
}
#[derive(Debug, Clone)]
pub struct DirectoryResolved {
    pub storage_dir: String,
    pub group: String,
    pub peers: Vec<String>,
    pub election_ms: u64,
    pub heartbeat_ms: u64,
    pub rpc_timeout_ms: u64,
}
impl DirectorySection {
    pub(crate) fn with_fallback(mut self, fallback: Self) -> Self {
        self.storage_dir = self.storage_dir.or(fallback.storage_dir);
        self.group = self.group.or(fallback.group);
        self.peers = self.peers.or(fallback.peers);
        self.election_ms = self.election_ms.or(fallback.election_ms);
        self.heartbeat_ms = self.heartbeat_ms.or(fallback.heartbeat_ms);
        self.rpc_timeout_ms = self.rpc_timeout_ms.or(fallback.rpc_timeout_ms);
        self
    }
    pub(crate) fn resolve(
        &self,
        node: &RemoteNodeSection,
        security: &RemoteSecurityResolved,
    ) -> Result<DirectoryResolved, ConfigError> {
        let required = |field, value: &Option<String>, max| {
            value
                .as_ref()
                .filter(|s| {
                    !s.trim().is_empty() && s.len() <= max && !s.chars().any(char::is_control)
                })
                .cloned()
                .ok_or_else(|| invalid(field, "required, bounded and without control characters"))
        };
        let storage_dir = required("storage_dir", &self.storage_dir, 4096)?;
        let group = required("group", &self.group, 1011)?;
        let scheme = node.scheme.as_deref().unwrap_or("tcp");
        if !matches!(security, RemoteSecurityResolved::MutualTls { .. })
            && !(scheme == "mem" && matches!(security, RemoteSecurityResolved::Local))
        {
            return Err(ConfigError::Invalid {
                key: "remote.security.mode".into(),
                reason: "Directory requires mTLS, or Local memory transport".into(),
            });
        }
        let peers = self.peers.clone().ok_or_else(|| {
            invalid(
                "peers",
                "explicit voting members required; use [] for a solo group",
            )
        })?;
        if peers.len() > 16 {
            return Err(invalid("peers", "at most 16 remote voting members"));
        }
        let mut ids = std::collections::HashSet::new();
        for (i, peer) in peers.iter().enumerate() {
            let endpoint = SeedEndpoint::parse(peer, scheme)
                .map_err(|e| invalid(&format!("peers[{i}]"), e))?;
            if !endpoint.identity_is_explicit
                || endpoint.scheme != scheme
                || node.node_id.as_deref() == Some(&endpoint.node_id)
                || !ids.insert(endpoint.node_id)
            {
                return Err(invalid("peers", "each member needs a unique, explicit, non-self identity and matching transport"));
            }
        }
        let defaults = DirectoryTiming::default();
        let result = DirectoryResolved {
            storage_dir,
            group,
            peers,
            election_ms: self.election_ms.unwrap_or(defaults.election_ms),
            heartbeat_ms: self.heartbeat_ms.unwrap_or(defaults.heartbeat_ms),
            rpc_timeout_ms: self.rpc_timeout_ms.unwrap_or(defaults.rpc_timeout_ms),
        };
        DirectoryTiming {
            election_ms: result.election_ms,
            heartbeat_ms: result.heartbeat_ms,
            rpc_timeout_ms: result.rpc_timeout_ms,
        }
        .validate()?;
        Ok(result)
    }
}
fn invalid(field: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: format!("remote.directory.{field}"),
        reason: reason.into(),
    }
}
