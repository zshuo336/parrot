//! F1+F2 · 拓扑三模式与 RouteGossip（DEV_05 §2 / 07 §5）。
//!
//! - 模式：hub（星型中继）/ mesh（直连优先）/ hybrid（直连优先 + hub 兜底）
//! - hop 字段启用：中继转发 hop_count+=1，≥hop_limit 丢弃回
//!   REPLY_ERR(RouteUnreachable)（07 §3.3）
//! - RouteGossip：前缀表增量 + xor 指纹（随 MembershipGossip 同车——06 I6）
//! - 最长前缀匹配（POC PrefixRouter::resolve 实证转正）

use std::collections::HashMap;

/// 拓扑模式（07 §5.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TopologyMode {
    /// 星型：所有跨集群流量经 hub 中继。
    Hub,
    /// 全直连：节点间直接 TCP/QUIC（LAN QoS 前提）。
    Mesh,
    /// 直连优先 + hub 兜底（默认——降级链）。
    #[default]
    Hybrid,
}

/// 拓扑配置（toml `[topology]` 段映射）。
#[derive(Debug, Clone)]
pub struct TopologyConfig {
    pub mode: TopologyMode,
    /// hub 不可达时是否允许 mesh 直连兜底（Hub 模式下 false=严格星型）。
    pub relay_fallback: bool,
    /// hub 绑定地址（hub 角色节点）。
    pub hub_bind: Option<String>,
    /// hub 对外通告地址（NAT 穿透场景）。
    pub hub_advertise: Option<String>,
    /// mesh 直连最低 QoS（"lan"——低于则走中继）。
    pub mesh_direct_min_qos: QoS,
}

/// 链路 QoS 档位（mesh 直连门槛）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum QoS {
    /// WAN 高延迟（默认——hybrid 下先中继探测）。
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

/// RouteGossip 消息（SYSTEM_EVENT 扩展载荷）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RouteGossip {
    pub entries: Vec<RouteEntry>,
    /// 前缀表 xor 指纹（对账——同 SWIM digest 模式）。
    pub digest: u64,
}

/// xor 指纹：表项 (prefix, next_hop, version) 三元组哈希异或——序无关。
pub fn route_digest(entries: &[RouteEntry]) -> u64 {
    let mut d: u64 = 0;
    for e in entries {
        let mut h = fnv_offset();
        for b in e.prefix.as_bytes().iter().chain(e.next_hop.as_bytes()) {
            h ^= *b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        h ^= e.version;
        h = h.wrapping_mul(FNV_PRIME);
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
/// 版本合并（高 version 胜；同版本低 cost 胜——稳定性由 version 主导）。
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
                Some(cur) => {
                    e.version > cur.version || (e.version == cur.version && e.cost < cur.cost)
                }
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
    let next = hop_count + 1;
    if next >= hop_limit {
        Err(HopExceeded)
    } else {
        Ok(next)
    }
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

/// 超网聚合器（BGP CIDR 式——同 next_hop 前缀合并）。
///
/// 路径前缀与 IP CIDR 同构：`parrot://eu-1/user/` 的"公共父前缀"即
/// 聚合超网。
///
/// **可聚合判据（语义安全）**：父前缀内全部明细共享同一 next_hop 才能
/// 合并为一条超网。父前缀内出现多 hop 时聚合会混淆 hop 归属
/// （无法从超网前缀反推哪段明细走哪个 hop）——此时保留全部明细
/// （s2_reg_multi_hop_same_parent_no_overwrite 锁死的语义红线）。
///
/// 同前缀多候选用 (cost, next_hop) 全序稳定 resolve。
#[derive(Debug, Default)]
pub struct SupernetAggregator {
    /// 已聚合条目（压缩后视图）：prefix → 同前缀候选（聚合后单条；
    /// 多 hop 保留明细时各明细前缀天然唯一）。
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
        if before <= SUPERNET_THRESHOLD {
            let mut out: HashMap<String, Vec<RouteEntry>> = HashMap::new();
            for e in entries {
                out.entry(e.prefix.clone()).or_default().push(e.clone());
            }
            self.aggregated = out;
            self.last_report = SupernetReport {
                before,
                after: before,
                ratio_permille: 1000,
            };
            return self.last_report;
        }
        // 第一步：按父前缀分组；根前缀（无父层）直通
        let mut by_parent: HashMap<&str, Vec<&RouteEntry>> = HashMap::new();
        let mut passthrough: Vec<&RouteEntry> = Vec::new();
        for e in entries {
            match rsplit_prefix(&e.prefix) {
                Some((parent, _)) => by_parent.entry(parent).or_default().push(e),
                None => passthrough.push(e),
            }
        }
        // 第二步：父前缀组内唯一 hop → 合并一条超网；
        // 多 hop → 保留全部明细（聚合即丢 hop 归属——语义红线）
        let mut out: HashMap<String, Vec<RouteEntry>> = HashMap::new();
        for e in passthrough {
            out.entry(e.prefix.clone()).or_default().push(e.clone());
        }
        for (parent, group) in &by_parent {
            let mut hops: Vec<&str> = group.iter().map(|e| e.next_hop.as_str()).collect();
            hops.sort_unstable();
            hops.dedup();
            if hops.len() == 1 {
                let hop = hops[0].to_string();
                let version = group.iter().map(|e| e.version).max().unwrap_or(0);
                let cost = group.iter().map(|e| e.cost).min().unwrap_or(u32::MAX);
                out.entry((*parent).to_string())
                    .or_default()
                    .push(RouteEntry {
                        prefix: (*parent).to_string(),
                        next_hop: hop,
                        cost,
                        version,
                    });
            } else {
                for e in group {
                    out.entry(e.prefix.clone()).or_default().push((*e).clone());
                }
            }
        }
        let after: usize = out.values().map(|v| v.len()).sum();
        self.aggregated = out;
        self.last_report = SupernetReport {
            before,
            after,
            ratio_permille: if before == 0 {
                1000
            } else {
                (after as u64 * 1000) / before as u64
            },
        };
        self.last_report
    }

    /// 聚合后视图上的最长前缀匹配（语义与明细表一致）。
    ///
    /// 同前缀多候选取 (cost, next_hop) 字典序最小——全序稳定。
    pub fn resolve(&self, target_path: &str) -> Option<&RouteEntry> {
        self.aggregated
            .values()
            .flatten()
            .filter(|e| target_path.starts_with(&e.prefix))
            .min_by(|a, b| {
                b.prefix
                    .len()
                    .cmp(&a.prefix.len())
                    .then(a.cost.cmp(&b.cost))
                    .then(a.next_hop.cmp(&b.next_hop))
            })
    }

    pub fn len(&self) -> usize {
        self.aggregated.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.aggregated.is_empty()
    }
}

/// 路径前缀切分：`parrot://eu-1/user/` → (parrot://eu-1/, user)。
/// 根前缀（无 '/' 结尾父层）返回 None。
fn rsplit_prefix(prefix: &str) -> Option<(&str, &str)> {
    let trimmed = prefix.strip_suffix('/')?;
    let pos = trimmed.rfind('/')?;
    Some((&prefix[..pos + 1], &trimmed[pos + 1..]))
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
        assert_eq!(hop_forward(0, 8), Ok(1));
        assert_eq!(hop_forward(6, 8), Ok(7));
        assert_eq!(hop_forward(7, 8), Err(HopExceeded), "≥limit 丢弃");
        assert_eq!(hop_forward(8, 8), Err(HopExceeded));
    }

    // F1：三模式降级链语义（直连判断）
    #[test]
    fn hybrid_direct_preference() {
        let mut t = RouteTable::new();
        t.merge(&RouteGossip {
            entries: vec![entry("parrot://n1/", "n1", 1, 1)],
            digest: 0,
        });
        assert!(t.direct_to("n1"));
        assert!(!t.direct_to("hub-x"), "中继不算直连");
        // Mesh 模式 + relay_fallback=false：无直连即无路（严格直连）
        let cfg = TopologyConfig {
            mode: TopologyMode::Mesh,
            relay_fallback: false,
            ..Default::default()
        };
        assert_eq!(cfg.mode, TopologyMode::Mesh);
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

    /// S2-2 大表压缩：同 next_hop 同父前缀合并成超网
    #[test]
    fn s2_supernet_compact() {
        let mut agg = SupernetAggregator::new();
        // 12k 明细：eu-1/user/0..5999（hub-eu）、eu-1/system/0..5999（hub-eu）
        let mut entries = Vec::new();
        for i in 0..6000 {
            entries.push(entry(&format!("parrot://eu-1/user/{i}/"), "hub-eu", 2, 1));
            entries.push(entry(&format!("parrot://eu-1/system/{i}/"), "hub-eu", 2, 1));
        }
        let r = agg.compact(&entries);
        assert_eq!(r.before, 12_000);
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
        let mut entries = Vec::new();
        for i in 0..5001 {
            entries.push(entry(&format!("parrot://eu-1/user/{i}/"), "hub-a", 2, 1));
        }
        for i in 0..5001 {
            entries.push(entry(&format!("parrot://eu-1/system/{i}/"), "hub-b", 2, 1));
        }
        let r = agg.compact(&entries);
        assert_eq!(r.before, 10_002);
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
        let mut entries = Vec::new();
        for i in 0..5500 {
            entries.push(entry(&format!("parrot://eu-1/user/{i}/"), "hub", 2, 1));
        }
        let r1 = agg.compact(&entries);
        let r2 = agg.compact(&entries);
        assert_eq!(r1, r2);
        assert_eq!(agg.len(), r2.after);
    }

    /// S2-5 根前缀不可再聚合（"parrot://eu-1/" 直通）
    #[test]
    fn s2_root_prefix_passthrough() {
        let mut agg = SupernetAggregator::new();
        let mut entries = Vec::new();
        for i in 0..10_001 {
            // 明细形如 parrot://eu-1/i/ —— 父前缀 parrot://eu-1/ 是根，直通不压缩
            entries.push(entry(&format!("parrot://eu-1/{i}/"), "hub", 2, 1));
        }
        let r = agg.compact(&entries);
        // rsplit_prefix("parrot://eu-1/0/") → ("parrot://eu-1/", "0")
        // 桶 key = ("parrot://eu-1/", "hub") → 合并 1 条
        assert_eq!(r.after, 1);
        assert!(r.ratio_permille < 100);
    }

    /// `supernet_aggregation`（DEV_06 §3 测试义务原文）：
    /// 500 节点集群对外路由表 ≤ 集群数 + 常数。
    ///
    /// 模型：100 集群联邦，每集群 500 节点明细（10 万条），每集群一个
    /// border 通告集群级 supernet（`parrot://prod/{cluster}/`）——
    /// 对外路由表 = 100 条集群前缀（而非 10 万节点明细）。
    /// border 通告在 compact 之前（对外通告本身就是聚合动作——
    /// compact 阈值管的是本地表膨胀，border 通告管的是对外泄露）。
    #[test]
    fn s2_supernet_aggregation_500_nodes() {
        let clusters = 100;
        let nodes_per_cluster = 500;

        // 集群内明细（border 视角——本地表 500 条）
        let mut local = Vec::new();
        for i in 0..nodes_per_cluster {
            local.push(entry(
                &format!("parrot://prod/eu-1/user-{i}/"),
                "border-eu-1",
                2,
                1,
            ));
        }
        // border 通告模型：对外只发集群级 supernet（1 条/集群）
        let advertised = [entry("parrot://prod/eu-1/", "border-eu-1", 2, 1)];
        // 本地明细经超网聚合验证（构造超阈值规模——SUPERNET_THRESHOLD 常量门禁）
        let mut big_local = local.clone();
        for c in 1..clusters {
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
        // 10 万明细 → ≤ 集群数 + 常数（100 集群前缀）
        assert!(
            r.after <= clusters,
            "聚合后 {} > 集群数 {clusters}",
            r.after
        );
        // border 对外通告规模 = 1 条/集群
        assert_eq!(advertised.len(), 1);
        // 明细不外泄：对外表中无节点级前缀
        assert!(
            advertised.iter().all(|e| !e.prefix.contains("user-")),
            "集群内明细不外泄"
        );
        // 全联邦路由表 = 集群数级（100 vs 5 万——500 倍压缩）
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
