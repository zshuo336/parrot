//! 职责：RemoteActorSystem 组装与生命周期（05 §7 / DEV_01 §3.7）。
//!
//! 组装：Transport（mem/tcp）+ Ingress（LocalLookup 注入）+ NodeTable +
//! CallbackRegistry + 连接管理。facade 集成走 RemoteGateway trait 倒置
//! （parrot crate 只认接口——E5.2）。

use std::net::SocketAddr;
use std::sync::Arc;

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
    pub bind: Option<SocketAddr>,
    pub seeds: Vec<NodeAddr>,
    pub scheme: &'static str, // "tcp" | "mem"
    pub callback_capacity: usize,
    pub heartbeat_enabled: bool,
    /// 附加能力位（E4/E1：连 pb-only 对端时叠加 caps::PB；默认 0=纯 bin）。
    pub extra_caps: u32,
    /// 本节点拓扑角色（07 §6）：Normal（默认）/ Hub（星型中心——spoke
    /// 将其链路作为默认路由 uplink）/ Border（联邦边界，同 Hub 待遇）。
    pub topology_role: crate::handshake::TopologyRole,
}

impl RemoteConfig {
    pub fn tcp(node_id: impl Into<String>, bind: Option<SocketAddr>) -> Self {
        Self {
            node_id: node_id.into(),
            bind,
            seeds: Vec::new(),
            scheme: "tcp",
            callback_capacity: 65536,
            heartbeat_enabled: true,
            extra_caps: 0,
            topology_role: crate::handshake::TopologyRole::Normal,
        }
    }

    /// 声明拓扑角色（hub 节点显式 Hub——spoke 据握手 ACK 识别 uplink）。
    pub fn with_role(mut self, role: crate::handshake::TopologyRole) -> Self {
        self.topology_role = role;
        self
    }

    pub fn mem(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            bind: None,
            seeds: Vec::new(),
            scheme: "mem",
            callback_capacity: 65536,
            heartbeat_enabled: true,
            extra_caps: 0,
            topology_role: crate::handshake::TopologyRole::Normal,
        }
    }

    /// QUIC 载体（K3）：dev 自签（进程内 rcgen）——生产 CA 由 K4 mTLS 注入。
    pub fn quic(node_id: impl Into<String>, bind: Option<SocketAddr>) -> Self {
        Self {
            node_id: node_id.into(),
            bind,
            seeds: Vec::new(),
            scheme: "quic",
            callback_capacity: 65536,
            heartbeat_enabled: true,
            extra_caps: 0,
            topology_role: crate::handshake::TopologyRole::Normal,
        }
    }

    pub fn with_seeds(mut self, seeds: Vec<NodeAddr>) -> Self {
        self.seeds = seeds;
        self
    }
}

/// 远程系统（一个节点的远程面全部状态）。
pub struct RemoteActorSystem {
    pub config: RemoteConfig,
    pub nodes: NodeTable,
    pub callbacks: Arc<CallbackRegistry>,
    pub ingress: Arc<Ingress>,
    /// K1/S1：SWIM 成员表（gossip 入站合并点——Arc 共享给钩子与驱动）。
    pub membership: Arc<tokio::sync::Mutex<crate::swim::Membership>>,
    transport: Box<dyn Transport>,
    inbound_tx: mpsc::Sender<(Frame, FrameSender, String)>,
    inbound_rx: tokio::sync::Mutex<mpsc::Receiver<(Frame, FrameSender, String)>>,
    /// 已建立连接（node_id → sender/status）
    links: tokio::sync::Mutex<Vec<(String, FrameSender, Arc<NodeStatus>)>>,
    /// 星型 uplink（07 §6）：hub/border 链路的 sender——目标不在直连表
    /// 时的默认路由出口。
    uplink: std::sync::RwLock<Option<FrameSender>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// K0：admin 回执挂起表（req_id → oneshot）
    admin_pending: Arc<crate::admin::AdminPending>,
    /// K0：req_id 计数（单调）
    admin_req_id: std::sync::atomic::AtomicU64,
}

impl RemoteActorSystem {
    /// 组装（不建连——start 才 listen/connect）。
    pub fn new(
        config: RemoteConfig,
        local: Arc<dyn LocalLookup>,
    ) -> Result<Arc<Self>, RemoteError> {
        let callbacks = Arc::new(CallbackRegistry::new(config.callback_capacity));
        let ingress = Arc::new(Ingress::new(local, callbacks.clone()));
        let (in_tx, in_rx) = mpsc::channel::<(Frame, FrameSender, String)>(1024);
        let handshake = HandshakeBody {
            node_id: config.node_id.clone(),
            capabilities: crate::handshake::caps::BIN | config.extra_caps,
            topology_role: config.topology_role,
            ..Default::default()
        };
        // 断连清理：单链路 → fail_node（其它链路不受牵连——K6 多联）；
        // 系统级 shutdown 才 fail_all（shutdown() 方法内）。
        let cb2 = callbacks.clone();
        let dis: OnDisconnect = Arc::new(move |node_id: &str| {
            cb2.fail_node(
                node_id,
                crate::error::ErrCode::ConnectionLost,
                "connection lost",
            );
        });
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        // K1/S1：成员表挂系统（钩子合并 + 驱动读取同一实例）
        let membership = Arc::new(tokio::sync::Mutex::new(crate::swim::Membership::new(0)));
        let transport: Box<dyn Transport> = match config.scheme {
            "mem" => Box::new(crate::transport::memory::MemoryTransport::new(
                handshake,
                in_tx.clone(),
                dis,
                shutdown_tx.clone(),
            )),
            "quic" => {
                // rustls CryptoProvider 进程级幂等安装（ring 族）
                let _ = rustls::crypto::ring::default_provider().install_default();
                let bind = config
                    .bind
                    .unwrap_or_else(|| "127.0.0.1:0".parse().unwrap());
                Box::new(crate::transport::quic::QuicTransport::new(
                    handshake,
                    in_tx.clone(),
                    dis,
                    shutdown_tx.clone(),
                    bind,
                    crate::transport::quic::dev_crypto_insecure(),
                )?)
            }
            _ => Box::new(crate::transport::tcp::TcpTransport::new(
                handshake,
                in_tx.clone(),
                dis,
                shutdown_tx.clone(),
            )),
        };
        for seed in &config.seeds {
            transport_seed_table(&config, seed);
        }
        Ok(Arc::new(Self {
            config,
            nodes: NodeTable::new(),
            callbacks,
            ingress,
            membership,
            transport,
            inbound_tx: in_tx,
            inbound_rx: tokio::sync::Mutex::new(in_rx),
            links: tokio::sync::Mutex::new(Vec::new()),
            uplink: std::sync::RwLock::new(None),
            shutdown_tx,
            admin_pending: Arc::new(crate::admin::AdminPending::default()),
            admin_req_id: std::sync::atomic::AtomicU64::new(1),
        }))
    }

    /// 实际监听地址（bind :0 → 真实端口；未监听 None）。
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.transport.local_addr()
    }

    /// 启动：listen（若配置）+ 入站分发循环 + 连接种子。
    pub async fn start(self: &Arc<Self>) -> Result<(), RemoteError> {
        self.install_admin_hook();
        // hub 转发出口注入（07 §6 两两互通：A→hub→B 中转；Weak 防循环引用）
        *self.ingress.relay_slot() = Some(Arc::new(SystemRelay {
            system: Arc::downgrade(self),
            self_node: self.config.node_id.clone(),
        }));
        // 入站分发循环
        let this = self.clone();
        let mut shutdown = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut rx = this.inbound_rx.lock().await;
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    item = rx.recv() => {
                        match item {
                            Some((frame, back, from)) => {
                                this.ingress.dispatch(frame, &back, &from).await;
                            }
                            None => break,
                        }
                    }
                }
            }
        });
        // listen + accept 循环
        if let Some(bind) = self.config.bind {
            self.transport.listen(bind).await?;
            let this = self.clone();
            tokio::spawn(async move {
                while let Ok(conn) = this.transport.accept().await {
                    this.register_link(conn).await;
                }
            });
        }
        Ok(())
    }

    /// 测试/嵌入便利：两系统经 duplex 直连（mem 载体的显式形态——
    /// 等价 connect+accept 全时序，返回各自视角无需端点表）。
    pub async fn connect_mem_pair(
        self: &Arc<Self>,
        peer: &Arc<RemoteActorSystem>,
    ) -> Result<(), RemoteError> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        // 对端 accept（server 半边）——后台任务：与 client 侧握手并发
        // （顺序 await 会死锁：accept 等 HANDSHAKE 而 client 未启动）
        let peer_sys = peer.clone();
        let accept_task = tokio::spawn(async move {
            let peer_hs = HandshakeBody {
                node_id: peer_sys.config.node_id.clone(),
                ..Default::default()
            };
            let peer_in = peer_sys.inbound_tx.clone();
            // 对端（本端 self）id——peer 视角的邻居；断连只 fail 该邻居的挂起
            let neighbor = peer_hs.node_id.clone();
            let peer_dis: OnDisconnect = {
                let cb = peer_sys.callbacks.clone();
                Arc::new(move |down: &str| {
                    cb.fail_node(
                        down,
                        crate::error::ErrCode::ConnectionLost,
                        "connection lost",
                    );
                    let _ = neighbor;
                })
            };
            let peer_conn = crate::transport::run_connection(
                server,
                crate::transport::ConnParams {
                    side: crate::transport::ConnSide::Accept,
                    local_addr: None,
                    peer_addr: None,
                    scheme: "mem",
                    local_handshake: peer_hs,
                    inbound: peer_in,
                    on_disconnect: peer_dis,
                    shutdown: peer_sys.shutdown_tx.subscribe(),
                },
            )
            .await?;
            peer_sys.register_link(peer_conn).await;
            Ok::<(), RemoteError>(())
        });
        // 本端 connect
        let my_hs = HandshakeBody {
            node_id: self.config.node_id.clone(),
            capabilities: crate::handshake::caps::BIN | self.config.extra_caps,
            ..Default::default()
        };
        let my_in = self.inbound_tx.clone();
        let my_dis: OnDisconnect = {
            let cb = self.callbacks.clone();
            Arc::new(move |down: &str| {
                cb.fail_node(
                    down,
                    crate::error::ErrCode::ConnectionLost,
                    "connection lost",
                );
            })
        };
        let my_conn = crate::transport::run_connection(
            client,
            crate::transport::ConnParams {
                side: crate::transport::ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: my_hs,
                inbound: my_in,
                on_disconnect: my_dis,
                shutdown: self.shutdown_tx.subscribe(),
            },
        )
        .await?;
        self.register_link(my_conn).await;
        accept_task
            .await
            .map_err(|e| RemoteError::Transport(format!("mem pair accept: {e}")))??;
        Ok(())
    }

    /// 主动连接种子（start 后调用；RC7 断连重连同入口）。
    pub async fn connect(self: &Arc<Self>, addr: &NodeAddr) -> Result<(), RemoteError> {
        let conn = self.transport.connect(addr).await?;
        self.nodes.add_seed(addr.clone());
        self.register_link(conn).await;
        Ok(())
    }

    async fn register_link(self: &Arc<Self>, conn: ConnectionHandle) {
        let status = Arc::new(NodeStatus::default());
        status.set(NodeState::Connected);
        self.links
            .lock()
            .await
            .push((conn.node_id.clone(), conn.sender.clone(), status.clone()));
        // 星型 uplink 记录（07 §6）：对端角色是 hub/border → 此链路可做
        // 默认路由（目标不在直连表时经它中转）。多个 hub 取最新（hub
        // 主备是 P5 拓扑管理的职责）。
        if matches!(
            conn.peer_role,
            crate::handshake::TopologyRole::Hub | crate::handshake::TopologyRole::Border
        ) {
            let mut g = self.uplink.write().unwrap();
            *g = Some(conn.sender.clone());
            tracing::info!(node = %conn.node_id, role = ?conn.peer_role, "uplink set");
        }
        // NodeTable 同步（remote_ref require 校验通过）
        self.nodes.add_seed(NodeAddr {
            node_id: conn.node_id.clone(),
            scheme: conn.info.scheme.to_string(),
            addr: conn
                .info
                .peer
                .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap()),
        });
        // 关闭信号 → 移除 link + Disconnected（回调可能早于 NodeTable 填充——容忍）
        let mut closed = conn.closed;
        let node_id = conn.node_id.clone();
        let this = self.clone();
        tokio::spawn(async move {
            let _ = (&mut closed).await;
            this.links.lock().await.retain(|(n, _, _)| n != &node_id);
            // uplink 失效清理（该 hub 断连——读侧 try_send 失败自然兜底）
            let mut g = this.uplink.write().unwrap();
            if g.as_ref().map(|s| s.node_id() == node_id).unwrap_or(false) {
                *g = None;
                tracing::info!(node = %node_id, "uplink cleared");
            }
            if let Some((_, st)) = this.nodes.get(&node_id) {
                st.set(NodeState::Disconnected);
            }
        });
    }

    /// 已建链路快照（测试/诊断观测点——node_id + sender + 状态）。
    pub async fn links_snapshot(&self) -> Vec<(String, FrameSender, Arc<NodeStatus>)> {
        self.links.lock().await.clone()
    }

    /// 解析远程路径拿 ref（校验 parrot:// 前缀 + node 在表）。
    /// 星型放宽（07 §6）：node 不在直连表但有 uplink hub → 仍可建 ref
    /// （默认路由——帧先发 hub，由其中转到目标）。
    pub fn remote_ref(&self, path: &str) -> Result<RemoteActorRef, RemoteError> {
        let node = crate::node::node_of_path(path)
            .ok_or_else(|| RemoteError::Transport(format!("not a parrot:// path: {path}")))?;
        let has_uplink = self.uplink.read().map(|g| g.is_some()).unwrap_or(false);
        if self.nodes.require(node).is_err() && !has_uplink {
            return Err(RemoteError::UnknownNode(node.to_string()));
        }
        let links = self.links.try_lock();
        let nodes = match links {
            Ok(g) => g
                .iter()
                .map(|(n, s, st)| (n.clone(), s.clone(), st.clone()))
                .collect(),
            Err(_) => Vec::new(),
        };
        let inner = Arc::new(RemoteInner {
            nodes,
            callbacks: self.callbacks.clone(),
            self_node: self.config.node_id.clone(),
            uplink: self.uplink.read().ok().and_then(|g| g.clone()),
        });
        Ok(RemoteActorRef::new(path, node, inner))
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
        let membership = self.membership.clone();
        let self_node = self.config.node_id.clone();
        let hook = AdminHook {
            local,
            pending,
            membership,
            self_node,
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
        use std::sync::atomic::Ordering;
        let req_id = self.admin_req_id.fetch_add(1, Ordering::Relaxed);
        cmd.set_req_id(req_id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.admin_pending.insert(req_id, tx);
        let sender = self
            .links
            .lock()
            .await
            .iter()
            .find(|(n, _, _)| n == node)
            .map(|(_, s, _)| s.clone())
            .ok_or_else(|| RemoteError::UnknownNode(node.into()))?;
        let payload = crate::admin::encode_admin_cmd(&cmd);
        let frame = Frame {
            header: crate::frame::FrameHeader {
                frame_len: 0,
                version: crate::frame::PROTOCOL_VERSION,
                frame_type: crate::frame::frame_type::SYSTEM_EVENT,
                flags: 0,
                correlation_id: req_id,
                hop_count: 0,
                hop_limit: 8,
            },
            path: String::new(),
            type_key: String::new(),
            payload,
        };
        if sender.send(frame).await.is_err() {
            return Err(RemoteError::Transport(
                "admin send failed (link down)".into(),
            ));
        }
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(_)) => Err(RemoteError::Transport("admin reply channel dropped".into())),
            Err(_) => Err(RemoteError::Transport("admin timeout 10s".into())),
        }
    }

    pub async fn shutdown(&self) -> Result<(), RemoteError> {
        let _ = self.shutdown_tx.send(true); // ConnectionTask 全部退出 → closed 信号 → links 清
        self.links.lock().await.clear();
        self.callbacks
            .fail_all(crate::error::ErrCode::ConnectionLost, "system shutdown");
        Ok(())
    }
}

fn transport_seed_table(_config: &RemoteConfig, seed: &NodeAddr) {
    // NodeTable 在 connect 后填充（remote_ref require 时校验）
    let _ = seed;
}

/// RemoteGateway 实现（注入 parrot facade）。
pub struct RemoteGatewayImpl(Arc<RemoteActorSystem>);

/// K0 admin 钩子：AdminCommand 本地执行；AdminReply 完成挂起表。
///
/// K1/S1 叠加：入站 MembershipGossip → handle_gossip 合并（push 半程）；
/// digest 失配且来帧无 full_sync → 回发本表 full_sync（pull 半程——
/// 定向对账，非广播）。这是 S1 语义在真实系统入站路径的接线点。
struct AdminHook {
    local: Arc<dyn LocalLookup>,
    pending: Arc<crate::admin::AdminPending>,
    /// S1：gossip 合并目标（RemoteActorSystem.membership 共享）。
    membership: Arc<tokio::sync::Mutex<crate::swim::Membership>>,
    self_node: String,
}

#[async_trait::async_trait]
impl crate::ingress::SysEventHook for AdminHook {
    async fn on_event(&self, event: crate::admin::SysEvent, back: &FrameSender, _from: &str) {
        match event {
            crate::admin::SysEvent::AdminCommand(cmd) => {
                if !crate::admin::admin_allowed(_from) {
                    let _ = back
                        .send(Frame::reply_err(
                            cmd.req_id(),
                            "",
                            crate::error::ErrCode::Forbidden,
                            "admin requires role=admin",
                        ))
                        .await;
                    return;
                }
                crate::admin::handle_admin_command(cmd, &self.local, back).await;
            }
            crate::admin::SysEvent::AdminReply(r) => {
                self.pending.complete(r);
            }
            // K1/S1：入站 gossip 两级语义（DEV_06 §2）
            crate::admin::SysEvent::MembershipGossip(body) => {
                let Ok(g) = crate::swim::decode_gossip(&body) else {
                    tracing::warn!("gossip decode failed (from {_from})");
                    return;
                };
                let need_full = {
                    let mut m = self.membership.lock().await;
                    crate::swim::handle_gossip(&mut m, &g)
                };
                if need_full {
                    // pull 半程：回定向 full_sync（对端 digest 落后——补齐）
                    let members: Vec<_> = {
                        let m = self.membership.lock().await;
                        m.members.values().cloned().collect()
                    };
                    let digest = {
                        let m = self.membership.lock().await;
                        m.digest()
                    };
                    let full = crate::swim::MembershipGossip {
                        events: Vec::new(),
                        seen_from: self.self_node.clone(),
                        digest,
                        full_sync: Some(members),
                    };
                    let payload = crate::swim::encode_gossip(&full);
                    let _ = back
                        .send(Frame {
                            header: crate::frame::FrameHeader {
                                frame_len: 0,
                                version: crate::frame::PROTOCOL_VERSION,
                                frame_type: crate::frame::frame_type::SYSTEM_EVENT,
                                flags: 0,
                                correlation_id: 0,
                                hop_count: 0,
                                hop_limit: 8,
                            },
                            path: String::new(),
                            type_key: String::new(),
                            payload,
                        })
                        .await;
                }
            }
            _ => {}
        }
    }
}

impl RemoteGatewayImpl {
    pub fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
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
        // try_lock：转发路径非阻塞（busy 时短暂 miss → RouteUnreachable
        // 由上层语义兜底；避免 hub 转发把 ingress 循环挂死在锁上）
        let g = sys.links.try_lock().ok()?;
        g.iter()
            .find(|(n, _, _)| n == node)
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
}
