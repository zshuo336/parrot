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
    pub relays: Option<BTreeMap<String, Vec<String>>>,
    pub max_subscriptions: Option<usize>,
    pub max_subscription_bytes: Option<usize>,
    pub max_registrations: Option<usize>,
    pub max_registration_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ReceptionistResolved {
    pub default_deny: bool,
    pub roles: BTreeMap<String, Vec<String>>,
    pub relays: BTreeMap<String, Vec<String>>,
    pub subscriptions: SubscriptionLimits,
    pub registrations: RegistrationLimits,
}

/// Distinct table entries and declared retained table bytes, across all nodes.
/// Zero disables additional entries; removals and identical republication remain valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationLimits {
    pub max_registrations: usize,
    pub max_reserved_bytes: usize,
}

impl Default for RegistrationLimits {
    fn default() -> Self {
        Self {
            max_registrations: 65_536,
            max_reserved_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Receiver-lifetime admission. Zero in either dimension disables new streams.
/// Bytes reserve queue slots at the fixed maximum event size, not current fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionLimits {
    pub max_subscriptions: usize,
    pub max_reserved_bytes: usize,
}

impl Default for SubscriptionLimits {
    fn default() -> Self {
        Self {
            max_subscriptions: 1024,
            max_reserved_bytes: 256 * 1024 * 1024,
        }
    }
}

impl ReceptionistSection {
    pub(crate) fn resolve(&self) -> Result<ReceptionistResolved, ConfigError> {
        let result = ReceptionistResolved {
            default_deny: self.default_deny.unwrap_or(true),
            roles: self.roles.clone().unwrap_or_default(),
            relays: self.relays.clone().unwrap_or_default(),
            subscriptions: SubscriptionLimits {
                max_subscriptions: self
                    .max_subscriptions
                    .unwrap_or_else(|| SubscriptionLimits::default().max_subscriptions),
                max_reserved_bytes: self
                    .max_subscription_bytes
                    .unwrap_or_else(|| SubscriptionLimits::default().max_reserved_bytes),
            },
            registrations: RegistrationLimits {
                max_registrations: self
                    .max_registrations
                    .unwrap_or_else(|| RegistrationLimits::default().max_registrations),
                max_reserved_bytes: self
                    .max_registration_bytes
                    .unwrap_or_else(|| RegistrationLimits::default().max_reserved_bytes),
            },
        };
        result.validate()?;
        Ok(result)
    }
}

impl ReceptionistResolved {
    /// Validate again when converting a publicly constructed resolved config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, grants) in [("roles", &self.roles), ("relays", &self.relays)] {
            let invalid = |reason| ConfigError::Invalid {
                key: format!("remote.receptionist.{name}"),
                reason,
            };
            for (node, prefixes) in grants {
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
