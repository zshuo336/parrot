use super::super::operator_policy::load_policy_file;
use super::{messaging, DeployError, NativeMessageGrant, NativeMessageOperation};
use std::path::Path;

/// Immutable operator messaging authority, independent of native execution trust.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeMessagingPolicy {
    grants: Vec<NativeMessageGrant>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    grants: Vec<NativeMessageGrant>,
}

impl NativeMessagingPolicy {
    pub const MAX_FILE_BYTES: usize = super::super::operator_policy::MAX_FILE_BYTES;

    pub fn new(mut grants: Vec<NativeMessageGrant>) -> Result<Self, DeployError> {
        messaging::validate(&mut grants)?;
        grants.sort_by(|a, b| {
            (
                &a.source_path,
                &a.artifact_digest,
                &a.destination,
                &a.request_type_key,
                matches!(a.operation, NativeMessageOperation::Ask { .. }),
            )
                .cmp(&(
                    &b.source_path,
                    &b.artifact_digest,
                    &b.destination,
                    &b.request_type_key,
                    matches!(b.operation, NativeMessageOperation::Ask { .. }),
                ))
        });
        Ok(Self { grants })
    }

    pub fn grants(&self) -> &[NativeMessageGrant] {
        &self.grants
    }

    pub fn from_toml(text: &str) -> Result<Self, DeployError> {
        if text.len() > Self::MAX_FILE_BYTES {
            return Err(DeployError::Config(
                "native messaging policy exceeds 1 MiB".into(),
            ));
        }
        let document: Document = toml::from_str(text)
            .map_err(|error| DeployError::Config(format!("native messaging policy: {error}")))?;
        Self::new(document.grants)
    }

    /// Load one bounded regular operator file before startup. Subsequent edits
    /// cannot change the returned policy or an already initialized Node owner.
    pub async fn load_file(path: impl AsRef<Path>) -> Result<Self, DeployError> {
        load_policy_file(path, "native messaging policy", Self::from_toml).await
    }
}
