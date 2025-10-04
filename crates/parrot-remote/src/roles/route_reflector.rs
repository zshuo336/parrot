//! F6 · RouteReflector（DEV_05 §6 / 07 §5.2）。
//!
//! BGP RR 语义：border 节点把本地最优路由"反射"给对等 border——
//! 避免全互联（n² 链路）；RR 自身可以是某个 border（非专用组件）。
//!
//! 与 F2 RouteGossip 的关系：RouteGossip 是传播载体，RouteReflector
//! 是拓扑策略——决定"谁向谁反射"（client→RR→client 扇出，client 间不直连）。

use std::collections::{HashMap, HashSet};

use crate::topology::{RouteEntry, RouteGossip};

/// RouteReflector 状态（某 border 节点上的 RR 实例）。
///
/// - clients：本 RR 的客户端 border 集（配置注入——静态对等）；
/// - 反射规则：从某 client 收到的路由 → 反射给其它 client（且加自己的
///   next_hop 改写可选——纯透传形态不改动）；
/// - 自治防环：origin_id 记在 RouteEntry.version 之外（反射时携带
///   cluster_id/origin——同 origin 不回灌）。
#[derive(Debug)]
pub struct RouteReflector {
    pub id: String,
    pub clients: Vec<String>,
    /// 本地最佳表（反射出去的视图）。
    pub table: HashMap<String, RouteEntry>,
    /// 防环：prefix → 已见 origin 集。
    seen_origins: HashMap<String, HashSet<String>>,
}

impl RouteReflector {
    pub fn new(id: impl Into<String>, clients: Vec<String>) -> Self {
        Self {
            id: id.into(),
            clients,
            table: HashMap::new(),
            seen_origins: HashMap::new(),
        }
    }

    /// client → RR 的路由上报（反射入口）。
    ///
    /// 返回需要扇出给**其它** client 的增量（(client, entries) 列表——
    /// 宿主逐个发送 RouteGossip）。
    pub fn reflect_from(
        &mut self,
        client: &str,
        gossip: &RouteGossip,
        origin: &str,
    ) -> Vec<(String, Vec<RouteEntry>)> {
        let mut accepted: Vec<RouteEntry> = Vec::new();
        for e in &gossip.entries {
            // 最优合并（高 version 低 cost——同 F2 RouteTable 语义）先判
            let better = match self.table.get(&e.prefix) {
                Some(cur) => {
                    e.version > cur.version || (e.version == cur.version && e.cost < cur.cost)
                }
                None => true,
            };
            if !better {
                // 旧版本：记 origin（去重）但不反射
                self.seen_origins
                    .entry(e.prefix.clone())
                    .or_default()
                    .insert(origin.to_string());
                continue;
            }
            // 防环：同 origin 同 version 已见过 → 跳过（回灌抑制）
            let origins = self.seen_origins.entry(e.prefix.clone()).or_default();
            if origins.contains(origin)
                && self.table.get(&e.prefix).map(|c| c.version) == Some(e.version)
            {
                continue;
            }
            origins.insert(origin.to_string());
            self.table.insert(e.prefix.clone(), e.clone());
            accepted.push(e.clone());
        }
        if accepted.is_empty() {
            return Vec::new();
        }
        // 扇出给其它 client（不含来源）
        self.clients
            .iter()
            .filter(|c| c.as_str() != client)
            .map(|c| (c.clone(), accepted.clone()))
            .collect()
    }

    /// RR 本地路由通告（RR 自己也是 border——向 clients 广播）。
    pub fn advertise_local(&mut self, entry: RouteEntry) -> Vec<(String, Vec<RouteEntry>)> {
        self.seen_origins
            .entry(entry.prefix.clone())
            .or_default()
            .insert(self.id.clone());
        self.table.insert(entry.prefix.clone(), entry.clone());
        self.clients
            .iter()
            .map(|c| (c.clone(), vec![entry.clone()]))
            .collect()
    }

    /// 对账 digest（client 与 RR 之间的一致性检查——SWIM 同模式）。
    pub fn digest(&self) -> u64 {
        crate::topology::route_digest(&self.table.values().cloned().collect::<Vec<_>>())
    }
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

    // F6：client 上报 → 反射给其它 client（扇出、不回灌）
    #[test]
    fn reflect_fans_out_to_other_clients() {
        let mut rr = RouteReflector::new("rr-1", vec!["b1".into(), "b2".into(), "b3".into()]);
        let g = RouteGossip {
            entries: vec![entry("parrot://eu-1/", "b1", 1, 1)],
            digest: 0,
        };
        let out = rr.reflect_from("b1", &g, "eu-cluster");
        // b2/b3 收到，b1 不回
        assert_eq!(out.len(), 2);
        for (c, entries) in &out {
            assert_ne!(c, "b1");
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].prefix, "parrot://eu-1/");
        }
        // 同 origin 再来（防环）→ 不再反射
        let out2 = rr.reflect_from("b2", &g, "eu-cluster");
        assert!(out2.is_empty(), "same origin not re-advertised");
    }

    // F6：RR 本地通告
    #[test]
    fn advertise_local_broadcasts() {
        let mut rr = RouteReflector::new("rr-1", vec!["b1".into(), "b2".into()]);
        let out = rr.advertise_local(entry("parrot://hub/", "rr-1", 1, 1));
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|(_, e)| e[0].next_hop == "rr-1"));
    }

    // F6：最优合并（高 version 胜——旧反射不回退）
    #[test]
    fn reflect_best_version_wins() {
        let mut rr = RouteReflector::new("rr", vec!["b1".into(), "b2".into()]);
        let g1 = RouteGossip {
            entries: vec![entry("p/", "b1", 1, 5)],
            digest: 0,
        };
        let out1 = rr.reflect_from("b1", &g1, "o1");
        assert_eq!(out1.len(), 1);
        // 旧版本从另一 origin 来 → 拒
        let g2 = RouteGossip {
            entries: vec![entry("p/", "b2", 1, 4)],
            digest: 0,
        };
        let out2 = rr.reflect_from("b2", &g2, "o2");
        assert!(out2.is_empty(), "stale version not reflected");
        // 新版本 → 纳
        let g3 = RouteGossip {
            entries: vec![entry("p/", "b2", 1, 6)],
            digest: 0,
        };
        let out3 = rr.reflect_from("b2", &g3, "o2");
        assert_eq!(out3.len(), 1);
        assert_eq!(rr.table["p/"].version, 6);
    }
}
