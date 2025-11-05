//! F5 · DirectoryStore / DirectoryApi（DEV_05 §5 / 07 §6.4）。
//!
//! DirectoryStore：Raft 复制状态机——集群拓扑目录的权威存储。
//! 状态：`HashMap<node, DirEntry{endpoints, version, ttl_s}>`。
//! Cmd：Upsert / Remove / BorderDeclare / KeyAggregate。
//!
//! 本模块给出：① Cmd 序列化与状态机 apply；② DirectorySm（Raft::StateMachine
//! 实现）；③ Replica 封装（RaftNode + DirectorySm + client 语义）。
//! 传输挂接与 DirectoryApi（RESOLVE_Q 应答）在 F7 解析管线集成。

mod command;
mod durable;
mod request;
pub use request::{directory_command_digest, DirectoryRequestStatus};
pub mod client;
pub mod network;
pub use command::DirectoryApplyError;
pub use durable::{
    DirectoryRead, DirectoryReadTicket, DurableDirectory, DurableDirectoryError,
    DurableDirectoryProposal, DurableDirectoryStatus,
};

use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::raft::{Clock, LogIndex, RaftNode, StateMachine};

/// 目录条目（07 §6.4 表格原样）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirEntry {
    pub endpoints: Vec<String>,
    pub version: u64,
    pub ttl_s: u32,
}

/// Directory 命令（Raft 复制日志的 Cmd）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DirCmd {
    /// 节点注册/更新（graceful 启动、端点变化）。
    Upsert {
        node: String,
        endpoints: Vec<String>,
    },
    /// 节点摘除（graceful 离开）。
    Remove { node: String },
    /// border 声明前缀（该 border 代理此前缀的 RESOLVE）。
    BorderDeclare { prefix: String, node: String },
    /// Receptionist key 聚合上报（key → 承载节点集）。
    KeyAggregate { key: String, nodes: Vec<String> },
}

/// 查询视图（只读快照——client 侧）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DirQuery {
    Resolve { prefix_or_node: String },
    BorderFor { prefix: String },
    NodesForKey { key: String },
}
pub(super) fn validate_query(query: &DirQuery) -> Result<usize, DirectoryApplyError> {
    let text = match query {
        DirQuery::Resolve { prefix_or_node } => prefix_or_node,
        DirQuery::BorderFor { prefix } => prefix,
        DirQuery::NodesForKey { key } => key,
    };
    if text.len() > 4096 {
        return Err(DirectoryApplyError::Capacity);
    }
    Ok(text.len())
}

/// Directory 复制状态机。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DirectorySm {
    pub nodes: HashMap<String, DirEntry>,
    /// 前缀 → border 节点。
    pub borders: HashMap<String, String>,
    /// receptionist key → 节点集（集群间发现聚合）。
    pub key_index: HashMap<String, Vec<String>>,
    /// 版本单调源（Upsert/Remove 均递增——客户端缓存失效判定）。
    pub version: u64,
}

impl StateMachine for DirectorySm {
    type Error = DirectoryApplyError;
    fn apply(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        let cmd = DirCmd::decode(bytes)?;
        self.version
            .checked_add(1)
            .ok_or(DirectoryApplyError::RevisionExhausted)?;
        self.apply_validated(cmd);
        Ok(())
    }
}
impl DirectorySm {
    // Caller has validated the command and reserved revision capacity.
    fn apply_validated(&mut self, cmd: DirCmd) {
        self.version += 1;
        match cmd {
            DirCmd::Upsert { node, endpoints } => {
                let e = self.nodes.entry(node).or_insert(DirEntry {
                    endpoints: Vec::new(),
                    version: 0,
                    ttl_s: 3600,
                });
                e.endpoints = endpoints;
                e.version = self.version;
            }
            DirCmd::Remove { node } => {
                self.nodes.remove(&node);
                self.borders.retain(|_, b| b != &node);
                self.key_index.retain(|_, nodes| {
                    nodes.retain(|n| n != &node);
                    !nodes.is_empty()
                });
            }
            DirCmd::BorderDeclare { prefix, node } => {
                self.borders.insert(prefix, node);
            }
            DirCmd::KeyAggregate { key, nodes } => {
                self.key_index.insert(key, nodes);
            }
        }
    }
}

/// 单副本封装（测试与嵌入形态——生产 3/5 副本经 RaftNode 集群）。
pub struct DirectoryReplica {
    pub raft: RaftNode,
    pub sm: DirectorySm,
}

impl DirectoryReplica {
    pub fn new(
        id: impl Into<String>,
        peers: Vec<String>,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Self {
        Self {
            raft: RaftNode::new(id, peers, clock),
            sm: DirectorySm::default(),
        }
    }

    /// client 写（leader 才受理）。
    pub fn propose(&mut self, cmd: &DirCmd) -> Result<LogIndex, DirectoryWriteError> {
        let bytes = cmd.encode()?;
        self.raft
            .propose(bytes)
            .map_err(|e| DirectoryWriteError::NotLeader {
                shard: self.raft.id.clone(),
                leader: e.0,
            })
    }

    /// 驱动：apply 已提交条目到状态机（宿主周期调用）。
    pub fn apply_committed(&mut self) -> Result<usize, DirectoryApplyError> {
        let commands = self.sm.prepare(self.raft.pending_committed())?;
        let n = commands.len();
        self.raft.committed();
        for cmd in commands {
            self.sm.apply_validated(cmd);
        }
        Ok(n)
    }

    /// client 读（线性一致读的先决条件是读经 leader+lease——本实现
    /// 按 07 §6.4 目录语义：命中即可用，版本号供缓存失效判定）。
    pub fn query(&self, q: &DirQuery) -> Option<DirEntry> {
        self.sm.query(q)
    }
}

impl DirectorySm {
    pub fn query(&self, q: &DirQuery) -> Option<DirEntry> {
        match q {
            DirQuery::Resolve { prefix_or_node } => {
                // 节点直查（parrot://node 形态）
                if let Some(e) = self.nodes.get(prefix_or_node) {
                    return Some(e.clone());
                }
                // 前缀所属节点（"parrot://eu-1/user" → 节点 eu-1？——
                // 目录按节点注册，前缀解析取 border）
                None
            }
            DirQuery::BorderFor { prefix } => {
                // 最长前缀 border
                let hit = self
                    .borders
                    .iter()
                    .filter(|(p, _)| prefix.starts_with(p.as_str()))
                    .max_by_key(|(p, _)| p.len());
                hit.map(|(_, node)| DirEntry {
                    endpoints: vec![format!("parrot://{node}")],
                    version: self.version,
                    ttl_s: 60,
                })
            }
            DirQuery::NodesForKey { key } => self.key_index.get(key).map(|nodes| DirEntry {
                endpoints: nodes.iter().map(|n| format!("parrot://{n}")).collect(),
                version: self.version,
                ttl_s: 30,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::ManualClock;
    use std::sync::Arc;

    // F5：状态机确定性应用（Upsert/Remove/Border/KeyAggregate）
    #[test]
    fn directory_sm_apply_all_cmds() {
        let mut sm = DirectorySm::default();
        let cmds = [
            DirCmd::Upsert {
                node: "n1".into(),
                endpoints: vec!["tcp://10.0.0.1:7000".into()],
            },
            DirCmd::Upsert {
                node: "n2".into(),
                endpoints: vec!["tcp://10.0.0.2:7000".into()],
            },
            DirCmd::BorderDeclare {
                prefix: "parrot://eu-1/".into(),
                node: "n1".into(),
            },
            DirCmd::KeyAggregate {
                key: "media/track/r1".into(),
                nodes: vec!["n2".into()],
            },
        ];
        for c in &cmds {
            sm.apply(&bincode::serde::encode_to_vec(c, bincode::config::standard()).unwrap())
                .unwrap();
        }
        assert_eq!(sm.nodes.len(), 2);
        assert_eq!(sm.nodes["n1"].endpoints, vec!["tcp://10.0.0.1:7000"]);
        assert_eq!(sm.version, 4);
        // Remove 级联清理（border/key_index）
        sm.apply(
            &bincode::serde::encode_to_vec(
                &DirCmd::Remove { node: "n1".into() },
                bincode::config::standard(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(sm.nodes.len(), 1);
        assert!(
            !sm.borders.contains_key("parrot://eu-1/"),
            "Remove 级联 border"
        );
        // 查询视图（状态直接搬入副本）
        let mut r = DirectoryReplica::new("solo", vec![], Arc::new(ManualClock::new()));
        r.sm = sm.clone();
        let hit = r.query(&DirQuery::NodesForKey {
            key: "media/track/r1".into(),
        });
        assert_eq!(hit.unwrap().endpoints, vec!["parrot://n2"]);
    }

    // F5：单副本 propose → commit → apply 全链（Raft 单节点即时提交）
    #[test]
    fn directory_replica_single_node_commit() {
        let clock = Arc::new(ManualClock::new());
        let mut r = DirectoryReplica::new("solo", vec![], clock.clone());
        // 时钟拨过选举超时（含 jitter 上限 300ms）→ 单节点立即主
        clock.advance(1400);
        r.raft.tick();
        assert_eq!(r.raft.role, crate::raft::Role::Leader, "单节点立即主");
        let idx = r
            .propose(&DirCmd::Upsert {
                node: "n1".into(),
                endpoints: vec!["tcp://x:1".into()],
            })
            .unwrap();
        assert_eq!(idx, 1);
        // 无 peers——advance_commit 需多数派=1（自己）
        assert_eq!(r.raft.commit_index, 1, "单节点即时提交");
        assert_eq!(r.apply_committed().unwrap(), 1);
        assert!(r.sm.nodes.contains_key("n1"));
    }
}

// ---------------- S3（DEV_06 §3）：Directory 分片 ----------------

/// Directory 分片路由（07 §6.4 分片策略：一致性哈希——D1 HashRing 复用）。
///
/// 百万节点目录按 key 哈希到 N 个 shard（每个 shard 是一个 Raft 组）：
/// - 写：`route(key)` → shard leader → Raft 复制；
/// - 读：RESOLVE 按同一环路由（读写同环保证无跨片查找）。
///
/// 与 D1 的区别：D1 按 entity 路由（actor 亲和），S3 按 key 路由（目录
/// 键空间切分）。两者共享 HashRing 数学（256 vnode——均匀性已验证）。
pub struct DirectoryShards {
    /// shard id → 本地嵌入副本；new 创建单副本组，网络复制宿主尚未接线。
    pub shards: HashMap<String, DirectoryReplica>,
    ring: crate::sharding::HashRing,
    // Keep the configured participants even if a public test/embedded replica
    // is temporarily absent. Iterating only the live map would hide lost shards.
    shard_ids: Vec<String>,
    identity: std::sync::Arc<()>,
}

/// One position in a multi-shard proposal. A local append is not commitment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryPosition {
    pub shard: String,
    pub index: LogIndex,
    pub term: crate::raft::Term,
}

/// Receipt for all participant logs, scoped to this directory's fixed shard set.
#[derive(Debug, Clone)]
pub struct DirectoryProposal {
    identity: std::sync::Arc<()>,
    positions: Vec<DirectoryPosition>,
    command_digest: [u8; 32],
}
impl DirectoryProposal {
    pub fn positions(&self) -> &[DirectoryPosition] {
        &self.positions
    }
}

/// Current local observation of the complete receipt. No variant grants a
/// linearizable read lease. Conflict/unavailability is not proof that the
/// command was never committed elsewhere; resolve uncertain outcomes explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryProposalStatus {
    Pending,
    Committed,
    Conflict,
    Unavailable,
}

/// A failed admission identifies the unavailable or non-leading participant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DirectoryWriteError {
    #[error(transparent)]
    Command(#[from] DirectoryApplyError),
    #[error("directory has no configured shards")]
    NoShards,
    #[error("directory shard unavailable: {0}")]
    Unavailable(String),
    #[error("directory shard {shard} is not leader (hint: {leader:?})")]
    NotLeader {
        shard: String,
        leader: Option<String>,
    },
}

/// Read failures are distinct from an absent record in the checked API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DirectoryReadError {
    #[error("directory has no configured shards")]
    NoShards,
    #[error("directory shard unavailable: {0}")]
    Unavailable(String),
    #[error("directory aggregate revision overflow")]
    VersionOverflow,
}

impl DirectoryShards {
    /// N 个 shard（各自独立 Raft 组——id 形如 "dir-s0"）。
    pub fn new(shard_count: usize, clock: std::sync::Arc<dyn Clock>) -> Self {
        let ids: Vec<String> = (0..shard_count).map(|i| format!("dir-s{i}")).collect();
        let mut shards = HashMap::new();
        for id in &ids {
            shards.insert(
                id.clone(),
                DirectoryReplica::new(id.clone(), vec![], clock.clone()),
            );
        }
        Self {
            shards,
            ring: crate::sharding::HashRing::new(&ids, 256),
            shard_ids: ids,
            identity: std::sync::Arc::new(()),
        }
    }

    /// key → 目标 shard id（读写同环）。
    pub fn route(&self, key: &str) -> Option<&str> {
        self.ring.node(key)
    }

    /// Admit a command to its authoritative shard, or Remove to every shard.
    /// All configured participants must be present and leaders before any append.
    /// Admission is synchronous in this embedded host; Raft commitment remains
    /// independent per shard. Retain the entire receipt and wait for all positions.
    pub fn propose(&mut self, cmd: &DirCmd) -> Result<DirectoryProposal, DirectoryWriteError> {
        let bytes = cmd.encode()?;
        let targets = if matches!(cmd, DirCmd::Remove { .. }) {
            self.shard_ids.clone()
        } else {
            vec![self
                .route(&dir_cmd_key(cmd))
                .ok_or(DirectoryWriteError::NoShards)?
                .to_string()]
        };
        if targets.is_empty() {
            return Err(DirectoryWriteError::NoShards);
        }
        for id in &targets {
            let shard = self
                .shards
                .get(id)
                .ok_or_else(|| DirectoryWriteError::Unavailable(id.clone()))?;
            if shard.raft.role != crate::raft::Role::Leader {
                return Err(DirectoryWriteError::NotLeader {
                    shard: id.clone(),
                    leader: shard.raft.leader_hint.clone(),
                });
            }
        }
        let command_digest = Sha256::digest(&bytes).into();
        let mut positions = Vec::with_capacity(targets.len());
        // No await/callback or other state mutation occurs between preflight and
        // these appends, so a participant cannot lose leadership during admission.
        for id in targets {
            let shard = self
                .shards
                .get_mut(&id)
                .ok_or_else(|| DirectoryWriteError::Unavailable(id.clone()))?;
            let index =
                shard
                    .raft
                    .propose(bytes.clone())
                    .map_err(|e| DirectoryWriteError::NotLeader {
                        shard: id.clone(),
                        leader: e.0,
                    })?;
            positions.push(DirectoryPosition {
                shard: id,
                index,
                term: shard.raft.current_term,
            });
        }
        Ok(DirectoryProposal {
            identity: self.identity.clone(),
            positions,
            command_digest,
        })
    }

    /// Observe every original position, including its term and command digest.
    /// `Committed` does not imply `apply_committed_all` has been called, nor is
    /// multi-group commitment atomic. A missing/unavailable shard cannot be skipped.
    pub fn proposal_status(&self, proposal: &DirectoryProposal) -> DirectoryProposalStatus {
        if !std::sync::Arc::ptr_eq(&self.identity, &proposal.identity) {
            return DirectoryProposalStatus::Unavailable;
        }
        let mut pending = false;
        for position in &proposal.positions {
            let Some(shard) = self.shards.get(&position.shard) else {
                return DirectoryProposalStatus::Unavailable;
            };
            let Some(slot) = position
                .index
                .checked_sub(1)
                .and_then(|n| usize::try_from(n).ok())
            else {
                return DirectoryProposalStatus::Unavailable;
            };
            let Some(entry) = shard.raft.log.get(slot) else {
                // Admission appended every receipt position synchronously.
                // An available participant no longer retaining that position
                // has lost the original suffix; it is not awaiting quorum.
                return DirectoryProposalStatus::Conflict;
            };
            if entry.term != position.term
                || Sha256::digest(&entry.cmd)[..] != proposal.command_digest
            {
                return DirectoryProposalStatus::Conflict;
            }
            pending |= shard.raft.commit_index < position.index;
        }
        if pending {
            DirectoryProposalStatus::Pending
        } else {
            DirectoryProposalStatus::Committed
        }
    }

    /// 全 shard apply（宿主周期调用——各 shard 独立驱动）。
    pub fn apply_committed_all(&mut self) -> Result<usize, DirectoryApplyError> {
        // Preflight all locally pending groups before consuming any queue. This
        // prevents malformed application from partially updating this local view;
        // it does not make independent Raft-group commitment atomic.
        let mut prepared = Vec::with_capacity(self.shards.len());
        for (id, shard) in &self.shards {
            prepared.push((
                id.clone(),
                shard.sm.prepare(shard.raft.pending_committed())?,
            ));
        }
        let mut n = 0;
        for (id, commands) in prepared {
            let shard = self
                .shards
                .get_mut(&id)
                .expect("preflight preserved local shards");
            shard.raft.committed();
            n += commands.len();
            for cmd in commands {
                shard.sm.apply_validated(cmd);
            }
        }
        Ok(n)
    }

    /// Convenience local view; unavailable and absent both return None. Use
    /// `query_checked` when an unavailable shard must be reported to a caller.
    pub fn query(&self, q: &DirQuery) -> Option<DirEntry> {
        self.query_checked(q).ok().flatten()
    }

    /// Read a local applied view, distinguishing absence from unavailability.
    /// Node and service-key lookups use their fixed owner. Border lookup selects
    /// the globally longest authoritative prefix. This is not a quorum read.
    pub fn query_checked(&self, q: &DirQuery) -> Result<Option<DirEntry>, DirectoryReadError> {
        if self.shard_ids.is_empty() {
            return Err(DirectoryReadError::NoShards);
        }
        Ok(match q {
            DirQuery::Resolve { prefix_or_node } => self.read_owner(prefix_or_node)?.query(q),
            DirQuery::NodesForKey { key } => self.read_owner(&format!("key:{key}"))?.query(q),
            DirQuery::BorderFor { prefix } => {
                let mut best: Option<(&str, &str)> = None;
                let mut version = 0u64;
                for id in &self.shard_ids {
                    let shard = self
                        .shards
                        .get(id)
                        .ok_or_else(|| DirectoryReadError::Unavailable(id.clone()))?;
                    // Local aggregate revision increases for every applied shard
                    // command, even when a query changes to a different owner.
                    version = version
                        .checked_add(shard.sm.version)
                        .ok_or(DirectoryReadError::VersionOverflow)?;
                    for (p, node) in &shard.sm.borders {
                        if prefix.starts_with(p)
                            && self.route(&format!("border:{p}")) == Some(id.as_str())
                            && best.is_none_or(|(old, _)| p.len() > old.len())
                        {
                            best = Some((p, node));
                        }
                    }
                }
                best.map(|(_, node)| DirEntry {
                    endpoints: vec![format!("parrot://{node}")],
                    version,
                    ttl_s: 60,
                })
            }
        })
    }

    fn read_owner(&self, key: &str) -> Result<&DirectoryReplica, DirectoryReadError> {
        let id = self.route(key).ok_or(DirectoryReadError::NoShards)?;
        self.shards
            .get(id)
            .ok_or_else(|| DirectoryReadError::Unavailable(id.into()))
    }

    /// 观测：各 shard 条目占比（均匀性门禁——S3 验收用）。
    pub fn shard_sizes(&self) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = self
            .shards
            .iter()
            .map(|(id, s)| (id.clone(), s.sm.nodes.len()))
            .collect();
        v.sort();
        v
    }

    /// 观测：全目录节点数（跨 shard 汇总）。
    pub fn total_nodes(&self) -> usize {
        self.shards.values().map(|s| s.sm.nodes.len()).sum()
    }
}

/// Stable ownership uses the logical record key, never its changing value.
/// Remove is handled separately as a proposal to every configured shard, since
/// node references can occur in any border or service-key owner.
fn dir_cmd_key(cmd: &DirCmd) -> String {
    match cmd {
        DirCmd::Upsert { node, .. } | DirCmd::Remove { node } => node.clone(),
        DirCmd::BorderDeclare { prefix, .. } => format!("border:{prefix}"),
        DirCmd::KeyAggregate { key, .. } => format!("key:{key}"),
    }
}

#[cfg(test)]
mod s3_tests {
    use super::*;
    use crate::raft::ManualClock;
    use std::sync::Arc;

    /// S3-1 读写同环：propose 落到的 shard == query 命中的 shard
    #[test]
    fn s3_read_write_same_ring() {
        let clock = Arc::new(ManualClock::new());
        let mut ds = DirectoryShards::new(4, clock.clone());
        // 全 shard 拨过选举超时 → 各自 solo leader
        clock.advance(1400);
        for s in ds.shards.values_mut() {
            s.raft.tick();
        }
        let cmd = DirCmd::Upsert {
            node: "parrot://n1".into(),
            endpoints: vec!["tcp://10.0.0.1:7000".into()],
        };
        let proposal = ds.propose(&cmd).unwrap();
        assert_eq!(proposal.positions().len(), 1);
        let shard_id = &proposal.positions()[0].shard;
        assert_eq!(ds.apply_committed_all().unwrap(), 1);
        // 读经同一环 → 命中
        let hit = ds.query(&DirQuery::Resolve {
            prefix_or_node: "parrot://n1".into(),
        });
        assert!(hit.is_some(), "读写同环必命中");
        assert_eq!(
            ds.route("parrot://n1").unwrap(),
            shard_id.as_str(),
            "写路由 == 读路由"
        );
    }

    /// S3-2 均匀分布：1 万节点目录 → 4 shard 占比失衡 < ±15%
    #[test]
    fn s3_distribution_uniform() {
        let clock = Arc::new(ManualClock::new());
        let mut ds = DirectoryShards::new(4, clock.clone());
        clock.advance(1400);
        for s in ds.shards.values_mut() {
            s.raft.tick();
        }
        let n = 10_000;
        for i in 0..n {
            ds.propose(&DirCmd::Upsert {
                node: format!("parrot://n{i}"),
                endpoints: vec![format!("tcp://10.0.0.{}:7", i % 256)],
            })
            .unwrap();
        }
        ds.apply_committed_all().unwrap();
        assert_eq!(ds.total_nodes(), n);
        let sizes = ds.shard_sizes();
        let avg = n as f64 / 4.0;
        let max_skew = sizes
            .iter()
            .map(|(_, sz)| ((*sz as f64 - avg) / avg).abs())
            .fold(0.0f64, f64::max);
        assert!(max_skew < 0.15, "4 shard 偏差 {max_skew:.2}（门禁 ±15%）");
    }

    /// S3-3 Remove 经所有分片日志清理引用（跨片无残留）
    #[test]
    fn s3_remove_cascades_through_all_shard_logs() {
        let clock = Arc::new(ManualClock::new());
        let mut ds = DirectoryShards::new(2, clock.clone());
        clock.advance(1400);
        for s in ds.shards.values_mut() {
            s.raft.tick();
        }
        ds.propose(&DirCmd::Upsert {
            node: "n1".into(),
            endpoints: vec!["tcp://x:1".into()],
        })
        .unwrap();
        ds.propose(&DirCmd::BorderDeclare {
            prefix: "parrot://eu-1/".into(),
            node: "n1".into(),
        })
        .unwrap();
        ds.apply_committed_all().unwrap();
        assert_eq!(ds.total_nodes(), 1);
        // Remove n1 → 所有分片提交后清理 border 声明
        ds.propose(&DirCmd::Remove { node: "n1".into() }).unwrap();
        ds.apply_committed_all().unwrap();
        assert_eq!(ds.total_nodes(), 0);
        let borders: usize = ds.shards.values().map(|s| s.sm.borders.len()).sum();
        assert_eq!(borders, 0, "跨 shard 级联无残留");
    }

    /// S3-4 扩容再平衡：8 shard 全量重哈希后无丢失
    #[test]
    fn s3_scale_out_rehash() {
        let clock = Arc::new(ManualClock::new());
        let mut ds4 = DirectoryShards::new(4, clock.clone());
        clock.advance(1400);
        for s in ds4.shards.values_mut() {
            s.raft.tick();
        }
        let n = 1000;
        for i in 0..n {
            ds4.propose(&DirCmd::Upsert {
                node: format!("parrot://n{i}"),
                endpoints: vec!["tcp://x:1".into()],
            })
            .unwrap();
        }
        ds4.apply_committed_all().unwrap();
        assert_eq!(ds4.total_nodes(), n);

        // 8 shard 新环（模拟扩容：数据搬移 = 全量重放）
        let mut ds8 = DirectoryShards::new(8, clock.clone());
        clock.advance(1400);
        for s in ds8.shards.values_mut() {
            s.raft.tick();
        }
        for i in 0..n {
            ds8.propose(&DirCmd::Upsert {
                node: format!("parrot://n{i}"),
                endpoints: vec!["tcp://x:1".into()],
            })
            .unwrap();
        }
        ds8.apply_committed_all().unwrap();
        assert_eq!(ds8.total_nodes(), n, "扩容后无丢失");
        // 且仍可命中（8 环读写同环）
        assert!(ds8
            .query(&DirQuery::Resolve {
                prefix_or_node: "parrot://n0".into()
            })
            .is_some());
        // 一致性哈希性质：扩容仅迁移 ≈ n/2 键（4→8 保留旧节点时理论
        // 最优 1 - 4/8 = 50%，对比全量重哈希 100%）——门禁 55%
        let moved = (0..n)
            .filter(|i| {
                ds4.route(&format!("parrot://n{i}")) != ds8.route(&format!("parrot://n{i}"))
            })
            .count();
        let expect_upper = n as f64 * 0.55;
        assert!(
            moved as f64 <= expect_upper,
            "迁移 {moved} > 门禁 {expect_upper}"
        );
    }

    /// S3-REG-1（缺陷回归）：KeyAggregate 同 key 上报节点集变化
    /// （first 变化）不得漂移到别的 shard——否则同 key 双份、
    /// 查询扇出取 find_map 顺序不稳定。
    ///
    /// 正确语义：KeyAggregate 路由 key 必须由 **key 本身**决定
    /// （hash(key)），与上报节点集无关。
    #[test]
    fn s3_reg_key_aggregate_stable_routing() {
        let clock = Arc::new(ManualClock::new());
        let mut ds = DirectoryShards::new(4, clock.clone());
        clock.advance(1400);
        for s in ds.shards.values_mut() {
            s.raft.tick();
        }
        // 同 key 两次上报：节点集不同（first 从 n1 → n9）
        ds.propose(&DirCmd::KeyAggregate {
            key: "media/track/r1".into(),
            nodes: vec!["n1".into(), "n2".into()],
        })
        .unwrap();
        ds.propose(&DirCmd::KeyAggregate {
            key: "media/track/r1".into(),
            nodes: vec!["n9".into(), "n2".into()], // first 漂移
        })
        .unwrap();
        ds.apply_committed_all().unwrap();
        // 查询必须命中（且只有一份——不因 first 漂移分裂）
        let hit = ds.query(&DirQuery::NodesForKey {
            key: "media/track/r1".into(),
        });
        let entry = hit.expect("同 key 重上报必须可查");
        // 后写覆盖先写（Raft 顺序 apply）——节点集应为第二次的
        assert_eq!(
            entry.endpoints,
            vec!["parrot://n9".to_string(), "parrot://n2".to_string()],
            "后写覆盖（同一逻辑条目——无分裂）"
        );
        // 全 shard 只有这一个 key 条目（非两份）
        let copies: usize = ds
            .shards
            .values()
            .map(|s| s.sm.key_index.contains_key("media/track/r1") as usize)
            .sum();
        assert_eq!(copies, 1, "同 key 不得在多 shard 分裂成两份");
    }

    /// S3-REG-2：BorderDeclare 同前缀重复声明（节点重启换 id）——
    /// 声明必须落同一 shard 并覆盖（前缀路由 key 稳定）。
    #[test]
    fn s3_reg_border_declare_stable_routing() {
        let clock = Arc::new(ManualClock::new());
        let mut ds = DirectoryShards::new(4, clock.clone());
        clock.advance(1400);
        for s in ds.shards.values_mut() {
            s.raft.tick();
        }
        let first = "n1";
        let second = (0..1000)
            .map(|i| format!("border-{i}"))
            .find(|n| ds.route(n) != ds.route(first))
            .unwrap();
        ds.propose(&DirCmd::BorderDeclare {
            prefix: "parrot://eu-1/".into(),
            node: first.into(),
        })
        .unwrap();
        ds.propose(&DirCmd::BorderDeclare {
            prefix: "parrot://eu-1/".into(),
            node: second.clone(), // 声明者的节点分片确实变化
        })
        .unwrap();
        ds.apply_committed_all().unwrap();
        let hit = ds.query(&DirQuery::BorderFor {
            prefix: "parrot://eu-1/".into(),
        });
        assert_eq!(
            hit.expect("border 可查").endpoints,
            vec![format!("parrot://{second}")],
            "重声明覆盖（后写胜——单一逻辑条目）"
        );
    }
}
