//! 职责：NodeId/NodeAddr/NodeTable——静态种子表 + 连接状态机（DEV_01 §3.7 / 05 §2.3）。
//!
//! P1：静态表（配置注入）；P2 由 SWIM membership 动态维护（接口不变）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::RemoteError;

pub type NodeId = String;

/// 节点地址（scheme 由 Transport 解释；P1 = "tcp"）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeAddr {
    pub node_id: NodeId,
    pub scheme: String,
    pub addr: SocketAddr,
}

impl NodeAddr {
    pub fn tcp(node_id: impl Into<String>, addr: SocketAddr) -> Self {
        Self {
            node_id: node_id.into(),
            scheme: "tcp".into(),
            addr,
        }
    }

    /// QUIC 载体（K3）——地址语义与 TCP 同（UDP 端口）。
    pub fn quic(node_id: impl Into<String>, addr: SocketAddr) -> Self {
        Self {
            node_id: node_id.into(),
            scheme: "quic".into(),
            addr,
        }
    }

    /// mem 载体占位地址（进程内端点表寻址按 node_id，无真实端口）。
    pub fn mem(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            scheme: "mem".into(),
            addr: "0.0.0.0:0".parse().unwrap(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeState {
    Disconnected = 0,
    Connecting = 1,
    Connected = 2,
}

/// 连接状态（原子；is_alive 弱一致读）。
#[derive(Default)]
pub struct NodeStatus {
    state: AtomicU8,
}

impl NodeStatus {
    pub fn get(&self) -> NodeState {
        match self.state.load(Ordering::Acquire) {
            2 => NodeState::Connected,
            1 => NodeState::Connecting,
            _ => NodeState::Disconnected,
        }
    }
    pub fn set(&self, s: NodeState) {
        self.state.store(s as u8, Ordering::Release);
    }
}

/// 静态节点表：node_id → (addr, status)。P2 换 SWIM 动态表，接口不变。
#[derive(Default)]
pub struct NodeTable {
    nodes: Mutex<HashMap<NodeId, (NodeAddr, Arc<NodeStatus>)>>,
}

impl NodeTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_seed(&self, addr: NodeAddr) {
        self.nodes
            .lock()
            .unwrap()
            .entry(addr.node_id.clone())
            .or_insert_with(|| (addr, Arc::new(NodeStatus::default())));
    }

    pub fn get(&self, node_id: &str) -> Option<(NodeAddr, Arc<NodeStatus>)> {
        self.nodes.lock().unwrap().get(node_id).cloned()
    }

    pub fn contains(&self, node_id: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(node_id)
    }

    pub fn list(&self) -> Vec<NodeAddr> {
        self.nodes
            .lock()
            .unwrap()
            .values()
            .map(|(a, _)| a.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.nodes.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.lock().unwrap().is_empty()
    }

    /// remote_ref 出口校验：node 必须在表（否则 UnknownNode）。
    pub fn require(&self, node_id: &str) -> Result<(NodeAddr, Arc<NodeStatus>), RemoteError> {
        self.get(node_id)
            .ok_or_else(|| RemoteError::UnknownNode(node_id.to_string()))
    }
}

/// 从 `parrot://node/system/user/x` 提取 node 段；非 parrot:// 返回 None。
pub fn node_of_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("parrot://")?;
    rest.split('/').next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_table_lifecycle() {
        let t = NodeTable::new();
        let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        t.add_seed(NodeAddr::tcp("a", addr));
        assert!(t.contains("a"));
        assert!(!t.contains("b"));
        let (a, status) = t.require("a").unwrap();
        assert_eq!(a.addr, addr);
        assert_eq!(status.get(), NodeState::Disconnected);
        status.set(NodeState::Connected);
        assert_eq!(status.get(), NodeState::Connected);
        // 重复 add 不覆盖状态
        t.add_seed(NodeAddr::tcp("a", "127.0.0.1:9999".parse().unwrap()));
        let (a2, _) = t.require("a").unwrap();
        assert_eq!(a2.addr, addr);
        assert!(matches!(t.require("zz"), Err(RemoteError::UnknownNode(_))));
    }

    #[test]
    fn path_node_extraction() {
        assert_eq!(node_of_path("parrot://node-1/sys/user/x"), Some("node-1"));
        assert_eq!(node_of_path("/user/local"), None);
        assert_eq!(node_of_path("parrot://"), Some(""));
    }
}
