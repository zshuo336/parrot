//! P2 集群 POC：SWIM 成员关系 + Receptionist 服务发现（06 文档核心算法）。
//!
//! POC 边界：单进程内多节点（真实多进程部署时传输层换成 P1 的 TCP），
//! 协议报文与状态机逻辑和正式实现一致。

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

// ============ SWIM 报文（gossip 载荷） ============

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub node_id: String,
    pub addr: String, // POC 内用 mpsc 地址标识
    pub incarnation: u64,
    pub status: MemberStatus,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemberStatus {
    Alive,
    Suspect,
    Dead,
}

#[derive(Debug, Clone)]
pub enum SwimMsg {
    Ping { from: String, seq: u64 },
    PingReq { from: String, target: String, seq: u64 }, // 间接探测
    Ack { seq: u64 },
    /// gossip：全量/增量成员表传播（POC 用全量简化，正式版用增量 digest）
    Membership(Vec<Member>),
}

// ============ 节点 ============

pub struct SwimNode {
    pub node_id: String,
    members: tokio::sync::Mutex<HashMap<String, Member>>, // 含自己
    incarnation: std::sync::atomic::AtomicU64,
    seq_counter: std::sync::atomic::AtomicU64,
    /// 直接探测目标（K 个）
    pub probe_targets: Vec<String>,
}

impl SwimNode {
    pub fn new(node_id: &str) -> Arc<Self> {
        Arc::new(Self {
            node_id: node_id.into(),
            members: tokio::sync::Mutex::new(HashMap::from([(
                node_id.into(),
                Member {
                    node_id: node_id.into(),
                    addr: node_id.into(),
                    incarnation: 0,
                    status: MemberStatus::Alive,
                },
            )])),
            incarnation: std::sync::atomic::AtomicU64::new(0),
            seq_counter: std::sync::atomic::AtomicU64::new(0),
            probe_targets: vec![],
        })
    }

    pub async fn members(&self) -> Vec<Member> {
        self.members.lock().await.values().cloned().collect()
    }

    /// 收到 gossip 表：合并（incarnation 大者胜，Alive > Suspect > Dead 折叠）。
    pub async fn merge_membership(&self, incoming: Vec<Member>) {
        let mut m = self.members.lock().await;
        for inc in incoming {
            let e = m.entry(inc.node_id.clone()).or_insert(inc.clone());
            if inc.incarnation > e.incarnation {
                *e = inc;
            } else if inc.incarnation == e.incarnation {
                // 同代：状态取"更坏"的（SWIM 疑罪从有）
                let rank = |s: MemberStatus| match s {
                    MemberStatus::Alive => 0,
                    MemberStatus::Suspect => 1,
                    MemberStatus::Dead => 2,
                };
                if rank(inc.status) > rank(e.status) {
                    *e = inc;
                }
            }
        }
    }

    /// 被疑：自增 incarnation 反驳（refute）。
    pub async fn refute(&self) {
        let inc = self.incarnation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let mut m = self.members.lock().await;
        if let Some(me) = m.get_mut(&self.node_id) {
            me.incarnation = inc;
            me.status = MemberStatus::Alive;
        }
    }

    pub async fn mark_suspect(&self, node: &str) {
        let mut m = self.members.lock().await;
        if let Some(e) = m.get_mut(node) {
            e.status = MemberStatus::Suspect;
        }
    }
    pub async fn mark_dead(&self, node: &str) {
        let mut m = self.members.lock().await;
        if let Some(e) = m.get_mut(node) {
            e.status = MemberStatus::Dead;
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.seq_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }
}

// ============ 传输：节点名 → mpsc 队列 ============

#[derive(Clone)]
pub struct ClusterBus {
    queues: Arc<std::sync::Mutex<HashMap<String, mpsc::UnboundedSender<SwimMsg>>>>,
}

impl ClusterBus {
    pub fn new() -> Self {
        Self { queues: Arc::new(std::sync::Mutex::new(HashMap::new())) }
    }
    pub fn register(&self, node_id: &str) -> mpsc::UnboundedReceiver<SwimMsg> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.queues.lock().unwrap().insert(node_id.into(), tx);
        rx
    }
    pub fn send_to(&self, to: &str, msg: SwimMsg) {
        if let Some(tx) = self.queues.lock().unwrap().get(to) {
            let _ = tx.send(msg);
        }
    }
    /// 故障注入：丢弃发往某节点的全部报文（模拟网络分区/宕机）。
    pub fn partition(&self, node_id: &str) {
        self.queues.lock().unwrap().remove(node_id);
    }
}

// ============ 探测循环（每节点一个 task） ============

/// 单轮探测协议（可确定性单步执行——测试友好）：
/// ping(target) → 等 ack（超时 T_direct）→ 未达则 pingReq(k 间接节点) → 仍未达 → suspect
pub struct ProbeOutcome {
    pub target: String,
    pub reachable: bool,
    pub used_indirect: bool,
}

impl SwimNode {
    /// 直接 + 间接探测。bus 故障注入由测试控制。
    pub async fn probe(
        &self,
        bus: &ClusterBus,
        target: &str,
        via: &[String],
        ack_timeout: std::time::Duration,
    ) -> ProbeOutcome {
        let seq = self.seq_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        bus.send_to(target, SwimMsg::Ping { from: self.node_id.clone(), seq });

        // POC 用 sleep 模拟 ack 等待；真实实现是 pending map + oneshot
        tokio::time::sleep(ack_timeout).await;
        // POC 简化：ack 由 recv 循环立即回，因此这里查成员状态判断可达性
        let reachable = {
            // 若 target 已 partition，它不会回 ack —— 用 bus 是否可达近似
            bus.queues.lock().unwrap().contains_key(target)
        };
        if reachable {
            return ProbeOutcome { target: target.into(), reachable: true, used_indirect: false };
        }

        // 间接探测
        for v in via {
            bus.send_to(v, SwimMsg::PingReq { from: self.node_id.clone(), target: target.into(), seq });
        }
        tokio::time::sleep(ack_timeout).await;
        let reachable2 = bus.queues.lock().unwrap().contains_key(target);
        ProbeOutcome { target: target.into(), reachable: reachable2, used_indirect: true }
    }
}

// ============ Receptionist（06 §3）：服务注册 + 订阅推送 ============

pub struct Receptionist {
    /// key → (node, service_addr)
    registry: tokio::sync::Mutex<HashMap<String, Vec<(String, String)>>>,
    /// 订阅者：key → 推送通道
    subs: tokio::sync::Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Vec<(String, String)>>>>>,
}

impl Receptionist {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: Default::default(),
            subs: Default::default(),
        })
    }
    pub async fn register(&self, key: &str, node: &str, service_addr: &str) {
        let mut r = self.registry.lock().await;
        r.entry(key.to_string()).or_default().push((node.into(), service_addr.into()));
        let snapshot = r.get(key).cloned().unwrap_or_default();
        drop(r);
        self.publish(key, snapshot).await;
    }
    pub async fn subscribe(
        &self,
        key: &str,
    ) -> mpsc::UnboundedReceiver<Vec<(String, String)>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subs.lock().await.entry(key.to_string()).or_default().push(tx);
        rx
    }
    async fn publish(&self, key: &str, entries: Vec<(String, String)>) {
        let subs = self.subs.lock().await;
        if let Some(list) = subs.get(key) {
            for tx in list {
                let _ = tx.send(entries.clone());
            }
        }
    }
}
