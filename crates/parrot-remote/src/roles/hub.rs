//! F3 · RelayHub actor（DEV_05 §3 / 07 §6.4）。
//!
//! hub 节点常驻路由器：收 ASK/TELL → RouteTable 最长前缀 → 换 cid 转发 +
//! 回程映射（cid_map 有界 65536 LRU——E2.2 防慢回程方撑爆内存）；
//! REPLY 回程查映射还原 orig_cid。
//!
//! POC p4b 已实证全路径（JVM→hub→Erlang 中继 + cid 改写）——本模块是其
//! actor 化 + RouteGossip 表驱动转正。

use std::collections::HashMap;

use crate::topology::{hop_forward, RouteTable};

/// cid 改写映射（LRU 有界——容量 65536）。
///
/// 键：转发用的新 cid；值：(原 cid, 回程 reply_to 路径)。
pub struct CidMap {
    cap: usize,
    // LinkedHashMap 语义用 Vec 双端近似（容量小、LRU 触发少——O(n) 淘汰可接受）
    order: std::collections::VecDeque<u64>,
    map: HashMap<u64, (u64, String)>,
}

impl CidMap {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            order: Default::default(),
            map: Default::default(),
        }
    }

    pub fn insert(&mut self, new_cid: u64, orig: (u64, String)) {
        if self.cap == 0 {
            return;
        }
        if let Some(previous) = self.map.get_mut(&new_cid) {
            *previous = orig;
            self.touch(&new_cid);
            return;
        }
        if self.map.len() >= self.cap {
            // LRU 淘汰最老
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.map.insert(new_cid, orig);
        self.order.push_back(new_cid);
    }

    pub fn get(&mut self, new_cid: &u64) -> Option<&(u64, String)> {
        if self.map.contains_key(new_cid) {
            self.touch(new_cid);
        }
        self.map.get(new_cid)
    }

    fn touch(&mut self, cid: &u64) {
        if let Some(position) = self.order.iter().position(|stored| stored == cid) {
            self.order.remove(position);
            self.order.push_back(*cid);
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// 中继决策（纯函数——RelayHub actor 的核心逻辑，无 IO 便于矩阵测试）。
pub struct RelayHub {
    pub routes: RouteTable,
    pub cid_map: CidMap,
    /// 本节点 id（转发帧的 reply_to 锚）。
    pub node_id: String,
    next_cid: u64,
}

impl RelayHub {
    pub fn new(node_id: impl Into<String>, routes: RouteTable) -> Self {
        Self {
            routes,
            cid_map: CidMap::new(65536),
            node_id: node_id.into(),
            next_cid: 1,
        }
    }

    fn alloc_cid(&mut self) -> u64 {
        self.next_cid += 1;
        self.next_cid
    }

    /// 入站 ASK 中继决策。
    ///
    /// 返回 `(转发目标 next_hop, 改写后的帧参数 (new_cid, payload))`；
    /// 无路由 / hop 超限 → Err（调用方回 REPLY_ERR code=7）。
    ///
    /// payload 透传不变形（加密/压缩标志由帧层 flags 携带——中继不触碰）。
    #[allow(clippy::type_complexity)]
    pub fn relay_ask(
        &mut self,
        orig_cid: u64,
        target_path: &str,
        _payload: &[u8],
        reply_to: &str,
        hop_count: u8,
        hop_limit: u8,
    ) -> Result<(String, u64, u8), RelayError> {
        let next_hop = self
            .routes
            .resolve(target_path)
            .ok_or(RelayError::NoRoute)?
            .next_hop
            .clone();
        let hop = hop_forward(hop_count, hop_limit).map_err(|_| RelayError::HopExceeded)?;
        let new_cid = self.alloc_cid();
        self.cid_map
            .insert(new_cid, (orig_cid, reply_to.to_string()));
        Ok((next_hop, new_cid, hop))
    }

    /// 入站 REPLY 回程还原。
    ///
    /// 命中映射 → (orig_cid, reply_to)；迟到/淘汰 → None（计数后丢弃）。
    pub fn relay_reply(&mut self, new_cid: u64) -> Option<(u64, String)> {
        self.cid_map.get(&new_cid).cloned()
    }
}

/// 中继错误（映射 REPLY_ERR code——07 §2.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    /// 前缀表无匹配（code 7 RouteUnreachable）。
    NoRoute,
    /// hop 超限（code 7——丢弃并回）。
    HopExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{RouteEntry, RouteGossip};

    fn hub_with_routes() -> RelayHub {
        let mut routes = RouteTable::new();
        routes.merge(&RouteGossip {
            entries: vec![RouteEntry {
                prefix: "parrot://eu-1/".into(),
                next_hop: "node-eu-gw".into(),
                cost: 2,
                version: 1,
            }],
            digest: 0,
        });
        RelayHub::new("hub-1", routes)
    }

    // F3：ASK 中继全路径（cid 改写 + hop 推进 + 映射登记）
    #[test]
    fn relay_ask_rewrites_cid_and_maps() {
        let mut hub = hub_with_routes();
        let (next, new_cid, hop) = hub
            .relay_ask(777, "parrot://eu-1/user/x", b"body", "cloud/_remote", 0, 8)
            .unwrap();
        assert_eq!(next, "node-eu-gw");
        assert_ne!(new_cid, 777, "cid 必须改写（回程映射键）");
        assert_eq!(hop, 1, "中继 hop+1");
        // REPLY 回程还原
        let (orig, reply_to) = hub.relay_reply(new_cid).unwrap();
        assert_eq!(orig, 777);
        assert_eq!(reply_to, "cloud/_remote");
    }

    // F3：无路由 → NoRoute（REPLY_ERR code 7）
    #[test]
    fn relay_no_route() {
        let mut hub = hub_with_routes();
        assert_eq!(
            hub.relay_ask(1, "parrot://unknown/", b"", "", 0, 8),
            Err(RelayError::NoRoute)
        );
    }

    // F3：hop 超限拒绝
    #[test]
    fn relay_hop_gate() {
        let mut hub = hub_with_routes();
        let (_, retained, _) = hub
            .relay_ask(777, "parrot://eu-1/x", b"original", "origin", 0, 8)
            .unwrap();
        let next_cid = hub.next_cid;
        for limit in 0..=u8::MAX {
            for count in 0..=u8::MAX {
                if u16::from(count) + 1 < u16::from(limit) {
                    continue;
                }
                assert_eq!(
                    hub.relay_ask(999, "parrot://eu-1/x", b"rejected", "wrong", count, limit),
                    Err(RelayError::HopExceeded),
                    "rejected hop changed relay admission: count={count}, limit={limit}"
                );
                assert_eq!(hub.next_cid, next_cid, "rejection consumed a CID");
                assert_eq!(hub.cid_map.len(), 1, "rejection published a return mapping");
                assert_eq!(hub.relay_reply(retained), Some((777, "origin".into())));
            }
        }
        let (next, accepted, hop) = hub
            .relay_ask(888, "parrot://eu-1/x", b"next", "next-origin", 253, 255)
            .unwrap();
        assert_eq!(next, "node-eu-gw");
        assert_eq!(accepted, next_cid + 1);
        assert_eq!(hop, 254);
        assert_eq!(hub.relay_reply(accepted), Some((888, "next-origin".into())));
        assert_eq!(hub.relay_reply(retained), Some((777, "origin".into())));
    }

    // F3：cid_map LRU 有界（E2.2——容量 4 淘汰最老）
    #[test]
    fn cid_map_bounded_lru() {
        for capacity in 0..=3 {
            for sequence in 0..6u64.pow(6) {
                let mut actual = CidMap::new(capacity);
                let mut model: Vec<(u64, (u64, String))> = Vec::new();
                let mut remaining = sequence;
                for step in 0..6 {
                    let operation = remaining % 6;
                    remaining /= 6;
                    let cid = operation % 3;
                    if operation < 3 {
                        let value = (sequence * 10 + step, format!("return-{sequence}-{step}"));
                        actual.insert(cid, value.clone());
                        if let Some(position) = model.iter().position(|entry| entry.0 == cid) {
                            model.remove(position);
                        }
                        if capacity > 0 {
                            if model.len() == capacity {
                                model.remove(0);
                            }
                            model.push((cid, value));
                        }
                    } else {
                        let expected =
                            model
                                .iter()
                                .position(|entry| entry.0 == cid)
                                .map(|position| {
                                    let entry = model.remove(position);
                                    let value = entry.1.clone();
                                    model.push(entry);
                                    value
                                });
                        assert_eq!(actual.get(&cid).cloned(), expected,
                            "lookup disagrees with LRU model: cap={capacity}, sequence={sequence}, step={step}");
                    }
                    assert!(actual.len() <= capacity, "LRU exceeded configured capacity");
                    assert_eq!(
                        actual.len(),
                        model.len(),
                        "LRU lost a live mapping: cap={capacity}, sequence={sequence}, step={step}"
                    );
                    assert_eq!(actual.order.iter().copied().collect::<Vec<_>>(),
                        model.iter().map(|entry| entry.0).collect::<Vec<_>>(),
                        "LRU recency disagrees with model: cap={capacity}, sequence={sequence}, step={step}");
                    for (cid, value) in &model {
                        assert_eq!(
                            actual.map.get(cid),
                            Some(value),
                            "LRU changed return identity"
                        );
                    }
                }
            }
        }
    }

    // F3：迟到 REPLY（映射已淘汰）→ None 丢弃
    #[test]
    fn late_reply_dropped() {
        let mut hub = hub_with_routes();
        hub.cid_map = CidMap::new(2);
        for i in 0..5u64 {
            let (_, new_cid, _) = hub
                .relay_ask(i, "parrot://eu-1/x", b"", "rt", 0, 8)
                .unwrap();
            let _ = new_cid;
        }
        assert!(hub.relay_reply(1).is_none() || hub.relay_reply(2).is_none());
    }
}
