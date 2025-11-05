//! Committed desired state, including unfinished rollout ownership.
//! Stores implement atomic compare-and-set; successful commits are durable for
//! disk implementations. In-memory storage is explicitly non-durable.
use crate::{AppManifest, ApplicationLimits, UpgradePolicy};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingUpgrade {
    /// Revision that created this particular rollout, not the latest store revision.
    pub generation: u64,
    pub policy: UpgradePolicy,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredStateRecord {
    pub revision: u64,
    // TOML preserves NaN, infinities, signed zero and datetimes; JSON Value does not.
    #[serde(with = "manifest_text")]
    pub manifest: AppManifest,
    pub pending: BTreeMap<String, PendingUpgrade>,
    #[serde(default)]
    pub mirrors: BTreeMap<String, super::mirror::MirrorIntent>,
}
mod manifest_text {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        m: &AppManifest,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        serializer.serialize_str(&m.to_toml().map_err(S::Error::custom)?)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<AppManifest, D::Error> {
        use serde::{de::Error, Deserialize};
        let source = String::deserialize(deserializer)?;
        toml::from_str(&source).map_err(D::Error::custom)
    }
}
impl DesiredStateRecord {
    pub fn validate(&self, limits: &ApplicationLimits) -> Result<(), String> {
        crate::validate_with_limits(&self.manifest, limits).map_err(|e| format!("{e:?}"))?;
        if self.revision == 0 || self.pending.len() > self.manifest.components.len() {
            return Err("invalid desired revision or pending rollout count".into());
        }
        super::mirror::validate_journal(&self.mirrors, limits)?;
        for (name, pending) in &self.pending {
            let component = self
                .manifest
                .component(name)
                .ok_or("pending rollout references undeclared component")?;
            if pending.generation == 0
                || pending.generation > self.revision
                || pending.policy != component.upgrade
            {
                return Err("invalid pending rollout generation or policy".into());
            }
        }
        Ok(())
    }
}

pub trait DesiredStateStore: Send + Sync {
    /// Atomically replace expected revision (None = empty). Require exactly the
    /// next revision and the same application identity. On uncertain disk write
    /// outcome the store must fail closed until reopened; never return success
    /// for an unsynced commit. Do not implement this as separate read/write calls.
    fn commit(&self, expected: Option<u64>, state: &DesiredStateRecord) -> Result<(), String>;
    fn latest(&self) -> Result<Option<DesiredStateRecord>, String>;
    fn current_revision(&self) -> Result<Option<u64>, String> {
        Ok(self.latest()?.map(|state| state.revision))
    }
}

pub(crate) fn check_transition(
    current: Option<&DesiredStateRecord>,
    expected: Option<u64>,
    next: &DesiredStateRecord,
) -> Result<(), String> {
    if current.map(|s| s.revision) != expected {
        return Err("desired state revision conflict; refresh required".into());
    }
    let revision = expected
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("desired revision exhausted")?;
    if next.revision != revision || current.is_some_and(|s| s.manifest.name != next.manifest.name) {
        return Err("invalid desired revision or application identity transition".into());
    }
    Ok(())
}

pub struct MemStateStore {
    latest: Mutex<Option<DesiredStateRecord>>,
    limits: ApplicationLimits,
}
impl Default for MemStateStore {
    fn default() -> Self {
        Self::with_limits(ApplicationLimits::default())
    }
}
impl MemStateStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_limits(limits: ApplicationLimits) -> Self {
        Self {
            latest: Mutex::new(None),
            limits,
        }
    }
}
impl DesiredStateStore for MemStateStore {
    fn commit(&self, expected: Option<u64>, state: &DesiredStateRecord) -> Result<(), String> {
        state.validate(&self.limits)?;
        let mut current = self
            .latest
            .lock()
            .map_err(|_| "desired state lock poisoned")?;
        check_transition(current.as_ref(), expected, state)?;
        *current = Some(state.clone());
        Ok(())
    }
    fn latest(&self) -> Result<Option<DesiredStateRecord>, String> {
        Ok(self
            .latest
            .lock()
            .map_err(|_| "desired state lock poisoned")?
            .clone())
    }
    fn current_revision(&self) -> Result<Option<u64>, String> {
        Ok(self
            .latest
            .lock()
            .map_err(|_| "desired state lock poisoned")?
            .as_ref()
            .map(|state| state.revision))
    }
}
