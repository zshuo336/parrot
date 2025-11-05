//! Node process configuration: explicit environment > file > node defaults.
use parrot_config::{ConfigError, ParrotConfig, Resolved};
use std::{ffi::OsString, path::PathBuf};

/// Bound for native trust metadata; parsing this list does not authorize code.
pub const MAX_NATIVE_TRUST_DIGESTS: usize = 256;

/// Parse the executable's explicit operator request. This returns ordinary
/// strings, never execution authority. Library embedders must separately make
/// the unsafe NativeTrust grant; NodeSettings/RunningNode::start do not do so.
pub fn native_digest_requests(value: Option<OsString>) -> Result<Vec<String>, ConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let text = unicode("PARROT_TRUST_NATIVE_SHA256", value)?;
    if text.len() > MAX_NATIVE_TRUST_DIGESTS * 72 {
        return Err(invalid(
            "PARROT_TRUST_NATIVE_SHA256",
            "native trust metadata exceeds byte limit",
        ));
    }
    let digests: Vec<_> = if text.trim().is_empty() {
        Vec::new()
    } else {
        text.split(',').map(|s| s.trim().to_owned()).collect()
    };
    let digests =
        normalize_native_digests(&digests).map_err(|e| invalid("PARROT_TRUST_NATIVE_SHA256", e))?;
    if !digests.is_empty() && !cfg!(feature = "dylib") {
        return Err(invalid(
            "PARROT_TRUST_NATIVE_SHA256",
            "requires the dylib feature",
        ));
    }
    Ok(digests)
}
pub(crate) fn normalize_native_digests(digests: &[String]) -> Result<Vec<String>, String> {
    if digests.len() > MAX_NATIVE_TRUST_DIGESTS {
        return Err(format!(
            "native trust list exceeds {MAX_NATIVE_TRUST_DIGESTS} entries"
        ));
    }
    let mut normalized = Vec::with_capacity(digests.len());
    for digest in digests {
        let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("native trust requires SHA-256 digests of 64 hex digits".into());
        }
        normalized.push(digest.to_ascii_lowercase());
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

#[derive(Debug)]
pub struct NodeSettings {
    pub resolved: Resolved,
    pub actors: Vec<String>,
    pub ready_file: Option<PathBuf>,
    /// Optional host epoch budget, independent of guest business configuration.
    pub wasm_epoch_ticks: Option<u32>,
}
impl NodeSettings {
    pub fn load() -> Result<Self, ConfigError> {
        Self::from_environment(|key| std::env::var_os(key))
    }

    /// Inject environment reads without modifying process-global environment.
    pub fn from_environment(
        mut env: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self, ConfigError> {
        let mut cfg = match env("PARROT_CONFIG") {
            Some(path) => ParrotConfig::builder().load_file(path)?,
            None => ParrotConfig::builder().load_optional_file("parrot.toml")?,
        };
        for (key, field) in [
            ("PARROT_NODE_ID", &mut cfg.remote.node.node_id),
            ("PARROT_BIND", &mut cfg.remote.node.bind),
            ("PARROT_SCHEME", &mut cfg.remote.node.scheme),
            ("PARROT_TOPOLOGY_ROLE", &mut cfg.remote.node.topology_role),
            ("PARROT_SECURITY_MODE", &mut cfg.remote.security.mode),
            ("PARROT_TLS_CERT", &mut cfg.remote.security.cert_path),
            ("PARROT_TLS_KEY", &mut cfg.remote.security.key_path),
            ("PARROT_TLS_CA", &mut cfg.remote.security.ca_path),
        ] {
            if let Some(value) = env(key) {
                *field = Some(unicode(key, value)?);
            }
        }
        if let Some(value) = env("PARROT_SEEDS") {
            cfg.remote.node.seeds = Some(seed_strings(
                &unicode("PARROT_SEEDS", value)?,
                cfg.remote.node.scheme.as_deref().unwrap_or("tcp"),
            )?);
        }
        if let Some(value) = env("PARROT_ADMIN_NODES") {
            let value = unicode("PARROT_ADMIN_NODES", value)?;
            cfg.remote.security.admin_nodes = Some(if value.trim().is_empty() {
                Vec::new()
            } else {
                value.split(',').map(|s| s.trim().to_owned()).collect()
            });
        }
        for key in [
            "PARROT_DIRECTORY_PATH",
            "PARROT_DIRECTORY_GROUP",
            "PARROT_DIRECTORY_PEERS",
            "PARROT_DIRECTORY_ELECTION_MS",
            "PARROT_DIRECTORY_HEARTBEAT_MS",
            "PARROT_DIRECTORY_RPC_TIMEOUT_MS",
        ] {
            if let Some(value) = env(key) {
                let value = unicode(key, value)?;
                let section = cfg.remote.directory.get_or_insert_with(Default::default);
                match key {
                    "PARROT_DIRECTORY_PATH" => section.storage_dir = Some(value),
                    "PARROT_DIRECTORY_GROUP" => section.group = Some(value),
                    "PARROT_DIRECTORY_PEERS" => {
                        section.peers = Some(seed_strings(
                            &value,
                            cfg.remote.node.scheme.as_deref().unwrap_or("tcp"),
                        )?)
                    }
                    _ => {
                        let value = value
                            .parse()
                            .map_err(|_| invalid(key, "expected unsigned milliseconds"))?;
                        match key {
                            "PARROT_DIRECTORY_ELECTION_MS" => section.election_ms = Some(value),
                            "PARROT_DIRECTORY_HEARTBEAT_MS" => section.heartbeat_ms = Some(value),
                            _ => section.rpc_timeout_ms = Some(value),
                        }
                    }
                }
            }
        }
        let actors = env("PARROT_ACTORS")
            .map(|v| unicode("PARROT_ACTORS", v))
            .transpose()?;
        let all = crate::builtin_manifest_components_from(None);
        if let Some(filter) = &actors {
            if !filter.trim().is_empty() {
                for name in filter.split(',').map(str::trim) {
                    if !all.iter().any(|known| known == name) {
                        return Err(invalid(
                            "PARROT_ACTORS",
                            format!("unknown builtin actor: {name:?}"),
                        ));
                    }
                }
            }
        }
        let ready_file = env("PARROT_READY_FILE")
            .map(|path| {
                if path.is_empty() {
                    Err(invalid("PARROT_READY_FILE", "path must not be empty"))
                } else {
                    Ok(PathBuf::from(path))
                }
            })
            .transpose()?;
        let wasm_epoch_ticks = env("PARROT_WASM_EPOCH_TICKS")
            .map(|value| {
                unicode("PARROT_WASM_EPOCH_TICKS", value)?
                    .parse::<u32>()
                    .map_err(|_| invalid("PARROT_WASM_EPOCH_TICKS", "expected 1..=4294967295"))
            })
            .transpose()?;
        // Ordinary nodes have convenient defaults. Persistent voting members
        // must choose their identity and listener before those defaults apply.
        if cfg.remote.directory.is_some()
            && (cfg.remote.node.node_id.is_none()
                || (cfg.remote.node.scheme.as_deref().unwrap_or("tcp") != "mem"
                    && cfg.remote.node.bind.is_none()))
        {
            return Err(invalid(
                "remote.directory",
                "requires an explicit node identity and network listen address",
            ));
        }
        let defaults = ParrotConfig::builder()
            .remote_node_id("node-1")
            .remote_bind("127.0.0.1:9801");
        let settings = Self {
            resolved: cfg.with_fallback(defaults).build()?,
            actors: crate::builtin_manifest_components_from(actors.as_deref()),
            ready_file,
            wasm_epoch_ticks,
        };
        settings.validate()?;
        Ok(settings)
    }
    /// Validate all process settings, including directly constructed values.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_directory()?;
        if let Some(ticks) = self.wasm_epoch_ticks {
            if ticks == 0 {
                return Err(invalid(
                    "PARROT_WASM_EPOCH_TICKS",
                    "expected 1..=4294967295",
                ));
            }
            if !cfg!(feature = "wasm") {
                return Err(invalid(
                    "PARROT_WASM_EPOCH_TICKS",
                    "requires the wasm feature",
                ));
            }
        }
        Ok(())
    }
    /// A standard node may only advertise Directory when it owns its service.
    /// Recheck this for callers that construct NodeSettings directly.
    pub fn validate_directory(&self) -> Result<(), ConfigError> {
        let directory_role =
            self.resolved.remote.node.topology_role == parrot_config::TopologyRoleValue::Directory;
        if directory_role != self.resolved.remote.directory.is_some() {
            return Err(invalid("remote.directory", "Directory role and explicit storage/group/peers configuration must be enabled together"));
        }
        if directory_role
            && (self.resolved.remote.node.node_id.is_none()
                || (self.resolved.remote.node.scheme != "mem"
                    && self.resolved.remote.node.bind.is_none()))
        {
            return Err(invalid(
                "remote.directory",
                "requires an explicit node identity and network listen address",
            ));
        }
        Ok(())
    }
}
fn unicode(key: &str, value: OsString) -> Result<String, ConfigError> {
    value
        .into_string()
        .map_err(|_| invalid(key, "must be valid UTF-8"))
}
fn invalid(key: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.into(),
        reason: reason.into(),
    }
}

/// Legacy id=host:port and normal scheme://id@host:port share one validator.
pub fn seed_strings(value: &str, scheme: &str) -> Result<Vec<String>, ConfigError> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .enumerate()
        .map(|(index, raw)| {
            let raw = raw.trim();
            let endpoint = if let Some((id, address)) = raw.split_once('=') {
                format!("{scheme}://{}@{}", id.trim(), address.trim())
            } else {
                raw.to_owned()
            };
            let parsed = parrot_config::endpoint::SeedEndpoint::parse(&endpoint, scheme)
                .map_err(|e| invalid(&format!("PARROT_SEEDS[{index}]"), e))?;
            if parsed.scheme != scheme {
                return Err(invalid(
                    "PARROT_SEEDS",
                    "seed transport must match node transport",
                ));
            }
            Ok(endpoint)
        })
        .collect()
}
