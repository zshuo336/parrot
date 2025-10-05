//! # Parrot 配置切面（Config as a Crosscutting Concern）
//!
//! 业务逻辑与物理部署解耦：所有部署相关参数（节点身份、网络地址、拓扑
//! 角色、调优旋钮）从代码外置到 TOML 文件，业务代码只依赖配置抽象。
//!
//! ## 三层优先级（用户契约）
//!
//! ```text
//! 生效值 = 代码显式设置（builder/with_*，最高）
//!        ▷ TOML 文件（PARROT_CONFIG 或默认 parrot.toml）
//!        ▷ 编译期默认（与现写死常量一致——零迁移成本）
//! ```
//!
//! 实现机制：全字段 `Option` 即优先级——`代码.or(文件).unwrap_or(默认)`。
//! 与 `ThreadActorConfig` 的 Option 覆盖机制同构（merge_with_actor_config
//! 已是该范式），无新心智模型。
//!
//! ## 依赖倒置
//!
//! 本 crate 只依赖 parrot-api（类型协议）——`parrot` 与 `parrot-remote`
//! 各自 `From<&ParrotConfig>` 生成自己的运行时配置。不 import 任何
//! 运行时 crate，与 RemoteGateway 倒置模式同构（E5.2 分层铁律）。
//!
//! ## 最小用例
//!
//! ```no_run
//! use parrot_config::ParrotConfig;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let cfg = ParrotConfig::builder()
//!     .load_file("parrot.toml")?      // 文件层（不存在则跳过，仅 debug 日志）
//!     .thread_shared_pool_size(8)     // 代码层（最高优先级，覆盖文件）
//!     .build()?;                       // 校验 + 折叠默认
//! println!("{}", cfg.remote.node.node_id.as_deref().unwrap_or("<unset>"));
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

/// 配置环境变量：文件路径覆盖（K8s/Docker 注入配置的标准做法）。
pub const CONFIG_ENV: &str = "PARROT_CONFIG";

// ============================================================================
// 数据模型：三层全是 Option——Some=该层显式设置，None=让位下一层
// ============================================================================

/// 完整 Parrot 配置（thread 引擎 + remote 全域）。
///
/// 每个字段三层语义：`builder 代码设置 ▷ toml 文件键 ▷ Default`。
/// `build()` 后全部折叠为最终值（不再有 None——数值字段直接可用）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParrotConfig {
    /// `[thread]` 共享调度池。
    pub thread: ThreadSection,
    /// `[remote.*]` 远程层全域。
    pub remote: RemoteSection,
}

/// `[thread]`——ThreadActorSystem 调度域。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ThreadSection {
    pub shared_pool_size: Option<usize>,
    pub shared_burst_workers_max: Option<usize>,
    pub shared_burst_backlog_threshold_ms: Option<u64>,
    pub shared_burst_idle_timeout_ms: Option<u64>,
    pub shared_queue_capacity: Option<usize>,
    pub max_dedicated_threads: Option<usize>,
    pub default_mailbox_capacity: Option<usize>,
    pub default_ask_timeout_ms: Option<u64>,
    pub shutdown_timeout_ms: Option<u64>,
}

/// `[remote]` 根。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RemoteSection {
    pub node: RemoteNodeSection,
    pub transport: TransportSection,
    pub reorder: ReorderSection,
    pub codec: CodecSection,
}

/// `[remote.node]` 节点身份与拓扑。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RemoteNodeSection {
    pub node_id: Option<String>,
    pub bind: Option<String>,
    pub topology_role: Option<String>,
    pub direct_addr: Option<String>,
    pub seeds: Option<Vec<String>>,
    pub scheme: Option<String>,
}

/// `[remote.transport]` 连接层旋钮（原写死常量参数化）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TransportSection {
    pub heartbeat_interval_ms: Option<u64>,
    pub heartbeat_max_loss: Option<u32>,
    pub outbound_queue: Option<usize>,
    pub default_hop_limit: Option<u8>,
}

/// `[remote.reorder]` TELL 重排旋钮。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReorderSection {
    pub gap_timeout_ms: Option<u64>,
    pub buffer_cap: Option<usize>,
}

/// `[remote.codec]` 编解码协商。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CodecSection {
    pub extra_caps: Option<u32>,
}

// ============================================================================
// 编译期默认值（单一事实源——与原写死常量逐项一致）
// ============================================================================

/// 编译期默认层：迁移前写死常量的原值（修改=语义变更，需评审）。
#[derive(Debug, Clone, Copy)]
pub struct Defaults;

impl Defaults {
    pub fn heartbeat_interval_ms() -> u64 {
        2000
    }
    pub fn heartbeat_max_loss() -> u32 {
        5
    }
    pub fn outbound_queue() -> usize {
        1024
    }
    pub fn default_hop_limit() -> u8 {
        8
    }
    pub fn reorder_gap_timeout_ms() -> u64 {
        250
    }
    pub fn reorder_buffer_cap() -> usize {
        1024
    }
    pub fn callback_capacity() -> usize {
        65536
    }
    pub fn shared_pool_size() -> usize {
        num_cpus_default()
    }
    pub fn shared_queue_capacity() -> usize {
        10000
    }
    pub fn default_mailbox_capacity() -> usize {
        1024
    }
    pub fn default_ask_timeout_ms() -> u64 {
        5000
    }
    pub fn shutdown_timeout_ms() -> u64 {
        10000
    }
    pub fn max_dedicated_threads() -> usize {
        32
    }
    pub fn shared_burst_workers_max() -> usize {
        num_cpus_default()
    }
    pub fn shared_burst_backlog_threshold_ms() -> u64 {
        100
    }
    pub fn shared_burst_idle_timeout_ms() -> u64 {
        5000
    }
}

fn num_cpus_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

// ============================================================================
// 折叠后的最终值（build 产物——数值直接可用）
// ============================================================================

/// `build()` 产物：三层折叠完成的最终配置（无 Option——消费侧零样板）。
#[derive(Debug, Clone)]
pub struct Resolved {
    pub thread: ThreadResolved,
    pub remote: RemoteResolved,
}

#[derive(Debug, Clone)]
pub struct ThreadResolved {
    pub shared_pool_size: usize,
    pub shared_burst_workers_max: usize,
    pub shared_burst_backlog_threshold_ms: u64,
    pub shared_burst_idle_timeout_ms: u64,
    pub shared_queue_capacity: usize,
    pub max_dedicated_threads: usize,
    pub default_mailbox_capacity: usize,
    pub default_ask_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct RemoteResolved {
    pub node: RemoteNodeResolved,
    pub transport: TransportResolved,
    pub reorder: ReorderResolved,
    pub extra_caps: u32,
}

#[derive(Debug, Clone)]
pub struct RemoteNodeResolved {
    pub node_id: Option<String>,
    pub bind: Option<String>,
    pub topology_role: TopologyRoleValue,
    pub direct_addr: Option<String>,
    pub seeds: Vec<String>,
    pub scheme: String,
}

/// 拓扑角色（本 crate 不依赖 parrot-remote——字符串枚举镜像，From 转换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TopologyRoleValue {
    #[default]
    Normal,
    Hub,
    Border,
    Directory,
}

impl TopologyRoleValue {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "normal" => Some(Self::Normal),
            "hub" => Some(Self::Hub),
            "border" => Some(Self::Border),
            "directory" => Some(Self::Directory),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Hub => "hub",
            Self::Border => "border",
            Self::Directory => "directory",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransportResolved {
    pub heartbeat_interval_ms: u64,
    pub heartbeat_max_loss: u32,
    pub outbound_queue: usize,
    pub default_hop_limit: u8,
}

#[derive(Debug, Clone)]
pub struct ReorderResolved {
    pub gap_timeout_ms: u64,
    pub buffer_cap: usize,
}

// ============================================================================
// Builder + 文件加载 + 合并
// ============================================================================

impl ParrotConfig {
    pub fn builder() -> Self {
        Self::default()
    }

    /// 从 TOML 文件合并（文件层）。文件不存在 = 跳过（部署最小化为零配置
    /// 启动；可选性是特性不是错误）。已知键缺省合并、未知键记 warn 不失败
    /// （前向兼容：老配置配新版本不炸）。
    pub fn load_file(mut self, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        if !path.exists() {
            tracing::debug!(path = %path.display(), "config file absent — file layer skipped");
            return Ok(self);
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io {
                path: path.display().to_string(),
                source: e,
            })?;
        let expanded = expand_env(&raw);
        let file: toml::Value = toml::from_str(&expanded)
            .map_err(|e| ConfigError::Parse {
                path: path.display().to_string(),
                source: e,
            })?;
        self.merge_toml(&file);
        Ok(self)
    }

    /// 环境变量定位的标准入口：`PARROT_CONFIG` 路径（未设则 parrot.toml）。
    pub fn load_default_locations(self) -> Result<Self, ConfigError> {
        let path = std::env::var(CONFIG_ENV).unwrap_or_else(|_| "parrot.toml".into());
        self.load_file(path)
    }

    /// TOML 值合并进自身（仅填 None 字段——已 Some 的代码设置不被覆盖）。
    fn merge_toml(&mut self, file: &toml::Value) {
        if let Some(th) = file.get("thread").and_then(|v| v.as_table()) {
            merge_opt(&mut self.thread.shared_pool_size, "thread.shared_pool_size", th);
            merge_opt(
                &mut self.thread.shared_burst_workers_max,
                "thread.shared_burst_workers_max",
                th,
            );
            merge_opt(
                &mut self.thread.shared_burst_backlog_threshold_ms,
                "thread.shared_burst_backlog_threshold_ms",
                th,
            );
            merge_opt(
                &mut self.thread.shared_burst_idle_timeout_ms,
                "thread.shared_burst_idle_timeout_ms",
                th,
            );
            merge_opt(
                &mut self.thread.shared_queue_capacity,
                "thread.shared_queue_capacity",
                th,
            );
            merge_opt(
                &mut self.thread.max_dedicated_threads,
                "thread.max_dedicated_threads",
                th,
            );
            merge_opt(
                &mut self.thread.default_mailbox_capacity,
                "thread.default_mailbox_capacity",
                th,
            );
            merge_opt(
                &mut self.thread.default_ask_timeout_ms,
                "thread.default_ask_timeout_ms",
                th,
            );
            merge_opt(
                &mut self.thread.shutdown_timeout_ms,
                "thread.shutdown_timeout_ms",
                th,
            );
            warn_unknown(th, "thread", &[
                "shared_pool_size","shared_burst_workers_max","shared_burst_backlog_threshold_ms",
                "shared_burst_idle_timeout_ms","shared_queue_capacity","max_dedicated_threads",
                "default_mailbox_capacity","default_ask_timeout_ms","shutdown_timeout_ms",
            ]);
        }
        if let Some(rm) = file.get("remote").and_then(|v| v.as_table()) {
            if let Some(n) = rm.get("node").and_then(|v| v.as_table()) {
                let sec = &mut self.remote.node;
                if sec.node_id.is_none() {
                    if let Some(v) = n.get("node_id").and_then(|v| v.as_str()) {
                        sec.node_id = Some(v.to_string());
                    }
                }
                merge_opt_str(&mut sec.bind, "remote.node.bind", n);
                merge_opt_str(&mut sec.topology_role, "remote.node.topology_role", n);
                merge_opt_str(&mut sec.direct_addr, "remote.node.direct_addr", n);
                merge_opt_str(&mut sec.scheme, "remote.node.scheme", n);
                if sec.seeds.is_none() {
                    if let Some(arr) = n.get("seeds").and_then(|v| v.as_array()) {
                        let seeds: Vec<String> = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                        sec.seeds = Some(seeds);
                    }
                }
                warn_unknown(n, "remote.node", &[
                    "node_id","bind","topology_role","direct_addr","seeds","scheme",
                ]);
                if let Some(role) = &sec.topology_role {
                    if TopologyRoleValue::parse(role).is_none() {
                        tracing::warn!(
                            got = %role,
                            "remote.node.topology_role invalid (normal|hub|border|directory) — falls back normal"
                        );
                    }
                }
            }
            if let Some(t) = rm.get("transport").and_then(|v| v.as_table()) {
                let sec = &mut self.remote.transport;
                merge_opt(&mut sec.heartbeat_interval_ms, "remote.transport.heartbeat_interval_ms", t);
                merge_opt(&mut sec.heartbeat_max_loss, "remote.transport.heartbeat_max_loss", t);
                merge_opt(&mut sec.outbound_queue, "remote.transport.outbound_queue", t);
                merge_opt(&mut sec.default_hop_limit, "remote.transport.default_hop_limit", t);
                warn_unknown(t, "remote.transport", &[
                    "heartbeat_interval_ms","heartbeat_max_loss","outbound_queue","default_hop_limit",
                ]);
            }
            if let Some(r) = rm.get("reorder").and_then(|v| v.as_table()) {
                let sec = &mut self.remote.reorder;
                merge_opt(&mut sec.gap_timeout_ms, "remote.reorder.gap_timeout_ms", r);
                merge_opt(&mut sec.buffer_cap, "remote.reorder.buffer_cap", r);
                warn_unknown(r, "remote.reorder", &["gap_timeout_ms","buffer_cap"]);
            }
            if let Some(c) = rm.get("codec").and_then(|v| v.as_table()) {
                merge_opt(&mut self.remote.codec.extra_caps, "remote.codec.extra_caps", c);
                warn_unknown(c, "remote.codec", &["extra_caps"]);
            }
            warn_unknown(rm, "remote", &["node","transport","reorder","codec"]);
        }
        warn_unknown(
            file.as_table().unwrap_or(&toml::map::Map::new()),
            "<root>",
            &["thread", "remote"],
        );
    }

    /// 校验 + 折叠默认 → 最终值。
    pub fn build(self) -> Result<Resolved, ConfigError> {
        // 校验层：非法值在 build 期报错（fail-fast，不带入运行时）
        let tr = &self.remote.transport;
        if let Some(v) = tr.heartbeat_interval_ms {
            if v == 0 {
                return Err(ConfigError::Invalid {
                    key: "remote.transport.heartbeat_interval_ms".into(),
                    reason: "must be > 0 (0 = never heartbeat = never detect half-open)".into(),
                });
            }
        }
        if let Some(v) = tr.default_hop_limit {
            if v == 0 || v > 64 {
                return Err(ConfigError::Invalid {
                    key: "remote.transport.default_hop_limit".into(),
                    reason: "must be in 1..=64".into(),
                });
            }
        }
        if let Some(v) = tr.heartbeat_max_loss {
            if v == 0 {
                return Err(ConfigError::Invalid {
                    key: "remote.transport.heartbeat_max_loss".into(),
                    reason: "must be > 0".into(),
                });
            }
        }
        if let Some(v) = &self.remote.node.bind {
            if v.parse::<std::net::SocketAddr>().is_err() {
                return Err(ConfigError::Invalid {
                    key: "remote.node.bind".into(),
                    reason: format!("not a valid SocketAddr: {v}"),
                });
            }
        }
        for s in self.remote.node.seeds.iter().flatten() {
            if s.is_empty() {
                return Err(ConfigError::Invalid {
                    key: "remote.node.seeds".into(),
                    reason: "empty seed entry".into(),
                });
            }
        }

        // 折叠：代码（Some）已优先生效（merge_toml 只填 None）→ unwrap_or 默认
        let role = self
            .remote
            .node
            .topology_role
            .as_deref()
            .and_then(TopologyRoleValue::parse)
            .unwrap_or_default();
        Ok(Resolved {
            thread: ThreadResolved {
                shared_pool_size: self
                    .thread
                    .shared_pool_size
                    .unwrap_or_else(Defaults::shared_pool_size),
                shared_burst_workers_max: self
                    .thread
                    .shared_burst_workers_max
                    .unwrap_or_else(Defaults::shared_burst_workers_max),
                shared_burst_backlog_threshold_ms: self
                    .thread
                    .shared_burst_backlog_threshold_ms
                    .unwrap_or_else(Defaults::shared_burst_backlog_threshold_ms),
                shared_burst_idle_timeout_ms: self
                    .thread
                    .shared_burst_idle_timeout_ms
                    .unwrap_or_else(Defaults::shared_burst_idle_timeout_ms),
                shared_queue_capacity: self
                    .thread
                    .shared_queue_capacity
                    .unwrap_or_else(Defaults::shared_queue_capacity),
                max_dedicated_threads: self
                    .thread
                    .max_dedicated_threads
                    .unwrap_or_else(Defaults::max_dedicated_threads),
                default_mailbox_capacity: self
                    .thread
                    .default_mailbox_capacity
                    .unwrap_or_else(Defaults::default_mailbox_capacity),
                default_ask_timeout_ms: self
                    .thread
                    .default_ask_timeout_ms
                    .unwrap_or_else(Defaults::default_ask_timeout_ms),
                shutdown_timeout_ms: self
                    .thread
                    .shutdown_timeout_ms
                    .unwrap_or_else(Defaults::shutdown_timeout_ms),
            },
            remote: RemoteResolved {
                node: RemoteNodeResolved {
                    node_id: self.remote.node.node_id,
                    bind: self.remote.node.bind,
                    topology_role: role,
                    direct_addr: self.remote.node.direct_addr,
                    seeds: self.remote.node.seeds.unwrap_or_default(),
                    scheme: self.remote.node.scheme.unwrap_or_else(|| "tcp".into()),
                },
                transport: TransportResolved {
                    heartbeat_interval_ms: tr
                        .heartbeat_interval_ms
                        .unwrap_or_else(Defaults::heartbeat_interval_ms),
                    heartbeat_max_loss: tr
                        .heartbeat_max_loss
                        .unwrap_or_else(Defaults::heartbeat_max_loss),
                    outbound_queue: tr
                        .outbound_queue
                        .unwrap_or_else(Defaults::outbound_queue),
                    default_hop_limit: tr
                        .default_hop_limit
                        .unwrap_or_else(Defaults::default_hop_limit),
                },
                reorder: ReorderResolved {
                    gap_timeout_ms: self
                        .remote
                        .reorder
                        .gap_timeout_ms
                        .unwrap_or_else(Defaults::reorder_gap_timeout_ms),
                    buffer_cap: self
                        .remote
                        .reorder
                        .buffer_cap
                        .unwrap_or_else(Defaults::reorder_buffer_cap),
                },
                extra_caps: self.remote.codec.extra_caps.unwrap_or(0),
            },
        })
    }

    /// 全键文档（运维参考——`parrot-config --dump-docs` 的数据源）。
    pub fn documented() -> BTreeMap<&'static str, &'static str> {
        let mut m = BTreeMap::new();
        m.insert("thread.shared_pool_size", "共享调度池核心线程数（默认 CPU 核数）");
        m.insert("thread.shared_burst_workers_max", "弹性突发 worker 上限（默认 CPU 核数）");
        m.insert("thread.shared_burst_backlog_threshold_ms", "队列积压多久后扩突发 worker（默认 100ms）");
        m.insert("thread.shared_burst_idle_timeout_ms", "突发 worker 空闲多久回收（默认 5000ms）");
        m.insert("thread.shared_queue_capacity", "共享调度队列容量（默认 10000）");
        m.insert("thread.max_dedicated_threads", "专用线程上限（默认 32）");
        m.insert("thread.default_mailbox_capacity", "actor 默认邮箱容量（默认 1024）");
        m.insert("thread.default_ask_timeout_ms", "ask 默认超时（默认 5000ms）");
        m.insert("thread.shutdown_timeout_ms", "系统关闭超时（默认 10000ms）");
        m.insert("remote.node.node_id", "节点唯一 id（部署身份——必填于多节点拓扑）");
        m.insert("remote.node.bind", "监听地址 host:port（如 0.0.0.0:9801）");
        m.insert("remote.node.topology_role", "拓扑角色 normal|hub|border|directory（默认 normal）");
        m.insert("remote.node.direct_addr", "可直拨地址（方案 A——hub 据此注入 ROUTE_HINT）");
        m.insert("remote.node.seeds", "种子节点地址列表 [\"tcp://host:port\"]");
        m.insert("remote.node.scheme", "传输载体 tcp|mem（默认 tcp）");
        m.insert("remote.transport.heartbeat_interval_ms", "心跳间隔（默认 2000ms）");
        m.insert("remote.transport.heartbeat_max_loss", "心跳丢失多少次判半开断开（默认 5）");
        m.insert("remote.transport.outbound_queue", "出站帧队列容量（默认 1024，天然反压）");
        m.insert("remote.transport.default_hop_limit", "帧默认跳数上限（默认 8）");
        m.insert("remote.reorder.gap_timeout_ms", "TELL 重排缺口等待上限（默认 250ms）");
        m.insert("remote.reorder.buffer_cap", "重排缓冲帧数上限（默认 1024）");
        m.insert("remote.codec.extra_caps", "附加能力位（连 pb-only 对端时叠加）");
        m
    }
}

// --- builder 代码层 setter（链式——与 with_* 风格一致） ---

impl ParrotConfig {
    pub fn thread_shared_pool_size(mut self, v: usize) -> Self {
        self.thread.shared_pool_size = Some(v);
        self
    }
    pub fn thread_shared_queue_capacity(mut self, v: usize) -> Self {
        self.thread.shared_queue_capacity = Some(v);
        self
    }
    pub fn thread_default_mailbox_capacity(mut self, v: usize) -> Self {
        self.thread.default_mailbox_capacity = Some(v);
        self
    }
    pub fn thread_default_ask_timeout_ms(mut self, v: u64) -> Self {
        self.thread.default_ask_timeout_ms = Some(v);
        self
    }
    pub fn thread_shutdown_timeout_ms(mut self, v: u64) -> Self {
        self.thread.shutdown_timeout_ms = Some(v);
        self
    }
    pub fn remote_node_id(mut self, v: impl Into<String>) -> Self {
        self.remote.node.node_id = Some(v.into());
        self
    }
    pub fn remote_bind(mut self, v: impl Into<String>) -> Self {
        self.remote.node.bind = Some(v.into());
        self
    }
    pub fn remote_topology_role(mut self, v: impl Into<String>) -> Self {
        self.remote.node.topology_role = Some(v.into());
        self
    }
    pub fn remote_direct_addr(mut self, v: impl Into<String>) -> Self {
        self.remote.node.direct_addr = Some(v.into());
        self
    }
    pub fn remote_seeds(mut self, v: Vec<String>) -> Self {
        self.remote.node.seeds = Some(v);
        self
    }
    pub fn remote_heartbeat_interval_ms(mut self, v: u64) -> Self {
        self.remote.transport.heartbeat_interval_ms = Some(v);
        self
    }
    pub fn remote_heartbeat_max_loss(mut self, v: u32) -> Self {
        self.remote.transport.heartbeat_max_loss = Some(v);
        self
    }
    pub fn remote_outbound_queue(mut self, v: usize) -> Self {
        self.remote.transport.outbound_queue = Some(v);
        self
    }
    pub fn remote_default_hop_limit(mut self, v: u8) -> Self {
        self.remote.transport.default_hop_limit = Some(v);
        self
    }
    pub fn remote_reorder_gap_timeout_ms(mut self, v: u64) -> Self {
        self.remote.reorder.gap_timeout_ms = Some(v);
        self
    }
    pub fn remote_reorder_buffer_cap(mut self, v: usize) -> Self {
        self.remote.reorder.buffer_cap = Some(v);
        self
    }
}

// ============================================================================
// 错误类型
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config io: {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("config parse: {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("config invalid: {key}: {reason}")]
    Invalid { key: String, reason: String },
}

// ============================================================================
// 内部工具
// ============================================================================

/// 环境变量展开：`${VAR}` / `${VAR:-default}`（K8s ConfigMap 风格注入）。
fn expand_env(raw: &str) -> String {
    let mut out = raw.to_string();
    // 简单扫描——配置文件规模小，重复扫描可接受；无 '}' 收尾的残缺
    // `${` 保持原样（提示配置书写错误，不静默吞）
    while let Some(start) = out.find("${") {
        let Some(end_rel) = out[start..].find('}') else { break };
        let end = start + end_rel;
        let inner = &out[start + 2..end];
        let (name, default) = match inner.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (inner, None),
        };
        let val = std::env::var(name).ok().or_else(|| default.map(String::from));
        let replacement = val.unwrap_or_default();
        out.replace_range(start..=end, &replacement);
    }
    out
}

/// TOML 值按目标标量类型读取（int/float/string 混判——类型错误 warn 不失败）。
fn merge_opt<T: std::fmt::Debug + serde::de::DeserializeOwned>(
    slot: &mut Option<T>,
    key: &str,
    table: &toml::map::Map<String, toml::Value>,
) {
    if slot.is_some() {
        return; // 代码层已设——优先级保持
    }
    let leaf = key.rsplit('.').next().unwrap_or(key);
    let Some(v) = table.get(leaf) else {
        return;
    };
    match serde::de::Deserialize::deserialize(v.clone()) {
        Ok(typed) => *slot = Some(typed),
        Err(_) => tracing::warn!(key, ?v, "config value type mismatch — ignored"),
    }
}

fn merge_opt_str(
    slot: &mut Option<String>,
    key: &str,
    table: &toml::map::Map<String, toml::Value>,
) {
    if slot.is_some() {
        return;
    }
    let leaf = key.rsplit('.').next().unwrap_or(key);
    if let Some(v) = table.get(leaf).and_then(|v| v.as_str()) {
        *slot = Some(v.to_string());
    }
}

fn warn_unknown(
    table: &toml::map::Map<String, toml::Value>,
    section: &str,
    known: &[&str],
) {
    for k in table.keys() {
        if !known.contains(&k.as_str()) {
            tracing::warn!(section, key = %k, "unknown config key ignored (typo or newer version?)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // 三层优先级（用户契约——每个用例锁定一行契约）
    // =========================================================================

    // C1：全空 → 全默认（零配置启动 = 现行为，零迁移成本）
    #[test]
    fn c1_empty_means_all_defaults() {
        let r = ParrotConfig::builder().build().unwrap();
        assert_eq!(r.remote.transport.heartbeat_interval_ms, 2000);
        assert_eq!(r.remote.transport.heartbeat_max_loss, 5);
        assert_eq!(r.remote.transport.outbound_queue, 1024);
        assert_eq!(r.remote.transport.default_hop_limit, 8);
        assert_eq!(r.remote.reorder.gap_timeout_ms, 250);
        assert_eq!(r.remote.reorder.buffer_cap, 1024);
        assert_eq!(r.thread.default_mailbox_capacity, 1024);
        assert_eq!(r.thread.default_ask_timeout_ms, 5000);
        assert_eq!(r.remote.node.topology_role, TopologyRoleValue::Normal);
        assert!(r.remote.node.seeds.is_empty());
    }

    // C2：文件层生效（无代码设置）
    #[test]
    fn c2_file_overrides_defaults() {
        let tmp = std::env::temp_dir().join(format!("pcfg_c2_{}.toml", std::process::id()));
        std::fs::write(
            &tmp,
            "[remote.transport]\nheartbeat_interval_ms = 500\n\n[thread]\ndefault_mailbox_capacity = 7\n",
        )
        .unwrap();
        let r = ParrotConfig::builder().load_file(&tmp).unwrap().build().unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(r.remote.transport.heartbeat_interval_ms, 500);
        assert_eq!(r.thread.default_mailbox_capacity, 7);
        // 未覆盖的回默认
        assert_eq!(r.remote.transport.heartbeat_max_loss, 5);
    }

    // C3：代码 > 文件（核心契约）
    #[test]
    fn c3_code_beats_file() {
        let tmp = std::env::temp_dir().join(format!("pcfg_c3_{}.toml", std::process::id()));
        std::fs::write(
            &tmp,
            "[remote.transport]\nheartbeat_interval_ms = 999\n",
        )
        .unwrap();
        let r = ParrotConfig::builder()
            .load_file(&tmp)
            .unwrap()
            .remote_heartbeat_interval_ms(111) // 代码层
            .build()
            .unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(r.remote.transport.heartbeat_interval_ms, 111, "代码层必须赢");
    }

    // C4：builder 先设、后 load——顺序无关（契约是"层"不是"调用序"）
    #[test]
    fn c4_order_independent_code_wins() {
        let tmp = std::env::temp_dir().join(format!("pcfg_c4_{}.toml", std::process::id()));
        std::fs::write(&tmp, "[remote.transport]\noutbound_queue = 777\n").unwrap();
        let r = ParrotConfig::builder()
            .remote_outbound_queue(55) // 先设
            .load_file(&tmp) // 后 load 不覆盖
            .unwrap()
            .build()
            .unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(r.remote.transport.outbound_queue, 55);
    }

    // C5：文件不存在 = 特性（跳过，不报错）
    #[test]
    fn c5_missing_file_ok() {
        let r = ParrotConfig::builder()
            .load_file("/nonexistent/parrot.toml")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(r.remote.transport.heartbeat_interval_ms, 2000);
    }

    // =========================================================================
    // 校验（fail-fast）
    // =========================================================================

    #[test]
    fn v1_zero_heartbeat_rejected() {
        let e = ParrotConfig::builder()
            .remote_heartbeat_interval_ms(0)
            .build()
            .unwrap_err();
        assert!(e.to_string().contains("heartbeat_interval_ms"), "{e}");
    }

    #[test]
    fn v2_hop_limit_range() {
        assert!(ParrotConfig::builder().remote_default_hop_limit(0).build().is_err());
        assert!(ParrotConfig::builder().remote_default_hop_limit(65).build().is_err());
        assert!(ParrotConfig::builder().remote_default_hop_limit(8).build().is_ok());
    }

    #[test]
    fn v3_bad_bind_rejected() {
        let e = ParrotConfig::builder()
            .remote_bind("not-an-addr")
            .build()
            .unwrap_err();
        assert!(e.to_string().contains("bind"), "{e}");
    }

    #[test]
    fn v4_bad_toml_syntax() {
        let tmp = std::env::temp_dir().join(format!("pcfg_v4_{}.toml", std::process::id()));
        std::fs::write(&tmp, "[[broken").unwrap();
        let e = ParrotConfig::builder().load_file(&tmp).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert!(matches!(e, ConfigError::Parse { .. }), "{e}");
    }

    // =========================================================================
    // 环境变量展开
    // =========================================================================

    #[test]
    fn e1_env_expansion() {
        std::env::set_var("PCFG_TEST_NODE", "env-node-42");
        let tmp = std::env::temp_dir().join(format!("pcfg_e1_{}.toml", std::process::id()));
        std::fs::write(
            &tmp,
            "[remote.node]\nnode_id = \"${PCFG_TEST_NODE}\"\nbind = \"${PCFG_TEST_UNSET:-127.0.0.1:9801}\"\n",
        )
        .unwrap();
        let r = ParrotConfig::builder().load_file(&tmp).unwrap().build().unwrap();
        std::fs::remove_file(&tmp).ok();
        std::env::remove_var("PCFG_TEST_NODE");
        assert_eq!(r.remote.node.node_id.as_deref(), Some("env-node-42"));
        assert_eq!(r.remote.node.bind.as_deref(), Some("127.0.0.1:9801"));
    }

    // =========================================================================
    // 完整文件全字段
    // =========================================================================

    #[test]
    fn f1_full_file_all_sections() {
        let tmp = std::env::temp_dir().join(format!("pcfg_f1_{}.toml", std::process::id()));
        std::fs::write(
            &tmp,
            r#"
[thread]
shared_pool_size = 3
shared_burst_workers_max = 2
shared_burst_backlog_threshold_ms = 50
shared_burst_idle_timeout_ms = 3000
shared_queue_capacity = 500
max_dedicated_threads = 8
default_mailbox_capacity = 64
default_ask_timeout_ms = 1500
shutdown_timeout_ms = 2500

[remote.node]
node_id = "hub-1"
bind = "0.0.0.0:9800"
topology_role = "hub"
direct_addr = "10.1.1.1:9800"
seeds = ["tcp://seed-a:9800", "tcp://seed-b:9800"]

[remote.transport]
heartbeat_interval_ms = 1500
heartbeat_max_loss = 3
outbound_queue = 512
default_hop_limit = 12

[remote.reorder]
gap_timeout_ms = 100
buffer_cap = 128

[remote.codec]
extra_caps = 2
"#,
        )
        .unwrap();
        let r = ParrotConfig::builder().load_file(&tmp).unwrap().build().unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(r.thread.shared_pool_size, 3);
        assert_eq!(r.thread.shared_burst_workers_max, 2);
        assert_eq!(r.thread.shared_burst_backlog_threshold_ms, 50);
        assert_eq!(r.thread.shared_burst_idle_timeout_ms, 3000);
        assert_eq!(r.thread.shared_queue_capacity, 500);
        assert_eq!(r.thread.max_dedicated_threads, 8);
        assert_eq!(r.thread.default_mailbox_capacity, 64);
        assert_eq!(r.thread.default_ask_timeout_ms, 1500);
        assert_eq!(r.thread.shutdown_timeout_ms, 2500);
        assert_eq!(r.remote.node.node_id.as_deref(), Some("hub-1"));
        assert_eq!(r.remote.node.bind.as_deref(), Some("0.0.0.0:9800"));
        assert_eq!(r.remote.node.topology_role, TopologyRoleValue::Hub);
        assert_eq!(r.remote.node.direct_addr.as_deref(), Some("10.1.1.1:9800"));
        assert_eq!(r.remote.node.seeds.len(), 2);
        assert_eq!(r.remote.transport.heartbeat_interval_ms, 1500);
        assert_eq!(r.remote.transport.heartbeat_max_loss, 3);
        assert_eq!(r.remote.transport.outbound_queue, 512);
        assert_eq!(r.remote.transport.default_hop_limit, 12);
        assert_eq!(r.remote.reorder.gap_timeout_ms, 100);
        assert_eq!(r.remote.reorder.buffer_cap, 128);
        assert_eq!(r.remote.extra_caps, 2);
    }

    // topology_role 解析全枚举 + 非法回退 normal
    #[test]
    fn f2_role_parsing() {
        assert_eq!(TopologyRoleValue::parse("hub"), Some(TopologyRoleValue::Hub));
        assert_eq!(TopologyRoleValue::parse("HUB"), Some(TopologyRoleValue::Hub));
        assert_eq!(TopologyRoleValue::parse("border"), Some(TopologyRoleValue::Border));
        assert_eq!(TopologyRoleValue::parse("directory"), Some(TopologyRoleValue::Directory));
        assert_eq!(TopologyRoleValue::parse("nonsense"), None);
        // 非法角色 → 默认 Normal（warn 日志路径）
        let r = ParrotConfig::builder().remote_topology_role("nonsense").build().unwrap();
        assert_eq!(r.remote.node.topology_role, TopologyRoleValue::Normal);
    }

    // documented：全键有说明（运维文档完备性）
    #[test]
    fn d1_documented_complete() {
        let docs = ParrotConfig::documented();
        assert!(docs.len() >= 22);
        for (k, v) in &docs {
            assert!(!v.is_empty(), "key {k} has empty doc");
        }
    }
}
