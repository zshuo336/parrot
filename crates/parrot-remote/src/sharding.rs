//! Consistent placement and local generation bookkeeping for a sharding host.
//!
//! The coordinator reserves a generation before spawn. Hosts retain its lease
//! until the actual instance or cancelled constructor has retired, then confirm
//! `passivated`. Membership changes request retirement; they do not move an
//! actor, transfer state, drain messages or authorize another process to spawn.
//! There is no built-in distributed ShardRouter or receiver-side fencing here.
//! Independent coordinators can reserve the same key; production hosts still
//! need an authority protocol, actual retirement and retry/deduplication logic.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// 一致性哈希环：虚节点 256/node（防雪崩——DEV_04 §2.1）。
pub struct HashRing {
    nodes: Vec<String>,
    /// Sorted (hash, canonical node index); strings are owned once per node.
    vnodes: Vec<(u64, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HashRingError {
    #[error("invalid shard node identity at index {index}")]
    InvalidNode { index: usize },
    #[error("a nonempty ring requires at least one virtual node per node")]
    ZeroVirtualNodes,
    #[error("hash ring capacity is unrepresentable or unavailable")]
    Capacity,
}

/// FNV-1a 64 + 两轮强化混合（短键分散性——裸 FNV 对 "k{i}" 系列分布偏斜）。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    // splitmix64 finalizer（消除低位偏斜）
    let mut z = h.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl HashRing {
    /// Construct a ring from validated configuration.
    ///
    /// # Panics
    /// Panics on invalid configuration or unavailable capacity. Use `try_new`
    /// to report these errors. Callers must budget their configured ring size.
    #[track_caller]
    pub fn new(nodes: &[String], vnodes_per_node: usize) -> Self {
        Self::try_new(nodes, vnodes_per_node).expect("invalid hash ring configuration")
    }

    /// Preserve placement for valid members, canonicalize duplicates/order,
    /// and reject invalid identities, zero replication and capacity overflow.
    pub fn try_new(nodes: &[String], vnodes_per_node: usize) -> Result<Self, HashRingError> {
        for (index, node) in nodes.iter().enumerate() {
            parrot_config::endpoint::validate_node_id(node)
                .map_err(|_| HashRingError::InvalidNode { index })?;
        }
        if !nodes.is_empty() && vnodes_per_node == 0 {
            return Err(HashRingError::ZeroVirtualNodes);
        }
        let mut members = Vec::new();
        members
            .try_reserve_exact(nodes.len())
            .map_err(|_| HashRingError::Capacity)?;
        members.extend(nodes.iter());
        members.sort_unstable();
        members.dedup();
        let count = members
            .len()
            .checked_mul(vnodes_per_node)
            .ok_or(HashRingError::Capacity)?;
        let mut vnodes = Vec::new();
        vnodes
            .try_reserve_exact(count)
            .map_err(|_| HashRingError::Capacity)?;
        for (index, n) in members.iter().enumerate() {
            for i in 0..vnodes_per_node {
                let h = fnv1a(format!("{n}#{i}").as_bytes());
                vnodes.push((h, index));
            }
        }
        vnodes.sort_unstable();
        Ok(Self {
            nodes: members.into_iter().cloned().collect(),
            vnodes,
        })
    }

    /// key → 宿主节点（顺时针第一虚节点）。
    pub fn node(&self, key: &str) -> Option<&str> {
        if self.vnodes.is_empty() {
            return None;
        }
        let h = fnv1a(key.as_bytes());
        let idx = self.vnodes.partition_point(|(vh, _)| *vh < h);
        let idx = if idx == self.vnodes.len() { 0 } else { idx };
        Some(&self.nodes[self.vnodes[idx].1])
    }

    pub fn nodes(&self) -> Vec<String> {
        self.nodes.clone()
    }
}

/// 实体状态（Active/Migrating/Passivated——DEV_04 §2.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityStatus {
    Activating,
    Active,
    Migrating,
    Passivated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityState {
    /// Actual admitted generation's node, including while it is retiring.
    pub node: String,
    pub status: EntityStatus,
}

/// Opaque local generation identity. Retain it across spawn, cancellation and
/// actual retirement. Cloning/dropping this handle does not stop an instance.
#[derive(Debug, Clone)]
pub struct EntityLease {
    key: String,
    node: String,
    identity: Arc<()>,
}

impl EntityLease {
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn node(&self) -> &str {
        &self.node
    }
}

impl PartialEq for EntityLease {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl Eq for EntityLease {}

/// Host retirement request. A missing target means no node can currently own
/// the entity. The owner lease remains valid for cleanup, not new work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardMigration {
    pub owner: EntityLease,
    pub target: Option<String>,
}

struct EntityRecord {
    lease: EntityLease,
    status: EntityStatus,
}

struct CoordinatorState {
    ring: HashRing,
    entities: HashMap<String, EntityRecord>,
}

/// Local, linearizable bookkeeping; this object grants no cluster authority.
pub struct ShardCoordinator {
    state: RwLock<CoordinatorState>,
}

impl ShardCoordinator {
    pub fn new(nodes: &[String]) -> Self {
        Self::try_new(nodes).expect("invalid shard coordinator configuration")
    }

    pub fn try_new(nodes: &[String]) -> Result<Self, HashRingError> {
        Ok(Self {
            state: RwLock::new(CoordinatorState {
                ring: HashRing::try_new(nodes, 256)?,
                entities: HashMap::new(),
            }),
        })
    }

    /// Atomically replace placement and return all pending retirements in key
    /// order. Invalid membership leaves the previous state intact. A draining
    /// generation cannot become active again if membership later returns.
    pub fn rebalance(&self, alive: &[String]) -> Result<Vec<ShardMigration>, HashRingError> {
        let replacement = HashRing::try_new(alive, 256)?;
        let mut state = self.state.write().unwrap();
        state.ring = replacement;
        let CoordinatorState { ring, entities } = &mut *state;
        let mut moves = Vec::new();
        for (key, record) in entities.iter_mut() {
            if record.status == EntityStatus::Passivated {
                continue;
            }
            let target = destination(ring, key);
            if target != Some(record.lease.node()) {
                record.status = EntityStatus::Migrating;
            }
            if record.status == EntityStatus::Migrating {
                moves.push(ShardMigration {
                    owner: record.lease.clone(),
                    target: target.map(str::to_owned),
                });
            }
        }
        moves.sort_unstable_by(|left, right| left.owner.key().cmp(right.owner.key()));
        Ok(moves)
    }

    /// 实体路由决策：key → 宿主节点（惰性激活语义由引擎侧 spawn 兜底）。
    pub fn route(&self, key: &str) -> Option<String> {
        destination(&self.state.read().unwrap().ring, key).map(str::to_owned)
    }

    /// Reserve one local generation before calling spawn. An existing owned or
    /// draining generation must actually retire before another can be admitted.
    pub fn begin_activation(&self, key: &str) -> Option<EntityLease> {
        let mut state = self.state.write().unwrap();
        let node = destination(&state.ring, key)?.to_owned();
        if state
            .entities
            .get(key)
            .is_some_and(|entry| entry.status != EntityStatus::Passivated)
        {
            return None;
        }
        let lease = EntityLease {
            key: key.into(),
            node,
            identity: Arc::new(()),
        };
        state.entities.insert(
            key.into(),
            EntityRecord {
                lease: lease.clone(),
                status: EntityStatus::Activating,
            },
        );
        Some(lease)
    }

    /// Confirm successful spawn for this exact reservation. Stale, foreign,
    /// retired or invalidated completions cannot publish an active generation.
    pub fn activated(&self, lease: &EntityLease) -> bool {
        let mut state = self.state.write().unwrap();
        if destination(&state.ring, lease.key()) != Some(lease.node()) {
            return false;
        }
        if let Some(entry) = state.entities.get_mut(lease.key()) {
            if entry.lease == *lease
                && matches!(
                    entry.status,
                    EntityStatus::Activating | EntityStatus::Active
                )
            {
                entry.status = EntityStatus::Active;
                return true;
            }
        }
        false
    }

    /// Confirm actual retirement of the original instance/constructor. Hosts
    /// must not use path disappearance or a cancelled waiter as this evidence.
    pub fn passivated(&self, lease: &EntityLease) -> bool {
        let mut state = self.state.write().unwrap();
        if let Some(entry) = state.entities.get_mut(lease.key()) {
            if entry.lease == *lease {
                entry.status = EntityStatus::Passivated;
                return true;
            }
        }
        false
    }

    /// Release retired diagnostic metadata without letting a stale generation
    /// remove a replacement. Outstanding old handles remain distinguishable.
    pub fn forget_passivated(&self, lease: &EntityLease) -> bool {
        let mut state = self.state.write().unwrap();
        if state
            .entities
            .get(lease.key())
            .is_some_and(|entry| entry.lease == *lease && entry.status == EntityStatus::Passivated)
        {
            state.entities.remove(lease.key());
            return true;
        }
        false
    }

    /// 诊断：实体表快照。
    pub fn entities(&self) -> HashMap<String, EntityState> {
        self.state
            .read()
            .unwrap()
            .entities
            .iter()
            .map(|(key, entry)| {
                (
                    key.clone(),
                    EntityState {
                        node: entry.lease.node.clone(),
                        status: entry.status,
                    },
                )
            })
            .collect()
    }
}

/// 实体路径拆解：`/user/entity-{key}` → key。
pub fn entity_key_of(path: &str) -> Option<&str> {
    let local = if path.starts_with("parrot://") {
        crate::actor_path::split(path)?.1
    } else if crate::mirror::canonical(path) {
        path
    } else {
        return None;
    };
    let key = local.strip_prefix("/user/entity-")?;
    (!key.is_empty() && !key.contains('/')).then_some(key)
}

fn destination<'a>(ring: &'a HashRing, key: &str) -> Option<&'a str> {
    const PREFIX: &str = "/user/entity-";
    if key.is_empty() || key.len() > 4096 - PREFIX.len() || key.contains('/') {
        return None;
    }
    let node = ring.node(key)?;
    crate::actor_path::qualify(node, &format!("{PREFIX}{key}"))?;
    Some(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 环稳定性 + 分散性：同 key 恒定宿主；N 节点键分布均衡（±40% 容差）
    #[test]
    fn ring_stable_and_balanced() {
        let nodes: Vec<String> = (0..5).map(|i| format!("n{i}")).collect();
        let ring = HashRing::new(&nodes, 256);
        let mut dist: HashMap<String, usize> = HashMap::new();
        for i in 0..1000 {
            let key = format!("entity-{i}");
            let host = ring.node(&key).unwrap().to_string();
            assert!(nodes.contains(&host), "host must be a member");
            for _ in 0..3 {
                assert_eq!(ring.node(&key), Some(host.as_str()));
            }
            *dist.entry(host).or_default() += 1;
        }
        let avg = 1000 / nodes.len();
        for n in &nodes {
            let c = dist.get(n).copied().unwrap_or(0);
            assert!(
                (c as i64 - avg as i64).abs() < (avg as f64 * 0.4) as i64,
                "node {n} count {c} vs avg {avg} (±40%)"
            );
        }
    }

    // 最小扰动：撤 1 节点 → 迁移键比例 < 60%（虚节点 256 的收益）
    #[test]
    fn ring_minimal_disruption() {
        let nodes: Vec<String> = (0..5).map(|i| format!("n{i}")).collect();
        let ring1 = HashRing::new(&nodes, 256);
        let shrunk: Vec<String> = nodes[..4].to_vec();
        let ring2 = HashRing::new(&shrunk, 256);
        let keys: Vec<String> = (0..1000).map(|i| format!("k{i}")).collect();
        for key in &keys {
            assert_eq!(
                ring1.node(key) != ring2.node(key),
                ring1.node(key) == Some("n4")
            );
        }
        let moved = keys
            .iter()
            .filter(|k| ring1.node(k) != ring2.node(k))
            .count();
        let ratio = moved as f64 / keys.len() as f64;
        assert!(
            ratio > 0.0 && ratio < 0.6,
            "moved ratio {ratio:.2} (expect 0..60%)"
        );
    }

    // Membership removal requests exact-generation retirement, retaining its owner.
    #[test]
    fn coordinator_rebalance_moves() {
        let nodes: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let c = ShardCoordinator::new(&nodes);
        // 激活一批实体
        for i in 0..50 {
            let k = format!("e{i}");
            let lease = c.begin_activation(&k).unwrap();
            assert!(c.activated(&lease));
        }
        let before = c.entities();
        let alive: Vec<String> = vec!["b".into(), "c".into()];
        let moves = c.rebalance(&alive).unwrap();
        let expected: std::collections::BTreeSet<_> = before
            .iter()
            .filter(|(_, state)| state.node == "a")
            .map(|(key, _)| key.as_str())
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(moves.len(), expected.len());
        assert_eq!(
            moves
                .iter()
                .map(|item| item.owner.key())
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        let after = c.entities();
        for (key, state) in &before {
            assert_eq!(after[key].node, state.node);
            assert_eq!(
                after[key].status,
                if expected.contains(key.as_str()) {
                    EntityStatus::Migrating
                } else {
                    EntityStatus::Active
                }
            );
        }
        for item in &moves {
            assert_eq!(item.owner.node(), "a");
            assert!(alive.contains(item.target.as_ref().unwrap()));
            assert_eq!(c.route(item.owner.key()), item.target);
        }
    }

    // 实体路径拆解
    #[test]
    fn entity_key_parse() {
        assert_eq!(entity_key_of("/user/entity-sensor-42"), Some("sensor-42"));
        assert_eq!(entity_key_of("/user/other-thing"), None);
    }
}
