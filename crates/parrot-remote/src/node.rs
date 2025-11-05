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
    /// Explicit IDs must match HELLO; bare discovery endpoints may learn the ID
    /// only on unauthenticated transports. mTLS always requires an explicit ID.
    pub identity_is_explicit: bool,
    pub scheme: String,
    pub addr: SocketAddr,
}

impl std::str::FromStr for NodeAddr {
    type Err = String;
    /// Numeric IP endpoints or "mem://node"; DNS requires async config resolution.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_endpoint(&parrot_config::endpoint::SeedEndpoint::parse(s, "tcp")?)
    }
}

impl NodeAddr {
    pub(crate) fn from_endpoint(
        endpoint: &parrot_config::endpoint::SeedEndpoint,
    ) -> Result<Self, String> {
        if endpoint.scheme == "mem" {
            return Ok(Self::mem(&endpoint.node_id));
        }
        let addr = endpoint
            .socket_addr()
            .ok_or("hostname requires RemoteConfig::from_resolved_async")?;
        Ok(Self {
            node_id: endpoint.node_id.clone(),
            identity_is_explicit: endpoint.identity_is_explicit,
            scheme: endpoint.scheme.into(),
            addr,
        })
    }

    pub fn tcp(node_id: impl Into<String>, addr: SocketAddr) -> Self {
        Self {
            node_id: node_id.into(),
            identity_is_explicit: true,
            scheme: "tcp".into(),
            addr,
        }
    }

    /// QUIC 载体（K3）——地址语义与 TCP 同（UDP 端口）。
    pub fn quic(node_id: impl Into<String>, addr: SocketAddr) -> Self {
        Self {
            node_id: node_id.into(),
            identity_is_explicit: true,
            scheme: "quic".into(),
            addr,
        }
    }

    /// mem 载体占位地址（进程内端点表寻址按 node_id，无真实端口）。
    pub fn mem(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            identity_is_explicit: true,
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

/// A known destination may still require DNS before it has a socket address.
/// This is distinct from connection liveness and does not trigger resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEndpoint {
    Address(NodeAddr),
    Dns {
        node_id: NodeId,
        scheme: String,
        host: String,
        port: u16,
    },
}
impl From<NodeAddr> for NodeEndpoint {
    fn from(address: NodeAddr) -> Self {
        Self::Address(address)
    }
}
impl NodeEndpoint {
    pub fn node_id(&self) -> &str {
        match self {
            Self::Address(a) => &a.node_id,
            Self::Dns { node_id, .. } => node_id,
        }
    }
    pub fn scheme(&self) -> &str {
        match self {
            Self::Address(a) => &a.scheme,
            Self::Dns { scheme, .. } => scheme,
        }
    }
    pub fn address(&self) -> Option<&NodeAddr> {
        match self {
            Self::Address(a) => Some(a),
            Self::Dns { .. } => None,
        }
    }
}

/// Node routes retain separate ownership for configured seeds, active
/// connections, and replaceable membership discoveries.
#[derive(Default)]
struct NodeRecord {
    seed: Option<NodeAddr>,
    configured_dns: Option<NodeEndpoint>,
    connection: Option<NodeAddr>,
    discovered: Option<NodeEndpoint>,
    status: Arc<NodeStatus>,
}
impl NodeRecord {
    fn address(&self) -> Option<&NodeAddr> {
        self.discovered
            .as_ref()
            .and_then(NodeEndpoint::address)
            .or(self.seed.as_ref())
            .or(self.connection.as_ref())
    }
}
#[derive(Default)]
pub struct NodeTable {
    nodes: Mutex<HashMap<NodeId, NodeRecord>>,
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
            .or_default()
            .seed
            .get_or_insert(addr);
    }

    /// Configured DNS ownership survives connection and membership retirement.
    pub(crate) fn add_dns_seed(&self, endpoint: NodeEndpoint) {
        debug_assert!(matches!(endpoint, NodeEndpoint::Dns { .. }));
        self.nodes
            .lock()
            .unwrap()
            .entry(endpoint.node_id().to_owned())
            .or_default()
            .configured_dns
            .get_or_insert(endpoint);
    }

    pub(crate) fn connected(&self, addr: NodeAddr) {
        let mut nodes = self.nodes.lock().unwrap();
        let record = nodes.entry(addr.node_id.clone()).or_default();
        record.connection = Some(addr);
        record.status.set(NodeState::Connected);
    }

    pub(crate) fn disconnected(&self, node: &str) {
        let mut nodes = self.nodes.lock().unwrap();
        if let Some(record) = nodes.get_mut(node) {
            record.connection = None;
            record.status.set(NodeState::Disconnected);
            if record.address().is_none()
                && record.discovered.is_none()
                && record.configured_dns.is_none()
            {
                nodes.remove(node);
            }
        }
    }

    /// Replace this source's complete snapshot. Expiry/removal retires only
    /// discoveries, never caller-owned seeds or a still-active physical link.
    pub(crate) fn reconcile_membership(&self, routes: Vec<NodeEndpoint>) {
        let mut nodes = self.nodes.lock().unwrap();
        for record in nodes.values_mut() {
            record.discovered = None;
        }
        for route in routes {
            let id = route.node_id().to_owned();
            nodes.entry(id).or_default().discovered = Some(route);
        }
        nodes.retain(|_, record| {
            record.address().is_some()
                || record.discovered.is_some()
                || record.configured_dns.is_some()
        });
    }

    pub(crate) fn discovered(&self, node: &str) -> Option<NodeEndpoint> {
        self.nodes.lock().unwrap().get(node)?.discovered.clone()
    }

    /// Numeric discovery, seed, or active connection address; None for unresolved DNS.
    pub fn get(&self, node_id: &str) -> Option<(NodeAddr, Arc<NodeStatus>)> {
        let nodes = self.nodes.lock().unwrap();
        let record = nodes.get(node_id)?;
        Some((record.address()?.clone(), record.status.clone()))
    }
    /// Known identity, including an unresolved hostname. Use get for a socket.
    pub fn contains(&self, node_id: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(node_id)
    }
    /// Preferred advertised endpoint, without performing DNS or opening a socket.
    pub fn endpoint(&self, node_id: &str) -> Option<NodeEndpoint> {
        let nodes = self.nodes.lock().unwrap();
        let record = nodes.get(node_id)?;
        record
            .discovered
            .clone()
            .or_else(|| record.configured_dns.clone())
            .or_else(|| record.address().cloned().map(NodeEndpoint::Address))
    }
    /// All known preferred endpoints, including unresolved hostnames.
    pub fn endpoints(&self) -> Vec<NodeEndpoint> {
        self.nodes
            .lock()
            .unwrap()
            .values()
            .filter_map(|record| {
                record
                    .discovered
                    .clone()
                    .or_else(|| record.configured_dns.clone())
                    .or_else(|| record.address().cloned().map(NodeEndpoint::Address))
            })
            .collect()
    }
    /// Available numeric addresses only; unresolved DNS identities are omitted.
    pub fn list(&self) -> Vec<NodeAddr> {
        self.nodes
            .lock()
            .unwrap()
            .values()
            .filter_map(|record| record.address().cloned())
            .collect()
    }
    pub fn len(&self) -> usize {
        self.nodes.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.lock().unwrap().is_empty()
    }
    pub fn require(&self, node_id: &str) -> Result<(NodeAddr, Arc<NodeStatus>), RemoteError> {
        self.get(node_id).ok_or_else(|| {
            if self.contains(node_id) {
                RemoteError::Transport(format!("node {node_id} requires DNS resolution"))
            } else {
                RemoteError::UnknownNode(node_id.to_string())
            }
        })
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
    #[test]
    fn dns_identity_is_known_without_a_fabricated_socket_and_retires_independently() {
        let table = NodeTable::new();
        let dns = NodeEndpoint::Dns {
            node_id: "dns".into(),
            scheme: "tcp".into(),
            host: "localhost".into(),
            port: 9000,
        };
        table.reconcile_membership(vec![dns.clone()]);
        assert!(table.contains("dns"));
        assert_eq!(table.len(), 1);
        assert_eq!(table.endpoint("dns"), Some(dns.clone()));
        assert_eq!(table.endpoints(), vec![dns.clone()]);
        assert!(table.get("dns").is_none());
        assert!(table.list().is_empty());
        assert!(table
            .require("dns")
            .err()
            .unwrap()
            .to_string()
            .contains("requires DNS"));
        let socket = NodeAddr::tcp("dns", "127.0.0.1:9000".parse().unwrap());
        table.connected(socket.clone());
        assert_eq!(table.get("dns").unwrap().0, socket);
        table.disconnected("dns");
        assert!(table.contains("dns"));
        assert!(table.get("dns").is_none());
        table.add_seed(socket.clone());
        table.reconcile_membership(Vec::new());
        assert_eq!(table.get("dns").unwrap().0, socket);
        assert_eq!(table.endpoint("dns"), Some(socket.into()));
        let only = NodeTable::new();
        only.reconcile_membership(vec![dns]);
        only.reconcile_membership(Vec::new());
        assert!(only.is_empty());
    }
    #[test]
    fn configured_dns_survives_membership_and_connection_retirement_without_old_ip() {
        let table = NodeTable::new();
        let endpoint = NodeEndpoint::Dns {
            node_id: "peer".into(),
            scheme: "tcp".into(),
            host: "host.test".into(),
            port: 9000,
        };
        table.add_dns_seed(endpoint.clone());
        table.connected(NodeAddr::tcp("peer", "127.0.0.1:9000".parse().unwrap()));
        table.reconcile_membership(vec![NodeAddr::tcp(
            "peer",
            "127.0.0.2:9000".parse().unwrap(),
        )
        .into()]);
        table.reconcile_membership(Vec::new());
        table.disconnected("peer");
        assert_eq!(table.endpoint("peer"), Some(endpoint));
        assert!(table.contains("peer"));
        assert!(table.get("peer").is_none());
    }
}
