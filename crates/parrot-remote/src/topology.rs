//! Topology policy types and standalone route-table helpers (DEV_05 / 07 §5).
//!
//! - 模式：hub（星型中继）/ mesh（直连优先）/ hybrid（直连优先 + hub 兜底）
//! - hop 字段启用：中继转发 hop_count+=1，≥hop_limit 丢弃回
//!   REPLY_ERR(RouteUnreachable)（07 §3.3）
//! - RouteGossip data/merge helpers; membership transport wiring remains separate.
//! - 最长前缀匹配（POC PrefixRouter::resolve 实证转正）

use std::collections::HashMap;

/// 拓扑模式（07 §5.1）。
pub use parrot_config::topology::TopologyMode;

/// Originating mode/fallback policy plus legacy helper configuration fields.
/// Only `mode` and `relay_fallback` are consumed by RemoteActorSystem.
#[derive(Debug, Clone)]
pub struct TopologyConfig {
    pub mode: TopologyMode,
    /// Hub: allow direct traffic when no live hub exists. Hybrid: allow relay
    /// when direct discovery fails. Mesh never uses relay, regardless of this flag.
    pub relay_fallback: bool,
    /// Helper-only listener field; configure the actual remote.node.bind instead.
    pub hub_bind: Option<String>,
    /// Helper-only advertisement; the runtime uses remote.node.direct_addr.
    pub hub_advertise: Option<String>,
    /// Helper-only threshold; the runtime has no measured QoS classification yet.
    pub mesh_direct_min_qos: QoS,
}

/// QoS classification values for standalone helpers, not a runtime measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum QoS {
    /// WAN classification (the default helper value).
    #[default]
    Wan,
    /// LAN 亚毫秒（直连绿色）。
    Lan,
}

impl Default for TopologyConfig {
    fn default() -> Self {
        Self {
            mode: TopologyMode::default(),
            relay_fallback: true,
            hub_bind: None,
            hub_advertise: None,
            mesh_direct_min_qos: QoS::Lan,
        }
    }
}

/// 路由表项（RouteGossip 载荷）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouteEntry {
    /// 目标前缀（如 "parrot://eu-1/"）。
    pub prefix: String,
    /// 下一跳节点（hub 中继或直连目标）。
    pub next_hop: String,
    /// 路径成本（直连 1 / 一跳中继 2……越小越优）。
    pub cost: u32,
    /// 表项版本（单调递增——合并时高版本胜）。
    pub version: u64,
}

impl RouteEntry {
    /// A strict total preference for candidates with the same prefix.
    pub(crate) fn preferred_to(&self, other: &Self) -> bool {
        (std::cmp::Reverse(self.version), self.cost, &self.next_hop)
            < (
                std::cmp::Reverse(other.version),
                other.cost,
                &other.next_hop,
            )
    }
}

/// RouteGossip 消息（SYSTEM_EVENT 扩展载荷）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RouteGossip {
    pub entries: Vec<RouteEntry>,
    /// 前缀表 xor 指纹（对账——同 SWIM digest 模式）。
    pub digest: u64,
}

/// Order-independent change fingerprint over every field, with explicit string
/// lengths and little-endian integers. This is not an authentication digest.
pub fn route_digest(entries: &[RouteEntry]) -> u64 {
    let mut d: u64 = 0;
    for e in entries {
        let mut h = fnv_offset();
        for b in (e.prefix.len() as u64)
            .to_le_bytes()
            .into_iter()
            .chain(e.prefix.bytes())
            .chain((e.next_hop.len() as u64).to_le_bytes())
            .chain(e.next_hop.bytes())
            .chain(e.cost.to_le_bytes())
            .chain(e.version.to_le_bytes())
        {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        d ^= mix64(h);
    }
    d
}

const FNV_PRIME: u64 = 0x100_0000_01b3;
fn fnv_offset() -> u64 {
    0xcbf2_9ce4_8422_2325
}

fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// 前缀路由表（节点本地视图——gossip 收敛）。
///
/// 无自有状态可任意实例（POC PrefixRouter 转正）：最长前缀匹配 +
/// Merge prefers higher version, then lower cost, then lexically lower next hop.
/// The final tie-break makes independently received views converge.
#[derive(Debug, Default)]
pub struct RouteTable {
    entries: HashMap<String, RouteEntry>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 最长前缀匹配（POC `PrefixRouter::resolve` 语义）。
    pub fn resolve(&self, target_path: &str) -> Option<&RouteEntry> {
        self.entries
            .values()
            .filter(|e| target_path.starts_with(&e.prefix))
            .min_by(|a, b| {
                // 前缀长者优先；同长比 cost；同 cost 比 prefix（全序稳定）
                b.prefix
                    .len()
                    .cmp(&a.prefix.len())
                    .then(a.cost.cmp(&b.cost))
                    .then(a.prefix.cmp(&b.prefix))
            })
    }

    /// 合并 gossip 增量（收敛视图——幂等）。
    ///
    /// 返回实际接纳的条数（对账用）。
    pub fn merge(&mut self, gossip: &RouteGossip) -> usize {
        let mut taken = 0;
        for e in &gossip.entries {
            let accept = match self.entries.get(&e.prefix) {
                Some(cur) => e.preferred_to(cur),
                None => true,
            };
            if accept {
                self.entries.insert(e.prefix.clone(), e.clone());
                taken += 1;
            }
        }
        taken
    }

    /// 本地表项集（发 gossip 用）。
    pub fn entries(&self) -> Vec<RouteEntry> {
        self.entries.values().cloned().collect()
    }

    pub fn digest(&self) -> u64 {
        route_digest(&self.entries())
    }

    /// 直连端点存在性（hybrid 直连优先判据）。
    pub fn direct_to(&self, node: &str) -> bool {
        self.entries
            .values()
            .any(|e| e.next_hop == node && e.cost == 1)
    }
}

/// hop 检查与推进（07 §3.3——中继节点转发时调用）。
///
/// 返回 Err(()) 表示超限（调用方回 REPLY_ERR code=7 RouteUnreachable）。
/// Err 载荷为 HopExceeded 语义标记（无额外信息——code=7 已表意）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopExceeded;

#[inline]
pub fn hop_forward(hop_count: u8, hop_limit: u8) -> Result<u8, HopExceeded> {
    hop_count
        .checked_add(1)
        .filter(|next| *next < hop_limit)
        .ok_or(HopExceeded)
}

// ---------------- S2（DEV_06 §2）：超网聚合 ----------------

/// 超网聚合触发阈值（07 §14.4——路由表 >10k 条压缩）。
pub const SUPERNET_THRESHOLD: usize = 10_000;

/// 聚合结果统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SupernetReport {
    /// 聚合前条数。
    pub before: usize,
    /// 聚合后条数。
    pub after: usize,
    /// 压缩比（after/before * 1000——千分比）。
    pub ratio_permille: u64,
}

/// Compact redundant routes under an existing authoritative covering prefix.
/// A finite set of string-prefix children cannot establish parent coverage.
/// Only entries whose selected covering route has identical next hop, cost and
/// version can be removed. More-specific exceptions remain in the table.
#[derive(Debug, Default)]
pub struct SupernetAggregator {
    /// Selected prefix buckets, including distinct same-prefix candidates.
    aggregated: HashMap<String, Vec<RouteEntry>>,
    /// 聚合统计（上次 compact）。
    pub last_report: SupernetReport,
}

impl SupernetAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 执行聚合：明细表 → 压缩表。
    ///
    /// 超过 threshold 才真正压缩（小表直通——避免抖动）；
    /// 不超过时原样拷贝（语义零损耗）。
    pub fn compact(&mut self, entries: &[RouteEntry]) -> SupernetReport {
        let before = entries.len();
        let mut buckets: HashMap<String, Vec<RouteEntry>> = HashMap::new();
        for e in entries {
            buckets.entry(e.prefix.clone()).or_default().push(e.clone());
        }
        if before <= SUPERNET_THRESHOLD {
            self.aggregated = buckets;
            self.last_report = SupernetReport {
                before,
                after: before,
                ratio_permille: 1000,
            };
            return self.last_report;
        }
        // Decide entire prefix buckets after installing all shorter prefixes.
        // Removing only a bucket's winner could expose an inferior candidate.
        let mut ordered: Vec<_> = buckets.into_iter().collect();
        ordered.sort_by_key(|(prefix, _)| prefix.len());
        let mut out: HashMap<String, Vec<RouteEntry>> = HashMap::new();
        for (prefix, candidates) in ordered {
            let selected = candidates
                .iter()
                .min_by(|a, b| a.cost.cmp(&b.cost).then(a.next_hop.cmp(&b.next_hop)))
                .expect("route bucket is nonempty");
            if resolve_entries(&out, &prefix).is_some_and(|covering| {
                covering.next_hop == selected.next_hop
                    && covering.cost == selected.cost
                    && covering.version == selected.version
            }) {
                continue;
            }
            out.insert(prefix, candidates);
        }
        let after: usize = out.values().map(|v| v.len()).sum();
        self.aggregated = out;
        self.last_report = SupernetReport {
            before,
            after,
            ratio_permille: ((after as u128 * 1000) / before as u128) as u64,
        };
        self.last_report
    }

    /// 聚合后视图上的最长前缀匹配（语义与明细表一致）。
    ///
    /// 同前缀多候选取 (cost, next_hop) 字典序最小——全序稳定。
    pub fn resolve(&self, target_path: &str) -> Option<&RouteEntry> {
        resolve_entries(&self.aggregated, target_path)
    }

    pub fn len(&self) -> usize {
        self.aggregated.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.aggregated.is_empty()
    }
}

fn resolve_entries<'a>(
    entries: &'a HashMap<String, Vec<RouteEntry>>,
    target: &str,
) -> Option<&'a RouteEntry> {
    // Probe only valid UTF-8 boundaries, longest first, rather than scanning
    // every route for each admission and lookup.
    std::iter::once(target.len())
        .chain(target.char_indices().rev().map(|(offset, _)| offset))
        .find_map(|end| {
            entries.get(&target[..end]).and_then(|candidates| {
                candidates
                    .iter()
                    .min_by(|a, b| a.cost.cmp(&b.cost).then(a.next_hop.cmp(&b.next_hop)))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(prefix: &str, hop: &str, cost: u32, ver: u64) -> RouteEntry {
        RouteEntry {
            prefix: prefix.into(),
            next_hop: hop.into(),
            cost,
            version: ver,
        }
    }

    // F2：最长前缀匹配
    #[test]
    fn longest_prefix_wins() {
        let mut t = RouteTable::new();
        t.merge(&RouteGossip {
            entries: vec![
                entry("parrot://eu-1/", "hub-eu", 2, 1),
                entry("parrot://eu-1/user/", "node-eu-1", 1, 1),
            ],
            digest: 0,
        });
        assert_eq!(
            t.resolve("parrot://eu-1/user/x").unwrap().next_hop,
            "node-eu-1"
        );
        assert_eq!(
            t.resolve("parrot://eu-1/system/y").unwrap().next_hop,
            "hub-eu"
        );
        assert!(t.resolve("parrot://us-9/").is_none());
    }

    // F2：版本合并（高 version 胜；同版本低 cost 胜）
    #[test]
    fn version_and_cost_merge() {
        let mut t = RouteTable::new();
        t.merge(&RouteGossip {
            entries: vec![entry("p/", "h1", 2, 5)],
            digest: 0,
        });
        // 低版本拒
        assert_eq!(
            t.merge(&RouteGossip {
                entries: vec![entry("p/", "h2", 1, 4)],
                digest: 0
            }),
            0
        );
        // 同版本低 cost 纳
        assert_eq!(
            t.merge(&RouteGossip {
                entries: vec![entry("p/", "h2", 1, 5)],
                digest: 0
            }),
            1
        );
        assert_eq!(t.resolve("p/x").unwrap().next_hop, "h2");
    }

    // F2：digest 幂等（同集合异或序无关）
    #[test]
    fn digest_order_independent() {
        let a = vec![entry("a/", "h1", 1, 1), entry("b/", "h2", 2, 2)];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(route_digest(&a), route_digest(&b));
        assert_ne!(route_digest(&a), route_digest(&[entry("a/", "h1", 1, 2)]));
    }

    // F1：hop 推进语义（07 §3.3）
    #[test]
    fn hop_forward_gate() {
        for limit in 0..=u8::MAX {
            for count in 0..=u8::MAX {
                let advanced = u16::from(count) + 1;
                let expected = if advanced < u16::from(limit) {
                    Ok(advanced as u8)
                } else {
                    Err(HopExceeded)
                };
                assert_eq!(
                    hop_forward(count, limit),
                    expected,
                    "hop admission disagrees with widened model: count={count}, limit={limit}"
                );
            }
        }
    }

    // RouteTable classification only; actual mode behavior uses TCP contracts.
    #[test]
    fn route_table_distinguishes_direct_and_relay_next_hops() {
        let mut t = RouteTable::new();
        t.merge(&RouteGossip {
            entries: vec![
                entry("parrot://n1/", "n1", 1, 1),
                entry("parrot://n2/", "hub-x", 2, 1),
            ],
            digest: 0,
        });
        assert!(t.direct_to("n1"));
        assert!(!t.direct_to("hub-x"), "中继不算直连");
        assert!(!t.direct_to("missing"));
        assert_eq!(t.resolve("parrot://n2/actor").unwrap().next_hop, "hub-x");
        t.merge(&RouteGossip {
            entries: vec![entry("parrot://n2/", "n2", 1, 2)],
            digest: 0,
        });
        assert!(t.direct_to("n2"));
        assert!(!t.direct_to("hub-x"));
    }

    // ===== S2（DEV_06 §2）：超网聚合 + 路由表压缩 =====

    /// S2-1 小表直通（≤10k 不压缩——避免抖动）
    #[test]
    fn s2_small_table_passthrough() {
        let mut agg = SupernetAggregator::new();
        let entries = vec![entry("parrot://eu-1/", "hub-eu", 2, 1)];
        let r = agg.compact(&entries);
        assert_eq!(r.before, 1);
        assert_eq!(r.after, 1);
        assert_eq!(r.ratio_permille, 1000);
        // resolve 语义不变
        assert_eq!(agg.resolve("parrot://eu-1/x").unwrap().next_hop, "hub-eu");
    }

    /// Explicit covering advertisements permit redundant detail removal.
    #[test]
    fn s2_supernet_compact() {
        let mut agg = SupernetAggregator::new();
        // 12k 明细：eu-1/user/0..5999（hub-eu）、eu-1/system/0..5999（hub-eu）
        let mut entries = vec![
            entry("parrot://eu-1/user/", "hub-eu", 2, 1),
            entry("parrot://eu-1/system/", "hub-eu", 2, 1),
        ];
        for i in 0..6000 {
            entries.push(entry(&format!("parrot://eu-1/user/{i}/"), "hub-eu", 2, 1));
            entries.push(entry(&format!("parrot://eu-1/system/{i}/"), "hub-eu", 2, 1));
        }
        let r = agg.compact(&entries);
        assert_eq!(r.before, 12_002);
        // 明细被压成 父前缀超网：parrot://eu-1/user/ + parrot://eu-1/system/
        assert!(r.after <= 2, "after = {}", r.after);
        assert!(r.ratio_permille < 10, "压缩比 ‰ = {}", r.ratio_permille);
        // resolve 仍可达：明细路径命中聚合超网
        assert_eq!(
            agg.resolve("parrot://eu-1/user/123/echo").unwrap().next_hop,
            "hub-eu"
        );
        assert_eq!(
            agg.resolve("parrot://eu-1/system/42/x").unwrap().next_hop,
            "hub-eu"
        );
        // 无关节点仍不可达
        assert!(agg.resolve("parrot://us-9/").is_none());
    }

    /// S2-3 不同 next_hop 不合并（父前缀同但下一跳异 → 各留一条）
    #[test]
    fn s2_no_cross_hop_merge() {
        let mut agg = SupernetAggregator::new();
        let mut entries = vec![
            entry("parrot://eu-1/user/", "hub-a", 2, 1),
            entry("parrot://eu-1/system/", "hub-b", 2, 1),
        ];
        for i in 0..5001 {
            entries.push(entry(&format!("parrot://eu-1/user/{i}/"), "hub-a", 2, 1));
        }
        for i in 0..5001 {
            entries.push(entry(&format!("parrot://eu-1/system/{i}/"), "hub-b", 2, 1));
        }
        let r = agg.compact(&entries);
        assert_eq!(r.before, 10_004);
        // 两个父前缀（user/ system/）各一条 → 2 条
        assert_eq!(r.after, 2);
        assert_eq!(
            agg.resolve("parrot://eu-1/user/9/").unwrap().next_hop,
            "hub-a"
        );
        assert_eq!(
            agg.resolve("parrot://eu-1/system/9/").unwrap().next_hop,
            "hub-b"
        );
    }

    /// S2-4 聚合幂等：同输入两次 compact 结果一致
    #[test]
    fn s2_compact_idempotent() {
        let mut agg = SupernetAggregator::new();
        let mut entries = vec![entry("parrot://eu-1/user/", "hub", 2, 1)];
        for i in 0..11_000 {
            entries.push(entry(&format!("parrot://eu-1/user/{i}/"), "hub", 2, 1));
        }
        let r1 = agg.compact(&entries);
        let r2 = agg.compact(&entries);
        assert_eq!(r1, r2);
        assert_eq!(agg.len(), r2.after);
        assert!(r1.before > SUPERNET_THRESHOLD);
        assert_eq!(r1.after, 1);
    }

    /// S2-5 根前缀不可再聚合（"parrot://eu-1/" 直通）
    #[test]
    fn s2_root_prefix_passthrough() {
        let mut agg = SupernetAggregator::new();
        let mut entries = Vec::new();
        for i in 0..10_001 {
            entries.push(entry(&format!("parrot://eu-1/{i}/"), "hub", 2, 1));
        }
        let r = agg.compact(&entries);
        assert_eq!(r.after, entries.len());
        assert!(agg.resolve("parrot://eu-1/missing/").is_none());
        entries.push(entry("parrot://eu-1/", "hub", 2, 1));
        assert_eq!(agg.compact(&entries).after, 1);
        assert!(agg.resolve("parrot://another-node/x").is_none());
    }

    /// Actual helper compaction of 50,000 details under 100 explicit cluster
    /// advertisements. This is not a network federation or node-residency test.
    #[test]
    fn s2_supernet_aggregation_500_nodes() {
        let clusters = 100;
        let nodes_per_cluster = 500;

        let mut big_local = Vec::new();
        for c in 0..clusters {
            big_local.push(entry(
                &format!("parrot://prod/c{c}/"),
                &format!("border-c{c}"),
                2,
                1,
            ));
            for i in 0..nodes_per_cluster {
                big_local.push(entry(
                    &format!("parrot://prod/c{}/user-{i}/", c),
                    &format!("border-c{c}"),
                    2,
                    1,
                ));
            }
        }
        let mut agg = SupernetAggregator::new();
        let r = agg.compact(&big_local);
        assert_eq!(r.before, clusters * (nodes_per_cluster + 1));
        assert_eq!(r.after, clusters);
        for c in 0..clusters {
            for i in 0..nodes_per_cluster {
                let selected = agg
                    .resolve(&format!("parrot://prod/c{c}/user-{i}/x"))
                    .expect("advertised cluster must retain every detail");
                assert_eq!(selected.prefix, format!("parrot://prod/c{c}/"));
                assert_eq!(selected.next_hop, format!("border-c{c}"));
                assert_eq!((selected.cost, selected.version), (2, 1));
            }
        }
        assert!(agg.resolve("parrot://prod/unadvertised/x").is_none());
        let total_details = clusters * nodes_per_cluster;
        assert!(
            (total_details / r.after) >= 500,
            "压缩比 {} < 500",
            total_details / r.after
        );
    }

    /// S2-REG-1（缺陷回归）：同父前缀多 next_hop 桶不得互相覆盖。
    ///
    /// 历史 bug：compact 桶 key=(parent, hop) 但落表 key=prefix →
    /// 同父前缀异 hop 的桶后者覆盖前者——静默丢路由。
    /// 场景：svc-a 明细走 hub-a，svc-b 明细走 hub-b，父前缀相同。
    /// 正确语义：聚合视图两个 hop 都必须可达。
    #[test]
    fn s2_reg_multi_hop_same_parent_no_overwrite() {
        let mut agg = SupernetAggregator::new();
        let mut entries = Vec::new();
        // 超 10k 阈值触发真实压缩分支
        for i in 0..200 {
            entries.push(entry(
                &format!("parrot://eu-1/user/svc-a-{i}/"),
                "hub-a",
                2,
                1,
            ));
        }
        for i in 0..200 {
            entries.push(entry(
                &format!("parrot://eu-1/user/svc-b-{i}/"),
                "hub-b",
                2,
                1,
            ));
        }
        // filler：别的父前缀（不干扰本断言）
        for i in 0..9_601 {
            entries.push(entry(&format!("parrot://eu-1/sys-{i}/"), "hub-c", 2, 1));
        }
        let r = agg.compact(&entries);
        assert_eq!(r.before, 10_001);
        // 语义要求：svc-a 系列可达 hub-a 且 svc-b 系列可达 hub-b
        let hit_a = agg
            .resolve("parrot://eu-1/user/svc-a-42/x")
            .expect("svc-a 路由不得丢失");
        let hit_b = agg
            .resolve("parrot://eu-1/user/svc-b-42/x")
            .expect("svc-b 路由不得丢失");
        // 关键断言：两个 hop 都在（不能一个覆盖另一个）
        assert_eq!(hit_a.next_hop, "hub-a");
        assert_eq!(hit_b.next_hop, "hub-b");
    }
}
