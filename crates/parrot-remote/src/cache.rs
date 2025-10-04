//! F7 · 节点缓存 + RESOLVE/INVALIDATE 处理（DEV_05 §6 / 07 §3.2 ④）。
//!
//! 解析管线：miss/stale → RESOLVE_Q → Directory/border 代理 → RESOLVE_R
//! → 写缓存 → 直连；直连失败回源一次 → 仍失败中继降级（F1 降级链）。
//!
//! 三态（07 §6.4）：Fresh（TTL 60s 内）/ Stale（过期但可用 + 降级标记）/
//! Invalid（INVALIDATE 置位——必须重解析）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 缓存三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    /// TTL 内——直接命中。
    Fresh,
    /// 过期但目录不可达时续服务（300s 窗口——07 §9 降级）。
    Stale,
    /// INVALIDATE 置位——下次必须 RESOLVE。
    Invalid,
}

/// 缓存条目。
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub endpoints: Vec<String>,
    pub version: u64,
    pub state: CacheState,
    pub fetched_at: Instant,
    /// S2 LRU 逻辑时钟（put/get 推进）。
    pub lru: u64,
}

/// 目录解析缓存（前缀/节点 → 端点集）。
///
/// S2（DEV_06 §2）：工作集管理——容量上限 + LRU 淘汰 + 命中/穿透统计。
pub struct ResolveCache {
    entries: HashMap<String, CacheEntry>,
    /// Fresh 窗口（TTL）。
    pub ttl: Duration,
    /// Stale 续服务窗口（Directory 全灭兜底——07 §9）。
    pub stale_window: Duration,
    /// S2 工作集容量上限（默认 65536——07 §14.4 缓存工作集上限）。
    pub capacity: usize,
    /// LRU 时钟（单调递增——put/get 推进）。
    lru_clock: u64,
    /// 命中统计（S2 观测——命中率门禁 ≥90% 用）。
    hits: u64,
    misses: u64,
}

impl Default for ResolveCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(60),
            stale_window: Duration::from_secs(300),
            capacity: 65_536,
            lru_clock: 0,
            hits: 0,
            misses: 0,
        }
    }
}

/// S2 缓存命中率观测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    /// 命中率（万分比——整数避免浮点不稳）。
    pub hit_ratio_permille: u64,
}

impl CacheStats {
    pub fn hit_ratio_percent(&self) -> f64 {
        if self.hits + self.misses == 0 {
            0.0
        } else {
            self.hits as f64 * 100.0 / (self.hits + self.misses) as f64
        }
    }
}

impl ResolveCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// S2：容量显式构造。
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            ..Default::default()
        }
    }

    /// S2：命中统计快照。
    pub fn stats(&self) -> CacheStats {
        let total = self.hits + self.misses;
        CacheStats {
            hits: self.hits,
            misses: self.misses,
            hit_ratio_permille: self
                .hits
                .checked_mul(1000)
                .and_then(|h| {
                    if total == 0 {
                        None
                    } else {
                        h.checked_div(total)
                    }
                })
                .unwrap_or(0),
        }
    }

    /// S2：LRU 驱动（put/get 推进逻辑时钟）。
    fn tick_lru(&mut self) -> u64 {
        self.lru_clock += 1;
        self.lru_clock
    }

    /// S2：容量溢出 → 淘汰最久未用条目（Invalid 优先——即失效者先走）。
    fn evict_if_full(&mut self) {
        while self.entries.len() > self.capacity {
            // 双优先级：Valid 态排后（保留），Invalid 态最久未用者先走
            let victim = self
                .entries
                .iter()
                .min_by_key(|(_, e)| (e.state != CacheState::Invalid, e.lru, e.fetched_at))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    /// 查询：返回 (endpoints, state)——Invalid/miss 返回 None（须 RESOLVE）。
    ///
    /// 注意 Stale 命中也返回（可用 + 降级标记——调用方决定是否回源）。
    /// S2：命中/穿透计数（Invalid 视为穿透）。
    pub fn get(&mut self, key: &str, now: Instant) -> Option<(&[String], CacheState)> {
        let lru = self.tick_lru();
        // 真 miss（key 不存在）也计穿透——命中率观测完整
        let Some(e) = self.entries.get_mut(key) else {
            self.misses += 1;
            return None;
        };
        e.lru = lru;
        // state_of 内联（避免 &self/&mut self 交叉借用）
        let st = match e.state {
            CacheState::Invalid => CacheState::Invalid,
            _ => {
                let age = now.duration_since(e.fetched_at);
                if age <= self.ttl {
                    CacheState::Fresh
                } else if age <= self.stale_window {
                    CacheState::Stale
                } else {
                    CacheState::Invalid
                }
            }
        };
        match st {
            CacheState::Fresh | CacheState::Stale => {
                self.hits += 1;
                Some((&e.endpoints, st))
            }
            CacheState::Invalid => {
                self.misses += 1;
                None
            }
        }
    }

    /// RESOLVE_R 回写（Fetched）。
    pub fn put(
        &mut self,
        key: impl Into<String>,
        endpoints: Vec<String>,
        version: u64,
        now: Instant,
    ) {
        let lru = self.tick_lru();
        self.entries.insert(
            key.into(),
            CacheEntry {
                endpoints,
                version,
                state: CacheState::Fresh,
                fetched_at: now,
                lru,
            },
        );
        self.evict_if_full();
    }

    /// INVALIDATE 推送（Directory 条目变更 → 订阅节点）。
    pub fn invalidate(&mut self, key: &str) {
        if let Some(e) = self.entries.get_mut(key) {
            e.state = CacheState::Invalid;
        }
    }

    /// 批量失效（前缀匹配——Directory BorderDeclare/Remove 级联）。
    pub fn invalidate_prefix(&mut self, prefix: &str) -> usize {
        let hits: Vec<String> = self
            .entries
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        for k in &hits {
            self.invalidate(k);
        }
        hits.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 解析管线决策（07 §3.2 ④——纯函数便于矩阵测试）。
///
/// 返回需执行的动作序列：直连 / 回源后直连 / 中继降级。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveAction {
    /// 缓存 Fresh——零网络开销直接连（性能预算 §12 最后一行）。
    DirectCache { endpoint: String },
    /// Stale——先试直连（快路径），失败再走 ResolveThen。
    TryDirectElseResolve { endpoint: String },
    /// Invalid/miss——RESOLVE_Q 回源。
    Resolve { key: String },
    /// 缓存空端点集——中继降级（F1）。
    RelayFallback { key: String },
}

pub fn resolve_decision(cache: &mut ResolveCache, key: &str, now: Instant) -> ResolveAction {
    let got = cache.get(key, now);
    match got {
        Some((endpoints, CacheState::Fresh)) => match endpoints.first() {
            Some(ep) => ResolveAction::DirectCache {
                endpoint: ep.clone(),
            },
            None => ResolveAction::RelayFallback { key: key.into() },
        },
        Some((endpoints, CacheState::Stale)) => match endpoints.first() {
            Some(ep) => ResolveAction::TryDirectElseResolve {
                endpoint: ep.clone(),
            },
            None => ResolveAction::RelayFallback { key: key.into() },
        },
        // Invalid / miss（get 返回 None）
        Some((_, CacheState::Invalid)) | None => ResolveAction::Resolve { key: key.into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // F7：三态流转（Fresh → Stale → Invalid）
    #[test]
    fn cache_state_transitions() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("parrot://n1", vec!["tcp://1.1.1.1:7".into()], 1, t0);
        // Fresh：TTL 60s 内
        assert_eq!(
            c.get("parrot://n1", t0 + Duration::from_secs(30))
                .unwrap()
                .1,
            CacheState::Fresh
        );
        // Stale：60s..300s（可用 + 降级标记）
        assert_eq!(
            c.get("parrot://n1", t0 + Duration::from_secs(120))
                .unwrap()
                .1,
            CacheState::Stale
        );
        // 超 300s 窗口 → Invalid（须重解析）
        assert!(c
            .get("parrot://n1", t0 + Duration::from_secs(301))
            .is_none());
    }

    // F7：resolve_cache_hit_zero_rtt（命中零网络开销）
    #[test]
    fn resolve_cache_hit_zero_rtt() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("parrot://n1", vec!["tcp://1.1.1.1:7".into()], 1, t0);
        assert_eq!(
            resolve_decision(&mut c, "parrot://n1", t0 + Duration::from_secs(1)),
            ResolveAction::DirectCache {
                endpoint: "tcp://1.1.1.1:7".into()
            }
        );
    }

    // F7：invalidate_push（Directory 变更 → Invalid → 下次重新 RESOLVE）
    #[test]
    fn invalidate_push() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("parrot://eu-1/user/svc", vec!["tcp://x:1".into()], 3, t0);
        assert!(matches!(
            resolve_decision(&mut c, "parrot://eu-1/user/svc", t0),
            ResolveAction::DirectCache { .. }
        ));
        c.invalidate("parrot://eu-1/user/svc");
        assert_eq!(
            resolve_decision(&mut c, "parrot://eu-1/user/svc", t0),
            ResolveAction::Resolve {
                key: "parrot://eu-1/user/svc".into()
            }
        );
        // 新版本回写恢复 Fresh
        c.put("parrot://eu-1/user/svc", vec!["tcp://y:2".into()], 4, t0);
        assert!(matches!(
            resolve_decision(&mut c, "parrot://eu-1/user/svc", t0),
            ResolveAction::DirectCache { endpoint } if endpoint == "tcp://y:2"
        ));
    }

    // F7：directory_down_stale_service（目录全灭 → Stale 续服务 300s）
    #[test]
    fn directory_down_stale_service() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("parrot://n1", vec!["tcp://1.1.1.1:7".into()], 1, t0);
        // 目录挂了（无法 RESOLVE）——Stale 期决策仍是"试直连"
        let at = t0 + Duration::from_secs(200);
        assert_eq!(
            resolve_decision(&mut c, "parrot://n1", at),
            ResolveAction::TryDirectElseResolve {
                endpoint: "tcp://1.1.1.1:7".into()
            }
        );
    }

    // F7：前缀级联失效
    #[test]
    fn prefix_invalidate_cascade() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("parrot://eu-1/a", vec!["x".into()], 1, t0);
        c.put("parrot://eu-1/b", vec!["y".into()], 1, t0);
        c.put("parrot://us-1/c", vec!["z".into()], 1, t0);
        assert_eq!(c.invalidate_prefix("parrot://eu-1/"), 2);
        assert!(matches!(
            resolve_decision(&mut c, "parrot://eu-1/a", t0),
            ResolveAction::Resolve { .. }
        ));
        assert!(matches!(
            resolve_decision(&mut c, "parrot://us-1/c", t0),
            ResolveAction::DirectCache { .. }
        ));
    }

    // F7：空端点 → 中继降级
    #[test]
    fn empty_endpoints_relay_fallback() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("parrot://dead", vec![], 1, t0);
        assert_eq!(
            resolve_decision(&mut c, "parrot://dead", t0),
            ResolveAction::RelayFallback {
                key: "parrot://dead".into()
            }
        );
    }

    // ===== S2（DEV_06 §2）：缓存工作集 =====

    /// S2-6 容量上限 + LRU 淘汰：溢出时最久未用者先走
    #[test]
    fn s2_lru_eviction() {
        let mut c = ResolveCache::with_capacity(3);
        let t0 = Instant::now();
        c.put("k1", vec!["a".into()], 1, t0);
        c.put("k2", vec!["b".into()], 1, t0);
        c.put("k3", vec!["c".into()], 1, t0);
        // touch k1（变最新）
        c.get("k1", t0);
        // 溢出：k2 成最久未用 → 被淘汰
        c.put("k4", vec!["d".into()], 1, t0);
        assert_eq!(c.len(), 3);
        assert!(c.get("k2", t0).is_none(), "k2 应被 LRU 淘汰");
        assert!(c.get("k1", t0).is_some(), "k1 被 touch 保留");
        assert!(c.get("k4", t0).is_some());
    }

    /// S2-7 Invalid 优先淘汰（失效者先走——保留有效工作集）
    #[test]
    fn s2_invalid_evicted_first() {
        let mut c = ResolveCache::with_capacity(2);
        let t0 = Instant::now();
        c.put("fresh_a", vec!["a".into()], 1, t0);
        c.put("fresh_b", vec!["b".into()], 1, t0);
        // fresh_a 更老，但 fresh_b 被显式失效
        c.get("fresh_a", t0); // touch a → b 变最久未用
        c.invalidate("fresh_b");
        c.put("new_c", vec!["c".into()], 1, t0);
        // Invalid 的 fresh_b 先被淘汰，fresh_a 保留
        assert!(c.get("fresh_a", t0).is_some());
        assert!(c.get("new_c", t0).is_some());
        assert_eq!(c.len(), 2);
    }

    /// S2-8 命中率观测：stats 门禁（≥90%）基件
    #[test]
    fn s2_hit_ratio_stats() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        // 9 次命中
        c.put("k", vec!["a".into()], 1, t0);
        for _ in 0..9 {
            assert!(c.get("k", t0).is_some());
        }
        // 1 次穿透（miss）
        assert!(c.get("absent", t0).is_none());
        let s = c.stats();
        assert_eq!(s.hits, 9);
        assert_eq!(s.misses, 1);
        assert_eq!(s.hit_ratio_permille, 900);
        assert!((s.hit_ratio_percent() - 90.0).abs() < 1e-9);
    }

    /// S2-9 默认容量 65536（07 §14.4 工作集上限）
    #[test]
    fn s2_default_capacity() {
        let c = ResolveCache::new();
        assert_eq!(c.capacity, 65_536);
    }

    // ===== S2 深度回归（真实语义审计补充） =====

    /// S2-REG-1：容量 1 的极限压测——每次 put 都触发淘汰，get 仍可命中刚写入项
    #[test]
    fn s2_reg_capacity_one() {
        let mut c = ResolveCache::with_capacity(1);
        let t0 = Instant::now();
        c.put("k1", vec!["a".into()], 1, t0);
        assert!(c.get("k1", t0).is_some());
        c.put("k2", vec!["b".into()], 1, t0);
        assert_eq!(c.len(), 1, "容量 1：k1 被淘汰");
        assert!(c.get("k1", t0).is_none());
        assert!(c.get("k2", t0).is_some());
    }

    /// S2-REG-2：invalidate_prefix 不放大淘汰——失效标记后容量淘汰优先清走
    #[test]
    fn s2_reg_prefix_invalidate_then_evict() {
        let mut c = ResolveCache::with_capacity(4);
        let t0 = Instant::now();
        for i in 0..4 {
            c.put(format!("parrot://eu/{i}"), vec!["a".into()], 1, t0);
        }
        // 全部失效（前缀级联）
        assert_eq!(c.invalidate_prefix("parrot://eu/"), 4);
        assert_eq!(c.len(), 4, "失效是标记不是删除");
        // 溢出写入：Invalid 4 条应先于 Fresh 被清
        c.put("new", vec!["n".into()], 1, t0);
        assert!(c.len() <= 4);
        // 失效条目已腾位——new 可查且不误伤
        assert!(c.get("new", t0).is_some());
    }

    /// S2-REG-3：stats 计数不受 put 影响（只有 get 计命中/穿透）
    #[test]
    fn s2_reg_stats_only_count_gets() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("k", vec!["a".into()], 1, t0);
        let s = c.stats();
        assert_eq!((s.hits, s.misses), (0, 0), "put 不计数");
        let _ = c.get("k", t0);
        let _ = c.get("absent", t0);
        let s = c.stats();
        assert_eq!((s.hits, s.misses), (1, 1));
    }

    /// S2-REG-4：Stale 命中计 hit（可用续服务——07 §9 降级语义）
    #[test]
    fn s2_reg_stale_counts_as_hit() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("k", vec!["a".into()], 1, t0);
        let at = t0 + Duration::from_secs(120); // Stale 窗口
        assert!(c.get("k", at).is_some());
        let s = c.stats();
        assert_eq!(s.hits, 1, "Stale 续服务是命中不是穿透");
    }

    /// S2-REG-5：失效条目 get 计 miss（Invalid = 穿透——必须重新 RESOLVE）
    #[test]
    fn s2_reg_invalid_counts_as_miss() {
        let mut c = ResolveCache::new();
        let t0 = Instant::now();
        c.put("k", vec!["a".into()], 1, t0);
        c.invalidate("k");
        assert!(c.get("k", t0).is_none());
        let s = c.stats();
        assert_eq!(s.misses, 1, "Invalid 视为穿透");
    }
}
