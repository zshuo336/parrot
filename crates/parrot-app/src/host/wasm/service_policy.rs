use super::super::operator_policy::load_policy_file;
use super::{services, DeployError, WasmServiceGrant};
use std::path::Path;

/// Immutable operator discovery authority, independent of component settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WasmServicePolicy {
    grants: Vec<WasmServiceGrant>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    grants: Vec<WasmServiceGrant>,
}

impl WasmServicePolicy {
    pub const MAX_FILE_BYTES: usize = super::WasmMessagingPolicy::MAX_FILE_BYTES;

    pub fn new(mut grants: Vec<WasmServiceGrant>) -> Result<Self, DeployError> {
        services::validate(&mut grants)?;
        grants.sort_by(|a, b| {
            (&a.source_path, &a.artifact_digest, &a.key, a.operation).cmp(&(
                &b.source_path,
                &b.artifact_digest,
                &b.key,
                b.operation,
            ))
        });
        Ok(Self { grants })
    }
    pub fn grants(&self) -> &[WasmServiceGrant] {
        &self.grants
    }
    pub fn from_toml(text: &str) -> Result<Self, DeployError> {
        if text.len() > Self::MAX_FILE_BYTES {
            return Err(DeployError::Config(
                "Wasm service policy exceeds 1 MiB".into(),
            ));
        }
        let document: Document = toml::from_str(text)
            .map_err(|error| DeployError::Config(format!("Wasm service policy: {error}")))?;
        Self::new(document.grants)
    }
    pub async fn load_file(path: impl AsRef<Path>) -> Result<Self, DeployError> {
        load_policy_file(path, "Wasm service policy", Self::from_toml).await
    }
}
