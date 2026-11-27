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
//!     .load_default_locations()?     // 默认文件可选；PARROT_CONFIG 指定的文件必需
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

mod input;
#[cfg(test)]
use input::expand_env_with;
pub use input::MAX_CONFIG_BYTES;

pub mod directory;
pub mod endpoint;
pub mod receptionist;
pub mod topology;
pub mod tracing;

// ============================================================================
// 数据模型：三层全是 Option——Some=该层显式设置，None=让位下一层
// ============================================================================

/// 完整 Parrot 配置（thread 引擎 + remote 全域）。
///
/// 每个字段三层语义：`builder 代码设置 ▷ toml 文件键 ▷ Default`。
/// `build()` 后数值字段折叠为最终值；未配置的可选身份、端点和服务仍可为 None。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParrotConfig {
    pub tracing: tracing::TracingSection,
    pub topology: topology::TopologySection,
    /// `[thread]` 共享调度池。
    pub thread: ThreadSection,
    /// `[remote.*]` 远程层全域。
    pub remote: RemoteSection,
}

/// `[thread]`——ThreadActorSystem 调度域。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadSection {
    pub shared_pool_size: Option<usize>,
    pub shared_burst_workers_max: Option<usize>,
    pub shared_burst_backlog_threshold_ms: Option<u64>,
    pub shared_burst_idle_timeout_ms: Option<u64>,
    pub shared_queue_capacity: Option<usize>,
    pub shared_work_stealing: Option<bool>,
    pub max_dedicated_threads: Option<usize>,
    pub default_mailbox_capacity: Option<usize>,
    pub max_context_tasks: Option<usize>,
    pub default_ask_timeout_ms: Option<u64>,
    pub shutdown_timeout_ms: Option<u64>,
}

/// `[remote]` 根。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteSection {
    pub directory: Option<directory::DirectorySection>,
    pub receptionist: receptionist::ReceptionistSection,
    pub security: RemoteSecuritySection,
    pub node: RemoteNodeSection,
    pub transport: TransportSection,
    pub reorder: ReorderSection,
    pub codec: CodecSection,
}

/// `[remote.security]`: safe local default, mutual TLS, or explicit insecure development.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteSecuritySection {
    pub mode: Option<String>,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    pub ca_path: Option<String>,
    /// Require at least one X509 CRL in the CA bundle, even after replacement.
    pub require_crl: Option<bool>,
    pub admin_nodes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub enum RemoteSecurityResolved {
    #[default]
    Local,
    MutualTls {
        cert_path: String,
        key_path: String,
        ca_path: String,
        require_crl: bool,
        admin_nodes: Vec<String>,
    },
    InsecureDevelopment,
}
impl RemoteSecuritySection {
    fn resolve(&self) -> Result<RemoteSecurityResolved, ConfigError> {
        let invalid = |key: &str, reason: &str| ConfigError::Invalid {
            key: format!("remote.security.{key}"),
            reason: reason.into(),
        };
        let mode = self.mode.as_deref().unwrap_or("local");
        if mode == "mtls" {
            let required = |name: &str, value: &Option<String>| {
                value
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .cloned()
                    .ok_or_else(|| invalid(name, "required and nonempty in mtls mode"))
            };
            let admin_nodes = self.admin_nodes.clone().unwrap_or_default();
            for node in &admin_nodes {
                endpoint::validate_node_id(node).map_err(|e| invalid("admin_nodes", &e))?;
            }
            Ok(RemoteSecurityResolved::MutualTls {
                cert_path: required("cert_path", &self.cert_path)?,
                key_path: required("key_path", &self.key_path)?,
                ca_path: required("ca_path", &self.ca_path)?,
                require_crl: self.require_crl.unwrap_or(false),
                admin_nodes,
            })
        } else {
            if self.cert_path.is_some()
                || self.key_path.is_some()
                || self.ca_path.is_some()
                || self.require_crl.is_some()
                || self.admin_nodes.is_some()
            {
                return Err(invalid(
                    "mode",
                    "credentials, require_crl and admin_nodes require mtls mode",
                ));
            }
            match mode {
                "local" => Ok(RemoteSecurityResolved::Local),
                "insecure-development" => Ok(RemoteSecurityResolved::InsecureDevelopment),
                _ => Err(invalid(
                    "mode",
                    "must be local, mtls or insecure-development",
                )),
            }
        }
    }
}

/// `[remote.node]` 节点身份与拓扑。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct TransportSection {
    pub connection_capacity: Option<usize>,
    pub handshake_capacity: Option<usize>,
    pub heartbeat_interval_ms: Option<u64>,
    pub heartbeat_max_loss: Option<u32>,
    pub outbound_queue: Option<usize>,
    pub inbound_ask_capacity: Option<usize>,
    pub inbound_ask_bytes: Option<usize>,
    pub inbound_dispatch_capacity: Option<usize>,
    pub inbound_dispatch_per_connection: Option<usize>,
    pub inbound_dispatch_bytes: Option<usize>,
    pub default_hop_limit: Option<u8>,
}

/// `[remote.reorder]` TELL 重排旋钮。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReorderSection {
    pub gap_timeout_ms: Option<u64>,
    pub buffer_cap: Option<usize>,
    pub stream_capacity: Option<usize>,
    pub frame_capacity: Option<usize>,
    pub byte_capacity: Option<usize>,
    pub ordered_relays: Option<Vec<String>>,
    pub receiver_state: Option<OrderedReceiverState>,
    pub persist_sender_sessions: Option<bool>,
}

/// Cooperative, stable local directory for ordered receiver restart ownership.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedReceiverState {
    pub directory: std::path::PathBuf,
    pub realm: String,
}
impl OrderedReceiverState {
    pub fn validate(&self) -> Result<(), String> {
        if !self.directory.is_absolute() {
            return Err("ordered receiver state directory must be absolute".into());
        }
        endpoint::validate_node_id(&self.realm)
            .map_err(|e| format!("invalid ordered receiver realm: {e}"))
    }
}

/// `[remote.codec]` 编解码协商。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    pub fn connection_capacity() -> usize {
        1024
    }
    pub fn handshake_capacity() -> usize {
        64
    }
    pub fn heartbeat_interval_ms() -> u64 {
        2000
    }
    pub fn heartbeat_max_loss() -> u32 {
        5
    }
    pub fn outbound_queue() -> usize {
        1024
    }
    pub fn inbound_ask_capacity() -> usize {
        1024
    }
    pub fn inbound_ask_bytes() -> usize {
        64 * 1024 * 1024
    }
    pub fn inbound_dispatch_capacity() -> usize {
        1024
    }
    pub fn inbound_dispatch_per_connection() -> usize {
        64
    }
    pub fn inbound_dispatch_bytes() -> usize {
        67108864
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
    pub fn reorder_stream_capacity() -> usize {
        256
    }
    pub fn reorder_frame_capacity() -> usize {
        4096
    }
    pub fn reorder_byte_capacity() -> usize {
        64 * 1024 * 1024
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
    pub const fn max_context_tasks() -> usize {
        1024
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

/// `build()` 产物：数值与策略默认值已折叠，可选身份、端点和服务保留 Option。
#[derive(Debug, Clone)]
pub struct Resolved {
    pub tracing: tracing::TracingResolved,
    pub topology: topology::TopologyResolved,
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
    pub shared_work_stealing: bool,
    pub max_dedicated_threads: usize,
    pub default_mailbox_capacity: usize,
    pub max_context_tasks: usize,
    pub default_ask_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
}

impl ThreadResolved {
    /// Per-system worker and default queue admission limits, not an RSS budget.
    pub const MAX_WORKERS: usize = 4096;
    pub const MAX_QUEUE_CAPACITY: usize = 1_048_576;
    pub const MAX_CONTEXT_TASKS: usize = 65_536;

    /// Recheck public, mutable resolved values before allocating runtime resources.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (key, value, minimum, maximum) in [
            (
                "thread.shared_pool_size",
                self.shared_pool_size,
                1,
                Self::MAX_WORKERS,
            ),
            (
                "thread.shared_queue_capacity",
                self.shared_queue_capacity,
                1,
                Self::MAX_QUEUE_CAPACITY,
            ),
            (
                "thread.default_mailbox_capacity",
                self.default_mailbox_capacity,
                1,
                Self::MAX_QUEUE_CAPACITY,
            ),
            (
                "thread.max_dedicated_threads",
                self.max_dedicated_threads,
                0,
                Self::MAX_WORKERS,
            ),
            (
                "thread.max_context_tasks",
                self.max_context_tasks,
                0,
                Self::MAX_CONTEXT_TASKS,
            ),
        ] {
            if !(minimum..=maximum).contains(&value) {
                return Err(ConfigError::Invalid {
                    key: key.into(),
                    reason: format!("must be in {minimum}..={maximum}"),
                });
            }
        }
        if self
            .shared_pool_size
            .checked_add(self.shared_burst_workers_max)
            .is_none_or(|total| total > Self::MAX_WORKERS)
        {
            return Err(ConfigError::Invalid {
                key: "thread.shared_burst_workers_max".into(),
                reason: format!(
                    "core and burst workers together must be <= {}",
                    Self::MAX_WORKERS
                ),
            });
        }
        for (key, millis) in [
            (
                "thread.shared_burst_backlog_threshold_ms",
                self.shared_burst_backlog_threshold_ms,
            ),
            (
                "thread.shared_burst_idle_timeout_ms",
                self.shared_burst_idle_timeout_ms,
            ),
            ("thread.default_ask_timeout_ms", self.default_ask_timeout_ms),
            ("thread.shutdown_timeout_ms", self.shutdown_timeout_ms),
        ] {
            validate_thread_duration(key, std::time::Duration::from_millis(millis))?;
        }
        Ok(())
    }
}

/// Reject durations that cannot form a monotonic deadline on this platform.
/// Zero remains a valid immediate deadline or scaling threshold.
pub fn validate_thread_duration(
    key: &str,
    duration: std::time::Duration,
) -> Result<(), ConfigError> {
    if std::time::Instant::now().checked_add(duration).is_none() {
        return Err(ConfigError::Invalid {
            key: key.into(),
            reason: "duration cannot form a monotonic deadline on this platform".into(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct RemoteResolved {
    pub directory: Option<directory::DirectoryResolved>,
    pub receptionist: receptionist::ReceptionistResolved,
    pub security: RemoteSecurityResolved,
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
    pub connection_capacity: usize,
    pub handshake_capacity: usize,
    pub heartbeat_interval_ms: u64,
    pub heartbeat_max_loss: u32,
    pub outbound_queue: usize,
    pub inbound_ask_capacity: usize,
    pub inbound_ask_bytes: usize,
    pub inbound_dispatch_capacity: usize,
    pub inbound_dispatch_per_connection: usize,
    pub inbound_dispatch_bytes: usize,
    pub default_hop_limit: u8,
}

#[derive(Debug, Clone)]
pub struct ReorderResolved {
    pub gap_timeout_ms: u64,
    pub buffer_cap: usize,
    pub stream_capacity: usize,
    pub frame_capacity: usize,
    pub byte_capacity: usize,
    pub ordered_relays: Vec<String>,
    pub receiver_state: Option<OrderedReceiverState>,
    pub persist_sender_sessions: bool,
}

// ============================================================================
// Builder + 文件加载 + 合并
// ============================================================================

impl ParrotConfig {
    pub fn builder() -> Self {
        Self::default()
    }

    /// Load a required configuration file. Unknown keys and invalid value
    /// types are errors even if a higher-priority code value overrides them.
    /// The opened source must be a regular UTF-8 file. Raw and environment-
    /// expanded text must each fit [`MAX_CONFIG_BYTES`]. Symlinks to files work.
    pub fn load_file(self, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        self.read_file(path.as_ref(), false)
    }

    /// Load an optional file. Only NotFound is ignored; permission and parse
    /// failures remain errors. Use load_file for explicitly requested paths.
    pub fn load_optional_file(self, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        self.read_file(path.as_ref(), true)
    }

    fn read_file(self, path: &Path, optional: bool) -> Result<Self, ConfigError> {
        let raw = match input::read_source(path) {
            Ok(raw) => raw,
            Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(self)
            }
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.display().to_string(),
                    source,
                })
            }
        };
        let expanded = input::expand_env(&raw).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let file: Self = toml::from_str(&expanded).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        Ok(self.with_fallback(file))
    }

    /// PARROT_CONFIG is an explicit, required path. With no environment
    /// override, parrot.toml is optional to support zero-configuration startup.
    pub fn load_default_locations(self) -> Result<Self, ConfigError> {
        match std::env::var_os(CONFIG_ENV) {
            Some(path) => self.load_file(path),
            None => self.load_optional_file("parrot.toml"),
        }
    }

    /// Fill unset fields from a lower-priority layer, preserving every
    /// explicit value in this layer, including zero and empty collections.
    /// Validate the resulting effective configuration with build().
    pub fn with_fallback(mut self, fallback: Self) -> Self {
        macro_rules! merge {
            ($target:expr, $source:expr; $($field:ident),+ $(,)?) => {{
                let target = &mut $target;
                let source = $source;
                $(target.$field = target.$field.take().or(source.$field);)+
            }};
        }
        merge!(self.thread, fallback.thread; shared_pool_size, shared_burst_workers_max,
            shared_burst_backlog_threshold_ms, shared_burst_idle_timeout_ms,
            shared_queue_capacity, shared_work_stealing, max_dedicated_threads, default_mailbox_capacity,
            max_context_tasks, default_ask_timeout_ms, shutdown_timeout_ms);
        merge!(self.remote.node, fallback.remote.node; node_id, bind, topology_role, direct_addr, seeds, scheme);
        merge!(self.remote.transport, fallback.remote.transport; connection_capacity, handshake_capacity, heartbeat_interval_ms, heartbeat_max_loss, outbound_queue, inbound_ask_capacity, inbound_ask_bytes, inbound_dispatch_capacity, inbound_dispatch_per_connection, inbound_dispatch_bytes, default_hop_limit);
        merge!(self.remote.reorder, fallback.remote.reorder; gap_timeout_ms, buffer_cap, stream_capacity, frame_capacity, byte_capacity, ordered_relays, receiver_state, persist_sender_sessions);
        merge!(self.remote.codec, fallback.remote.codec; extra_caps);
        merge!(self.remote.security, fallback.remote.security; mode, cert_path, key_path, ca_path, require_crl, admin_nodes);
        merge!(self.remote.receptionist, fallback.remote.receptionist; default_deny, roles, relays, max_subscriptions, max_subscription_bytes, max_registrations, max_registration_bytes);
        merge!(self.tracing, fallback.tracing; enabled, otlp_endpoint, allow_insecure_http, capacity, socket, interval_ms, request_timeout_ms, max_attempts);
        self.topology = self.topology.with_fallback(fallback.topology);
        self.remote.directory = match (self.remote.directory, fallback.remote.directory) {
            (Some(primary), Some(fallback)) => Some(primary.with_fallback(fallback)),
            (primary, fallback) => primary.or(fallback),
        };
        self
    }

    /// Validate effective values and fill defaults, without DNS, certificate
    /// reads or runtime allocation. Runtime consumers still check their own
    /// capabilities and any subsequently mutated resolved values.
    pub fn build(self) -> Result<Resolved, ConfigError> {
        let tracing = self.tracing.resolve()?;
        let security = self.remote.security.resolve()?;
        let receptionist = self.remote.receptionist.resolve()?;
        let directory = self
            .remote
            .directory
            .as_ref()
            .map(|cfg| cfg.resolve(&self.remote.node, &security))
            .transpose()?;
        for (key, value) in [
            ("thread.shared_pool_size", self.thread.shared_pool_size),
            (
                "thread.shared_queue_capacity",
                self.thread.shared_queue_capacity,
            ),
            (
                "thread.default_mailbox_capacity",
                self.thread.default_mailbox_capacity,
            ),
            (
                "remote.transport.outbound_queue",
                self.remote.transport.outbound_queue,
            ),
        ] {
            if value == Some(0) {
                return Err(ConfigError::Invalid {
                    key: key.into(),
                    reason: "must be > 0".into(),
                });
            }
        }
        if let Some(role) = &self.remote.node.topology_role {
            if TopologyRoleValue::parse(role).is_none() {
                return Err(ConfigError::Invalid {
                    key: "remote.node.topology_role".into(),
                    reason: "must be normal, hub, border or directory".into(),
                });
            }
        }
        if let Some(scheme) = &self.remote.node.scheme {
            if !matches!(scheme.as_str(), "tcp" | "quic" | "mem") {
                return Err(ConfigError::Invalid {
                    key: "remote.node.scheme".into(),
                    reason: "must be tcp, quic or mem".into(),
                });
            }
        }
        // 校验层：非法值在 build 期报错（fail-fast，不带入运行时）
        let tr = &self.remote.transport;
        if tr.outbound_queue.is_some_and(|v| v > usize::MAX >> 3) {
            return Err(ConfigError::Invalid {
                key: "remote.transport.outbound_queue".into(),
                reason: format!("must be within 1..={}", usize::MAX >> 3),
            });
        }
        for (name, value) in [
            ("connection_capacity", tr.connection_capacity),
            ("handshake_capacity", tr.handshake_capacity),
            ("inbound_ask_capacity", tr.inbound_ask_capacity),
            ("inbound_ask_bytes", tr.inbound_ask_bytes),
            ("inbound_dispatch_capacity", tr.inbound_dispatch_capacity),
            (
                "inbound_dispatch_per_connection",
                tr.inbound_dispatch_per_connection,
            ),
            ("inbound_dispatch_bytes", tr.inbound_dispatch_bytes),
        ] {
            // Match Tokio semaphore bounds without coupling the config crate
            // to an async runtime; weighted permits also use u32.
            let max = (usize::MAX >> 3).min(u32::MAX as usize);
            if value.is_some_and(|v| v == 0 || v > max) {
                return Err(ConfigError::Invalid {
                    key: format!("remote.transport.{name}"),
                    reason: format!("must be within 1..={max}"),
                });
            }
        }
        for (name, value) in [
            ("stream_capacity", self.remote.reorder.stream_capacity),
            ("frame_capacity", self.remote.reorder.frame_capacity),
            ("byte_capacity", self.remote.reorder.byte_capacity),
        ] {
            let max = (usize::MAX >> 3).min(u32::MAX as usize);
            if value.is_some_and(|v| v == 0 || v > max) {
                return Err(ConfigError::Invalid {
                    key: format!("remote.reorder.{name}"),
                    reason: format!("must be within 1..={max}"),
                });
            }
        }
        if let Some(nodes) = &self.remote.reorder.ordered_relays {
            if nodes.len() > 4096
                || nodes
                    .iter()
                    .any(|node| endpoint::validate_node_id(node).is_err())
            {
                return Err(ConfigError::Invalid {
                    key: "remote.reorder.ordered_relays".into(),
                    reason: "must contain at most 4096 valid relay node IDs".into(),
                });
            }
        }
        if self.remote.reorder.persist_sender_sessions == Some(true)
            && self.remote.reorder.receiver_state.is_none()
        {
            return Err(ConfigError::Invalid {
                key: "remote.reorder.persist_sender_sessions".into(),
                reason: "requires remote.reorder.receiver_state".into(),
            });
        }
        if let Some(state) = &self.remote.reorder.receiver_state {
            state.validate().map_err(|reason| ConfigError::Invalid {
                key: "remote.reorder.receiver_state".into(),
                reason,
            })?;
        }
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
        let heartbeat = std::time::Duration::from_millis(
            tr.heartbeat_interval_ms
                .unwrap_or_else(Defaults::heartbeat_interval_ms),
        );
        let now = std::time::Instant::now();
        if now.checked_add(heartbeat).is_none()
            || heartbeat
                .checked_mul(
                    tr.heartbeat_max_loss
                        .unwrap_or_else(Defaults::heartbeat_max_loss),
                )
                .and_then(|deadline| now.checked_add(deadline))
                .is_none()
        {
            return Err(ConfigError::Invalid {
                key: "remote.transport.heartbeat_interval_ms".into(),
                reason: "heartbeat interval and loss deadline must fit the monotonic clock".into(),
            });
        }
        if let Some(v) = &self.remote.node.bind {
            if v.parse::<std::net::SocketAddr>().is_err() {
                return Err(ConfigError::Invalid {
                    key: "remote.node.bind".into(),
                    reason: format!("not a valid SocketAddr: {v}"),
                });
            }
        }
        if let Some(node_id) = &self.remote.node.node_id {
            endpoint::validate_node_id(node_id).map_err(|reason| ConfigError::Invalid {
                key: "remote.node.node_id".into(),
                reason,
            })?;
        }
        let scheme = self.remote.node.scheme.as_deref().unwrap_or("tcp");
        if let Some(address) = self
            .remote
            .node
            .direct_addr
            .as_deref()
            .filter(|_| scheme != "mem")
        {
            let invalid = |reason| ConfigError::Invalid {
                key: "remote.node.direct_addr".into(),
                reason,
            };
            if address.contains("://") || address.contains('@') {
                return Err(invalid(
                    "must be host:port without a scheme or node identity".into(),
                ));
            }
            endpoint::SeedEndpoint::parse(address, scheme).map_err(invalid)?;
        }
        for (index, seed) in self.remote.node.seeds.iter().flatten().enumerate() {
            let endpoint = endpoint::SeedEndpoint::parse(seed, scheme).map_err(|reason| {
                ConfigError::Invalid {
                    key: format!("remote.node.seeds[{index}]"),
                    reason,
                }
            })?;
            if endpoint.scheme != scheme {
                return Err(ConfigError::Invalid {
                    key: format!("remote.node.seeds[{index}]"),
                    reason: "seed transport must match the configured node transport".into(),
                });
            }
        }

        // 折叠：代码（Some）已优先生效（with_fallback 只填 None）→ unwrap_or 默认
        let role = self
            .remote
            .node
            .topology_role
            .as_deref()
            .and_then(TopologyRoleValue::parse)
            .unwrap_or_default();
        let resolved = Resolved {
            tracing,
            topology: self.topology.resolve(),
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
                shared_work_stealing: self.thread.shared_work_stealing.unwrap_or(false),
                max_dedicated_threads: self
                    .thread
                    .max_dedicated_threads
                    .unwrap_or_else(Defaults::max_dedicated_threads),
                default_mailbox_capacity: self
                    .thread
                    .default_mailbox_capacity
                    .unwrap_or_else(Defaults::default_mailbox_capacity),
                max_context_tasks: self
                    .thread
                    .max_context_tasks
                    .unwrap_or_else(Defaults::max_context_tasks),
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
                directory,
                receptionist,
                security,
                node: RemoteNodeResolved {
                    node_id: self.remote.node.node_id,
                    bind: self.remote.node.bind,
                    topology_role: role,
                    direct_addr: self.remote.node.direct_addr,
                    seeds: self.remote.node.seeds.unwrap_or_default(),
                    scheme: self.remote.node.scheme.unwrap_or_else(|| "tcp".into()),
                },
                transport: TransportResolved {
                    connection_capacity: tr
                        .connection_capacity
                        .unwrap_or_else(Defaults::connection_capacity),
                    handshake_capacity: tr
                        .handshake_capacity
                        .unwrap_or_else(Defaults::handshake_capacity),
                    heartbeat_interval_ms: tr
                        .heartbeat_interval_ms
                        .unwrap_or_else(Defaults::heartbeat_interval_ms),
                    heartbeat_max_loss: tr
                        .heartbeat_max_loss
                        .unwrap_or_else(Defaults::heartbeat_max_loss),
                    outbound_queue: tr.outbound_queue.unwrap_or_else(Defaults::outbound_queue),
                    inbound_ask_capacity: tr
                        .inbound_ask_capacity
                        .unwrap_or_else(Defaults::inbound_ask_capacity),
                    inbound_ask_bytes: tr
                        .inbound_ask_bytes
                        .unwrap_or_else(Defaults::inbound_ask_bytes),
                    inbound_dispatch_capacity: tr
                        .inbound_dispatch_capacity
                        .unwrap_or_else(Defaults::inbound_dispatch_capacity),
                    inbound_dispatch_per_connection: tr
                        .inbound_dispatch_per_connection
                        .unwrap_or_else(Defaults::inbound_dispatch_per_connection),
                    inbound_dispatch_bytes: tr
                        .inbound_dispatch_bytes
                        .unwrap_or_else(Defaults::inbound_dispatch_bytes),
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
                    stream_capacity: self
                        .remote
                        .reorder
                        .stream_capacity
                        .unwrap_or_else(Defaults::reorder_stream_capacity),
                    frame_capacity: self
                        .remote
                        .reorder
                        .frame_capacity
                        .unwrap_or_else(Defaults::reorder_frame_capacity),
                    byte_capacity: self
                        .remote
                        .reorder
                        .byte_capacity
                        .unwrap_or_else(Defaults::reorder_byte_capacity),
                    ordered_relays: self
                        .remote
                        .reorder
                        .ordered_relays
                        .clone()
                        .unwrap_or_default(),
                    receiver_state: self.remote.reorder.receiver_state.clone(),
                    persist_sender_sessions: self
                        .remote
                        .reorder
                        .persist_sender_sessions
                        .unwrap_or(false),
                },
                extra_caps: self.remote.codec.extra_caps.unwrap_or(0),
            },
        };
        resolved.thread.validate()?;
        Ok(resolved)
    }

    /// 全键文档，供宿主程序展示运维参考；本 crate 不提供独立命令行程序。
    pub fn documented() -> BTreeMap<&'static str, &'static str> {
        let mut m = BTreeMap::new();
        m.insert("tracing.enabled", "Application runner recording/export switch; defaults to endpoint presence; false suppresses inherited tracing; replay does not activate it");
        m.insert("tracing.otlp_endpoint", "Exact OTLP/HTTP JSON collector URL, at most 8192 bytes; HTTPS by default; validated by the application runner before owning resources");
        m.insert("tracing.allow_insecure_http", "Explicit plaintext collector permission (default false); does not affect actor transport authentication");
        m.insert(
            "tracing.capacity",
            "Completed span/history capacity, 1..=65536 (default 4096)",
        );
        m.insert("tracing.socket", "Optional private Unix query socket path; relative to runner working directory; requires enabled tracing");
        m.insert(
            "tracing.interval_ms",
            "Export polling interval, 1..=3600000 milliseconds (default 1000)",
        );
        m.insert(
            "tracing.request_timeout_ms",
            "Per-attempt collector HTTP timeout, 1..=3600000 milliseconds (default 5000)",
        );
        m.insert(
            "tracing.max_attempts",
            "Total HTTP attempts per batch including the first, 1..=1024 (default 8)",
        );
        m.insert(
            "remote.receptionist.default_deny",
            "Deny unmatched node/service pairs (default true); false explicitly permits unmatched pairs",
        );
        m.insert(
            "remote.receptionist.roles",
            "Node ID to service-key prefix grants; complete segment matching; higher-priority maps replace all inherited grants",
        );
        m.insert("remote.receptionist.max_subscriptions", "Maximum retained service receivers (default 1024); closed or drained receivers count until dropped; zero disables admission");
        m.insert("remote.receptionist.relays", "Authenticated relay node to forwarded service-key prefixes; denies forwarding by default; replaces the entire inherited map; owner grants still apply");
        m.insert("remote.receptionist.max_subscription_bytes", "Reserved service receiver queue bytes (default 268435456); slots charged at the fixed event ceiling plus queue allowance; zero disables admission");
        m.insert("remote.receptionist.max_registrations", "Distinct service entries across local and remote nodes (default 65536); zero disables additional entries");
        m.insert("remote.receptionist.max_registration_bytes", "Declared retained service-table bytes (default 268435456); zero disables additional entries; not process RSS");
        m.insert(
            "topology.mode",
            "Originating business policy: hub|mesh|hybrid (default hybrid)",
        );
        m.insert(
            "topology.relay_fallback",
            "Hub: direct fallback; hybrid: relay fallback; mesh: no relay",
        );
        m.insert(
            "remote.directory.storage_dir",
            "现有私有目录；显式开启持久Directory宿主",
        );
        m.insert("remote.directory.group", "固定Raft组身份，1–1011字节");
        m.insert(
            "remote.directory.peers",
            "显式投票成员，与发现seeds独立；[]为单节点组",
        );
        m.insert(
            "remote.directory.election_ms",
            "选举基准毫秒；默认2000，抖动窗口同基准",
        );
        m.insert("remote.directory.heartbeat_ms", "Raft心跳毫秒；默认150");
        m.insert(
            "remote.directory.rpc_timeout_ms",
            "Raft网络尝试总毫秒；默认5000",
        );
        m.insert(
            "thread.shared_pool_size",
            "共享调度池核心worker任务数1..=4096（默认CPU核数）；与burst合计不超过4096",
        );
        m.insert(
            "thread.shared_burst_workers_max",
            "弹性突发worker上限（默认CPU核数；0禁用）；与core合计不超过4096",
        );
        m.insert(
            "thread.shared_burst_backlog_threshold_ms",
            "队列积压多久后扩突发 worker（默认 100ms）",
        );
        m.insert(
            "thread.shared_burst_idle_timeout_ms",
            "突发 worker 空闲多久回收（默认 5000ms）",
        );
        m.insert(
            "thread.shared_queue_capacity",
            "共享就绪队列容量1..=1048576（默认10000）；不限制actor总数",
        );
        m.insert(
            "thread.shared_work_stealing",
            "共享池worker本地FIFO与工作窃取（默认false）；所有就绪队列共用容量",
        );
        m.insert(
            "thread.max_context_tasks",
            "每个actor待完成用户定时器/stream上限0..=65536（默认1024；0禁用）",
        );
        m.insert(
            "thread.max_dedicated_threads",
            "专用线程上限0..=4096（默认32；0禁用）",
        );
        m.insert(
            "thread.default_mailbox_capacity",
            "actor默认邮箱容量1..=1048576（默认1024）",
        );
        m.insert(
            "thread.default_ask_timeout_ms",
            "ask 默认超时（默认 5000ms）",
        );
        m.insert("thread.shutdown_timeout_ms", "系统关闭超时（默认 10000ms）");
        m.insert(
            "remote.node.node_id",
            "节点唯一 id（部署身份——必填于多节点拓扑）",
        );
        m.insert(
            "remote.node.bind",
            "监听IP与端口（如0.0.0.0:9801或[::1]:9801）；不解析域名",
        );
        m.insert(
            "remote.node.topology_role",
            "拓扑角色 normal|hub|border|directory（默认 normal）",
        );
        m.insert(
            "remote.node.direct_addr",
            "网络传输的公布地址host:port，不含scheme或node@；域名保持未解析；mem忽略此项",
        );
        m.insert(
            "remote.node.seeds",
            "种子节点地址列表 [\"tcp://host:port\"]",
        );
        m.insert("remote.node.scheme", "传输载体 tcp|quic|mem（默认 tcp）");
        m.insert(
            "remote.security.mode",
            "local（默认，仅回环且禁止网络管理命令）|mtls|insecure-development",
        );
        m.insert(
            "remote.security.cert_path",
            "mtls 节点 PEM 证书；CN 与 SAN 对应 node_id",
        );
        m.insert("remote.security.key_path", "mtls 节点 PEM 私钥路径");
        m.insert("remote.security.ca_path", "mtls 信任 CA PEM 集合路径");
        m.insert(
            "remote.security.require_crl",
            "mtls 启动时要求 CA PEM 包含签发者撤销列表",
        );
        m.insert(
            "remote.security.admin_nodes",
            "允许管理命令的已认证节点 ID 列表；默认空",
        );
        m.insert(
            "remote.transport.heartbeat_interval_ms",
            "正数心跳间隔（默认2000ms）；间隔及间隔乘丢失次数须可形成单调时钟期限",
        );
        m.insert(
            "remote.transport.heartbeat_max_loss",
            "心跳丢失多少次判半开断开（默认 5）",
        );
        m.insert(
            "remote.transport.connection_capacity",
            "物理连接总上限（含握手中连接，默认 1024）",
        );
        m.insert(
            "remote.transport.handshake_capacity",
            "并发建连与握手上限（默认 64）",
        );
        m.insert(
            "remote.transport.outbound_queue",
            "出站帧队列容量1..=usize::MAX>>3（默认1024，满时反压）；不是RSS预算",
        );
        m.insert(
            "remote.transport.inbound_ask_capacity",
            "入站 ASK 并发上限（默认 1024）",
        );
        m.insert(
            "remote.transport.inbound_ask_bytes",
            "已接收 ASK 的编码长度总上限（默认 67108864 字节）",
        );
        m.insert(
            "remote.transport.inbound_dispatch_capacity",
            "慢入站分发总数量上限（含排队与执行，默认 1024）",
        );
        m.insert(
            "remote.transport.inbound_dispatch_per_connection",
            "每条连接的慢入站分发数量上限（默认 64）",
        );
        m.insert(
            "remote.transport.inbound_dispatch_bytes",
            "慢入站分发保留的载荷和字符串字节预算（默认 67108864）",
        );
        m.insert(
            "remote.transport.default_hop_limit",
            "帧默认跳数上限（默认 8）",
        );
        m.insert(
            "remote.reorder.gap_timeout_ms",
            "TELL 重排缺口等待上限（默认 250ms）",
        );
        m.insert("remote.reorder.buffer_cap", "重排缓冲帧数上限（默认 1024）");
        m.insert(
            "remote.reorder.stream_capacity",
            "节点累计存活重排worker上限（默认256）",
        );
        m.insert(
            "remote.reorder.frame_capacity",
            "节点累计重排排队、缓冲、投递帧上限（默认4096）",
        );
        m.insert(
            "remote.reorder.byte_capacity",
            "节点累计重排编码字节上限，含帧头和长度前缀（默认67108864）",
        );
        m.insert(
            "remote.reorder.ordered_relays",
            "可为有序TELL原始来源背书的已认证中继节点（默认空）",
        );
        m.insert(
            "remote.reorder.persist_sender_sessions",
            "将发送会话的关闭信息持久化到实例恢复目录（默认关闭）",
        );
        m.insert(
            "remote.reorder.receiver_state.directory",
            "可选接收实例恢复目录，须为稳定本机绝对路径",
        );
        m.insert(
            "remote.reorder.receiver_state.realm",
            "恢复目录绑定的独立集群作用域，须与node身份共同保持",
        );
        m.insert(
            "remote.codec.extra_caps",
            "附加能力位（连 pb-only 对端时叠加）",
        );
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
    pub fn thread_shared_work_stealing(mut self, v: bool) -> Self {
        self.thread.shared_work_stealing = Some(v);
        self
    }
    pub fn thread_default_mailbox_capacity(mut self, v: usize) -> Self {
        self.thread.default_mailbox_capacity = Some(v);
        self
    }
    pub fn thread_max_context_tasks(mut self, v: usize) -> Self {
        self.thread.max_context_tasks = Some(v);
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
    pub fn topology_mode(mut self, mode: topology::TopologyMode) -> Self {
        self.topology.mode = Some(mode);
        self
    }
    pub fn topology_relay_fallback(mut self, enabled: bool) -> Self {
        self.topology.relay_fallback = Some(enabled);
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
    pub fn remote_inbound_ask_capacity(mut self, v: usize) -> Self {
        self.remote.transport.inbound_ask_capacity = Some(v);
        self
    }
    pub fn remote_inbound_ask_bytes(mut self, v: usize) -> Self {
        self.remote.transport.inbound_ask_bytes = Some(v);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values_are_literal_even_when_self_referential() {
        let mut lookups = Vec::new();
        let expanded = expand_env_with("前:${SELF}|${A}|${B}:后", |name| {
            lookups.push(name.to_owned());
            assert!(
                lookups.len() <= 3,
                "replacement values must not be rescanned"
            );
            Some(
                match name {
                    "SELF" => "${SELF}",
                    "A" => "${B}",
                    "B" => "${A}",
                    _ => unreachable!(),
                }
                .into(),
            )
        })
        .unwrap();
        assert_eq!(expanded, "前:${SELF}|${B}|${A}:后");
        assert_eq!(lookups, ["SELF", "A", "B"]);
    }

    #[test]
    fn env_defaults_cover_missing_and_empty_but_preserve_nonempty_values() {
        let expanded = expand_env_with(
            "${MISSING:-后备}:${EMPTY:-empty}:${SET:-unused}:${MISSING}:${SET}:${MISSING:-a:-b}",
            |name| match name {
                "SET" => Some("零".into()),
                "EMPTY" => Some(String::new()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(expanded, "后备:empty:零::零:a:-b");
    }

    #[test]
    fn env_expansion_preserves_incomplete_tokens_and_literal_dollars() {
        assert_eq!(
            expand_env_with("前 $x ${OK} 后 ${unfinished", |_| Some("值".into())).unwrap(),
            "前 $x 值 后 ${unfinished"
        );
        assert_eq!(
            expand_env_with("no placeholders", |_| unreachable!()).unwrap(),
            "no placeholders"
        );
        let mut lookups = 0;
        assert_eq!(
            expand_env_with("${X}", |_| {
                lookups += 1;
                assert_eq!(lookups, 1, "replacement values must not be rescanned");
                Some("$${unfinished}".into())
            })
            .unwrap(),
            "$${unfinished}"
        );
    }

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
        let r = ParrotConfig::builder()
            .load_file(&tmp)
            .unwrap()
            .build()
            .unwrap();
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
        std::fs::write(&tmp, "[remote.transport]\nheartbeat_interval_ms = 999\n").unwrap();
        let r = ParrotConfig::builder()
            .load_file(&tmp)
            .unwrap()
            .remote_heartbeat_interval_ms(111) // 代码层
            .build()
            .unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(
            r.remote.transport.heartbeat_interval_ms, 111,
            "代码层必须赢"
        );
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

    // C5：可选文件不存在时才跳过，显式 load_file 不吞掉路径错误。
    #[test]
    fn c5_missing_file_ok() {
        let r = ParrotConfig::builder()
            .load_optional_file("/nonexistent/parrot.toml")
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
        assert!(ParrotConfig::builder()
            .remote_default_hop_limit(0)
            .build()
            .is_err());
        assert!(ParrotConfig::builder()
            .remote_default_hop_limit(65)
            .build()
            .is_err());
        assert!(ParrotConfig::builder()
            .remote_default_hop_limit(8)
            .build()
            .is_ok());
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
        // Set the environment before the child test harness starts. Mutating
        // process-global environment in a parallel test harness is unsound on
        // platforms whose environment APIs are not thread-safe.
        const CHILD: &str = "PCFG_ENV_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::e1_env_expansion", "--nocapture"])
                .env(CHILD, "1")
                .env("PCFG_TEST_NODE", "env-node-42")
                .env("PCFG_TEST_EMPTY", "")
                .env("PCFG_TEST_LOOP", "${PCFG_TEST_LOOP}")
                .env_remove("PCFG_TEST_UNSET")
                .spawn()
                .unwrap();
            // Guard an accidental return of the original infinite loop.
            // Include process startup allowance for instrumented/debug hosts.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("configuration expansion subprocess did not terminate");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            return;
        }
        let tmp = std::env::temp_dir().join(format!("pcfg_e1_{}.toml", std::process::id()));
        std::fs::write(
            &tmp,
            "[remote.node]\nnode_id = \"${PCFG_TEST_NODE}\"\nbind = \"${PCFG_TEST_UNSET:-127.0.0.1:9801}\"\n",
        )
        .unwrap();
        let r = ParrotConfig::builder()
            .load_file(&tmp)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(r.remote.node.node_id.as_deref(), Some("env-node-42"));
        assert_eq!(r.remote.node.bind.as_deref(), Some("127.0.0.1:9801"));

        std::fs::write(&tmp,
            "[remote.node]\nnode_id = \"${PCFG_TEST_LOOP}\"\nbind = \"${PCFG_TEST_EMPTY:-127.0.0.1:9802}\"\n",
        ).unwrap();
        let r = ParrotConfig::builder()
            .load_file(&tmp)
            .unwrap()
            .build()
            .unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(r.remote.node.node_id.as_deref(), Some("${PCFG_TEST_LOOP}"));
        assert_eq!(r.remote.node.bind.as_deref(), Some("127.0.0.1:9802"));
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
[tracing]
enabled = true
otlp_endpoint = "https://collector.example/v1/traces"
allow_insecure_http = false
capacity = 512
socket = "/private/parrot/trace.sock"
interval_ms = 200
request_timeout_ms = 1500
max_attempts = 3

[topology]
mode = "hub"
relay_fallback = false

[thread]
shared_pool_size = 3
shared_burst_workers_max = 2
shared_burst_backlog_threshold_ms = 50
shared_burst_idle_timeout_ms = 3000
shared_queue_capacity = 500
shared_work_stealing = true
max_dedicated_threads = 8
default_mailbox_capacity = 64
max_context_tasks = 42
default_ask_timeout_ms = 1500
shutdown_timeout_ms = 2500

[remote.node]
node_id = "hub-1"
bind = "0.0.0.0:9800"
topology_role = "hub"
direct_addr = "10.1.1.1:9800"
seeds = ["tcp://seed-a:9800", "tcp://seed-b:9800"]
scheme = "tcp"

[remote.transport]
connection_capacity = 127
handshake_capacity = 11
heartbeat_interval_ms = 1500
heartbeat_max_loss = 3
outbound_queue = 512
inbound_ask_capacity = 19
inbound_ask_bytes = 98304
inbound_dispatch_capacity = 23
inbound_dispatch_per_connection = 5
inbound_dispatch_bytes = 131072
default_hop_limit = 12

[remote.reorder]
gap_timeout_ms = 100
buffer_cap = 128
stream_capacity = 17
frame_capacity = 257
byte_capacity = 262144
ordered_relays = ["hub-1", "relay-1"]
persist_sender_sessions = true
[remote.reorder.receiver_state]
directory = "/var/lib/parrot/ordered-receiver"
realm = "contract-cluster"

[remote.codec]
extra_caps = 2

[remote.security]
mode = "mtls"
cert_path = "cert.pem"
key_path = "key.pem"
ca_path = "ca.pem"
require_crl = true
admin_nodes = ["operator"]

[remote.receptionist]
default_deny = true
roles = { hub-1 = ["edge/public"], peer-1 = ["cloud/query"] }
relays = { relay-1 = ["edge/public"] }
max_subscriptions = 512
max_subscription_bytes = 134217728
max_registrations = 8192
max_registration_bytes = 67108864

[remote.directory]
storage_dir = "/private/directory"
group = "services"
peers = ["tcp://voter@127.0.0.1:9801"]
election_ms = 1800
heartbeat_ms = 90
rpc_timeout_ms = 2400
"#,
        )
        .unwrap();
        let layer = ParrotConfig::builder().load_file(&tmp).unwrap();
        let serialized = toml::to_string(&layer).unwrap();
        assert_eq!(
            toml::to_string(&ParrotConfig::default().with_fallback(layer.clone())).unwrap(),
            serialized,
            "every configured field must survive layer merging"
        );
        fn collect_keys(
            value: &toml::Value,
            prefix: &str,
            keys: &mut std::collections::BTreeSet<String>,
        ) {
            if matches!(
                prefix,
                "remote.receptionist.roles" | "remote.receptionist.relays"
            ) {
                keys.insert(prefix.to_owned());
                return;
            }
            if let Some(table) = value.as_table() {
                for (key, value) in table {
                    let key = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    collect_keys(value, &key, keys);
                }
            } else {
                keys.insert(prefix.to_owned());
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        collect_keys(
            &toml::from_str::<toml::Value>(&serialized).unwrap(),
            "",
            &mut keys,
        );
        assert_eq!(
            keys,
            ParrotConfig::documented()
                .keys()
                .map(|key| key.to_string())
                .collect(),
            "documentation must enumerate exactly the exercised configuration keys"
        );
        let r = layer.build().unwrap();
        assert!(r.tracing.enabled);
        assert_eq!(
            r.tracing.otlp_endpoint.as_deref(),
            Some("https://collector.example/v1/traces")
        );
        assert!(!r.tracing.allow_insecure_http);
        assert_eq!(r.tracing.capacity, 512);
        assert_eq!(
            r.tracing.socket.as_deref(),
            Some(Path::new("/private/parrot/trace.sock"))
        );
        assert_eq!(r.tracing.interval_ms, 200);
        assert_eq!(r.tracing.request_timeout_ms, 1500);
        assert_eq!(r.tracing.max_attempts, 3);
        assert_eq!(r.topology.mode, topology::TopologyMode::Hub);
        assert!(!r.topology.relay_fallback);
        std::fs::remove_file(&tmp).ok();
        assert_eq!(r.thread.shared_pool_size, 3);
        assert_eq!(r.thread.shared_burst_workers_max, 2);
        assert_eq!(r.thread.shared_burst_backlog_threshold_ms, 50);
        assert_eq!(r.thread.shared_burst_idle_timeout_ms, 3000);
        assert_eq!(r.thread.shared_queue_capacity, 500);
        assert!(r.thread.shared_work_stealing);
        assert_eq!(r.thread.max_dedicated_threads, 8);
        assert_eq!(r.thread.default_mailbox_capacity, 64);
        assert_eq!(r.thread.max_context_tasks, 42);
        assert_eq!(r.thread.default_ask_timeout_ms, 1500);
        assert_eq!(r.thread.shutdown_timeout_ms, 2500);
        assert_eq!(r.remote.node.node_id.as_deref(), Some("hub-1"));
        assert_eq!(r.remote.node.bind.as_deref(), Some("0.0.0.0:9800"));
        assert_eq!(r.remote.node.topology_role, TopologyRoleValue::Hub);
        assert_eq!(r.remote.node.direct_addr.as_deref(), Some("10.1.1.1:9800"));
        assert_eq!(r.remote.node.seeds.len(), 2);
        assert_eq!(r.remote.transport.heartbeat_interval_ms, 1500);
        assert_eq!(r.remote.transport.heartbeat_max_loss, 3);
        assert_eq!(r.remote.transport.connection_capacity, 127);
        assert_eq!(r.remote.transport.handshake_capacity, 11);
        assert_eq!(r.remote.transport.outbound_queue, 512);
        assert_eq!(r.remote.transport.inbound_ask_capacity, 19);
        assert_eq!(r.remote.transport.inbound_ask_bytes, 98304);
        assert_eq!(r.remote.transport.inbound_dispatch_capacity, 23);
        assert_eq!(r.remote.transport.inbound_dispatch_per_connection, 5);
        assert_eq!(r.remote.transport.inbound_dispatch_bytes, 131072);
        assert_eq!(r.remote.transport.default_hop_limit, 12);
        assert_eq!(r.remote.reorder.gap_timeout_ms, 100);
        assert_eq!(r.remote.reorder.buffer_cap, 128);
        assert_eq!(r.remote.reorder.stream_capacity, 17);
        assert!(r.remote.reorder.persist_sender_sessions);
        assert_eq!(r.remote.reorder.frame_capacity, 257);
        assert_eq!(r.remote.reorder.byte_capacity, 262144);
        assert_eq!(r.remote.reorder.ordered_relays, ["hub-1", "relay-1"]);
        assert_eq!(r.remote.extra_caps, 2);
        assert!(matches!(
            r.remote.security,
            RemoteSecurityResolved::MutualTls {
                require_crl: true,
                ..
            }
        ));
        assert!(r.remote.receptionist.default_deny);
        assert_eq!(r.remote.receptionist.roles.len(), 2);
        assert_eq!(r.remote.receptionist.roles["hub-1"], ["edge/public"]);
        assert_eq!(r.remote.receptionist.roles["peer-1"], ["cloud/query"]);
        assert_eq!(r.remote.receptionist.relays.len(), 1);
        assert_eq!(r.remote.receptionist.relays["relay-1"], ["edge/public"]);
        assert_eq!(r.remote.receptionist.subscriptions.max_subscriptions, 512);
        assert_eq!(r.remote.receptionist.registrations.max_registrations, 8192);
        assert_eq!(
            r.remote.receptionist.registrations.max_reserved_bytes,
            67108864
        );
        assert_eq!(
            r.remote.receptionist.subscriptions.max_reserved_bytes,
            134217728
        );
        let directory = r.remote.directory.unwrap();
        assert_eq!(directory.storage_dir, "/private/directory");
        assert_eq!(directory.group, "services");
        assert_eq!(directory.peers, ["tcp://voter@127.0.0.1:9801"]);
        assert_eq!(directory.election_ms, 1800);
        assert_eq!(directory.heartbeat_ms, 90);
        assert_eq!(directory.rpc_timeout_ms, 2400);
    }

    // topology_role 解析全枚举 + 非法值拒绝
    #[test]
    fn f2_role_parsing() {
        assert_eq!(
            TopologyRoleValue::parse("hub"),
            Some(TopologyRoleValue::Hub)
        );
        assert_eq!(
            TopologyRoleValue::parse("HUB"),
            Some(TopologyRoleValue::Hub)
        );
        assert_eq!(
            TopologyRoleValue::parse("border"),
            Some(TopologyRoleValue::Border)
        );
        assert_eq!(
            TopologyRoleValue::parse("directory"),
            Some(TopologyRoleValue::Directory)
        );
        assert_eq!(TopologyRoleValue::parse("nonsense"), None);
        let error = ParrotConfig::builder()
            .remote_topology_role("nonsense")
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("topology_role"));
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
