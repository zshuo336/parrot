use super::super::operator_policy::load_policy_file;
use super::{messaging, DeployError, WasmMessageGrant};
use std::path::Path;

/// An explicit operator policy, independent of guest and manifest configuration.
/// Construction validates all grants; the policy cannot change after binding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WasmMessagingPolicy {
    grants: Vec<WasmMessageGrant>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    grants: Vec<WasmMessageGrant>,
}

impl WasmMessagingPolicy {
    pub const MAX_FILE_BYTES: usize = super::super::operator_policy::MAX_FILE_BYTES;

    pub fn new(mut grants: Vec<WasmMessageGrant>) -> Result<Self, DeployError> {
        messaging::validate(&mut grants)?;
        // Grant order has no authority semantics. Canonical order also makes
        // independently loaded equivalent node policies compare equally.
        grants.sort_by(|a, b| {
            (
                &a.source_path,
                &a.artifact_digest,
                &a.destination,
                &a.request_type_key,
                matches!(a.operation, super::WasmMessageOperation::Ask { .. }),
            )
                .cmp(&(
                    &b.source_path,
                    &b.artifact_digest,
                    &b.destination,
                    &b.request_type_key,
                    matches!(b.operation, super::WasmMessageOperation::Ask { .. }),
                ))
        });
        Ok(Self { grants })
    }

    pub fn grants(&self) -> &[WasmMessageGrant] {
        &self.grants
    }

    pub fn from_toml(text: &str) -> Result<Self, DeployError> {
        if text.len() > Self::MAX_FILE_BYTES {
            return Err(DeployError::Artifact(
                "Wasm messaging policy exceeds 1 MiB".into(),
            ));
        }
        let document: Document = toml::from_str(text)
            .map_err(|error| DeployError::Config(format!("Wasm messaging policy: {error}")))?;
        Self::new(document.grants)
    }

    /// Load a bounded regular file on a blocking worker before host startup.
    /// Symlinks to regular operator files are allowed; the opened file is
    /// rechecked and read once, so later file changes cannot widen a live policy.
    pub async fn load_file(path: impl AsRef<Path>) -> Result<Self, DeployError> {
        load_policy_file(path, "Wasm messaging policy", Self::from_toml).await
    }
}
