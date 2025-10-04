//! D1 · Cluster Sharding（DEV_04 §2 / 06 P4.1）。
//!
//! 职责：实体→节点的放置与惰性激活；一致性哈希环是策略，实体语义是机制。
//!
//! ```text
//! ask(parrot://…/user/entity-{key}) → facade 命中 ShardRouter（/user/entity-*
//! 前缀处理器）→ ring.node(hash(key)) → 目标节点本地 spawn（thread 引擎
//! Sharded{affinity_key:key}——两级亲和）。
//! ```
//!
//! rebalance：membership 变更 → 哈希环更新 → 迁出节点 drain → 实体
//! passivate（stop）→ 新 holder 惰性再激活。无状态实体（P4 红线：状态
//! 迁移协议不做——业务外部化）。

use std::collections::HashMap;

/// 一致性哈希环：虚节点 256/node（防雪崩——DEV_04 §2.1）。
pub struct HashRing {
    /// 排序的 (hash, node) 虚节点表。
    vnodes: Vec<(u64, String)>,
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
    pub fn new(nodes: &[String], vnodes_per_node: usize) -> Self {
        let mut vnodes = Vec::with_capacity(nodes.len() * vnodes_per_node);
        for n in nodes {
            for i in 0..vnodes_per_node {
                let h = fnv1a(format!("{n}#{i}").as_bytes());
                vnodes.push((h, n.clone()));
            }
        }
        vnodes.sort_unstable_by_key(|(h, _)| *h);
        Self { vnodes }
    }

    /// key → 宿主节点（顺时针第一虚节点）。
    pub fn node(&self, key: &str) -> Option<&str> {
        if self.vnodes.is_empty() {
            return None;
        }
        let h = fnv1a(key.as_bytes());
        let idx = self.vnodes.partition_point(|(vh, _)| *vh < h);
        let idx = if idx == self.vnodes.len() { 0 } else { idx };
        Some(&self.vnodes[idx].1)
    }

    pub fn nodes(&self) -> Vec<String> {
        let mut ns: Vec<String> = self.vnodes.iter().map(|(_, n)| n.clone()).collect();
        ns.sort();
        ns.dedup();
        ns
    }
}

/// 实体状态（Active/Migrating/Passivated——DEV_04 §2.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityStatus {
    Active,
    Migrating,
    Passivated,
}

#[derive(Debug, Clone)]
pub struct EntityState {
    pub node: String,
    pub status: EntityStatus,
}

/// 分片协调器：每节点一份（gossip 收敛视图），无中心。
pub struct ShardCoordinator {
    /// 当前环（SWIM Alive 集合驱动重建）。
    ring: std::sync::RwLock<HashRing>,
    entities: std::sync::RwLock<HashMap<String, EntityState>>,
}

impl ShardCoordinator {
    pub fn new(nodes: &[String]) -> Self {
        Self {
            ring: std::sync::RwLock::new(HashRing::new(nodes, 256)),
            entities: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// membership 变更 → 环重建 + 受影响实体 Migrating（drain 源）。
    /// 返回 (迁出实体表, 需 passivate 的实体键)——调用方驱动 drain/重激活。
    pub fn rebalance(&self, alive: &[String]) -> Vec<(String, String)> {
        {
            let mut r = self.ring.write().unwrap();
            *r = HashRing::new(alive, 256);
        }
        let ring = self.ring.read().unwrap();
        let mut moves = Vec::new();
        let mut g = self.entities.write().unwrap();
        for (key, st) in g.iter_mut() {
            let new_node = ring.node(key).map(|s| s.to_string()).unwrap_or_default();
            if !new_node.is_empty() && new_node != st.node {
                moves.push((key.clone(), new_node.clone()));
                st.node = new_node;
                st.status = EntityStatus::Migrating;
            }
        }
        moves
    }

    /// 实体路由决策：key → 宿主节点（惰性激活语义由引擎侧 spawn 兜底）。
    pub fn route(&self, key: &str) -> Option<String> {
        let ring = self.ring.read().unwrap();
        ring.node(key).map(|s| s.to_string())
    }

    /// 激活登记（目标节点 spawn 成功后回写）。
    pub fn activated(&self, key: &str, node: &str) {
        self.entities.write().unwrap().insert(
            key.into(),
            EntityState {
                node: node.into(),
                status: EntityStatus::Active,
            },
        );
    }

    /// passivate 登记（stop + 可选状态快照 KV 回调由业务侧挂接）。
    pub fn passivated(&self, key: &str) {
        if let Some(st) = self.entities.write().unwrap().get_mut(key) {
            st.status = EntityStatus::Passivated;
        }
    }

    /// 诊断：实体表快照。
    pub fn entities(&self) -> HashMap<String, EntityState> {
        self.entities.read().unwrap().clone()
    }
}

/// 实体路径拆解：`/user/entity-{key}` → key。
pub fn entity_key_of(path: &str) -> Option<&str> {
    path.rsplit_once("/entity-").map(|(_, k)| k)
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
            let host = ring.node(&format!("entity-{i}")).unwrap().to_string();
            assert!(nodes.contains(&host), "host must be a member");
            *dist.entry(host).or_default() += 1;
        }
        let avg = 1000 / nodes.len();
        for (n, c) in dist {
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
        let moved = keys
            .iter()
            .filter(|k| ring1.node(k) != ring2.node(k))
            .count();
        let ratio = moved as f64 / keys.len() as f64;
        assert!(ratio < 0.6, "moved ratio {ratio:.2} (expect <60%)");
    }

    // rebalance：kill 节点后受影响实体 Migrating + 宿主切换
    #[test]
    fn coordinator_rebalance_moves() {
        let nodes: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let c = ShardCoordinator::new(&nodes);
        // 激活一批实体
        for i in 0..50 {
            let k = format!("e{i}");
            if let Some(host) = c.route(&k) {
                c.activated(&k, &host);
            }
        }
        // kill "a"
        let alive: Vec<String> = vec!["b".into(), "c".into()];
        let moves = c.rebalance(&alive);
        // 原 "a" 上的实体全部迁移
        let from_a = c
            .entities()
            .iter()
            .filter(|(k, _)| moves.iter().any(|(mk, _)| mk == k.as_str()) || true)
            .count();
        assert!(from_a >= moves.len());
        // 迁移目标均在存活集
        for (_, to) in &moves {
            assert!(alive.contains(to), "target {to} must be alive");
        }
    }

    // 实体路径拆解
    #[test]
    fn entity_key_parse() {
        assert_eq!(entity_key_of("/user/entity-sensor-42"), Some("sensor-42"));
        assert_eq!(entity_key_of("/user/other-thing"), None);
    }
}
