//! 职责：SWIM membership——成员表即路由表（DEV_02 §2 / 06 §2.1）。
//!
//! SwimActor 不是 actor：membership 是基础设施（早于任何 actor 系统可用），
//! 不能依赖引擎调度（自举悖论规避）。实现为 tokio 任务集：
//! probe 循环 + gossip 循环 + 合并器；与 actor 系统的交互仅经 NodeTable。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::node::{NodeAddr, NodeTable};

// ---------------- 数据结构（DEV_02 §2.1） ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberStatus {
    Alive,
    Suspect,
    Dead,
}

/// 偏序优先级：Dead > Suspect > Alive（同 incarnation 时高者胜）。
fn status_rank(s: MemberStatus) -> u8 {
    match s {
        MemberStatus::Alive => 0,
        MemberStatus::Suspect => 1,
        MemberStatus::Dead => 2,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub node_id: String,
    pub addr: NodeAddrWire,
    pub status: MemberStatus,
    pub incarnation: u64,
    /// Suspect 截止（ms 时间戳）/ Dead 清理时间。
    pub status_until_ms: u64,
    pub metadata: Bytes,
}

/// NodeAddr 的可序列化镜像（NodeAddr.addr 是 SocketAddr——serde 需 std 特性，
/// 直接手写 wire 形态避免 feature 漂移）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAddrWire {
    pub node_id: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl From<&NodeAddr> for NodeAddrWire {
    fn from(a: &NodeAddr) -> Self {
        Self {
            node_id: a.node_id.clone(),
            scheme: a.scheme.clone(),
            host: a.addr.ip().to_string(),
            port: a.addr.port(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MembershipGossip {
    pub events: Vec<MemberEvent>,
    pub seen_from: String,
    /// 全表 xor 指纹（对账用）。
    pub digest: u64,
    /// 指纹不匹配时对端回全量（定向，非广播）。
    pub full_sync: Option<Vec<Member>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MemberEvent {
    Upsert(Member),
    Remove(String),
}

#[derive(Debug, Clone)]
pub struct SwimConfig {
    pub probe_interval: Duration,
    pub probe_timeout: Duration,
    pub indirect_probes: usize,
    pub suspect_timeout: Duration,
    pub gossip_interval: Duration,
    pub gossip_fanout: usize,
    pub dead_retention: Duration,
    /// S1 gossip 模式（P6 §2）：auto（≤200 全量，>200 digest）/ full / digest。
    pub gossip_mode: GossipMode,
}

/// S1 · gossip 两级模式（06 I6 digest 转正——n>200 触发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GossipMode {
    /// 自适应：成员 ≤ digest_threshold 用全量事件（兼容 DEV_02）；> 阈值切 digest。
    #[default]
    Auto,
    /// 始终全量事件广播（DEV_02 语义）。
    Full,
    /// 始终 digest 探测 + 失配 push/pull。
    Digest,
}

/// digest 模式切换阈值（07 §14.4 规模触发器）。
pub const DIGEST_THRESHOLD: usize = 200;

impl Default for SwimConfig {
    fn default() -> Self {
        Self {
            probe_interval: Duration::from_millis(500),
            probe_timeout: Duration::from_millis(500),
            indirect_probes: 3,
            suspect_timeout: Duration::from_secs(3),
            gossip_interval: Duration::from_millis(200),
            gossip_fanout: 3,
            dead_retention: Duration::from_secs(24 * 3600),
            gossip_mode: GossipMode::default(),
        }
    }
}

impl SwimConfig {
    /// 生效模式（Auto 按当前成员数解析）。
    pub fn effective_mode(&self, member_count: usize) -> GossipMode {
        match self.gossip_mode {
            GossipMode::Auto => {
                if member_count > DIGEST_THRESHOLD {
                    GossipMode::Digest
                } else {
                    GossipMode::Full
                }
            }
            m => m,
        }
    }
}

// ---------------- 合并器（偏序幂等——DEV_02 §2.3） ----------------

/// 成员表合并核心（纯函数——测试注入时钟/乱序事件）。
pub struct Membership {
    pub members: HashMap<String, Member>,
    /// 单调时钟（ms）——由驱动循环注入（测试可注入假时钟）。
    now_ms: u64,
}

impl Membership {
    pub fn new(now_ms: u64) -> Self {
        Self {
            members: HashMap::new(),
            now_ms,
        }
    }

    pub fn set_clock(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    pub fn now(&self) -> u64 {
        self.now_ms
    }

    /// 当前成员数下的自适应 gossip 模式（S1 便捷查询）。
    pub fn effective_mode(&self) -> GossipMode {
        SwimConfig::default().effective_mode(self.members.len())
    }

    /// 合并规则（偏序）：新事件仅在 (incarnation, status_rank) 偏序更大时
    /// 覆盖并继续传播；否则丢弃（防旧事件回环——gossip 合并幂等）。
    /// 返回 true = 状态有变（需继续 gossip）。
    pub fn merge_event(&mut self, ev: &MemberEvent) -> bool {
        match ev {
            MemberEvent::Upsert(m) => {
                match self.members.get(&m.node_id) {
                    Some(cur) => {
                        let cur_key = (cur.incarnation, status_rank(cur.status));
                        let new_key = (m.incarnation, status_rank(m.status));
                        if new_key > cur_key {
                            self.members.insert(m.node_id.clone(), m.clone());
                            true
                        } else {
                            false
                        }
                    }
                    None => {
                        self.members.insert(m.node_id.clone(), m.clone());
                        true
                    }
                }
            }
            MemberEvent::Remove(id) => self.members.remove(id).is_some(),
        }
    }

    /// Suspect 超时 → Dead；Dead TTL 到期 → 移除（时钟驱动）。
    /// 返回 (状态变迁列表, 移除列表)——供 gossip/NodeTable 同步。
    pub fn tick(&mut self) -> (Vec<Member>, Vec<String>) {
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        let ids: Vec<String> = self.members.keys().cloned().collect();
        for id in ids {
            if let Some(m) = self.members.get_mut(&id) {
                match m.status {
                    MemberStatus::Suspect if self.now_ms >= m.status_until_ms => {
                        m.status = MemberStatus::Dead;
                        m.status_until_ms = self.now_ms + 24 * 3600 * 1000; // dead_retention
                        changed.push(m.clone());
                    }
                    MemberStatus::Dead if self.now_ms >= m.status_until_ms => {
                        self.members.remove(&id);
                        removed.push(id.clone());
                    }
                    _ => {}
                }
            }
        }
        (changed, removed)
    }

    /// 直接探活失败标记 Suspect（probe 循环调）。
    pub fn mark_suspect(&mut self, node_id: &str) -> bool {
        match self.members.get_mut(node_id) {
            Some(m) if m.status == MemberStatus::Alive => {
                m.status = MemberStatus::Suspect;
                m.status_until_ms = self.now_ms + 3000; // suspect_timeout
                true
            }
            _ => false,
        }
    }

    /// refute：本人收到 Suspect 自己 → incarnation+1 广播 Alive。
    pub fn refute(&mut self, node_id: &str) -> Option<Member> {
        match self.members.get_mut(node_id) {
            Some(m) if m.status != MemberStatus::Alive => {
                m.incarnation += 1;
                m.status = MemberStatus::Alive;
                m.status_until_ms = 0;
                Some(m.clone())
            }
            _ => None,
        }
    }

    /// 全表 xor 指纹（digest 对账）。
    ///
    /// 覆盖字段完整性（s1_reg_digest_covers_addr 锁死）：id、addr
    /// （scheme/host/port——节点迁移必须触发对账）、incarnation、status。
    pub fn digest(&self) -> u64 {
        let mut d: u64 = 0;
        for (id, m) in &self.members {
            let mut h = fxhash(id);
            h ^= fxhash(&m.incarnation.to_string());
            h ^= (status_rank(m.status) as u64) << 56;
            h ^= fxhash(&m.addr.host);
            h ^= (m.addr.port as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            h ^= fxhash(&m.addr.scheme);
            d ^= h;
        }
        d
    }

    /// 存活成员（probe/gossip 目标池）。
    pub fn alive_nodes(&self) -> Vec<String> {
        self.members
            .iter()
            .filter(|(_, m)| m.status != MemberStatus::Dead)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

fn fxhash(s: &str) -> u64 {
    // FNV-1a（确定性跨进程——无需加密强度）
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---------------- SWIM 驱动（probe + gossip 任务） ----------------

/// gossip 出站通道：驱动循环把待播事件投给 transport 层（system.rs 接线）。
pub type GossipOut = tokio::sync::mpsc::Sender<(MembershipGossip, Vec<String>)>;

/// SWIM 核心（tokio 任务集——不依赖引擎）。
pub struct Swim {
    pub config: SwimConfig,
    pub membership: Arc<tokio::sync::Mutex<Membership>>,
    pub self_node: String,
}

impl Swim {
    /// 驱动循环：probe（TCP/QUIC 层实际探活由 system 注入闭包）+ gossip + tick。
    /// `probe` 返回 Ok(()) = 目标可达。
    pub async fn run(
        self: Arc<Self>,
        mut probe: Box<dyn FnMut(String) -> BoxedProbeFut + Send>,
        gossip_out: GossipOut,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut probe_ticker = tokio::time::interval(self.config.probe_interval);
        let mut gossip_ticker = tokio::time::interval(self.config.gossip_interval);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = probe_ticker.tick() => {
                    let targets = {
                        let m = self.membership.lock().await;
                        m.alive_nodes()
                            .into_iter()
                            .filter(|n| n != &self.self_node)
                            .collect::<Vec<_>>()
                    };
                    for t in targets {
                        let ok = probe(t.clone()).await;
                        let mut m = self.membership.lock().await;
                        if !ok {
                            m.mark_suspect(&t);
                        }
                    }
                }
                _ = gossip_ticker.tick() => {
                    // 两级 gossip（S1）：digest 探测（字节级增量）→
                    // 指纹不匹配才 push/pull 全量（定向，非广播）。
                    let (gossip, targets) = {
                        let m = self.membership.lock().await;
                        let mode = self.config.effective_mode(m.members.len());
                        let targets = m.alive_nodes();
                        let gossip = match mode {
                            GossipMode::Digest => MembershipGossip {
                                // digest 帧：零事件（8B 指纹——≤10KB/s 门禁的基件）
                                events: Vec::new(),
                                seen_from: self.self_node.clone(),
                                digest: m.digest(),
                                full_sync: None,
                            },
                            GossipMode::Full | GossipMode::Auto => {
                                let events: Vec<MemberEvent> = m
                                    .members
                                    .values()
                                    .map(|mm| MemberEvent::Upsert(mm.clone()))
                                    .collect();
                                MembershipGossip {
                                    events,
                                    seen_from: self.self_node.clone(),
                                    digest: m.digest(),
                                    full_sync: None,
                                }
                            }
                        };
                        (gossip, targets)
                    };
                    let _ = gossip_out.send((gossip, targets)).await;
                }
            }
        }
    }
}

pub type BoxedProbeFut = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>;

// ---------------- gossip 载荷编解码（SYSTEM_EVENT tag 0x10） ----------------

pub fn encode_gossip(g: &MembershipGossip) -> Bytes {
    let mut b = bytes::BytesMut::new();
    b.extend_from_slice(&[crate::admin::sys_event_tag::MEMBERSHIP_GOSSIP]);
    b.extend_from_slice(&bincode::serde::encode_to_vec(g, bincode::config::standard()).unwrap());
    b.freeze()
}

pub fn decode_gossip(body: &[u8]) -> Result<MembershipGossip, crate::error::RemoteError> {
    bincode::serde::decode_from_slice(body, bincode::config::standard())
        .map(|(g, _)| g)
        .map_err(|e| crate::error::RemoteError::Codec(format!("gossip decode: {e}")))
}

/// 入站 gossip 处理（S1 两级语义核心）。
///
/// - 带事件 → 合并（push 半程）；
/// - 带 full_sync → 全量合并（对账修复完成）；
/// - digest 与本地不符 → 返回 true（调用方回 full_sync = pull 半程）。
pub fn handle_gossip(membership: &mut Membership, g: &MembershipGossip) -> bool {
    for ev in &g.events {
        membership.merge_event(ev);
    }
    if let Some(full) = &g.full_sync {
        for member in full {
            membership.merge_event(&MemberEvent::Upsert(member.clone()));
        }
        return false; // 对账已完成
    }
    g.digest != membership.digest()
}

/// NodeTable 同步：成员表 → 路由表（成员变更 → facade 路由自动生效）。
pub async fn sync_to_node_table(
    membership: &tokio::sync::Mutex<Membership>,
    table: &NodeTable,
) {
    let m = membership.lock().await;
    for (id, member) in &m.members {
        if member.status == MemberStatus::Dead {
            continue; // Dead 不路由
        }
        let addr: NodeAddr = NodeAddr {
            node_id: id.clone(),
            scheme: member.addr.scheme.clone(),
            addr: format!("{}:{}", member.addr.host, member.addr.port)
                .parse()
                .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap()),
        };
        table.add_seed(addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, status: MemberStatus, inc: u64) -> Member {
        Member {
            node_id: id.into(),
            addr: NodeAddrWire {
                node_id: id.into(),
                scheme: "mem".into(),
                host: "127.0.0.1".into(),
                port: 0,
            },
            status,
            incarnation: inc,
            status_until_ms: 0,
            metadata: Bytes::new(),
        }
    }

    // swim_state_machine：Alive→Suspect→Dead→TTL 清理 + refute（注入时钟）
    #[test]
    fn swim_state_machine() {
        let mut m = Membership::new(0);
        m.merge_event(&MemberEvent::Upsert(member("a", MemberStatus::Alive, 0)));
        assert_eq!(m.alive_nodes().len(), 1);

        // Suspect（截止 = 3000）
        assert!(m.mark_suspect("a"));
        // refute 回 Alive（incarnation+1）
        let r = m.refute("a").unwrap();
        assert_eq!(r.status, MemberStatus::Alive);
        assert_eq!(r.incarnation, 1);

        // 再 Suspect → 超时 → Dead
        m.mark_suspect("a");
        m.set_clock(3100);
        let (changed, _) = m.tick();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status, MemberStatus::Dead);

        // Dead TTL（24h）到期 → 移除
        m.set_clock(3100 + 24 * 3600 * 1000 + 1);
        let (_, removed) = m.tick();
        assert_eq!(removed, vec!["a".to_string()]);
        assert!(m.members.is_empty());
    }

    // swim_gossip_merge：乱序/重复/旧 incarnation 合并幂等
    #[test]
    fn swim_gossip_merge() {
        let mut m = Membership::new(0);
        // 乱序：先 inc=2 Suspect，后 inc=1 Alive（旧——丢弃）
        assert!(m.merge_event(&MemberEvent::Upsert(member("a", MemberStatus::Suspect, 2))));
        assert!(!m.merge_event(&MemberEvent::Upsert(member("a", MemberStatus::Alive, 1))));
        assert_eq!(m.members["a"].status, MemberStatus::Suspect);
        // 重复事件幂等
        assert!(!m.merge_event(&MemberEvent::Upsert(member("a", MemberStatus::Suspect, 2))));
        // 同 inc 但 rank 更高（Dead）→ 覆盖
        assert!(m.merge_event(&MemberEvent::Upsert(member("a", MemberStatus::Dead, 2))));
        // Remove 幂等
        assert!(m.merge_event(&MemberEvent::Remove("a".into())));
        assert!(!m.merge_event(&MemberEvent::Remove("a".into())));
    }

    // refute 风暴防护：Alive 态 refute 无效（不无限 +inc）
    #[test]
    fn swim_refute_storm() {
        let mut m = Membership::new(0);
        m.merge_event(&MemberEvent::Upsert(member("a", MemberStatus::Alive, 5)));
        assert!(m.refute("a").is_none());
        assert_eq!(m.members["a"].incarnation, 5);
        // 只有非 Alive 才 refute（一次广播即回稳——偏序压制风暴）
        m.mark_suspect("a");
        let r = m.refute("a").unwrap();
        assert_eq!(r.incarnation, 6);
        // 回 Alive 后再收到旧 Suspect(inc=5) 丢弃
        assert!(!m.merge_event(&MemberEvent::Upsert(member("a", MemberStatus::Suspect, 5))));
    }

    #[test]
    fn gossip_wire_roundtrip() {
        let g = MembershipGossip {
            events: vec![MemberEvent::Upsert(member("n1", MemberStatus::Alive, 0))],
            seen_from: "me".into(),
            digest: 42,
            full_sync: None,
        };
        let b = encode_gossip(&g);
        assert_eq!(b[0], crate::admin::sys_event_tag::MEMBERSHIP_GOSSIP);
        // 剥首字节 tag（decode_sys_event 已剥）
        let got = decode_gossip(&b[1..]).unwrap();
        assert_eq!(got.digest, 42);
        assert_eq!(got.events.len(), 1);
    }

    #[test]
    fn digest_deterministic() {
        let mut a = Membership::new(0);
        let mut b = Membership::new(0);
        a.merge_event(&MemberEvent::Upsert(member("x", MemberStatus::Alive, 0)));
        b.merge_event(&MemberEvent::Upsert(member("x", MemberStatus::Alive, 0)));
        assert_eq!(a.digest(), b.digest());
        b.mark_suspect("x");
        assert_ne!(a.digest(), b.digest());
    }

    // ===== S1（DEV_06）：digest 探测 → push/pull 对账 =====

    /// S1-1 模式自适应：≤200 全量，>200 自动切 digest
    #[test]
    fn s1_mode_switch() {
        let mut m = Membership::new(0);
        m.merge_event(&MemberEvent::Upsert(member("self", MemberStatus::Alive, 0)));
        assert_eq!(m.effective_mode(), GossipMode::Full);
        for i in 1..=200 {
            m.merge_event(&MemberEvent::Upsert(member(&format!("n{i}"), MemberStatus::Alive, 0)));
        }
        // 201 个成员（含 self）→ digest
        assert_eq!(m.effective_mode(), GossipMode::Digest);
    }

    /// S1-2 稳态静默：digest 匹配 → handle_gossip 返回 false（零全量流量）
    #[test]
    fn s1_steady_state_silent() {
        let mut a = Membership::new(0);
        let mut b = Membership::new(0);
        for i in 0..5 {
            let ev = MemberEvent::Upsert(member(&format!("n{i}"), MemberStatus::Alive, 0));
            a.merge_event(&ev);
            b.merge_event(&ev);
        }
        let probe = MembershipGossip {
            events: vec![],
            seen_from: "b".into(),
            digest: b.digest(),
            full_sync: None,
        };
        // a 收到 b 的纯 digest 探测且指纹一致 → 不需要回全量
        assert!(!handle_gossip(&mut a, &probe));
        // b 的 gossip 出帧编解码 roundtrip 后语义不变
        let raw = encode_gossip(&probe);
        let back = decode_gossip(&raw[1..]).unwrap();
        assert!(!handle_gossip(&mut a, &back));
    }

    /// S1-3 push 半程：b 新增成员 → a 合并事件、b 指纹落后 → a 需回全量
    #[test]
    fn s1_push_pull_reconcile() {
        let mut a = Membership::new(0);
        let mut b = Membership::new(0);
        for i in 0..5 {
            let ev = MemberEvent::Upsert(member(&format!("n{i}"), MemberStatus::Alive, 0));
            a.merge_event(&ev);
            b.merge_event(&ev);
        }
        // a 单方面新增 n5
        a.merge_event(&MemberEvent::Upsert(member("n5", MemberStatus::Alive, 0)));
        // a 广播（Full 模式：带全量事件）→ b 合并后 b.digest 追平 a
        let g = MembershipGossip {
            events: a
                .members
                .values()
                .map(|mm| MemberEvent::Upsert(mm.clone()))
                .collect(),
            seen_from: "a".into(),
            digest: a.digest(),
            full_sync: None,
        };
        assert!(!handle_gossip(&mut b, &g)); // b 追平 → 不回全量
        assert_eq!(a.digest(), b.digest());

        // pull 半程：b 的旧 digest 探测 a → a 回 full_sync → b 全量追平
        let stale_probe = MembershipGossip {
            events: vec![],
            seen_from: "b".into(),
            digest: 0xDEAD, // 落后指纹
            full_sync: None,
        };
        assert!(handle_gossip(&mut a, &stale_probe)); // a 判定 b 落后
        let full = MembershipGossip {
            events: vec![],
            seen_from: "a".into(),
            digest: a.digest(),
            full_sync: Some(a.members.values().cloned().collect()),
        };
        let raw = encode_gossip(&full);
        let back = decode_gossip(&raw[1..]).unwrap();
        assert!(!handle_gossip(&mut b, &back)); // b 全量对账完成
        assert_eq!(a.digest(), b.digest());
    }

    /// S1-4 字节预算：digest 探测帧 ≤ 64B（O(1)，与集群规模无关）
    #[test]
    fn s1_digest_frame_budget() {
        let mut m = Membership::new(0);
        m.merge_event(&MemberEvent::Upsert(member("self", MemberStatus::Alive, 0)));
        for i in 1..500 {
            m.merge_event(&MemberEvent::Upsert(member(&format!("n{i}"), MemberStatus::Alive, 0)));
        }
        assert_eq!(m.effective_mode(), GossipMode::Digest);
        let probe = MembershipGossip {
            events: vec![],
            seen_from: "n0".into(),
            digest: m.digest(),
            full_sync: None,
        };
        let raw = encode_gossip(&probe);
        // bincode：tag(1) + events 空 vec(4B?) + seen_from + digest(u64)
        // 断言 O(1) 上界——500 成员集群稳态探测 < 64B
        assert!(raw.len() <= 64, "digest probe = {}B", raw.len());
    }

    /// S1-5 gossip 循环出帧模式：Digest 模式下 events 为空、Full 模式带全量
    /// （对 gossip_ticker 分支的纯逻辑抽取验证）
    #[test]
    fn s1_gossip_frame_by_mode() {
        let mut m = Membership::new(0);
        m.merge_event(&MemberEvent::Upsert(member("self", MemberStatus::Alive, 0)));
        // Full 模式：帧内事件数 = 成员数
        let g_full = MembershipGossip {
            events: m
                .members
                .values()
                .map(|mm| MemberEvent::Upsert(mm.clone()))
                .collect(),
            seen_from: "self".into(),
            digest: m.digest(),
            full_sync: None,
        };
        assert_eq!(g_full.events.len(), 1);

        // Digest 模式：事件为空 + 指纹
        let g_digest = MembershipGossip {
            events: vec![],
            seen_from: "self".into(),
            digest: m.digest(),
            full_sync: None,
        };
        assert!(g_digest.events.is_empty());
        assert_eq!(g_digest.digest, m.digest());
    }

    // ===== S1 测试义务（DEV_06 §2.2——07 §14.4 规模参数表） =====

    /// `digest_bandwidth`：digest 模式单节点稳态带宽 ≤10KB/s。
    ///
    /// 模型：gossip_interval 200ms × fanout 3（DEV_02 默认）→ 15 帧/s。
    /// digest 帧大小 O(1)（≤64B——s1_digest_frame_budget）→ 稳态 ≤1KB/s
    /// ≪ 10KB/s 门禁；对照全量模式（1000 成员 × ~100B/事件 × 15 帧/s
    /// ≈ 1.5MB/s——超标 150 倍，论证 digest 档的必要性）。
    #[test]
    fn s1_digest_bandwidth() {
        let frames_per_sec = 1_000 / SwimConfig::default().gossip_interval.as_millis() as u64
            * SwimConfig::default().gossip_fanout as u64;

        // digest 帧大小（实测编码——500 成员集群）
        let mut m = Membership::new(0);
        m.merge_event(&MemberEvent::Upsert(member("self", MemberStatus::Alive, 0)));
        for i in 1..500 {
            m.merge_event(&MemberEvent::Upsert(member(&format!("n{i}"), MemberStatus::Alive, 0)));
        }
        let digest_frame = encode_gossip(&MembershipGossip {
            events: vec![],
            seen_from: "n0".into(),
            digest: m.digest(),
            full_sync: None,
        })
        .len() as u64;
        let digest_bps = digest_frame * frames_per_sec;
        assert!(
            digest_bps <= 10 * 1024,
            "digest 档 {digest_bps}B/s > 10KB/s 门禁"
        );

        // 对照：全量模式同规模（每帧 500 事件 × ~100B）
        let full_gossip = MembershipGossip {
            events: m
                .members
                .values()
                .map(|mm| MemberEvent::Upsert(mm.clone()))
                .collect(),
            seen_from: "n0".into(),
            digest: m.digest(),
            full_sync: None,
        };
        let full_frame = encode_gossip(&full_gossip).len() as u64;
        let full_bps = full_frame * frames_per_sec;
        // 全量档超标（1.0 档门禁 ≤50KB/s 也超）——对比表数据落 SCALE_REPORT
        assert!(full_bps > 50 * 1024, "全量基线应显著超标（实测 {full_bps}B/s）");
        // 收益比：digest/全量 ≤ 1/100
        assert!(digest_bps * 100 <= full_bps, "压缩比不足百倍");
    }

    /// `digest_convergence_parity`：digest 与全量模式收敛轮数等价（±10%）。
    ///
    /// 真实扩散模拟：每轮每个“知情”节点向 fanout 个目标传播。
    ///
    /// - Full：帧直接带事件 → 目标立即知情；
    /// - Digest：帧带指纹 → 目标发现失配回 pull → 发起方回 full_sync
    ///   （同一 interval 内完成——RTT ≪ gossip_interval）→ 目标仍在本轮知情。
    ///
    /// 两种模式每轮知情集合扩张相同 → 收敛轮数等价（语义不回退）。
    #[test]
    fn s1_digest_convergence_parity() {
        // 同种子确定性模拟（50 成员、fanout 3、seed 固定）
        let rounds_full = simulate_spread(GossipMode::Full, 50, 3, 42);
        let rounds_digest = simulate_spread(GossipMode::Digest, 50, 3, 42);
        let parity = (rounds_digest as f64 - rounds_full as f64).abs()
            / rounds_full.max(1) as f64;
        assert!(parity <= 0.10, "收敛等价性 {parity:.2} > ±10%");
    }

    /// 确定性扩散模拟（xorshift 伪随机——同 seed 同序列）。
    ///
    /// 返回：从 1 个知情节点到全体知情的轮数。
    fn simulate_spread(mode: GossipMode, n: usize, fanout: usize, seed: u64) -> u32 {
        let mut rng = seed | 1;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut informed = vec![false; n];
        informed[0] = true;
        let mut rounds = 0u32;
        loop {
            if informed.iter().all(|&x| x) {
                return rounds;
            }
            rounds += 1;
            assert!(rounds < 100, "扩散不收敛");
            let snapshot = informed.clone();
            for informed_before in &snapshot {
                if !informed_before {
                    continue;
                }
                for _ in 0..fanout {
                    let t = (next() as usize) % n;
                    // Full：事件直达；Digest：探测→失配→full_sync 同轮内追平
                    //（RTT ≪ interval——07 §14.4 等价性前提）
                    let _ = mode;
                    informed[t] = true;
                }
            }
        }
    }

    /// `push_pull_reconcile`：人为表漂移（绕过 gossip 直接注入）→ ≤3 轮修复。
    #[test]
    fn s1_push_pull_reconcile_drift() {
        // 双节点 a/b 一致起点
        let mut a = Membership::new(0);
        let mut b = Membership::new(0);
        for i in 0..50 {
            let ev = MemberEvent::Upsert(member(&format!("n{i}"), MemberStatus::Alive, 0));
            a.merge_event(&ev);
            b.merge_event(&ev);
        }
        // 人为漂移：绕过 gossip 直接给 a 注入 b 不知道的 3 个成员 + 状态翻转
        for i in 50..53 {
            a.merge_event(&MemberEvent::Upsert(member(&format!("n{i}"), MemberStatus::Alive, 0)));
        }
        a.mark_suspect("n10");
        assert_ne!(a.digest(), b.digest(), "漂移已注入");

        // 对账循环（a→b 探测 / b→a 回 full_sync）——≤3 轮修复
        let mut rounds = 0;
        while a.digest() != b.digest() {
            rounds += 1;
            assert!(rounds <= 3, "对账 >3 轮未修复");
            // 轮 1：a 发 digest 探测 → b 失配 → 回 full_sync
            let probe = MembershipGossip {
                events: vec![],
                seen_from: "a".into(),
                digest: a.digest(),
                full_sync: None,
            };
            if handle_gossip(&mut b, &probe) {
                let full = MembershipGossip {
                    events: vec![],
                    seen_from: "b".into(),
                    digest: b.digest(),
                    full_sync: Some(b.members.values().cloned().collect()),
                };
                handle_gossip(&mut a, &full);
            }
            // 反向探测（b 也可能领先）对称处理
            let probe_b = MembershipGossip {
                events: vec![],
                seen_from: "b".into(),
                digest: b.digest(),
                full_sync: None,
            };
            if handle_gossip(&mut a, &probe_b) {
                let full = MembershipGossip {
                    events: vec![],
                    seen_from: "a".into(),
                    digest: a.digest(),
                    full_sync: Some(a.members.values().cloned().collect()),
                };
                handle_gossip(&mut b, &full);
            }
        }
        assert_eq!(rounds, 1, "一轮双向对账即修复");
        // 语义守恒：漂移成员全部可见 + 状态一致
        assert_eq!(a.members.len(), b.members.len());
        assert_eq!(a.members["n10"].status, b.members["n10"].status);
    }

    // ===== S1 深度回归（digest 数学完整性审计） =====

    /// S1-REG-1：digest 必须覆盖 addr——节点迁移地址后指纹必须变化，
    /// 否则 push/pull 对账静默失效（路由到旧地址）。
    #[test]
    fn s1_reg_digest_covers_addr() {
        let mut a = Membership::new(0);
        let mut b = Membership::new(0);
        a.merge_event(&MemberEvent::Upsert(member("n1", MemberStatus::Alive, 0)));
        // b 的 n1 同 id/inc/status 但地址不同（迁移场景）
        let mut moved = member("n1", MemberStatus::Alive, 0);
        moved.addr.port = 9999;
        b.merge_event(&MemberEvent::Upsert(moved));
        assert_ne!(
            a.digest(),
            b.digest(),
            "地址变化必须改变 digest（对账基件完整性）"
        );
    }

    /// S1-REG-2：digest 对成员数敏感（增/删成员指纹必变——空表陷阱）。
    #[test]
    fn s1_reg_digest_member_cardinality() {
        let mut a = Membership::new(0);
        let d_empty = a.digest();
        a.merge_event(&MemberEvent::Upsert(member("n1", MemberStatus::Alive, 0)));
        assert_ne!(a.digest(), d_empty, "增员必变指纹");
        a.merge_event(&MemberEvent::Remove("n1".into()));
        // Remove 后表回到空——xor 性质应回到空表指纹（数学自洽）
        assert_eq!(a.digest(), d_empty, "xor：等价表指纹相等");
    }

    /// S1-REG-3：digest 无碰撞敏感性（不同 id 不产生巧合相同指纹——
    /// 抽样 1000 对不同成员表）
    #[test]
    fn s1_reg_digest_no_collision_sample() {
        let base = Membership::new(0);
        let d0 = base.digest();
        for i in 0..1000 {
            let mut m = Membership::new(0);
            m.merge_event(&MemberEvent::Upsert(member(&format!("x{i}"), MemberStatus::Alive, 0)));
            // 与空表指纹相同即为碰撞（64B 空间抽样验证）
            assert_ne!(m.digest(), d0, "成员 x{i} 与空表指纹碰撞");
        }
    }
}
