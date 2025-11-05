//! 职责：RemoteActorSystem 组装与生命周期（05 §7 / DEV_01 §3.7）。
//!
//! 组装：Transport（mem/tcp）+ Ingress（LocalLookup 注入）+ NodeTable +
//! CallbackRegistry + 连接管理。facade 集成走 RemoteGateway trait 倒置
//! （parrot crate 只认接口——E5.2）。

mod discovery;
mod dispatch;
mod probes;
mod receptionist;
mod seeds;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::error::RemoteError;
use crate::frame::Frame;
use crate::handshake::HandshakeBody;
use crate::ingress::{Ingress, LocalLookup};
use crate::node::{NodeAddr, NodeState, NodeStatus, NodeTable};
use crate::ref_::{RemoteActorRef, RemoteInner};
use crate::registry::CallbackRegistry;
use crate::transport::{ConnectionHandle, FrameSender, OnDisconnect, Transport};

/// P1 配置（静态节点表；P2 增 gossip/集群字段——接口预留位）。
#[derive(Clone)]
pub struct RemoteConfig {
    pub node_id: String,
    /// Optional bounded frame capture; disabled by default.
    pub capture: Option<crate::capture::FrameCapture>,
    pub security: crate::security::RemoteSecurity,
    /// Node identity to service-key grants for local and incoming discovery.
    /// Denies all registrations/subscriptions unless explicitly configured.
    pub receptionist_acl: crate::acl::AclRules,
    pub bind: Option<SocketAddr>,
    pub seeds: Vec<NodeAddr>,
    /// Original configured seed sources, authoritative for bootstrap when nonempty.
    /// `seeds` then contains the initial resolved snapshot. `with_seeds` replaces
    /// both forms; modifying the snapshot alone does not change these sources.
    pub seed_endpoints: Vec<parrot_config::endpoint::SeedEndpoint>,
    pub scheme: &'static str, // "tcp" | "quic" | "mem"
    pub callback_capacity: usize,
    pub heartbeat_enabled: bool,
    /// 附加能力位（E4/E1：连 pb-only 对端时叠加 caps::PB；默认 0=纯 bin）。
    pub extra_caps: u32,
    /// 本节点拓扑角色（07 §6）：Normal（默认）/ Hub（星型中心——spoke
    /// 将其链路作为默认路由 uplink）/ Border（联邦边界，同 Hub 待遇）。
    pub topology_role: crate::handshake::TopologyRole,
    /// Policy for originating business messages. Administrative and mirror
    /// traffic always resolves the actual destination directly.
    pub topology: crate::topology::TopologyConfig,
    /// 本节点可被直拨的监听地址 "host:port"（方案 A：握手时声明，hub 据此
    /// 向其他 spoke 注入 ROUTE_HINT；None = 不可直拨，只走 hub 中转）。
    pub direct_addr: Option<String>,
    /// 连接层运行时旋钮（配置切面——None=编译期默认，等价迁移前行为）。
    pub knobs: Option<crate::transport::RuntimeKnobs>,
    pub swim: crate::swim::SwimConfig,
}

impl RemoteConfig {
    // Shared by seed configuration, direct dialing and durable admission. This
    // checks only local invariants; an offline peer is still a valid destination.
    pub(crate) fn validate_peer_address(&self, peer: &NodeAddr) -> Result<(), RemoteError> {
        let invalid =
            |reason: String| RemoteError::Transport(format!("invalid peer address: {reason}"));
        parrot_config::endpoint::validate_node_id(&peer.node_id).map_err(invalid)?;
        if peer.scheme != self.scheme {
            return Err(invalid(
                "peer transport does not match node transport".into(),
            ));
        }
        if self.scheme != "mem" {
            if peer.addr.port() == 0 {
                return Err(invalid("peer network port must be nonzero".into()));
            }
            self.security.check_address(peer.addr)?;
        }
        if self.security.credentials().is_some() && !peer.identity_is_explicit {
            return Err(invalid(
                "mTLS requires an explicit node ID in the endpoint".into(),
            ));
        }
        Ok(())
    }

    pub fn with_capture(mut self, capture: crate::capture::FrameCapture) -> Self {
        self.capture = Some(capture);
        self
    }

    pub fn tcp(node_id: impl Into<String>, bind: Option<SocketAddr>) -> Self {
        Self {
            node_id: node_id.into(),
            capture: None,
            security: Default::default(),
            receptionist_acl: Default::default(),
            bind,
            seeds: Vec::new(),
            seed_endpoints: Vec::new(),
            scheme: "tcp",
            callback_capacity: 65536,
            heartbeat_enabled: true,
            extra_caps: 0,
            topology_role: crate::handshake::TopologyRole::Normal,
            topology: Default::default(),
            direct_addr: None,
            knobs: None,
            swim: Default::default(),
        }
    }

    /// Require CA-verified certificates and allow administrative commands only
    /// from the supplied authenticated node IDs. Certificates need CN and SAN=node ID.
    pub fn with_mtls(
        mut self,
        credentials: crate::tls::TlsConfig,
        admin_nodes: impl IntoIterator<Item = String>,
    ) -> Self {
        self.security = crate::security::RemoteSecurity::MutualTls {
            credentials,
            admin_nodes: admin_nodes.into_iter().collect(),
        };
        self
    }

    /// Development only: permits unauthenticated network and administrative access.
    pub fn with_insecure_development(mut self) -> Self {
        self.security = crate::security::RemoteSecurity::InsecureDevelopment;
        self
    }

    /// 声明拓扑角色（hub 节点显式 Hub——spoke 据握手 ACK 识别 uplink）。
    pub fn with_role(mut self, role: crate::handshake::TopologyRole) -> Self {
        self.topology_role = role;
        self
    }

    pub fn with_topology(mut self, topology: crate::topology::TopologyConfig) -> Self {
        self.topology = topology;
        self
    }

    /// 声明可直拨地址（方案 A：spoke 有 listen 端口时设置——hub 会把它
    /// 以 ROUTE_HINT 注入给通信对端，促成 spoke 两两直连）。
    pub fn with_direct_addr(mut self, addr: impl Into<String>) -> Self {
        self.direct_addr = Some(addr.into());
        self
    }

    pub fn mem(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            capture: None,
            security: Default::default(),
            receptionist_acl: Default::default(),
            bind: None,
            seeds: Vec::new(),
            seed_endpoints: Vec::new(),
            scheme: "mem",
            callback_capacity: 65536,
            heartbeat_enabled: true,
            extra_caps: 0,
            topology_role: crate::handshake::TopologyRole::Normal,
            topology: Default::default(),
            direct_addr: None,
            knobs: None,
            swim: Default::default(),
        }
    }

    /// QUIC requires with_mtls or an explicit with_insecure_development choice.
    pub fn quic(node_id: impl Into<String>, bind: Option<SocketAddr>) -> Self {
        Self {
            node_id: node_id.into(),
            capture: None,
            security: Default::default(),
            receptionist_acl: Default::default(),
            bind,
            seeds: Vec::new(),
            seed_endpoints: Vec::new(),
            scheme: "quic",
            callback_capacity: 65536,
            heartbeat_enabled: true,
            extra_caps: 0,
            topology_role: crate::handshake::TopologyRole::Normal,
            topology: Default::default(),
            direct_addr: None,
            knobs: None,
            swim: Default::default(),
        }
    }

    pub fn with_seeds(mut self, seeds: Vec<NodeAddr>) -> Self {
        self.seeds = seeds;
        self.seed_endpoints.clear();
        self
    }

    /// Convert numeric or memory endpoints without performing DNS. Hostnames
    /// require `from_resolved_async`; no configured endpoint is silently dropped.
    pub fn from_resolved(r: &parrot_config::Resolved) -> Result<Self, parrot_config::ConfigError> {
        let mut cfg = Self::resolved_base(r)?;
        for (index, value) in r.remote.node.seeds.iter().enumerate() {
            let endpoint = Self::seed_endpoint(value, cfg.scheme, index)?;
            cfg.seeds.push(
                NodeAddr::from_endpoint(&endpoint).map_err(|reason| {
                    config_error(format!("remote.node.seeds[{index}]"), reason)
                })?,
            );
        }
        Ok(cfg)
    }

    /// Resolve all seed hostnames under one total deadline. Resolution is atomic:
    /// a failed name, empty DNS answer or timeout never returns a partial config.
    pub async fn from_resolved_async(
        r: &parrot_config::Resolved,
        timeout: Duration,
    ) -> Result<Self, parrot_config::ConfigError> {
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .filter(|_| !timeout.is_zero())
            .ok_or_else(|| {
                config_error(
                    "remote.node.seeds",
                    "DNS deadline must be positive and representable",
                )
            })?;
        let mut cfg = Self::resolved_base(r)?;
        let endpoints = r
            .remote
            .node
            .seeds
            .iter()
            .enumerate()
            .map(|(index, value)| Self::seed_endpoint(value, cfg.scheme, index))
            .collect::<Result<Vec<_>, _>>()?;
        if endpoints
            .iter()
            .any(|endpoint| endpoint.scheme != "mem" && endpoint.socket_addr().is_none())
        {
            cfg.seed_endpoints = endpoints.clone();
        }
        cfg.seeds = resolve_endpoints(endpoints, deadline, discovery::lookup_host).await?;
        Ok(cfg)
    }

    fn seed_endpoint(
        value: &str,
        scheme: &str,
        index: usize,
    ) -> Result<parrot_config::endpoint::SeedEndpoint, parrot_config::ConfigError> {
        let key = format!("remote.node.seeds[{index}]");
        let endpoint = parrot_config::endpoint::SeedEndpoint::parse(value, scheme)
            .map_err(|e| config_error(&key, e))?;
        if endpoint.scheme != scheme {
            return Err(config_error(
                &key,
                "seed transport must match the configured node transport",
            ));
        }
        Ok(endpoint)
    }

    fn resolved_base(r: &parrot_config::Resolved) -> Result<Self, parrot_config::ConfigError> {
        let scheme = parrot_config::endpoint::transport_scheme(&r.remote.node.scheme)
            .map_err(|e| config_error("remote.node.scheme", e))?;
        let node_id = r
            .remote
            .node
            .node_id
            .clone()
            .unwrap_or_else(|| "parrot-node".into());
        parrot_config::endpoint::validate_node_id(&node_id)
            .map_err(|e| config_error("remote.node.node_id", e))?;
        let bind = r
            .remote
            .node
            .bind
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|e| {
                config_error("remote.node.bind", format!("invalid socket address: {e}"))
            })?;
        let mut cfg = match scheme {
            "tcp" => Self::tcp(node_id, bind),
            "quic" => Self::quic(node_id, bind),
            "mem" => Self::mem(node_id),
            _ => unreachable!(),
        };
        r.remote.receptionist.validate()?;
        cfg.receptionist_acl = crate::acl::AclRules {
            roles: r.remote.receptionist.roles.clone().into_iter().collect(),
            default_deny: r.remote.receptionist.default_deny,
        };
        cfg.receptionist_acl
            .validate()
            .map_err(|error| config_error("remote.receptionist", error))?;
        cfg.security = match &r.remote.security {
            parrot_config::RemoteSecurityResolved::Local => crate::RemoteSecurity::Local,
            parrot_config::RemoteSecurityResolved::InsecureDevelopment => {
                crate::RemoteSecurity::InsecureDevelopment
            }
            parrot_config::RemoteSecurityResolved::MutualTls {
                cert_path,
                key_path,
                ca_path,
                admin_nodes,
            } => crate::RemoteSecurity::MutualTls {
                credentials: crate::tls::TlsConfig::from_paths(cert_path, key_path, ca_path)
                    .map_err(|e| config_error("remote.security", e.to_string()))?,
                admin_nodes: admin_nodes.iter().cloned().collect(),
            },
        };
        cfg.topology_role = match r.remote.node.topology_role {
            parrot_config::TopologyRoleValue::Normal => crate::handshake::TopologyRole::Normal,
            parrot_config::TopologyRoleValue::Hub => crate::handshake::TopologyRole::Hub,
            parrot_config::TopologyRoleValue::Border => crate::handshake::TopologyRole::Border,
            parrot_config::TopologyRoleValue::Directory => {
                crate::handshake::TopologyRole::Directory
            }
        };
        cfg.direct_addr = r.remote.node.direct_addr.clone();
        cfg.topology.mode = r.topology.mode;
        cfg.topology.relay_fallback = r.topology.relay_fallback;
        cfg.extra_caps = r.remote.extra_caps;
        cfg.knobs = Some(crate::transport::RuntimeKnobs {
            heartbeat_enabled: cfg.heartbeat_enabled,
            heartbeat_interval: Duration::from_millis(r.remote.transport.heartbeat_interval_ms),
            heartbeat_max_loss: r.remote.transport.heartbeat_max_loss,
            outbound_queue: r.remote.transport.outbound_queue,
            inbound_ask_capacity: r.remote.transport.inbound_ask_capacity,
            inbound_ask_bytes: r.remote.transport.inbound_ask_bytes,
            inbound_dispatch_capacity: r.remote.transport.inbound_dispatch_capacity,
            inbound_dispatch_per_connection: r.remote.transport.inbound_dispatch_per_connection,
            inbound_dispatch_bytes: r.remote.transport.inbound_dispatch_bytes,
            reorder_gap_timeout_ms: r.remote.reorder.gap_timeout_ms,
            reorder_buffer_cap: r.remote.reorder.buffer_cap,
            default_hop_limit: r.remote.transport.default_hop_limit,
        });
        cfg.validate()
            .map_err(|e| config_error("remote", e.to_string()))?;
        Ok(cfg)
    }

    fn membership_gossip_enabled(&self) -> bool {
        self.heartbeat_enabled
            && !(matches!(self.security, crate::security::RemoteSecurity::Local)
                && self.scheme != "mem")
    }

    fn member_address(
        &self,
        listener: Option<SocketAddr>,
    ) -> Result<crate::swim::NodeAddrWire, RemoteError> {
        let (host, port) = if self.scheme == "mem" {
            ("127.0.0.1".to_owned(), 0)
        } else if let Some(advertised) = &self.direct_addr {
            // DIRECT_ADDR is host:port, not a different scheme or node identity.
            if advertised.contains("://") || advertised.contains('@') {
                return Err(RemoteError::Transport(
                    "direct_addr must be host:port without a scheme or node identity".into(),
                ));
            }
            let endpoint = parrot_config::endpoint::SeedEndpoint::parse(advertised, self.scheme)
                .map_err(|error| RemoteError::Transport(format!("invalid direct_addr: {error}")))?;
            (endpoint.host.unwrap_or_default(), endpoint.port)
        } else if let Some(listener) =
            listener.filter(|address| !address.ip().is_unspecified() && address.port() != 0)
        {
            (listener.ip().to_string(), listener.port())
        } else {
            // Outbound-only and wildcard-bound nodes have no known dialable
            // endpoint without an explicit advertisement. Never publish a
            // connection's ephemeral source port as a listener.
            (String::new(), 0)
        };
        Ok(crate::swim::NodeAddrWire {
            node_id: self.node_id.clone(),
            scheme: self.scheme.into(),
            host,
            port,
        })
    }

    pub fn with_swim_config(mut self, config: crate::swim::SwimConfig) -> Self {
        self.swim = config;
        self
    }

    pub fn with_receptionist_acl(mut self, acl: crate::acl::AclRules) -> Self {
        self.receptionist_acl = acl;
        self
    }

    fn handshake(&self) -> HandshakeBody {
        HandshakeBody {
            node_id: self.node_id.clone(),
            capabilities: crate::handshake::caps::BIN
                | crate::handshake::caps::TRACE_CONTEXT
                | crate::handshake::caps::TELL_COMPLETION
                | crate::handshake::caps::DURABLE_DELIVERY
                | crate::handshake::caps::MEASURED_METRICS
                | crate::handshake::caps::MIRROR_CONTROL
                | crate::handshake::caps::MIRROR_ALLOCATION
                | crate::handshake::caps::MIRROR_SHADOW
                | if self.membership_gossip_enabled() {
                    crate::handshake::caps::MEMBERSHIP_GOSSIP
                        | crate::handshake::caps::MEMBERSHIP_PROBES
                        | crate::handshake::caps::RECEPTIONIST_SYNC
                        | crate::handshake::caps::RECEPTIONIST_NATIVE_PATHS
                } else {
                    0
                }
                | self.extra_caps,
            topology_role: self.topology_role,
            direct_addr: self.direct_addr.clone(),
            hop_limit: self.knobs.unwrap_or_default().default_hop_limit,
            ..Default::default()
        }
    }

    fn validate(&self) -> Result<(), RemoteError> {
        self.receptionist_acl
            .validate()
            .map_err(RemoteError::Transport)?;
        let invalid =
            |reason: String| RemoteError::Transport(format!("invalid remote config: {reason}"));
        parrot_config::endpoint::transport_scheme(self.scheme).map_err(invalid)?;
        parrot_config::endpoint::validate_node_id(&self.node_id).map_err(invalid)?;
        self.security
            .validate(&self.node_id, self.scheme, self.bind)?;
        self.member_address(None)?;
        for endpoint in &self.seed_endpoints {
            let encoded = if endpoint.scheme == "mem" {
                format!("mem://{}", endpoint.node_id)
            } else {
                let host = endpoint
                    .host
                    .as_deref()
                    .ok_or_else(|| invalid("network seed requires host".into()))?;
                let host = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.to_owned()
                };
                let identity = if endpoint.identity_is_explicit {
                    format!("{}@", endpoint.node_id)
                } else {
                    String::new()
                };
                format!("{}://{identity}{host}:{}", endpoint.scheme, endpoint.port)
            };
            parrot_config::endpoint::validate_node_id(&endpoint.node_id).map_err(invalid)?;
            let mut parsed = Self::seed_endpoint(&encoded, self.scheme, 0)
                .map_err(|error| invalid(error.to_string()))?;
            // Bare IPv6 spelling can normalize while its original alias remains.
            if !endpoint.identity_is_explicit {
                parsed.node_id = endpoint.node_id.clone();
            }
            if parsed != *endpoint {
                return Err(invalid("inconsistent original seed endpoint".into()));
            }
            if self.security.credentials().is_some() && !endpoint.identity_is_explicit {
                return Err(invalid("mTLS seed requires explicit node identity".into()));
            }
        }
        for seed in &self.seeds {
            self.validate_peer_address(seed)?;
        }
        if [self.swim.probe_interval, self.swim.gossip_interval]
            .iter()
            .any(|interval| std::time::Instant::now().checked_add(*interval).is_none())
        {
            return Err(invalid("membership interval exceeds clock bounds".into()));
        }
        if self.swim.indirect_probes > 32
            || self.swim.probe_timeout.is_zero()
            || self.swim.probe_timeout
                > Duration::from_millis(u64::from(crate::swim::probe::MAX_PROBE_MS))
        {
            return Err(invalid(
                "invalid membership probe budget or helper count".into(),
            ));
        }
        let k = self.knobs.unwrap_or_default();
        let max = tokio::sync::Semaphore::MAX_PERMITS.min(u32::MAX as usize);
        if k.inbound_ask_capacity == 0
            || k.inbound_ask_capacity > max
            || k.inbound_ask_bytes == 0
            || k.inbound_ask_bytes > max
        {
            return Err(invalid(
                "inbound ASK limits must be within 1..=min(u32::MAX, Semaphore::MAX_PERMITS)"
                    .into(),
            ));
        }
        for (name, limit) in [
            ("inbound_dispatch_capacity", k.inbound_dispatch_capacity),
            (
                "inbound_dispatch_per_connection",
                k.inbound_dispatch_per_connection,
            ),
            ("inbound_dispatch_bytes", k.inbound_dispatch_bytes),
        ] {
            if limit == 0 || limit > max {
                return Err(invalid(format!("{name} must be within 1..={max}")));
            }
        }
        if k.outbound_queue == 0 || k.outbound_queue > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(invalid(
                "outbound_queue exceeds channel capacity bounds".into(),
            ));
        }
        if k.heartbeat_interval.is_zero()
            || k.heartbeat_max_loss == 0
            || !(1..=64).contains(&k.default_hop_limit)
            || std::time::Instant::now()
                .checked_add(k.heartbeat_interval)
                .is_none()
            || k.heartbeat_interval
                .checked_mul(k.heartbeat_max_loss)
                .and_then(|duration| std::time::Instant::now().checked_add(duration))
                .is_none()
        {
            return Err(invalid(
                "invalid heartbeat deadline, loss count or hop limit".into(),
            ));
        }
        Ok(())
    }

    /// 配置切面：旋钮覆盖（代码层——最高优先级）。
    pub fn with_knobs(mut self, knobs: crate::transport::RuntimeKnobs) -> Self {
        self.knobs = Some(knobs);
        self
    }
}

/// 远程系统（一个节点的远程面全部状态）。
pub struct RemoteActorSystem {
    pub config: RemoteConfig,
    pub nodes: NodeTable,
    pub callbacks: Arc<CallbackRegistry>,
    pub ingress: Arc<Ingress>,
    seq_counters: Arc<std::sync::Mutex<std::collections::HashMap<String, u32>>>,
    /// K1/S1：SWIM 成员表（gossip 入站合并点——Arc 共享给钩子与驱动）。
    pub membership: Arc<tokio::sync::Mutex<crate::swim::Membership>>,
    pub(crate) receptionist: Arc<std::sync::Mutex<crate::receptionist::Receptionist>>,
    metrics: Arc<crate::metrics::TransportMetrics>,
    transport: Arc<dyn Transport>,
    inbound_tx: mpsc::Sender<(Frame, FrameSender, String)>,
    inbound_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<(Frame, FrameSender, String)>>>,
    /// 已建立连接（node_id → sender/status）
    links: std::sync::RwLock<Vec<(String, FrameSender, Arc<NodeStatus>)>>,
    /// 星型 uplink（07 §6）：hub/border 链路的 sender——目标不在直连表
    /// 时的默认路由出口。
    uplink: std::sync::RwLock<Option<FrameSender>>,
    /// 对端直拨地址表（方案 A：hub 注入 ROUTE_HINT 的数据源；握手
    /// DIRECT_ADDR 声明维护）。
    dial_addrs: std::sync::Mutex<std::collections::HashMap<String, String>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    started: tokio::sync::Mutex<bool>,
    background: crate::transport::ConnectionTasks,
    direct_connections: crate::transport::ConnectionTasks,
    link_gate: std::sync::Mutex<()>,
    discovery_gates: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>,
    >,
    discovery_slots: Arc<tokio::sync::Semaphore>,
    /// K0：admin 回执挂起表（req_id → oneshot）
    admin_pending: Arc<crate::admin::AdminPending>,
    /// B1（DEV_09）：admin-v2 回执挂起表 + req_id 计数 + 组件执行器槽。
    admin_pending_v2: Arc<crate::admin_v2::AdminPendingV2>,
    component_executor: ExecutorSlot,
    probes: Arc<crate::swim::probe::ProbeState>,
    probe_cursor: std::sync::atomic::AtomicUsize,
}

impl RemoteActorSystem {
    /// Record actual local handlers descended from incoming causal trace parents.
    /// The recorder is process-local and never serialized or shared with a peer.
    pub fn set_trace_recorder(&self, recorder: Option<parrot_api::trace::TraceRecorder>) {
        self.ingress.set_trace_recorder(recorder);
    }
    /// Bind a recorder until the returned attachment is dropped or replaced.
    /// Already imported requests retain their recorder. This does not create
    /// roots for untraced requests or change authentication and admission.
    pub fn attach_trace_recorder(
        &self,
        recorder: parrot_api::trace::TraceRecorder,
    ) -> crate::TraceRecorderAttachment {
        self.attach_trace_recorder_with_policy(recorder, crate::IncomingTracePolicy::PropagateOnly)
    }
    /// Bind a recorder and explicit policy for untraced incoming requests.
    /// Replacement and guard ownership match `attach_trace_recorder`. Automatic
    /// roots are per request, not per connection or application lifetime; they
    /// do not represent a recorded transport operation or bypass admission.
    pub fn attach_trace_recorder_with_policy(
        &self,
        recorder: parrot_api::trace::TraceRecorder,
        policy: crate::IncomingTracePolicy,
    ) -> crate::TraceRecorderAttachment {
        self.ingress.attach_trace_recorder(recorder, policy)
    }
    /// 组装（不建连——start 才 listen/connect）。mem 在构造时独占预留
    /// 进程内节点地址；重复身份返回错误，shutdown/Drop 后可重用。
    pub fn new(
        mut config: RemoteConfig,
        local: Arc<dyn LocalLookup>,
    ) -> Result<Arc<Self>, RemoteError> {
        let mut knobs = config.knobs.unwrap_or_default();
        // Either explicit disabling entry point wins. Store the effective
        // choice so every transport and connect_mem_pair uses the same value.
        knobs.heartbeat_enabled &= config.heartbeat_enabled;
        config.heartbeat_enabled = knobs.heartbeat_enabled;
        if config.knobs.is_some() || !knobs.heartbeat_enabled {
            config.knobs = Some(knobs);
        }
        config.validate()?;
        let callbacks = Arc::new(CallbackRegistry::new(config.callback_capacity));
        let ingress = Arc::new(
            Ingress::with_ask_limits(
                local,
                callbacks.clone(),
                knobs.inbound_ask_capacity,
                knobs.inbound_ask_bytes,
            )?
            .with_control_policy(config.security.control_policy()),
        );
        // 配置切面：重排旋钮下发（None=默认 250ms/1024，等价迁移前行为）
        if let Some(k) = &config.knobs {
            ingress.set_reorder_knobs(
                Duration::from_millis(k.reorder_gap_timeout_ms),
                k.reorder_buffer_cap,
            );
        }
        let (in_tx, in_rx) = mpsc::channel::<(Frame, FrameSender, String)>(1024);
        let handshake = config.handshake();
        let admin_pending = Arc::new(crate::admin::AdminPending::new(config.callback_capacity));
        let admin_pending_v2 = Arc::new(crate::admin_v2::AdminPendingV2::new(
            config.callback_capacity,
        ));
        let dis = admin_disconnect_handler(&ingress, &admin_pending, &admin_pending_v2);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        // K1/S1：成员表挂系统（钩子合并 + 驱动读取同一实例）
        let membership = Arc::new(tokio::sync::Mutex::new(crate::swim::Membership::new(0)));
        let metrics = Arc::new(crate::metrics::TransportMetrics::default());
        let transport: Box<dyn Transport> = match config.scheme {
            "mem" => Box::new(
                crate::transport::memory::MemoryTransport::new(
                    handshake,
                    in_tx.clone(),
                    dis,
                    shutdown_tx.clone(),
                    config.knobs,
                )?
                .with_capture(config.capture.clone())
                .with_metrics(metrics.clone()),
            ),
            "quic" => {
                let bind = config
                    .bind
                    .unwrap_or_else(|| "127.0.0.1:0".parse().unwrap());
                Box::new(
                    crate::transport::quic::QuicTransport::with_security(
                        handshake,
                        in_tx.clone(),
                        dis,
                        shutdown_tx.clone(),
                        bind,
                        &config.security,
                        config.knobs,
                    )?
                    .with_capture(config.capture.clone())
                    .with_metrics(metrics.clone()),
                )
            }
            "tcp" => Box::new(
                crate::transport::tcp::TcpTransport::new(
                    handshake,
                    in_tx.clone(),
                    dis,
                    shutdown_tx.clone(),
                    config.knobs,
                )
                .with_security(config.security.clone())?
                .with_capture(config.capture.clone())
                .with_metrics(metrics.clone()),
            ),
            _ => unreachable!("validated transport"),
        };
        let nodes = NodeTable::new();
        if config.seed_endpoints.is_empty() {
            for seed in &config.seeds {
                nodes.add_seed(seed.clone());
            }
        } else {
            for endpoint in &config.seed_endpoints {
                if endpoint.scheme == "mem" || endpoint.socket_addr().is_some() {
                    nodes.add_seed(
                        NodeAddr::from_endpoint(endpoint).map_err(RemoteError::Transport)?,
                    );
                } else {
                    nodes.add_dns_seed(crate::node::NodeEndpoint::Dns {
                        node_id: endpoint.node_id.clone(),
                        scheme: endpoint.scheme.into(),
                        host: endpoint.host.clone().unwrap_or_default(),
                        port: endpoint.port,
                    });
                }
            }
        }
        let system = Arc::new(Self {
            config,
            nodes,
            callbacks,
            ingress,
            seq_counters: Default::default(),
            membership,
            receptionist: Default::default(),
            transport: Arc::from(transport),
            inbound_tx: in_tx,
            inbound_rx: Arc::new(tokio::sync::Mutex::new(in_rx)),
            links: std::sync::RwLock::new(Vec::new()),
            uplink: std::sync::RwLock::new(None),
            dial_addrs: std::sync::Mutex::new(Default::default()),
            shutdown_tx,
            started: tokio::sync::Mutex::new(false),
            background: Default::default(),
            direct_connections: crate::transport::ConnectionTasks::with_metrics(metrics.clone()),
            metrics,
            link_gate: Default::default(),
            discovery_gates: Default::default(),
            discovery_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            admin_pending,
            admin_pending_v2,
            component_executor: Arc::new(std::sync::RwLock::new(None)),
            probes: Arc::new(crate::swim::probe::ProbeState::default()),
            probe_cursor: std::sync::atomic::AtomicUsize::new(0),
        });
        system.ingress.attach_mirror_system(Arc::downgrade(&system));
        Ok(system)
    }

    /// 实际监听地址（bind :0 → 真实端口；未监听 None）。
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.transport.local_addr()
    }

    /// 启动：listen（若配置）+ 入站分发循环 + 连接种子。
    pub async fn start(self: &Arc<Self>) -> Result<(), RemoteError> {
        let mut started = self.started.lock().await;
        if *self.shutdown_tx.borrow() {
            return Err(RemoteError::Transport("system is shut down".into()));
        }
        if *started {
            return Ok(());
        }
        // Bind before spawning tasks: failed startup must not leave background owners.
        if let Some(bind) = self.config.bind {
            self.transport.listen(bind).await?;
        }
        if *self.shutdown_tx.borrow() {
            return Err(RemoteError::Transport(
                "system shut down during startup".into(),
            ));
        }
        if self.config.membership_gossip_enabled() {
            let mut shutdown = self.shutdown_tx.subscribe();
            let mut membership = tokio::select! {
                biased;
                _ = shutdown.wait_for(|closed| *closed) => return Err(crate::transport::ConnectionTasks::error()),
                membership = self.membership.lock() => membership,
            };
            membership.publish_local(self.config.member_address(self.local_addr())?);
        }
        *started = true;
        self.install_admin_hook();
        self.start_receptionist();
        // hub 转发出口注入（07 §6 两两互通：A→hub→B 中转；Weak 防循环引用）
        *self.ingress.relay_slot() = Some(Arc::new(SystemRelay {
            system: Arc::downgrade(self),
            self_node: self.config.node_id.clone(),
        }));
        // Membership probes require correlated replies. They share the owned
        // driver lifetime and use authorized helper links after direct failure.
        let (gossip_tx, mut gossip_rx) = tokio::sync::mpsc::channel(64);
        let swim = Arc::new(crate::swim::Swim {
            config: self.config.swim.clone(),
            membership: self.membership.clone(),
            self_node: self.config.node_id.clone(),
        });
        let probe_weak = Arc::downgrade(self);
        let probe = Box::new(move |node: String| -> crate::swim::BoxedProbeFut {
            let weak = probe_weak.clone();
            Box::pin(async move {
                probes::observe(weak, node).await != crate::swim::ProbeOutcome::Failed
            })
        });
        if self.config.heartbeat_enabled {
            self.spawn_background(swim.clone().run_observer(
                probe,
                gossip_tx,
                self.shutdown_tx.subscribe(),
            ));
            let gossip_weak = Arc::downgrade(self);
            self.spawn_background(async move {
                while let Some((gossip, targets)) = gossip_rx.recv().await {
                    let Some(system) = gossip_weak.upgrade() else {
                        break;
                    };
                    let payload = crate::swim::encode_gossip(&gossip);
                    for target in targets {
                        let Some(sender) = system.direct_sender_of(&target) else {
                            continue;
                        };
                        if !system
                            .config
                            .security
                            .control_policy()
                            .allows_cluster(&sender)
                            || sender.peer_capabilities()
                                & crate::handshake::caps::MEMBERSHIP_GOSSIP
                                == 0
                        {
                            continue;
                        }
                        let frame = Frame {
                            header: crate::frame::FrameHeader {
                                frame_len: 0,
                                version: crate::frame::PROTOCOL_VERSION,
                                frame_type: crate::frame::frame_type::SYSTEM_EVENT,
                                flags: 0,
                                correlation_id: 0,
                                hop_count: 0,
                                hop_limit: system
                                    .config
                                    .knobs
                                    .unwrap_or_default()
                                    .default_hop_limit,
                                seq: crate::frame::SEQ_NONE,
                            },
                            path: String::new(),
                            type_key: String::new(),
                            payload: payload.clone(),
                        };
                        let _ = sender.send_frame_sync(frame);
                    }
                }
            });
        }
        if self.config.membership_gossip_enabled() {
            let weak = Arc::downgrade(self);
            let membership = self.membership.clone();
            self.spawn_background(async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(100));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    // Never retain the system while waiting on a public mutex.
                    let routes: Vec<_> = membership
                        .lock()
                        .await
                        .members
                        .values()
                        .filter_map(crate::swim::member_route)
                        .collect();
                    let Some(system) = weak.upgrade() else {
                        break;
                    };
                    let routes = routes
                        .into_iter()
                        .filter(|route| {
                            route.node_id() != system.config.node_id
                                && route.scheme() == system.config.scheme
                                && route.address().is_none_or(|address| {
                                    system.config.validate_peer_address(address).is_ok()
                                })
                        })
                        .collect();
                    system.nodes.reconcile_membership(routes);
                }
            });
        }
        // Slow operations are serialized within each physical connection,
        // under shared limits; replies and ASK admission remain independent.
        let ingress = self.ingress.clone();
        let inbound = self.inbound_rx.clone();
        let knobs = self.config.knobs.unwrap_or_default();
        self.spawn_background(async move {
            let mut rx = inbound.lock().await;
            dispatch::run(ingress, &mut rx, knobs).await;
        });
        let table = self.ingress.relay_table.clone();
        let mut shutdown = self.shutdown_tx.subscribe();
        self.spawn_background(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    _ = tick.tick() => {
                        for (cid, back) in table.expire() {
                            back.send_reply(Frame::reply_err(cid, "", crate::error::ErrCode::Timeout, "relay expired"));
                        }
                    }
                }
            }
        });
        if self.config.bind.is_some() || self.config.scheme == "mem" || self.config.scheme == "quic"
        {
            let weak = Arc::downgrade(self);
            let transport = self.transport.clone();
            self.spawn_background(async move {
                loop {
                    match transport.accept().await {
                        Ok(conn) => {
                            let Some(this) = weak.upgrade() else {
                                conn.sender.close();
                                return;
                            };
                            let _ = this.register_link(conn);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "remote accept failed");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            });
        }
        if !self.config.seeds.is_empty() || !self.config.seed_endpoints.is_empty() {
            let weak = Arc::downgrade(self);
            self.spawn_background(seeds::run(weak, seeds::groups(&self.config)?));
        }
        Ok(())
    }

    fn spawn_background(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut shutdown = self.shutdown_tx.subscribe();
        let _ = self.background.spawn(async move {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|closed| *closed) => {},
                _ = future => {},
            }
        });
    }

    /// 测试/嵌入便利：两系统经 duplex 直连（mem 载体的显式形态——
    /// 等价 connect+accept 全时序，返回各自视角无需端点表）。
    pub async fn connect_mem_pair(
        self: &Arc<Self>,
        peer: &Arc<RemoteActorSystem>,
    ) -> Result<(), RemoteError> {
        if self.config.scheme != "mem" || peer.config.scheme != "mem" {
            return Err(RemoteError::Transport(
                "connect_mem_pair requires two memory systems".into(),
            ));
        }
        let (client, server) = tokio::io::duplex(64 * 1024);
        let build = |system: &Arc<Self>, side| crate::transport::ConnParams {
            side,
            local_addr: None,
            peer_addr: None,
            scheme: "mem",
            local_handshake: system.config.handshake(),
            inbound: system.inbound_tx.clone(),
            on_disconnect: admin_disconnect_handler(
                &system.ingress,
                &system.admin_pending,
                &system.admin_pending_v2,
            ),
            shutdown: system.shutdown_tx.subscribe(),
            knobs: system.config.knobs,
        };
        // Both handshakes remain scoped to this caller; each resulting driver
        // belongs to its system even if the caller is cancelled before publish.
        let mine = self
            .direct_connections
            .run(crate::transport::run_connection_captured(
                client,
                build(self, crate::transport::ConnSide::Connect),
                Some(&self.direct_connections),
                crate::security::PeerTrust::InProcess.expecting(&peer.config.node_id),
                self.config.capture.as_ref(),
            ));
        let theirs = peer
            .direct_connections
            .run(crate::transport::run_connection_captured(
                server,
                build(peer, crate::transport::ConnSide::Accept),
                Some(&peer.direct_connections),
                crate::security::PeerTrust::InProcess,
                peer.config.capture.as_ref(),
            ));
        let (mine, theirs) = tokio::try_join!(mine, theirs)?;
        let mine_sender = mine.sender.clone();
        let peer_sender = theirs.sender.clone();
        if let Err(error) = self.register_link(mine) {
            peer_sender.close();
            return Err(error);
        }
        if let Err(error) = peer.register_link(theirs) {
            mine_sender.close();
            return Err(error);
        }
        Ok(())
    }

    /// 主动连接种子（start 后调用；RC7 断连重连同入口）。
    pub async fn connect(self: &Arc<Self>, addr: &NodeAddr) -> Result<(), RemoteError> {
        Self::connect_weak(&Arc::downgrade(self), addr).await
    }

    // Background dial futures retain the transport, never the enclosing system.
    async fn connect_weak(
        weak: &std::sync::Weak<Self>,
        addr: &NodeAddr,
    ) -> Result<(), RemoteError> {
        Self::connect_weak_identity(weak, addr, true)
            .await
            .map(|_| ())
    }

    async fn connect_weak_identity(
        weak: &std::sync::Weak<Self>,
        addr: &NodeAddr,
        retain_numeric_seed: bool,
    ) -> Result<String, RemoteError> {
        let (transport, mut shutdown) = {
            let this = weak
                .upgrade()
                .ok_or_else(crate::transport::ConnectionTasks::error)?;
            if *this.shutdown_tx.borrow() {
                return Err(crate::transport::ConnectionTasks::error());
            }
            this.config.validate_peer_address(addr)?;
            (this.transport.clone(), this.shutdown_tx.subscribe())
        };
        let conn = tokio::select! {
            biased;
            _ = shutdown.wait_for(|value| *value) => return Err(crate::transport::ConnectionTasks::error()),
            conn = transport.connect(addr) => conn?,
        };
        let Some(this) = weak.upgrade() else {
            conn.sender.close();
            return Err(crate::transport::ConnectionTasks::error());
        };
        let identity = conn.node_id.clone();
        this.register_link_at(conn, retain_numeric_seed.then_some(addr))?;
        Ok(identity)
    }

    fn register_link(self: &Arc<Self>, conn: ConnectionHandle) -> Result<(), RemoteError> {
        self.register_link_at(conn, None)
    }

    fn register_link_at(
        self: &Arc<Self>,
        conn: ConnectionHandle,
        seed: Option<&NodeAddr>,
    ) -> Result<(), RemoteError> {
        let _gate = self.link_gate.lock().unwrap();
        if *self.shutdown_tx.borrow() {
            conn.sender.close();
            return Err(crate::transport::ConnectionTasks::error());
        }
        if let Some(addr) = seed {
            self.nodes.add_seed(addr.clone());
        }
        let status = Arc::new(NodeStatus::default());
        status.set(NodeState::Connected);
        self.links.write().unwrap().push((
            conn.node_id.clone(),
            conn.sender.clone(),
            status.clone(),
        ));
        // 星型 uplink 记录（07 §6）：对端角色是 hub/border → 此链路可做
        // 默认路由（目标不在直连表时经它中转）。多个 hub 取最新（hub
        // 主备是 P5 拓扑管理的职责）。
        if self
            .config
            .security
            .control_policy()
            .allows_cluster(&conn.sender)
            && matches!(
                conn.peer_role,
                crate::handshake::TopologyRole::Hub | crate::handshake::TopologyRole::Border
            )
        {
            let mut g = self.uplink.write().unwrap();
            *g = Some(conn.sender.clone());
            tracing::info!(node = %conn.node_id, role = ?conn.peer_role, "uplink set");
        }
        // 方案 A：对端声明的直拨地址入表（hub 中转时作为 ROUTE_HINT 数据源）
        if let Some(d) = &conn.peer_dial_addr {
            self.dial_addrs
                .lock()
                .unwrap()
                .insert(conn.node_id.clone(), d.clone());
        }
        // NodeTable 同步（remote_ref require 校验通过）
        self.nodes.connected(NodeAddr {
            node_id: conn.node_id.clone(),
            identity_is_explicit: true,
            scheme: conn.info.scheme.to_string(),
            addr: conn
                .info
                .peer
                .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap()),
        });
        if let Some((_, node_status)) = self.nodes.get(&conn.node_id) {
            node_status.set(NodeState::Connected);
        }
        // Registration is synchronous, but membership may be locked by the
        // gossip driver. Queue an owned update rather than silently skipping
        // it on try_lock failure. Recheck the physical link after acquiring
        // the membership lock so a delayed update cannot resurrect a closed link.
        let membership = self.membership.clone();
        let weak = Arc::downgrade(self);
        let registered_sender = conn.sender.clone();
        let provisional_address = if conn.info.scheme == "mem" {
            ("127.0.0.1".to_owned(), 0)
        } else if let Some(seed) = seed {
            (seed.addr.ip().to_string(), seed.addr.port())
        } else {
            (String::new(), 0)
        };
        let member = crate::swim::Member {
            node_id: conn.node_id.clone(),
            addr: crate::swim::NodeAddrWire {
                node_id: conn.node_id.clone(),
                scheme: conn.info.scheme.to_string(),
                host: provisional_address.0,
                port: provisional_address.1,
            },
            status: crate::swim::MemberStatus::Alive,
            incarnation: 0,
            status_until_ms: 0,
            metadata: bytes::Bytes::new(),
        };
        self.spawn_background(async move {
            let mut membership = membership.lock().await;
            let Some(system) = weak.upgrade() else {
                return;
            };
            if !registered_sender.is_closed() && system.direct_sender_of(&member.node_id).is_some()
            {
                membership.merge_event(&crate::swim::MemberEvent::Upsert(member));
            }
        });
        // 关闭信号 → 移除 link + Disconnected（回调可能早于 NodeTable 填充——容忍）
        let mut closed = conn.closed;
        let node_id = conn.node_id.clone();
        let weak = Arc::downgrade(self);
        let closed_sender = conn.sender.clone();
        self.spawn_background(async move {
            let _ = (&mut closed).await;
            tracing::debug!(node = %node_id, "connection closed (register_link watcher)");
            status.set(NodeState::Disconnected);
            let Some(this) = weak.upgrade() else {
                return;
            };
            {
                let _gate = this.link_gate.lock().unwrap();
                let has_replacement = {
                    let mut links = this.links.write().unwrap();
                    links.retain(|(_, sender, _)| !sender.tx.same_channel(&closed_sender.tx));
                    links.iter().any(|(node, _, _)| node == &node_id)
                };
                // An old connection closing must not remove a newer connection
                // or mark its node disconnected.
                if has_replacement {
                    return;
                }
                // 直拨地址随链路失效清除（节点重启换端口后由新握手重新声明）
                this.dial_addrs.lock().unwrap().remove(&node_id);
                // uplink 失效清理（该 hub 断连——读侧 try_send 失败自然兜底）
                let mut g = this.uplink.write().unwrap();
                if g.as_ref()
                    .map(|s| s.tx.same_channel(&closed_sender.tx))
                    .unwrap_or(false)
                {
                    *g = None;
                    tracing::info!(node = %node_id, "uplink cleared");
                }
                this.nodes.disconnected(&node_id);
            }
            let membership = this.membership.clone();
            let timeout = this.config.swim.suspect_timeout;
            let can_probe = this.config.membership_gossip_enabled()
                && closed_sender.peer_capabilities() & crate::handshake::caps::MEMBERSHIP_PROBES
                    != 0;
            drop(this);
            let observed = membership.lock().await.members.get(&node_id).cloned();
            // Losing one path is not node failure when another member can get
            // a correlated response from the target. No Arc<System> spans IO.
            if can_probe
                && probes::observe(weak.clone(), node_id.clone()).await
                    == crate::swim::ProbeOutcome::Reachable
            {
                return;
            }
            let mut membership = membership.lock().await;
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.direct_sender_of(&node_id).is_none()
                && membership.members.get(&node_id) == observed.as_ref()
            {
                membership.mark_suspect_with_timeout(&node_id, timeout);
            }
        });
        Ok(())
    }

    /// Observe actual direct/indirect membership replies without changing membership.
    /// Unavailable is not evidence of either reachability or failure.
    pub async fn probe_member(self: &Arc<Self>, node: &str) -> crate::swim::ProbeOutcome {
        probes::observe(Arc::downgrade(self), node.to_owned()).await
    }

    /// 已建链路快照（测试/诊断观测点——node_id + sender + 状态）。
    pub async fn links_snapshot(&self) -> Vec<(String, FrameSender, Arc<NodeStatus>)> {
        self.links.read().unwrap().clone()
    }

    /// Required identities without a currently open, handshaken direct link.
    /// Seed/discovery addresses and uplink routes do not satisfy registration.
    /// This is a local observation, not an application health check or a lease:
    /// a connection may close immediately after the check. No dialing occurs.
    pub fn missing_direct_connections<'a>(&self, required: &[&'a str]) -> Vec<&'a str> {
        if *self.shutdown_tx.borrow() {
            return required.to_vec();
        }
        let links = self.links.read().unwrap();
        required
            .iter()
            .copied()
            .filter(|node| {
                !links
                    .iter()
                    .any(|(id, sender, _)| id == node && !sender.is_closed())
            })
            .collect()
    }

    /// Install a bounded Wire ingress policy. Shadow lookup must return raw local refs.
    pub fn register_wire_mirror(
        &self,
        policy: crate::mirror::WireMirrorPolicy,
        options: crate::mirror::WireMirrorOptions,
    ) -> Result<crate::mirror::WireMirrorHandle, crate::mirror::WireMirrorError> {
        self.ingress.register_wire_mirror(policy, options)
    }

    /// Atomically install an owned policy group; existing sources are never replaced.
    pub fn register_wire_mirror_group(
        &self,
        policies: Vec<(
            crate::mirror::WireMirrorPolicy,
            crate::mirror::WireMirrorOptions,
        )>,
    ) -> Result<crate::mirror::WireMirrorGroup, crate::mirror::WireMirrorError> {
        self.ingress.register_wire_mirror_group(policies)
    }

    pub fn remove_wire_mirror(&self, prefix: &str) -> Option<crate::mirror::WireMirrorHandle> {
        self.ingress.remove_wire_mirror(prefix)
    }

    /// 解析远程路径拿 ref（校验 parrot:// 前缀 + node 在表）。
    /// 星型放宽（07 §6）：node 不在直连表但有 uplink hub → 仍可建 ref
    /// （默认路由——帧先发 hub，由其中转到目标）。
    /// 方案 A：learned 直连地址存在且链路已断 → 触发重拨（hint 数据源
    /// 是 hub，地址新鲜度可信；拨号异步，本次仍走 uplink 兜底）。
    pub(crate) fn shadow_ref(self: &Arc<Self>, path: &str) -> Result<RemoteActorRef, RemoteError> {
        let (node, _) = crate::mirror::remote_shadow(path)
            .ok_or_else(|| RemoteError::Transport("invalid remote shadow path".into()))?;
        // No eager background dial and no uplink fallback. The bounded mirror
        // worker owns direct discovery/dialing and its cancellation deadline.
        let inner = Arc::new(RemoteInner {
            nodes: Vec::new(),
            system: Some(Arc::downgrade(self)),
            callbacks: self.callbacks.clone(),
            self_node: self.config.node_id.clone(),
            uplink: None,
            seq_counters: self.seq_counters.clone(),
        });
        Ok(RemoteActorRef::new(path, node, inner))
    }

    /// Restore this node's exact engine-local path from a canonical discovery
    /// address. Foreign, malformed and closed-system paths do not resolve.
    pub fn local_actor_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        if *self.shutdown_tx.borrow() {
            return None;
        }
        let (node, local) = crate::actor_path::split(path)?;
        (node == self.config.node_id).then_some(local)
    }

    pub fn remote_ref(self: &Arc<Self>, path: &str) -> Result<RemoteActorRef, RemoteError> {
        let node = crate::node::node_of_path(path)
            .ok_or_else(|| RemoteError::Transport(format!("not a parrot:// path: {path}")))?;
        let has_uplink = self.relay_sender().is_some()
            && self.config.topology.mode != crate::topology::TopologyMode::Mesh
            && (self.config.topology.mode == crate::topology::TopologyMode::Hub
                || self.config.topology.relay_fallback);
        let in_links = self
            .links
            .read()
            .unwrap()
            .iter()
            .any(|(n, sender, _)| n == node && !sender.is_closed());
        if !self.nodes.contains(node) && !in_links && !has_uplink {
            return Err(RemoteError::UnknownNode(node.to_string()));
        }
        // 方案 A 重拨：有学习地址 + 链路缺失 → spawn 直拨（不阻塞 ref 构建）
        if !in_links && self.may_dial_business() {
            if let Some(addr) = self.ingress.learned.lock().unwrap().get(node).cloned() {
                if let Ok(sa) = addr.parse::<std::net::SocketAddr>() {
                    let weak = Arc::downgrade(self);
                    let node_s = node.to_string();
                    self.spawn_background(async move {
                        let na = crate::node::NodeAddr::tcp(node_s.clone(), sa);
                        if let Err(e) = Self::connect_weak(&weak, &na).await {
                            tracing::debug!(node = %node_s, %addr, err = %e, "re-dial direct link failed");
                        }
                    });
                }
            }
        }
        let inner = Arc::new(RemoteInner {
            nodes: Vec::new(),
            system: Some(Arc::downgrade(self)),
            callbacks: self.callbacks.clone(),
            self_node: self.config.node_id.clone(),
            uplink: self.uplink.read().ok().and_then(|g| g.clone()),
            seq_counters: self.seq_counters.clone(),
        });
        Ok(RemoteActorRef::new(path, node, inner))
    }

    fn direct_sender_of(&self, node: &str) -> Option<FrameSender> {
        self.links
            .read()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, sender, _)| id == node && !sender.is_closed())
            .map(|(_, sender, _)| sender.clone())
    }

    pub(crate) fn sender_of(&self, node: &str) -> Option<FrameSender> {
        if *self.shutdown_tx.borrow() {
            return None;
        }
        use crate::topology::TopologyMode;
        let fallback = self.config.topology.relay_fallback;
        match self.config.topology.mode {
            TopologyMode::Hub if self.is_relay_endpoint() => {
                self.direct_sender_of(node).or_else(|| self.relay_sender())
            }
            TopologyMode::Hub => self
                .relay_sender()
                .or_else(|| fallback.then(|| self.direct_sender_of(node)).flatten()),
            TopologyMode::Mesh => self.direct_sender_of(node),
            TopologyMode::Hybrid => self
                .direct_sender_of(node)
                .or_else(|| fallback.then(|| self.relay_sender()).flatten()),
        }
    }

    fn relay_sender(&self) -> Option<FrameSender> {
        self.uplink
            .read()
            .unwrap()
            .clone()
            .filter(|s| !s.is_closed())
    }

    fn may_dial_business(&self) -> bool {
        self.is_relay_endpoint()
            || self.config.topology.mode != crate::topology::TopologyMode::Hub
            || (self.config.topology.relay_fallback && self.relay_sender().is_none())
    }

    fn is_relay_endpoint(&self) -> bool {
        matches!(
            self.config.topology_role,
            crate::handshake::TopologyRole::Hub | crate::handshake::TopologyRole::Border
        )
    }

    /// Resolve a discovered endpoint only when a message needs it.
    /// The caller owns the wait, so request cancellation also cancels dialing.
    /// Gates coalesce concurrent callers and slots bound physical dial work.
    pub(crate) async fn sender_for(
        weak: &std::sync::Weak<Self>,
        node: &str,
    ) -> Option<FrameSender> {
        Self::resolve_sender(weak, node, true).await
    }

    // Management frames carry no relay destination. They share discovery
    // admission with business calls but must resolve a direct physical peer.
    pub(crate) async fn resolve_sender(
        weak: &std::sync::Weak<Self>,
        node: &str,
        allow_uplink: bool,
    ) -> Option<FrameSender> {
        let existing = |system: &Self| {
            if *system.shutdown_tx.borrow() {
                None
            } else {
                system.direct_sender_of(node)
            }
        };
        let (gate, slots, mut shutdown) = {
            let system = weak.upgrade()?;
            if *system.shutdown_tx.borrow() {
                return None;
            }
            if allow_uplink
                && system.config.topology.mode == crate::topology::TopologyMode::Hub
                && !system.is_relay_endpoint()
            {
                if let Some(sender) = system.relay_sender() {
                    return Some(sender);
                }
                if !system.config.topology.relay_fallback {
                    return None;
                }
            }
            if let Some(sender) = existing(&system) {
                return Some(sender);
            }
            if system.nodes.discovered(node).is_none() {
                return allow_uplink.then(|| system.sender_of(node)).flatten();
            }
            let mut gates = system.discovery_gates.lock().unwrap();
            gates.retain(|_, gate| gate.strong_count() > 0);
            let gate = gates
                .get(node)
                .and_then(std::sync::Weak::upgrade)
                .unwrap_or_else(|| {
                    let gate = Arc::new(tokio::sync::Mutex::new(()));
                    gates.insert(node.to_owned(), Arc::downgrade(&gate));
                    gate
                });
            (
                gate,
                system.discovery_slots.clone(),
                system.shutdown_tx.subscribe(),
            )
        };
        let dial = async {
            let _gate = gate.lock_owned().await;
            let (route, transport, config) = {
                let system = weak.upgrade()?;
                if let Some(sender) = existing(&system) {
                    return Some(sender);
                }
                let route = system.nodes.discovered(node)?;
                (route, system.transport.clone(), system.config.clone())
            };
            let _slot = slots.acquire_owned().await.ok()?;
            if weak.upgrade()?.nodes.discovered(node).as_ref() != Some(&route) {
                return None;
            }
            let addresses = discovery::addresses(&route).await.ok()?;
            for address in addresses {
                if address.addr.ip().is_unspecified() && address.scheme != "mem"
                    || config.validate_peer_address(&address).is_err()
                {
                    continue;
                }
                if weak.upgrade()?.nodes.discovered(node).as_ref() != Some(&route) {
                    return None;
                }
                // A silent first DNS address must not consume the whole shared
                // five-second deadline. Numeric routes retain their prior budget.
                let result = if route.address().is_some() {
                    transport.connect(&address).await
                } else {
                    match tokio::time::timeout(Duration::from_secs(1), transport.connect(&address))
                        .await
                    {
                        Ok(result) => result,
                        Err(_) => continue,
                    }
                };
                let connection = match result {
                    Ok(connection) => connection,
                    Err(_) => continue,
                };
                let sender = connection.sender.clone();
                let Some(system) = weak.upgrade() else {
                    sender.close();
                    return None;
                };
                if system.nodes.discovered(node).as_ref() != Some(&route) {
                    sender.close();
                    return None;
                }
                // Discovery must not promote this temporary route to a static seed.
                system.register_link_at(connection, None).ok()?;
                return Some(sender);
            }
            None
        };
        let resolved = tokio::select! {
            biased;
            _ = shutdown.wait_for(|closed| *closed) => None,
            result = tokio::time::timeout(Duration::from_secs(5), dial) => result.ok().flatten(),
        };
        if allow_uplink {
            // Re-read policy and connection liveness after an awaited dial.
            // Cancellation drops this future before any fallback is attempted.
            weak.upgrade()?.sender_of(node)
        } else {
            resolved
        }
    }

    /// facade 三级路由的远程出口实现（parrot::RemoteGateway 注入面）。
    pub fn gateway(self: &Arc<Self>) -> RemoteGatewayImpl {
        RemoteGatewayImpl(self.clone())
    }

    // ---------------- K0 管理协议（DEV_02 §0.3） ----------------

    /// admin 钩子：入站 AdminCommand → 本地执行；AdminReply → 挂起表完成。
    fn install_admin_hook(self: &Arc<Self>) {
        let local = self.ingress.local.clone();
        let pending = self.admin_pending.clone();
        let pending_v2 = self.admin_pending_v2.clone();
        let executor = self.component_executor.clone();
        let membership = self.membership.clone();
        let self_node = self.config.node_id.clone();
        let hook = AdminHook {
            metrics: self.metrics.clone(),
            local,
            pending,
            pending_v2,
            executor,
            membership,
            self_node,
            control_policy: self.config.security.control_policy(),
            system: Arc::downgrade(self),
        };
        // Ingress.sys_event 是 immutable 字段——经 interior mutability 换入
        let ingress = self.ingress.clone();
        *ingress.sys_event_slot() = Some(Arc::new(hook));
    }

    /// 按名在指定节点 spawn（集群内指定节点形态；P4 自动选址只是路由层加
    /// 哈希环，本 API 签名不变——DEV_02 §0.3）。
    pub async fn spawn_named(
        self: &Arc<Self>,
        node: &str,
        path: &str,
        props: &str,
    ) -> Result<RemoteActorRef, RemoteError> {
        let reply = self
            .admin_roundtrip(
                node,
                crate::admin::AdminCommand::SpawnLocal {
                    req_id: 0, // admin_roundtrip 回填
                    props: props.into(),
                    path: path.into(),
                    reply_to: format!("parrot://{}/_admin", self.config.node_id),
                },
            )
            .await?;
        match reply {
            crate::admin::AdminReply::Spawned { path, .. } => {
                let full = format!("parrot://{node}{path}");
                self.remote_ref(&full)
            }
            crate::admin::AdminReply::Failed { code, detail, .. } => Err(RemoteError::Transport(
                format!("spawn failed: {code} {detail}"),
            )),
            other => Err(RemoteError::Transport(format!(
                "unexpected admin reply: {other:?}"
            ))),
        }
    }

    /// 管理性 stop（带回执）。
    pub async fn admin_stop(self: &Arc<Self>, node: &str, path: &str) -> Result<(), RemoteError> {
        let reply = self
            .admin_roundtrip(
                node,
                crate::admin::AdminCommand::AdminStop {
                    req_id: 0,
                    target_path: path.into(),
                    reply_to: format!("parrot://{}/_admin", self.config.node_id),
                },
            )
            .await?;
        match reply {
            crate::admin::AdminReply::Stopped { .. } => Ok(()),
            crate::admin::AdminReply::Failed { code, detail, .. } => Err(RemoteError::Transport(
                format!("admin stop failed: {code} {detail}"),
            )),
            other => Err(RemoteError::Transport(format!(
                "unexpected admin reply: {other:?}"
            ))),
        }
    }

    /// admin 命令往返：req_id 分配 → SYSTEM_EVENT 发送 → 挂起等回执。
    async fn admin_roundtrip(
        self: &Arc<Self>,
        node: &str,
        mut cmd: crate::admin::AdminCommand,
    ) -> Result<crate::admin::AdminReply, RemoteError> {
        let req_id = self.callbacks.next_cid();
        cmd.set_req_id(req_id);
        self.admin_exchange(
            node,
            &self.admin_pending,
            req_id,
            crate::admin::encode_admin_cmd(&cmd),
            Duration::from_secs(10),
            0,
        )
        .await
    }

    async fn admin_exchange<R: crate::admin_pending::AdminResponse>(
        self: &Arc<Self>,
        node: &str,
        pending: &crate::admin_pending::Pending<R>,
        req_id: u64,
        payload: bytes::Bytes,
        timeout: Duration,
        required_caps: u32,
    ) -> Result<R, RemoteError> {
        let mut shutdown = self.shutdown_tx.subscribe();
        let frame = Frame {
            header: crate::frame::FrameHeader {
                frame_len: 0,
                version: crate::frame::PROTOCOL_VERSION,
                frame_type: crate::frame::frame_type::SYSTEM_EVENT,
                flags: 0,
                correlation_id: req_id,
                hop_count: 0,
                hop_limit: self.config.knobs.unwrap_or_default().default_hop_limit,
                seq: crate::frame::SEQ_NONE,
            },
            path: String::new(),
            type_key: String::new(),
            payload,
        };
        let operation = async {
            let sender = Self::resolve_sender(&Arc::downgrade(self), node, false)
                .await
                .ok_or_else(|| RemoteError::UnknownNode(node.into()))?;
            let (rx, _guard) = {
                let _gate = self.link_gate.lock().unwrap();
                if *self.shutdown_tx.borrow() {
                    return Err(RemoteError::Transport("system is shut down".into()));
                }
                if sender.peer_capabilities() & required_caps != required_caps {
                    return Err(RemoteError::Transport(
                        if required_caps == crate::handshake::caps::MEASURED_METRICS {
                            "peer does not advertise measured metrics".into()
                        } else {
                            format!("peer does not advertise required admin capabilities {required_caps:#x}")
                        },
                    ));
                }
                pending.register_on(req_id, &sender)?
            };
            sender.send(frame).await?;
            tokio::select! {
                biased;
                result = rx => result.map_err(|_| RemoteError::Transport("admin reply channel closed".into())),
                _ = sender.closed() => Err(RemoteError::Transport("admin connection lost".into())),
            }
        };
        tokio::select! {
            biased;
            result = tokio::time::timeout(timeout, operation) => result.map_err(|_| RemoteError::Transport(format!("admin timeout {}s", timeout.as_secs())))?,
            _ = shutdown.wait_for(|closed| *closed) => Err(RemoteError::Transport("system is shut down".into())),
        }
    }

    // ── B1（DEV_09 §3.2）：admin-v2 组件级部署三方法 ──────────

    /// 安装目标节点组件执行器（方言挂接点：parrot Executor / 网关 admin 分支）。
    pub fn set_component_executor(&self, ex: Option<Arc<dyn crate::admin_v2::ComponentExecutor>>) {
        *self.component_executor.write().unwrap() = ex;
    }

    /// v2 命令往返（与 v1 admin_roundtrip 同构——挂起表/超时独立）。
    async fn admin_roundtrip_v2(
        self: &Arc<Self>,
        node: &str,
        mut cmd: crate::admin_v2::AdminCommandV2,
    ) -> Result<crate::admin_v2::AdminReplyV2, RemoteError> {
        let req_id = self.callbacks.next_cid();
        cmd.set_req_id(req_id);
        self.admin_exchange(
            node,
            &self.admin_pending_v2,
            req_id,
            crate::admin_v2::encode_admin_cmd_v2(&cmd),
            Duration::from_secs(30),
            match cmd {
                crate::admin_v2::AdminCommandV2::MetricsReportV2 { .. } => {
                    crate::handshake::caps::MEASURED_METRICS
                }
                crate::admin_v2::AdminCommandV2::MirrorControl { command, .. } => {
                    command.required_capabilities()
                }
                _ => 0,
            },
        )
        .await
    }

    /// Control a bounded Mirror session on an authorized, capable direct peer.
    /// Opening is idempotent by owner and installs nothing; retain the returned
    /// incarnation/sequence for apply, status and fully acknowledged close.
    pub async fn control_mirror(
        self: &Arc<Self>,
        node: &str,
        command: crate::mirror_control::MirrorControl,
    ) -> Result<crate::mirror_control::MirrorControlReply, RemoteError> {
        command.validate().map_err(RemoteError::Transport)?;
        let reply = self
            .admin_roundtrip_v2(
                node,
                crate::admin_v2::AdminCommandV2::MirrorControl {
                    req_id: 0,
                    command: command.clone(),
                },
            )
            .await?;
        match reply {
            crate::admin_v2::AdminReplyV2::MirrorControl { result, .. }
                if command.accepts(&result) =>
            {
                Ok(result)
            }
            crate::admin_v2::AdminReplyV2::Failed { code, detail, .. } => Err(
                RemoteError::Transport(format!("mirror control failed: {code:#06x} {detail}")),
            ),
            other => Err(RemoteError::Transport(format!(
                "unexpected mirror control reply: {other:?}"
            ))),
        }
    }

    /// 部署组件到指定节点（四方言统一入口；实例路径清单回执）。
    pub async fn deploy_component(
        self: &Arc<Self>,
        node: &str,
        c: crate::admin_v2::ComponentDeploy,
    ) -> Result<Vec<String>, RemoteError> {
        let reply = self
            .admin_roundtrip_v2(
                node,
                crate::admin_v2::AdminCommandV2::DeployComponent {
                    req_id: 0,
                    component: c,
                },
            )
            .await?;
        match reply {
            crate::admin_v2::AdminReplyV2::Deployed { instances, .. } => Ok(instances),
            crate::admin_v2::AdminReplyV2::Failed { code, detail, .. } => {
                Err(RemoteError::AdminRejected {
                    operation: "deploy",
                    code,
                    detail,
                })
            }
            other => Err(RemoteError::Transport(format!(
                "unexpected admin v2 reply: {other:?}"
            ))),
        }
    }

    /// 排空组件（优雅停——在途消息处理完或超时；返回 (drained, aborted)）。
    pub async fn drain_component(
        self: &Arc<Self>,
        node: &str,
        prefix: &str,
        timeout: std::time::Duration,
    ) -> Result<(usize, usize), RemoteError> {
        let reply = self
            .admin_roundtrip_v2(
                node,
                crate::admin_v2::AdminCommandV2::DrainComponent {
                    req_id: 0,
                    path_prefix: prefix.into(),
                    timeout_ms: timeout.as_millis() as u64,
                },
            )
            .await?;
        match reply {
            crate::admin_v2::AdminReplyV2::Drained {
                drained, aborted, ..
            } => Ok((drained, aborted)),
            crate::admin_v2::AdminReplyV2::Failed { code, detail, .. } => {
                Err(RemoteError::AdminRejected {
                    operation: "drain",
                    code,
                    detail,
                })
            }
            other => Err(RemoteError::Transport(format!(
                "unexpected admin v2 reply: {other:?}"
            ))),
        }
    }

    /// 停组件（立即——路径前缀下全部实例）。
    pub async fn stop_component(
        self: &Arc<Self>,
        node: &str,
        prefix: &str,
    ) -> Result<(), RemoteError> {
        let reply = self
            .admin_roundtrip_v2(
                node,
                crate::admin_v2::AdminCommandV2::StopComponent {
                    req_id: 0,
                    path_prefix: prefix.into(),
                },
            )
            .await?;
        match reply {
            crate::admin_v2::AdminReplyV2::Stopped { .. } => Ok(()),
            crate::admin_v2::AdminReplyV2::Failed { code, detail, .. } => {
                Err(RemoteError::AdminRejected {
                    operation: "stop",
                    code,
                    detail,
                })
            }
            other => Err(RemoteError::Transport(format!(
                "unexpected admin v2 reply: {other:?}"
            ))),
        }
    }

    /// 查组件状态（路径前缀匹配）。
    pub async fn component_status(
        self: &Arc<Self>,
        node: &str,
        prefix: &str,
    ) -> Result<Vec<crate::admin_v2::ComponentStateReport>, RemoteError> {
        let reply = self
            .admin_roundtrip_v2(
                node,
                crate::admin_v2::AdminCommandV2::ComponentStatus {
                    req_id: 0,
                    path_prefix: prefix.into(),
                },
            )
            .await?;
        match reply {
            crate::admin_v2::AdminReplyV2::Status { states, .. } => Ok(states),
            crate::admin_v2::AdminReplyV2::Failed { code, detail, .. } => {
                Err(RemoteError::AdminRejected {
                    operation: "status",
                    code,
                    detail,
                })
            }
            other => Err(RemoteError::Transport(format!(
                "unexpected admin v2 reply: {other:?}"
            ))),
        }
    }

    /// 采集目标节点指标快照（观测五件套——探针拉取）。
    pub async fn metrics_report(
        self: &Arc<Self>,
        node: &str,
    ) -> Result<crate::admin_v2::MetricsSnapshot, RemoteError> {
        let reply = self
            .admin_roundtrip_v2(
                node,
                crate::admin_v2::AdminCommandV2::MetricsReport { req_id: 0 },
            )
            .await?;
        match reply {
            crate::admin_v2::AdminReplyV2::Metrics { snapshot, .. } => Ok(snapshot),
            crate::admin_v2::AdminReplyV2::Failed { code, detail, .. } => {
                Err(RemoteError::AdminRejected {
                    operation: "metrics",
                    code,
                    detail,
                })
            }
            other => Err(RemoteError::Transport(format!(
                "unexpected admin v2 reply: {other:?}"
            ))),
        }
    }

    /// Availability-aware snapshot from the target's owned transport and executor.
    pub async fn measured_metrics(
        self: &Arc<Self>,
        node: &str,
    ) -> Result<crate::metrics::MeasuredMetrics, RemoteError> {
        let reply = self
            .admin_roundtrip_v2(
                node,
                crate::admin_v2::AdminCommandV2::MetricsReportV2 { req_id: 0 },
            )
            .await?;
        match reply {
            crate::admin_v2::AdminReplyV2::MeasuredMetrics { metrics, .. } => {
                metrics
                    .validate()
                    .map_err(|e| RemoteError::Transport(e.into()))?;
                Ok(metrics)
            }
            crate::admin_v2::AdminReplyV2::Failed { code, detail, .. } => {
                Err(RemoteError::AdminRejected {
                    operation: "measured metrics",
                    code,
                    detail,
                })
            }
            other => Err(RemoteError::Transport(format!(
                "unexpected measured metrics reply: {other:?}"
            ))),
        }
    }

    /// Fence all owned work synchronously. Safe without an active Tokio runtime.
    pub(crate) fn begin_close(&self) {
        let _gate = self.link_gate.lock().unwrap();
        self.ingress.close();
        self.shutdown_tx.send_replace(true);
        let retired_receptionist = std::mem::take(&mut *self.receptionist.lock().unwrap());
        self.probes.close();
        self.admin_pending.close("system shutdown");
        self.admin_pending_v2.close("system shutdown");
        self.background.close();
        self.direct_connections.close();
        self.transport.close();
        for (node, sender, status) in self.links.write().unwrap().drain(..) {
            sender.close();
            status.set(NodeState::Disconnected);
            self.nodes.disconnected(&node);
        }
        *self.uplink.write().unwrap() = None;
        self.dial_addrs.lock().unwrap().clear();
        self.callbacks
            .fail_all(crate::error::ErrCode::ConnectionLost, "system shutdown");
        drop(_gate);
        drop(retired_receptionist);
    }

    pub(crate) fn shutdown_signal(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Cancel owned work and wait for its futures and sockets to be released.
    pub async fn shutdown(&self) -> Result<(), RemoteError> {
        self.begin_close();
        let _lifecycle = self.started.lock().await;
        // Tracker ownership survives cancellation of this shutdown caller.
        self.background.wait().await;
        self.direct_connections.wait().await;
        self.transport.shutdown().await;
        self.ingress.wait_closed().await;
        let mut inbound = self.inbound_rx.lock().await;
        inbound.close();
        while inbound.try_recv().is_ok() {}
        drop(inbound);
        Ok(())
    }
}

impl Drop for RemoteActorSystem {
    fn drop(&mut self) {
        // Async cleanup runs when the executor next polls the cancelled work;
        // callers requiring a completion barrier must use shutdown().
        self.begin_close();
    }
}

fn admin_disconnect_handler(
    ingress: &Arc<Ingress>,
    pending: &Arc<crate::admin::AdminPending>,
    pending_v2: &Arc<crate::admin_v2::AdminPendingV2>,
) -> OnDisconnect {
    let (ingress, pending, pending_v2) = (ingress.clone(), pending.clone(), pending_v2.clone());
    Arc::new(move |connection| {
        ingress.connection_closed(connection);
        pending.fail_connection(connection, "connection lost");
        pending_v2.fail_connection(connection, "connection lost");
    })
}

async fn resolve_endpoints<F, Fut>(
    endpoints: Vec<parrot_config::endpoint::SeedEndpoint>,
    deadline: tokio::time::Instant,
    mut resolve: F,
) -> Result<Vec<NodeAddr>, parrot_config::ConfigError>
where
    F: FnMut(String, u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    let mut seeds = Vec::new();
    for (index, endpoint) in endpoints.into_iter().enumerate() {
        let key = format!("remote.node.seeds[{index}]");
        if endpoint.scheme == "mem" || endpoint.socket_addr().is_some() {
            seeds.push(NodeAddr::from_endpoint(&endpoint).map_err(|e| config_error(&key, e))?);
            continue;
        }
        let host = endpoint.host.as_deref().expect("network endpoint host");
        let addresses = tokio::time::timeout_at(deadline, resolve(host.to_owned(), endpoint.port))
            .await
            .map_err(|_| config_error(&key, "total DNS resolution deadline exceeded"))?
            .map_err(|e| config_error(&key, format!("DNS resolution failed: {e}")))?;
        let mut found = false;
        for addr in addresses {
            found = true;
            let seed = NodeAddr {
                node_id: endpoint.node_id.clone(),
                identity_is_explicit: endpoint.identity_is_explicit,
                scheme: endpoint.scheme.into(),
                addr,
            };
            if !seeds.contains(&seed) {
                seeds.push(seed);
            }
        }
        if !found {
            return Err(config_error(&key, "DNS returned no addresses"));
        }
    }
    Ok(seeds)
}

fn config_error(key: impl Into<String>, reason: impl Into<String>) -> parrot_config::ConfigError {
    parrot_config::ConfigError::Invalid {
        key: key.into(),
        reason: reason.into(),
    }
}

/// RemoteGateway 实现（注入 parrot facade）。
pub struct RemoteGatewayImpl(Arc<RemoteActorSystem>);

/// K0 admin 钩子：AdminCommand 本地执行；AdminReply 完成挂起表。
///
/// K1/S1 叠加：入站 MembershipGossip → handle_gossip 合并（push 半程）；
/// digest 失配且来帧无 full_sync → 回发本表 full_sync（pull 半程——
/// 定向对账，非广播）。这是 S1 语义在真实系统入站路径的接线点。
type ExecutorSlot = Arc<std::sync::RwLock<Option<Arc<dyn crate::admin_v2::ComponentExecutor>>>>;

struct AdminHook {
    system: std::sync::Weak<RemoteActorSystem>,
    metrics: Arc<crate::metrics::TransportMetrics>,
    control_policy: crate::security::ControlPolicy,
    local: Arc<dyn LocalLookup>,
    pending: Arc<crate::admin::AdminPending>,
    /// B1（DEV_09）：v2 回执完成 + 目标节点组件执行器。
    pending_v2: Arc<crate::admin_v2::AdminPendingV2>,
    executor: ExecutorSlot,
    /// S1：gossip 合并目标（RemoteActorSystem.membership 共享）。
    membership: Arc<tokio::sync::Mutex<crate::swim::Membership>>,
    self_node: String,
}

#[async_trait::async_trait]
impl crate::ingress::SysEventHook for AdminHook {
    fn on_reply_error(
        &self,
        id: u64,
        code: crate::error::ErrCode,
        detail: &str,
        source: &FrameSender,
    ) -> bool {
        self.pending.fail_from(id, code, detail, source)
            || self.pending_v2.fail_from(id, code, detail, source)
    }
    async fn on_event(&self, event: crate::admin::SysEvent, back: &FrameSender, _from: &str) {
        match event {
            crate::admin::SysEvent::AdminCommand(cmd) => {
                if !crate::admin::admin_allowed(&self.control_policy, back) {
                    let reply = crate::admin::AdminReply::Failed {
                        req_id: cmd.req_id(),
                        code: crate::error::ErrCode::Forbidden as u16,
                        detail: "administrative access requires an authorized connection identity"
                            .into(),
                    };
                    back.send_reply(crate::admin::reply_frame(cmd.req_id(), "", &reply));
                    return;
                }
                crate::admin::handle_admin_command(cmd, &self.local, back).await;
            }
            crate::admin::SysEvent::AdminReply(r) => {
                self.pending.complete_from(r, back);
            }
            // B1（DEV_09）：v2 命令 → 本节点执行器；回执 → 挂起表。
            crate::admin::SysEvent::AdminCommandV2(cmd) => {
                if !crate::admin::admin_allowed(&self.control_policy, back) {
                    let reply = crate::admin_v2::AdminReplyV2::Failed {
                        req_id: cmd.req_id(),
                        code: crate::error::ErrCode::Forbidden as u16,
                        detail: "administrative access requires an authorized connection identity"
                            .into(),
                    };
                    back.send_reply(crate::admin_v2::reply_frame_v2(cmd.req_id(), "", &reply));
                    return;
                }
                if let crate::admin_v2::AdminCommandV2::MirrorControl { req_id, command } = cmd {
                    let result = if back.peer_capabilities() & command.required_capabilities()
                        != command.required_capabilities()
                    {
                        Err("mirror control capability was not negotiated".into())
                    } else if let Some(system) = self.system.upgrade() {
                        system.ingress.control_mirror(command).await
                    } else {
                        Err("remote system retired".into())
                    };
                    let reply = match result {
                        Ok(result) => {
                            crate::admin_v2::AdminReplyV2::MirrorControl { req_id, result }
                        }
                        Err(detail) => crate::admin_v2::failed_v2(
                            req_id,
                            crate::admin_v2::v2_err::MIRROR_CONTROL,
                            detail,
                        ),
                    };
                    back.send_reply(crate::admin_v2::reply_frame_v2(req_id, "", &reply));
                    return;
                }
                use crate::ingress::unwind::{self, Cleanup};
                let cleanup = Cleanup::default();
                let mut close = cleanup.close_on_failure(back);
                let mut executor = cleanup.owned(self.executor.read().unwrap().clone());
                let req_id = cmd.req_id();
                let (reply_to, reply) =
                    if matches!(cmd, crate::admin_v2::AdminCommandV2::MetricsReportV2 { .. }) {
                        let mut metrics = self.metrics.snapshot();
                        let outcome = if let Some(executor) = executor.value().as_deref() {
                            unwind::run(&cleanup, || executor.measured_metrics())
                                .await
                                .map(|runtime| {
                                    metrics.merge_runtime(runtime);
                                })
                        } else {
                            Ok(())
                        };
                        let reply = match outcome {
                            Ok(()) => {
                                crate::admin_v2::AdminReplyV2::MeasuredMetrics { req_id, metrics }
                            }
                            Err(()) => crate::admin_v2::failed_v2(
                                req_id,
                                crate::error::ErrCode::ProtocolViolation as u16,
                                "measured metrics executor panicked",
                            ),
                        };
                        (String::new(), reply)
                    } else {
                        let reply = crate::admin_v2::execute_admin_command_v2(
                            cmd,
                            executor.value().as_deref(),
                            &cleanup,
                        )
                        .await;
                        (format!("parrot://{}/_admin", self.self_node), reply)
                    };
                executor.retire();
                let reply = crate::admin_v2::finish_admin_reply_v2(req_id, reply, cleanup.failed());
                close.disarm();
                back.send_reply(crate::admin_v2::reply_frame_v2(req_id, &reply_to, &reply));
            }
            crate::admin::SysEvent::AdminReplyV2(r) => {
                self.pending_v2.complete_from(r, back);
            }
            crate::admin::SysEvent::MembershipProbe(event) => {
                probes::handle(self.system.clone(), event, back);
            }
            crate::admin::SysEvent::ReceptionistSync(body) => {
                receptionist::receive(self.system.clone(), &body, back).await;
            }
            // K1/S1：入站 gossip 两级语义（DEV_06 §2）
            crate::admin::SysEvent::MembershipGossip(body) => {
                let Ok(g) = crate::swim::decode_gossip(&body) else {
                    tracing::warn!("gossip decode failed (from {_from})");
                    return;
                };
                let need_full = {
                    let mut m = self.membership.lock().await;
                    let need_full = crate::swim::handle_gossip(&mut m, &g);
                    let refuted = m.refute(&self.self_node).is_some();
                    need_full || refuted
                };
                if need_full {
                    // pull 半程：回定向 full_sync（对端 digest 落后——补齐）
                    let (members, digest) = {
                        let m = self.membership.lock().await;
                        (m.members.values().cloned().collect(), m.digest())
                    };
                    let full = crate::swim::MembershipGossip {
                        events: Vec::new(),
                        seen_from: self.self_node.clone(),
                        digest,
                        full_sync: Some(members),
                    };
                    let payload = crate::swim::encode_gossip(&full);
                    let _ = back.send_frame_sync(Frame {
                        header: crate::frame::FrameHeader {
                            frame_len: 0,
                            version: crate::frame::PROTOCOL_VERSION,
                            frame_type: crate::frame::frame_type::SYSTEM_EVENT,
                            flags: 0,
                            correlation_id: 0,
                            hop_count: 0,
                            hop_limit: 8,
                            seq: crate::frame::SEQ_NONE,
                        },
                        path: String::new(),
                        type_key: String::new(),
                        payload,
                    });
                }
            }
        }
    }
}

impl RemoteGatewayImpl {
    pub fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        if *self.0.shutdown_tx.borrow() {
            return None;
        }
        crate::actor_path::split(path)?;
        self.0
            .remote_ref(path)
            .ok()
            .map(|r| Box::new(r) as Box<dyn parrot_api::address::ActorRef>)
    }
}

// ══════════════════════════════════════════════════════════════════════════
// hub 转发出口（07 §6）：星型拓扑两两互通——A→hub→B 中转。
// Weak 防循环引用（RemoteActorSystem 持 ingress，ingress 持 router，router
// 只 Weak 指回系统——shutdown 后自动失效）。
// ══════════════════════════════════════════════════════════════════════════

struct SystemRelay {
    system: std::sync::Weak<RemoteActorSystem>,
    /// self node_id 副本（trait 返回 &str 借用 self——避免 upgrade 临时值）
    self_node: String,
}

#[async_trait::async_trait]
impl crate::ingress::RelayRouter for SystemRelay {
    fn sender_of(&self, node: &str) -> Option<crate::transport::FrameSender> {
        let sys = self.system.upgrade()?;
        // Registry critical sections contain no awaits. Contention must not
        // turn an existing route into a spurious RouteUnreachable error.
        let g = sys.links.read().unwrap();
        g.iter()
            .rev()
            .find(|(n, sender, _)| n == node && !sender.is_closed())
            .map(|(_, s, _)| s.clone())
    }

    fn next_cid(&self) -> u64 {
        // 与本地 ask 共用 cid 计数器（CallbackRegistry 分配器）——
        // 转发 cid 与本地 cid 同空间，防 REPLY 回程映射撞号
        self.system
            .upgrade()
            .map(|s| s.callbacks.next_cid())
            .unwrap_or(0)
    }

    fn self_node(&self) -> &str {
        &self.self_node
    }

    fn dial_addr_of(&self, node: &str) -> Option<String> {
        let sys = self.system.upgrade()?;
        let addr = sys.dial_addrs.lock().unwrap().get(node).cloned();
        addr
    }

    /// 方案 A 学习回调：后台拨直连（不阻塞 ingress——spawn；已在连/拨中
    /// 忽略；失败静默，后续 hint 或回落 uplink 兜底）。
    fn on_hint(&self, node: &str, addr: &str) {
        let Some(sys) = self.system.upgrade() else {
            return;
        };
        if !sys.may_dial_business() {
            return;
        }
        // 已有该节点直连链路 → 无需动作（新地址不同时也重拨：先移除旧链）
        let already = sys
            .links
            .read()
            .map(|g| g.iter().any(|(n, _, _)| n == node))
            .unwrap_or(false);
        if already {
            return;
        }
        let weak = Arc::downgrade(&sys);
        let node = node.to_string();
        let addr = addr.to_string();
        sys.spawn_background(async move {
            match addr.parse::<std::net::SocketAddr>() {
                Ok(sa) => {
                    let na = crate::node::NodeAddr::tcp(node.clone(), sa);
                    match RemoteActorSystem::connect_weak(&weak, &na).await {
                        Ok(()) => tracing::info!(%node, %addr, "direct link established"),
                        Err(e) => {
                            tracing::debug!(%node, %addr, err = %e, "direct dial failed (fallback uplink)")
                        }
                    }
                }
                Err(_) => tracing::debug!(%addr, "hint addr unparseable"),
            }
        });
    }
}

#[cfg(test)]
mod config_resolution_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn dns_uses_one_total_deadline_and_never_publishes_partial_results() {
        let endpoints = ["tcp://one@one.test:80", "tcp://two@two.test:80"]
            .into_iter()
            .map(|value| parrot_config::endpoint::SeedEndpoint::parse(value, "tcp").unwrap())
            .collect();
        let begin = tokio::time::Instant::now();
        let result = resolve_endpoints(
            endpoints,
            begin + Duration::from_millis(100),
            |_, port| async move {
                tokio::time::sleep(Duration::from_millis(60)).await;
                Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
            },
        )
        .await;
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("seeds[1]") && error.contains("deadline"),
            "{error}"
        );
        assert!(
            begin.elapsed() < Duration::from_millis(120),
            "timeout was incorrectly reset for the second seed"
        );
    }

    #[tokio::test]
    async fn empty_dns_answer_is_an_error() {
        let endpoints =
            vec![
                parrot_config::endpoint::SeedEndpoint::parse("tcp://empty.test:80", "tcp").unwrap(),
            ];
        let result = resolve_endpoints(
            endpoints,
            tokio::time::Instant::now() + Duration::from_secs(1),
            |_, _| async { Ok(vec![]) },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("no addresses"));
    }
}

#[cfg(test)]
mod admin_contract_tests;

#[cfg(test)]
mod metrics_contract_tests;

#[cfg(test)]
mod mirror_control_tests;
