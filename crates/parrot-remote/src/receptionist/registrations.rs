use super::{
    validate_event, Receptionist, ReceptionistEntry, ReceptionistEvent, ReceptionistKey,
    ReceptionistSync,
};
use parrot_api::{errors::ActorError, types::ActorResult};
use parrot_config::receptionist::RegistrationLimits;
use std::collections::{HashMap, HashSet};

/// Declared table allowance per distinct (node, key, path), excluding application
/// snapshots and generation cleanup closures. This is not allocator RSS.
pub const REGISTRATION_OVERHEAD: usize = 512;

pub fn registration_bytes(node: &str, key: &ReceptionistKey, path: &str) -> u128 {
    std::mem::size_of::<ReceptionistEntry>() as u128
        + REGISTRATION_OVERHEAD as u128
        + 3 * key.as_str().len() as u128
        + 2 * path.len() as u128
        + node.len() as u128
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Usage {
    entries: u128,
    bytes: u128,
}

#[derive(Default)]
pub(super) struct Budget {
    pub(super) limits: RegistrationLimits,
    total: Usage,
    nodes: HashMap<String, Usage>,
}

impl Budget {
    pub(super) fn new(limits: RegistrationLimits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }
    fn check(&self, usage: Usage) -> ActorResult<()> {
        if (usage.entries > self.limits.max_registrations as u128
            && usage.entries > self.total.entries)
            || (usage.bytes > self.limits.max_reserved_bytes as u128
                && usage.bytes > self.total.bytes)
        {
            return Err(ActorError::InternalError(
                "receptionist registration budget exhausted".into(),
            ));
        }
        Ok(())
    }
    pub(super) fn insert(&mut self, entry: &ReceptionistEntry) {
        let bytes = registration_bytes(&entry.node, &entry.key, &entry.remote_path);
        self.total.entries += 1;
        self.total.bytes += bytes;
        let node = self.nodes.entry(entry.node.clone()).or_default();
        node.entries += 1;
        node.bytes += bytes;
    }
    pub(super) fn remove(&mut self, entry: &ReceptionistEntry) {
        let bytes = registration_bytes(&entry.node, &entry.key, &entry.remote_path);
        self.total.entries -= 1;
        self.total.bytes -= bytes;
        let node = self
            .nodes
            .get_mut(&entry.node)
            .expect("registered entry owns node budget");
        node.entries -= 1;
        node.bytes -= bytes;
        if node.entries == 0 {
            self.nodes.remove(&entry.node);
        }
    }
}

impl Receptionist {
    pub(crate) fn discovery_usage(&self, node: &str) -> (u128, u128) {
        let usage = self
            .registrations
            .nodes
            .get(node)
            .copied()
            .unwrap_or_default();
        (usage.entries, usage.bytes)
    }

    pub(crate) fn check_registration(
        &self,
        node: &str,
        key: &ReceptionistKey,
        path: &str,
    ) -> ActorResult<()> {
        validate_event(key, path)?;
        let exists = self.entries.get(key.as_str()).is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.node == node && entry.remote_path == path)
        });
        if exists {
            return Ok(());
        }
        self.registrations.check(Usage {
            entries: self.registrations.total.entries + 1,
            bytes: self.registrations.total.bytes + registration_bytes(node, key, path),
        })
    }

    /// Bounded low-level publication. Caller broadcasts the returned event.
    pub fn try_register(
        &mut self,
        node: &str,
        key: &ReceptionistKey,
        path: &str,
    ) -> ActorResult<ReceptionistEvent> {
        self.check_registration(node, key, path)?;
        Ok(self.register(node, key, path))
    }

    /// Atomically replace one node's complete snapshot using its existing budget.
    /// Rejection leaves the table, owners and notifications unchanged.
    pub fn try_merge_sync(
        &mut self,
        sync: &ReceptionistSync,
    ) -> ActorResult<Vec<ReceptionistEvent>> {
        let mut unique = HashSet::with_capacity(sync.entries.len());
        let mut next = Usage::default();
        for entry in &sync.entries {
            if entry.node != sync.from {
                return Err(ActorError::InternalError(
                    "receptionist snapshot has foreign entry owner".into(),
                ));
            }
            validate_event(&entry.key, &entry.remote_path)?;
            if unique.insert(entry) {
                next.entries += 1;
                next.bytes += registration_bytes(&entry.node, &entry.key, &entry.remote_path);
            }
        }
        let previous = self
            .registrations
            .nodes
            .get(&sync.from)
            .copied()
            .unwrap_or_default();
        self.registrations.check(Usage {
            entries: self.registrations.total.entries - previous.entries + next.entries,
            bytes: self.registrations.total.bytes - previous.bytes + next.bytes,
        })?;
        Ok(self.merge_sync(sync))
    }

    /// Commit a complete selected discovery cohort, including explicit withdrawals.
    pub(crate) fn try_merge_views(
        &mut self,
        views: &[ReceptionistSync],
    ) -> ActorResult<Vec<ReceptionistEvent>> {
        let mut projected = self.registrations.total;
        let mut owners = HashSet::new();
        for sync in views {
            if !owners.insert(&sync.from) {
                return Err(ActorError::InternalError(
                    "duplicate discovery owner".into(),
                ));
            }
            let old = self
                .registrations
                .nodes
                .get(&sync.from)
                .copied()
                .unwrap_or_default();
            projected.entries -= old.entries;
            projected.bytes -= old.bytes;
            let mut entries = HashSet::new();
            for entry in &sync.entries {
                if entry.node != sync.from {
                    return Err(ActorError::InternalError("foreign discovery owner".into()));
                }
                validate_event(&entry.key, &entry.remote_path)?;
                if entries.insert(entry) {
                    projected.entries += 1;
                    projected.bytes +=
                        registration_bytes(&entry.node, &entry.key, &entry.remote_path);
                }
            }
        }
        self.registrations.check(projected)?;
        let mut pending: HashSet<_> = views.iter().flat_map(|view| &view.entries).collect();
        let mut events = Vec::new();
        let registrations = &mut self.registrations;
        self.entries.retain(|_, entries| {
            entries.retain(|entry| {
                let keep = !owners.contains(&entry.node) || pending.remove(entry);
                if !keep {
                    registrations.remove(entry);
                    events.push(ReceptionistEvent::Unregistered {
                        key: entry.key.clone(),
                        remote_path: entry.remote_path.clone(),
                    });
                }
                keep
            });
            !entries.is_empty()
        });
        for entry in views.iter().flat_map(|view| &view.entries) {
            if pending.remove(entry) {
                self.insert_entry(entry.clone());
                events.push(ReceptionistEvent::Registered {
                    key: entry.key.clone(),
                    remote_path: entry.remote_path.clone(),
                });
            }
        }
        self.forget_removed_owners(&events);
        Ok(events)
    }

    /// Finite registration admission with the default subscription budget.
    pub fn with_registration_limits(limits: RegistrationLimits) -> Self {
        Self::with_limits(Default::default(), limits)
    }
}

#[cfg(test)]
mod tests;
