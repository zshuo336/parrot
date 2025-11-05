//! Deployment grants for the local node and authenticated direct peers.
use crate::{endpoint::validate_node_id, ConfigError};
use std::collections::BTreeMap;

/// `[remote.receptionist]`. Each explicit map replaces the lower-priority map,
/// including an empty map. Layers never silently union security grants.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReceptionistSection {
    pub default_deny: Option<bool>,
    pub roles: Option<BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone)]
pub struct ReceptionistResolved {
    pub default_deny: bool,
    pub roles: BTreeMap<String, Vec<String>>,
}

impl ReceptionistSection {
    pub(crate) fn resolve(&self) -> Result<ReceptionistResolved, ConfigError> {
        let result = ReceptionistResolved {
            default_deny: self.default_deny.unwrap_or(true),
            roles: self.roles.clone().unwrap_or_default(),
        };
        result.validate()?;
        Ok(result)
    }
}

impl ReceptionistResolved {
    /// Validate again when converting a publicly constructed resolved config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |reason| ConfigError::Invalid {
            key: "remote.receptionist.roles".into(),
            reason,
        };
        for (node, prefixes) in &self.roles {
            validate_node_id(node).map_err(&invalid)?;
            if !valid_segments(node) {
                return Err(invalid(format!("invalid ACL node identity: {node:?}")));
            }
            for prefix in prefixes {
                if !valid_segments(prefix.strip_suffix('/').unwrap_or(prefix)) {
                    return Err(invalid(format!(
                        "invalid service prefix for {node:?}: {prefix:?}"
                    )));
                }
            }
        }
        Ok(())
    }
}

// Match the runtime ACL's segment grammar, including its optional trailing slash.
fn valid_segments(value: &str) -> bool {
    !value.is_empty()
        && !value.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '%' | '?' | '#' | '*' | '\\')
        })
        && value.split('/').all(|s| !matches!(s, "" | "." | ".."))
}
