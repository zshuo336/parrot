//! F4 · 自研 Raft 内核（DEV_05 §4——决策档严格边界）。
//!
//! **只做**：leader 选举 + 日志复制 + 单一 commit 点。
//! **不做**：membership 变更协议（joint consensus——重启重配置替代）、
//! snapshot 压缩（条目量小全量重放）。
//!
//! 时钟注入（DEV_05 §10.2）：选举计时由 `Clock` 注入——测试确定性 /
//! 生产 tokio interval。**勿用系统时间差判断任期**（时钟跳变混沌）。
//!
//! 传输（§10.6）：Raft RPC 复用 ASK/REPLY 帧承载（cid=proposal id）——
//! 不新增帧类型码点（07 X2 纪律）。本内核是纯状态机：网络收发由宿主
//! （DirectoryStore）驱动，`step()` 消费入站 RPC、`poll()` 产出出站 RPC。

use std::collections::HashMap;

// ── 类型 ────────────────────────────────────────────────

pub type NodeId = String;
pub type Term = u64;
pub type LogIndex = u64;

/// Raft RPC（出/入站——宿主编解码承载于 ASK/REPLY）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RaftRpc {
    RequestVote {
        term: Term,
        candidate: NodeId,
        last_log_index: LogIndex,
        last_log_term: Term,
    },
    RequestVoteResp {
        term: Term,
        vote_granted: bool,
    },
    AppendEntries {
        term: Term,
        leader: NodeId,
        prev_log_index: LogIndex,
        prev_log_term: Term,
        entries: Vec<LogEntry>,
        leader_commit: LogIndex,
    },
    AppendEntriesResp {
        term: Term,
        success: bool,
        /// 拒绝时：follower 日志末尾（leader 据此回退 nextIndex）。
        match_index: LogIndex,
    },
}

/// 日志条目（Cmd 泛型由宿主特化——Directory 的 Upsert/Remove 等）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub term: Term,
    pub index: LogIndex,
    pub cmd: Vec<u8>, // 宿主序列化（bincode）——内核不解释
}

/// 注入时钟（选举/心跳计时——确定性测试根基）。
pub trait Clock: Send + Sync {
    /// 单调毫秒（测试可任意拨动）。
    fn now_ms(&self) -> u64;
    /// 本节点选举超时区间（随机化防活锁——论文 §5.2）。
    fn election_timeout_ms(&self) -> u64;
    /// 心跳周期（«选举超时）。
    fn heartbeat_ms(&self) -> u64;
}

/// 生产时钟（真实单调时间 + 固定参数）。
pub struct RealClock {
    pub election_ms: u64,
    pub heartbeat_ms: u64,
}

impl Default for RealClock {
    fn default() -> Self {
        Self {
            election_ms: 1000, // 07 §14.3：3s 选主门禁的基件
            heartbeat_ms: 150,
        }
    }
}

impl Clock for RealClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    fn election_timeout_ms(&self) -> u64 {
        self.election_ms + (self.now_ms() % 300) // 抖动防同步选举
    }
    fn heartbeat_ms(&self) -> u64 {
        self.heartbeat_ms
    }
}

/// 角色三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

/// 复制状态机（宿主实现——DirectoryStore 等）。
pub trait StateMachine {
    fn apply(&mut self, cmd: &[u8]);
}

/// 空状态机（选举/复制测试用）。
#[derive(Default)]
pub struct NullSm;

impl StateMachine for NullSm {
    fn apply(&mut self, _cmd: &[u8]) {}
}

// ── 内核 ────────────────────────────────────────────────

/// Raft 节点状态机（纯逻辑——无 IO、无定时器线程）。
///
/// 驱动协议：宿主循环 `tick()` → 取 `outbox()` 发送 → 收到 RPC `step()`
/// → commit 推进时取 `committed()` 应用到状态机。
pub struct RaftNode {
    pub id: NodeId,
    pub peers: Vec<NodeId>, // 不含自己

    // 持久态（论文 Figure 2）
    pub current_term: Term,
    pub voted_for: Option<NodeId>,
    pub log: Vec<LogEntry>, // index 从 1 起（0=哨兵）

    // 易失态
    pub commit_index: LogIndex,
    pub last_applied: LogIndex,
    pub role: Role,
    pub leader_hint: Option<NodeId>,

    // leader 易失态
    next_index: HashMap<NodeId, LogIndex>,
    match_index: HashMap<NodeId, LogIndex>,

    // candidate
    votes_received: Vec<NodeId>,

    // 计时（注入时钟）
    clock: std::sync::Arc<dyn Clock>,
    last_heartbeat_ms: u64,
    /// 选举超时抖动（node id 哈希——防同钟集群 split vote；论文 §5.2）
    timeout_jitter_ms: u64,

    // 出站队列（宿主取走后清空）
    outbox: Vec<(NodeId, RaftRpc)>,
    // 待 apply 的已提交条目（宿主 drain）
    pending_apply: Vec<LogEntry>,
}

impl RaftNode {
    pub fn new(id: impl Into<String>, peers: Vec<NodeId>, clock: std::sync::Arc<dyn Clock>) -> Self {
        let id = id.into();
        let jitter = id_jitter(&id) % 300;
        Self {
            id,
            peers,
            current_term: 0,
            voted_for: None,
            log: Vec::new(),
            commit_index: 0,
            last_applied: 0,
            role: Role::Follower,
            leader_hint: None,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            votes_received: Vec::new(),
            clock,
            last_heartbeat_ms: 0,
            timeout_jitter_ms: jitter,
            outbox: Vec::new(),
            pending_apply: Vec::new(),
        }
    }

    // ── 驱动面 ──

    /// 计时推进（宿主周期调用——间隔由 heartbeat_ms 定）。
    pub fn tick(&mut self) {
        let now = self.clock.now_ms();
        match self.role {
            Role::Follower | Role::Candidate => {
                let timeout = self.clock.election_timeout_ms() + self.timeout_jitter_ms;
                if now.saturating_sub(self.last_heartbeat_ms) >= timeout {
                    self.start_election();
                }
            }
            Role::Leader => {
                // 周期心跳（AppendEntries 空 entries 也带 commit 推进）
                if now.saturating_sub(self.last_heartbeat_ms) >= self.clock.heartbeat_ms() {
                    self.last_heartbeat_ms = now;
                    self.broadcast_append();
                }
            }
        }
    }

    /// 入站 RPC（宿主收到后调用）。
    pub fn step(&mut self, from: NodeId, rpc: RaftRpc) {
        match rpc {
            RaftRpc::RequestVote { term, candidate, last_log_index, last_log_term } => {
                self.step_request_vote(from, term, candidate, last_log_index, last_log_term)
            }
            RaftRpc::RequestVoteResp { term, vote_granted } => {
                self.step_vote_resp(from, term, vote_granted)
            }
            RaftRpc::AppendEntries { term, leader, prev_log_index, prev_log_term, entries, leader_commit } => {
                self.step_append(from, term, leader, prev_log_index, prev_log_term, entries, leader_commit)
            }
            RaftRpc::AppendEntriesResp { term, success, match_index } => {
                self.step_append_resp(from, term, success, match_index)
            }
        }
    }

    /// leader 提交命令（宿主 client 写入路径）。
    ///
    /// 非 leader → Err（宿主转发给 leader_hint）。
    pub fn propose(&mut self, cmd: Vec<u8>) -> Result<LogIndex, NotLeader> {
        if self.role != Role::Leader {
            return Err(NotLeader(self.leader_hint.clone()));
        }
        let index = self.log.len() as LogIndex + 1;
        self.log.push(LogEntry {
            term: self.current_term,
            index,
            cmd,
        });
        self.broadcast_append();
        // 单节点集群：自投即多数派——直接推进 commit
        if self.peers.is_empty() {
            self.advance_commit();
        }
        Ok(index)
    }

    /// 出站 RPC 队列（宿主取走清空）。
    pub fn outbox(&mut self) -> Vec<(NodeId, RaftRpc)> {
        std::mem::take(&mut self.outbox)
    }

    /// 已提交未应用的条目（宿主 apply 到状态机后丢弃）。
    pub fn committed(&mut self) -> Vec<LogEntry> {
        std::mem::take(&mut self.pending_apply)
    }

    fn last_log_term(&self) -> Term {
        self.log.last().map(|e| e.term).unwrap_or(0)
    }

    fn last_log_index(&self) -> LogIndex {
        self.log.len() as LogIndex
    }

    fn quorum(&self) -> usize {
        // 严格多数 >N/2（N=peers+1）：等价 floor(N/2)+1 = (N+1)/2+1
        //（偶数 N=4 → 3；奇数 N=5 → 3——div_ceil(N/2) 对偶数会少 1）
        let n = self.peers.len() + 1;
        n / 2 + 1
    }

    // ── 选举 ──

    fn start_election(&mut self) {
        self.current_term += 1;
        self.role = Role::Candidate;
        self.voted_for = Some(self.id.clone());
        self.leader_hint = None;
        self.votes_received = vec![self.id.clone()]; // 自投
        self.last_heartbeat_ms = self.clock.now_ms();
        let (li, lt) = (self.last_log_index(), self.last_log_term());
        let (term, me) = (self.current_term, self.id.clone());
        for p in self.peers.clone() {
            self.outbox.push((
                p,
                RaftRpc::RequestVote {
                    term,
                    candidate: me.clone(),
                    last_log_index: li,
                    last_log_term: lt,
                },
            ));
        }
        // 单节点集群：立即胜出
        if self.votes_received.len() >= self.quorum() {
            self.become_leader();
        }
    }

    fn become_leader(&mut self) {
        self.role = Role::Leader;
        self.leader_hint = Some(self.id.clone());
        let ni = self.last_log_index() + 1;
        self.next_index.clear();
        self.match_index.clear();
        for p in &self.peers {
            self.next_index.insert(p.clone(), ni);
            self.match_index.insert(p.clone(), 0);
        }
        self.last_heartbeat_ms = self.clock.now_ms();
        self.broadcast_append(); // 立即心跳确立权威
    }

    fn step_request_vote(
        &mut self,
        from: NodeId,
        term: Term,
        candidate: NodeId,
        last_log_index: LogIndex,
        last_log_term: Term,
    ) {
        if term < self.current_term {
            self.outbox.push((
                from,
                RaftRpc::RequestVoteResp { term: self.current_term, vote_granted: false },
            ));
            return;
        }
        if term > self.current_term {
            // 高任期——无条件转 follower（论文 §5.1）
            self.current_term = term;
            self.role = Role::Follower;
            self.voted_for = None;
            self.last_heartbeat_ms = self.clock.now_ms();
        }
        // 日志新旧检查（§5.4.1：candidate 日志至少一样新）
        let up_to_date =
            (last_log_term > self.last_log_term())
                || (last_log_term == self.last_log_term() && last_log_index >= self.last_log_index());
        let grant = up_to_date
            && (self.voted_for.is_none() || self.voted_for.as_deref() == Some(candidate.as_str()));
        if grant {
            self.voted_for = Some(candidate.clone());
            self.last_heartbeat_ms = self.clock.now_ms(); // 让出计时
        }
        self.outbox.push((
            from,
            RaftRpc::RequestVoteResp { term: self.current_term, vote_granted: grant },
        ));
    }

    fn step_vote_resp(&mut self, from: NodeId, term: Term, granted: bool) {
        if term > self.current_term {
            self.current_term = term;
            self.role = Role::Follower;
            self.voted_for = None;
            return;
        }
        if self.role != Role::Candidate || term != self.current_term || !granted {
            return;
        }
        if !self.votes_received.contains(&from) {
            self.votes_received.push(from);
        }
        if self.votes_received.len() >= self.quorum() {
            self.become_leader();
        }
    }

    // ── 日志复制 ──

    fn broadcast_append(&mut self) {
        debug_assert_eq!(self.role, Role::Leader);
        let (term, me, commit) = (self.current_term, self.id.clone(), self.commit_index);
        for p in self.peers.clone() {
            let ni = *self.next_index.get(&p).unwrap_or(&1);
            let (pli, plt) = if ni <= 1 {
                (0, 0)
            } else {
                let e = &self.log[(ni - 2) as usize];
                (e.index, e.term)
            };
            let entries: Vec<LogEntry> =
                self.log[(ni as usize).saturating_sub(1)..].to_vec();
            self.outbox.push((
                p,
                RaftRpc::AppendEntries {
                    term,
                    leader: me.clone(),
                    prev_log_index: pli,
                    prev_log_term: plt,
                    entries,
                    leader_commit: commit,
                },
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step_append(
        &mut self,
        from: NodeId,
        term: Term,
        leader: NodeId,
        prev_log_index: LogIndex,
        prev_log_term: Term,
        entries: Vec<LogEntry>,
        leader_commit: LogIndex,
    ) {
        if term < self.current_term {
            self.outbox.push((
                from,
                RaftRpc::AppendEntriesResp { term: self.current_term, success: false, match_index: self.last_log_index() },
            ));
            return;
        }
        if term > self.current_term {
            self.current_term = term;
            self.voted_for = None;
        }
        // 合法 leader——计时重置
        self.role = Role::Follower;
        self.leader_hint = Some(leader);
        self.last_heartbeat_ms = self.clock.now_ms();

        // 一致性检查（§5.3）
        let consistent = if prev_log_index == 0 {
            true
        } else {
            match self.log.get((prev_log_index - 1) as usize) {
                Some(e) => e.term == prev_log_term,
                None => false,
            }
        };
        if !consistent {
            self.outbox.push((
                from,
                RaftRpc::AppendEntriesResp { term: self.current_term, success: false, match_index: self.last_log_index() },
            ));
            return;
        }
        // 追加/覆盖冲突（同 index 不同 term → 截断后重放）
        for e in entries {
            let slot = e.index as usize - 1;
            if slot < self.log.len() {
                if self.log[slot].term != e.term {
                    self.log.truncate(slot); // 冲突截断（未提交的旧任期条目）
                    self.log.push(e);
                }
            } else {
                self.log.push(e);
            }
        }
        // commit 推进（min(leader_commit, 本地末尾)）
        let new_commit = leader_commit.min(self.last_log_index());
        if new_commit > self.commit_index {
            // 论文 §5.4.2：只提交当前任期条目（通过复制计数间接提交旧条目——
            // 本实现 commit_index 单点由 leader 推进，follower 信任 leader_commit
            // 且 leader 已保证其 commit 过当前任期，安全）
            self.commit_index = new_commit;
            let newly = self.log[(self.last_applied as usize)..(self.commit_index as usize)].to_vec();
            self.pending_apply.extend(newly);
            self.last_applied = self.commit_index;
        }
        self.outbox.push((
            from,
            RaftRpc::AppendEntriesResp { term: self.current_term, success: true, match_index: self.last_log_index() },
        ));
    }

    fn step_append_resp(&mut self, from: NodeId, term: Term, success: bool, match_index: LogIndex) {
        if term > self.current_term {
            self.current_term = term;
            self.role = Role::Follower;
            self.voted_for = None;
            return;
        }
        if self.role != Role::Leader {
            return;
        }
        if success {
            let mi = self.match_index.entry(from.clone()).or_insert(0);
            *mi = (*mi).max(match_index);
            // nextIndex 前推
            let ni = self.next_index.entry(from).or_insert(1);
            *ni = (*ni).max(match_index + 1);
            self.advance_commit();
        } else {
            // 回退重试（线性——简单正确；条目量小可接受）
            let ni = self.next_index.entry(from).or_insert(1);
            *ni = (*ni).saturating_sub(1).max(1);
            self.broadcast_append();
        }
    }

    fn advance_commit(&mut self) {
        // 多数派 match_index 的中位数（含 leader 自己的 last_log_index）
        let mut indexes: Vec<LogIndex> = self
            .peers
            .iter()
            .filter_map(|p| self.match_index.get(p).copied())
            .collect();
        indexes.push(self.last_log_index());
        indexes.sort_unstable();
        let majority = indexes[indexes.len() / 2];
        // §5.4.2：只提交当前任期（当前任期条目复制到多数派才推进）
        if majority > self.commit_index {
            let cur_term_entry = self
                .log
                .get((majority as usize).saturating_sub(1))
                .map(|e| e.term == self.current_term)
                .unwrap_or(false);
            if cur_term_entry {
                self.commit_index = majority;
                let newly =
                    self.log[(self.last_applied as usize)..(self.commit_index as usize)].to_vec();
                self.pending_apply.extend(newly);
                self.last_applied = self.commit_index;
                self.broadcast_append(); // 通知 follower commit
            }
        }
    }
}

/// propose 非领导错误（携带 leader 提示——宿主重定向）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotLeader(pub Option<NodeId>);

/// 节点 id → 确定性抖动（同 id 同抖动——测试可复现；不同 id 错峰）。
fn id_jitter(id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ── 测试宿主（无网络模拟网——jepsen 式基件） ─────────────

/// 确定性手动时钟。
pub struct ManualClock {
    pub now: std::sync::atomic::AtomicU64,
    pub election_ms: u64,
    pub heartbeat_ms: u64,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            now: std::sync::atomic::AtomicU64::new(0),
            election_ms: 1000,
            heartbeat_ms: 150,
        }
    }
    pub fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn election_timeout_ms(&self) -> u64 {
        self.election_ms
    }
    fn heartbeat_ms(&self) -> u64 {
        self.heartbeat_ms
    }
}

/// 三节点测试网（内存消息总线——步骤交错确定性）。
pub struct TestNet {
    pub nodes: HashMap<NodeId, RaftNode>,
    /// 分区矩阵（true=连通）。
    pub links: HashMap<(NodeId, NodeId), bool>,
}

impl TestNet {
    pub fn new3(clock: std::sync::Arc<dyn Clock>) -> Self {
        let ids = ["a", "b", "c"];
        let mut nodes = HashMap::new();
        for id in ids {
            let peers: Vec<NodeId> = ids.iter().filter(|x| **x != id).map(|s| s.to_string()).collect();
            nodes.insert(id.to_string(), RaftNode::new(id, peers, clock.clone()));
        }
        let mut links = HashMap::new();
        for x in ids {
            for y in ids {
                links.insert((x.to_string(), y.to_string()), true);
            }
        }
        Self { nodes, links }
    }

    pub fn linked(&self, a: &str, b: &str) -> bool {
        *self.links.get(&(a.into(), b.into())).unwrap_or(&false)
    }

    /// 投递所有出站消息（按投递轮——同轮内随机序由 HashMap 迭代序充当）。
    pub fn flush(&mut self) {
        // 取走全部出站（快照投递——模拟同 RTT）
        let mut inflight: Vec<(NodeId, NodeId, RaftRpc)> = Vec::new();
        for (id, n) in self.nodes.iter_mut() {
            for (to, rpc) in n.outbox() {
                inflight.push((id.clone(), to, rpc));
            }
        }
        for (from, to, rpc) in inflight {
            if !self.linked(&from, &to) {
                continue; // 分区丢弃
            }
            if let Some(n) = self.nodes.get_mut(&to) {
                n.step(from.clone(), rpc);
            }
        }
    }

    /// 全网 tick + flush 一轮。
    pub fn round(&mut self, ms: u64, clock: &ManualClock) {
        clock.advance(ms);
        for n in self.nodes.values_mut() {
            n.tick();
        }
        // 收敛 flush：消息级联直到静默（一"轮"内的 RPC 链全通）
        for _ in 0..8 {
            self.flush();
            let pending: usize = self.nodes.values().map(|n| n.outbox.len()).sum();
            if pending == 0 {
                break;
            }
        }
    }

    pub fn leader(&self) -> Option<&NodeId> {
        self.nodes.iter().find(|(_, n)| n.role == Role::Leader).map(|(id, _)| id)
    }

    pub fn term(&self) -> Term {
        self.nodes.values().map(|n| n.current_term).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // F4：3 节点选主（超时 → 选举 → 多数派确认）
    #[test]
    fn raft_election_basic() {
        let clock = Arc::new(ManualClock::new());
        let mut net = TestNet::new3(clock.clone());
        // 初始全 follower
        assert_eq!(net.leader(), None);
        // 拨过选举超时 → 选出主
        net.round(1100, &clock);
        net.flush();
        let leader = net.leader().expect("leader elected");
        // 唯一 leader
        assert_eq!(
            net.nodes.values().filter(|n| n.role == Role::Leader).count(),
            1,
            "exactly one leader"
        );
        // 全员同任期且认同 leader
        for n in net.nodes.values() {
            assert_eq!(n.leader_hint.as_deref(), Some(leader.as_str()));
        }
    }

    // F4：日志复制——leader propose → 多数派落日志 → commit 推进
    #[test]
    fn raft_log_replication() {
        let clock = Arc::new(ManualClock::new());
        let mut net = TestNet::new3(clock.clone());
        net.round(1100, &clock);
        let leader = net.leader().unwrap().clone();
        // 并发 1000 提交
        for i in 0..1000u32 {
            net.nodes.get_mut(&leader).unwrap().propose(i.to_le_bytes().to_vec()).unwrap();
        }
        net.round(0, &clock); // 收敛（一次 RTT + resp 级联）
        let ln = net.nodes.get(&leader).unwrap();
        assert_eq!(ln.commit_index, 1000, "all committed in one RTT");
        for n in net.nodes.values() {
            assert_eq!(n.log.len(), 1000, "log fully replicated");
        }
        // apply 结算（宿主 drain committed）
        let applied = net.nodes.get_mut(&leader).unwrap().committed();
        assert_eq!(applied.len(), 1000);
        assert_eq!(&applied[42].cmd, &42u32.to_le_bytes());
    }

    // F4：kill leader → 3s 内新主（07 §14.3 门禁）
    #[test]
    fn raft_leader_kill_reelects() {
        use std::time::{Duration, Instant};
        let clock = Arc::new(ManualClock::new());
        let mut net = TestNet::new3(clock.clone());
        net.round(1100, &clock);
        let old = net.leader().unwrap().clone();
        // kill：摘除节点 + 断链
        net.nodes.remove(&old);
        let survivors: Vec<NodeId> = net.nodes.keys().cloned().collect();
        for s in &survivors {
            net.links.insert((s.clone(), old.clone()), false);
            net.links.insert((old.clone(), s.clone()), false);
        }
        // 门禁：≤3s（选举超时 1s + jitter ≤300ms + 投票一轮）
        let budget = Instant::now();
        let mut elapsed_ms = 0u64;
        while net.leader().is_none() && elapsed_ms <= 3000 {
            net.round(100, &clock);
            elapsed_ms += 100;
        }
        assert!(budget.elapsed() < Duration::from_secs(3), "wall clock sanity");
        let new_leader = net.leader().expect("re-elected within budget");
        assert_ne!(new_leader, &old);
        assert_eq!(net.nodes.values().filter(|n| n.role == Role::Leader).count(), 1);
    }

    // F4：对称分区 30s——少数派零提交、多数派继续；愈合一致
    #[test]
    fn raft_partition_no_split_brain() {
        let clock = Arc::new(ManualClock::new());
        let mut net = TestNet::new3(clock.clone());
        net.round(1100, &clock);
        let leader = net.leader().unwrap().clone();
        // 分区：少数派 = 非 leader 的某节点（leader 所在侧为多数派）
        let minority: NodeId = ["a", "b", "c"]
            .into_iter()
            .find(|x| *x != leader)
            .unwrap()
            .to_string();
        for x in ["a", "b", "c"] {
            net.links.insert((x.into(), minority.clone()), false);
            net.links.insert((minority.clone(), x.into()), false);
        }
        // 分区期 30s（心跳/选举推进）
        for _ in 0..30 {
            net.round(1000, &clock);
        }
        // 少数派：无 leader（Candidate 徘徊或 Follower——绝无 Leader 提交）
        let m = net.nodes.get(&minority).unwrap();
        assert_ne!(m.role, Role::Leader, "minority must not lead");
        assert!(m.commit_index == 0, "minority zero commits");
        // 多数派继续工作（原 leader 或其侧新主——quorum=2 达成）
        let l2 = net.leader().expect("majority side has leader").clone();
        assert_ne!(l2, minority);
        let before = net.nodes.get(&l2).unwrap().log.len();
        net.nodes.get_mut(&l2).unwrap().propose(vec![1]).unwrap();
        net.round(0, &clock);
        assert_eq!(net.nodes.get(&l2).unwrap().log.len(), before + 1);
        // 愈合（少数派 term 已因反复竞选升高——leader 见高 term 退位重选，
        // 需要：选举 + 全量日志同步，给足收敛轮次）
        for x in ["a", "b", "c"] {
            net.links.insert((x.into(), minority.clone()), true);
            net.links.insert((minority.clone(), x.into()), true);
        }
        for _ in 0..15 {
            net.round(200, &clock);
        }
        // 一致：少数派日志追平多数派（更高任期的高 log 同步）
        let target = net.nodes.get(&l2).unwrap().log.len();
        let m2 = net.nodes.get(&minority).unwrap();
        assert_eq!(m2.log.len(), target, "healed minority catches up");
        assert_eq!(m2.log.last().map(|e| e.term), net.nodes.get(&l2).unwrap().log.last().map(|e| e.term));
    }

    // F4：确定性——同日志序列两节点 apply 后状态相等
    #[test]
    fn raft_determinism() {
        #[derive(Default, Clone, PartialEq)]
        struct KV(Vec<(Vec<u8>, Vec<u8>)>);
        impl StateMachine for KV {
            fn apply(&mut self, cmd: &[u8]) {
                // cmd = key(1B) ++ value——upsert 语义
                self.0.retain(|(k, _)| k.as_slice() != &cmd[..1]);
                self.0.push((cmd[..1].to_vec(), cmd[1..].to_vec()));
            }
        }
        let clock = Arc::new(ManualClock::new());
        let mut net = TestNet::new3(clock.clone());
        net.round(1100, &clock);
        let leader = net.leader().unwrap().clone();
        for i in 0..100u8 {
            let k = i % 10;
            net.nodes.get_mut(&leader).unwrap().propose(vec![k, i]).unwrap();
            if i % 7 == 0 {
                net.round(0, &clock);
            }
        }
        net.round(0, &clock);
        // 两 follower 分别 apply
        let mut sms: HashMap<&str, KV> = HashMap::new();
        for (id, n) in net.nodes.iter_mut() {
            let sm = sms.entry(id.as_str()).or_default();
            for e in n.committed() {
                sm.apply(&e.cmd);
            }
        }
        let vals: Vec<KV> = sms.into_values().collect();
        assert!(vals.windows(2).all(|w| w[0] == w[1]), "same log → same state");
    }
}
