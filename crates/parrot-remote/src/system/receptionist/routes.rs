//! Complete views belong to physical sources; path vectors prevent relay feedback.
use crate::{
    receptionist::{registration_bytes, Receptionist, ReceptionistEntry, ReceptionistSync},
    transport::{ConnectionIdentity, FrameSender},
};
use parrot_api::{
    receptionist::{ReceptionistEvent, ReceptionistKey},
    types::ActorResult,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub(super) const MAX_HOPS: usize = 32;
pub(super) const MAX_ORIGINS: usize = 1024;
pub(super) const MAX_VIEWS: usize = 65536;
const MAX_SOURCES: usize = 1024;
const MAX_SOURCE_BYTES: u128 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Advertisement {
    pub path: Vec<String>,
    pub entries: Vec<ReceptionistEntry>,
}
impl Advertisement {
    pub(super) fn owner(&self) -> &str {
        self.path.first().expect("validated origin path")
    }
    fn key(&self) -> &ReceptionistKey {
        &self.entries.first().expect("nonempty key projection").key
    }
    fn bytes(&self) -> u128 {
        512 + self
            .path
            .iter()
            .map(|node| node.len() as u128 + 64)
            .sum::<u128>()
            + self
                .entries
                .iter()
                .map(|e| registration_bytes(&e.node, &e.key, &e.remote_path))
                .sum::<u128>()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Sync {
    pub from: String,
    pub views: Vec<Advertisement>,
}
pub(super) fn encode(sync: &Sync) -> bytes::Bytes {
    let mut payload = vec![crate::admin::sys_event_tag::RECEPTIONIST_ROUTES];
    payload.extend(bincode::serde::encode_to_vec(sync, bincode::config::standard()).unwrap());
    payload.into()
}

struct Source {
    identity: ConnectionIdentity,
    node: String,
    views: Vec<Advertisement>,
}
impl Source {
    fn bytes(&self) -> u128 {
        512 + self.node.len() as u128 + self.views.iter().map(Advertisement::bytes).sum::<u128>()
    }
}

#[derive(Default)]
pub(crate) struct Routes {
    sources: HashMap<usize, Source>,
    active: HashMap<(String, ReceptionistKey), Advertisement>,
}
fn exhausted() -> parrot_api::errors::ActorError {
    parrot_api::errors::ActorError::InternalError(
        "receptionist discovery source budget exhausted".into(),
    )
}

impl Routes {
    fn select<'a>(
        sources: impl Iterator<Item = &'a Source>,
    ) -> HashMap<(String, ReceptionistKey), Advertisement> {
        let mut chosen: HashMap<(String, ReceptionistKey), &Advertisement> = HashMap::new();
        for source in sources {
            for view in &source.views {
                let current = chosen
                    .entry((view.owner().to_owned(), view.key().clone()))
                    .or_insert(view);
                if (view.path.len(), &view.path) < (current.path.len(), &current.path) {
                    *current = view;
                }
            }
        }
        chosen
            .into_iter()
            .map(|(owner, view)| (owner, view.clone()))
            .collect()
    }

    fn commit(
        &mut self,
        selected: HashMap<(String, ReceptionistKey), Advertisement>,
        table: &mut Receptionist,
    ) -> ActorResult<Vec<ReceptionistEvent>> {
        let mut owners: std::collections::BTreeMap<String, Vec<ReceptionistEntry>> = self
            .active
            .keys()
            .map(|(owner, _)| (owner.clone(), Vec::new()))
            .collect();
        for ((owner, _), view) in &selected {
            owners
                .entry(owner.clone())
                .or_default()
                .extend(view.entries.iter().cloned());
        }
        let views: Vec<_> = owners
            .into_iter()
            .map(|(from, entries)| ReceptionistSync { from, entries })
            .collect();
        let events = table.try_merge_views(&views)?;
        self.active = selected;
        Ok(events)
    }

    pub(super) fn replace(
        &mut self,
        back: &FrameSender,
        views: Vec<Advertisement>,
        table: &mut Receptionist,
    ) -> ActorResult<Vec<ReceptionistEvent>> {
        let next = Source {
            identity: back.retain_identity(),
            node: back.node_id().into(),
            views,
        };
        let retained = || {
            self.sources
                .values()
                .filter(|source| source.node != back.node_id())
        };
        if retained().count() >= MAX_SOURCES
            || retained().map(Source::bytes).sum::<u128>() + next.bytes() > MAX_SOURCE_BYTES
        {
            return Err(exhausted());
        }
        let selected = Self::select(retained().chain(std::iter::once(&next)));
        if selected
            .keys()
            .map(|(owner, _)| owner)
            .collect::<HashSet<_>>()
            .len()
            > MAX_ORIGINS
            || selected.len() > MAX_VIEWS
        {
            return Err(exhausted());
        }
        let events = self.commit(selected, table)?;
        self.sources
            .retain(|_, source| source.node != back.node_id());
        self.sources.insert(next.identity.key(), next);
        Ok(events)
    }

    pub(super) fn prune(
        &mut self,
        current: &HashSet<usize>,
        available: &HashSet<String>,
        table: &mut Receptionist,
    ) -> Vec<ReceptionistEvent> {
        self.sources.retain(|key, source| {
            if !current.contains(key) || !available.contains(&source.node) {
                return false;
            }
            source
                .views
                .retain(|view| view.path.iter().all(|node| available.contains(node)));
            true
        });
        let mut selected = Self::select(self.sources.values());
        if let Ok(events) = self.commit(selected.clone(), table) {
            return events;
        }
        // An alternate route may expose a larger view. Never keep a dead route
        // because its replacement cannot fit; retry growing owners on later ticks.
        let mut totals: HashMap<&str, (usize, u128)> = HashMap::new();
        for ((owner, _), view) in &selected {
            let next = totals.entry(owner).or_default();
            next.0 += view.entries.len();
            next.1 += view
                .entries
                .iter()
                .map(|e| registration_bytes(&e.node, &e.key, &e.remote_path))
                .sum::<u128>();
        }
        let growing: HashSet<_> = totals
            .into_iter()
            .filter_map(|(owner, (count, bytes))| {
                let (old_count, old_bytes) = table.discovery_usage(owner);
                (count as u128 > old_count || bytes > old_bytes).then(|| owner.to_owned())
            })
            .collect();
        selected.retain(|(owner, _), _| !growing.contains(owner));
        self.commit(selected, table)
            .expect("non-growing discovery retirement fits existing budget")
    }

    pub(super) fn snapshot(&self, local: ReceptionistSync, peer: &str) -> Sync {
        let node = local.from.clone();
        let mut views: Vec<_> = self
            .active
            .values()
            .filter(|view| view.path.len() < MAX_HOPS && !view.path.iter().any(|hop| hop == peer))
            .cloned()
            .collect();
        for view in &mut views {
            view.path.push(node.clone());
        }
        let mut keys: std::collections::BTreeMap<_, Vec<_>> = std::collections::BTreeMap::new();
        for entry in local.entries {
            keys.entry(entry.key.as_str().to_owned())
                .or_default()
                .push(entry);
        }
        views.extend(keys.into_values().map(|entries| Advertisement {
            path: vec![node.clone()],
            entries,
        }));
        // A full table can include MAX_ORIGINS foreign owners plus local keys.
        // Keep the projected wire view valid, prioritizing the node's own keys.
        views.sort_by(|a, b| {
            (a.path.len() != 1, &a.path, a.key().as_str()).cmp(&(
                b.path.len() != 1,
                &b.path,
                b.key().as_str(),
            ))
        });
        let mut owners = HashSet::new();
        let mut count = 0;
        views.retain(|view| {
            if count == MAX_VIEWS || (owners.len() == MAX_ORIGINS && !owners.contains(view.owner()))
            {
                return false;
            }
            owners.insert(view.owner().to_owned());
            count += 1;
            true
        });
        Sync { from: node, views }
    }
}

#[cfg(test)]
mod tests;
