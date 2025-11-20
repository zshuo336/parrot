//! AppManifest 数据模型（DEV_09 §3.1 A1——09 §2.2 原样落地）。
//!
//! 应用定义与运行时执行解耦的单一事实源：Rust 类型（程序化构造/测试断言）
//! + TOML 文件（`*.app.toml`，运维可读可 diff），serde 互转。
//!
//! 分层铁律（09 §2.2）：本 crate 不依赖 `parrot` 主 crate——引擎访问经
//! parrot-remote 的 PropsFactory/facade 协议。

use crate::limits::{AdmissionError, ApplicationLimits};
use std::collections::BTreeMap;
use std::path::Path;

// ============================================================================
// 核心类型（09 §2.2 / §5.3 / §6.1 原样）
// ============================================================================

/// 一个多引擎应用的完整声明。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    pub name: String,
    /// semver 文法校验（手写校验省依赖——A1 规约）。
    pub version: String,
    pub components: Vec<ComponentSpec>,
    #[serde(default)]
    pub wiring: Vec<WireSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<crate::mirror::MirrorSpec>,
    /// 并入 parrot-config 文件层之前（Y3：overlay > 代码 > parrot.toml > 默认）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_overlay: Option<toml::value::Table>,
}

/// 组件声明（09 §2.2 ComponentSpec 原样）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentSpec {
    pub name: String,
    pub engine: EngineKind,
    pub artifact: ArtifactRef,
    /// R2 双形态：主 artifact 之外的备选形态（如 Dylib 主 + Wasm 沙箱）。
    /// 部署系统按节点能力/策略选择；两形态须同键同行为。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_artifact: Option<ArtifactRef>,
    #[serde(default)]
    pub instances: InstancePolicy,
    #[serde(default)]
    pub placement: PlacementConstraint,
    #[serde(default)]
    pub upgrade: UpgradePolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<toml::value::Table>,
    #[serde(default)]
    pub hooks: ComponentHooks,
}

pub(crate) enum HookPathError {
    Outside,
    Invalid,
    Empty,
}

struct HookDiagnostic<'a> {
    error: HookPathError,
    hook: &'a str,
    component: &'a str,
}
impl std::fmt::Display for HookDiagnostic<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hook = self.hook;
        let component = self.component;
        match self.error {
            HookPathError::Outside => {
                write!(formatter, "hook {hook:?} is outside component {component}")
            }
            HookPathError::Invalid => write!(
                formatter,
                "invalid hook path {hook:?} for component {component}"
            ),
            HookPathError::Empty => write!(
                formatter,
                "empty hook path/segment for component {component}"
            ),
        }
    }
}

impl ComponentSpec {
    /// Resolve a component-relative hook for one concrete instance. Absolute
    /// paths use the logical `/user/{component}` root, expanded for every pool
    /// or shard instance; `.` addresses the instance root itself.
    pub fn hook_path(&self, hook: &str, instance: &str) -> Result<String, String> {
        let suffix = self.hook_suffix(hook).map_err(|error| {
            HookDiagnostic {
                error,
                hook,
                component: &self.name,
            }
            .to_string()
        })?;
        Ok(if suffix.is_empty() {
            instance.into()
        } else {
            format!("{instance}/{suffix}")
        })
    }

    // Borrow the canonical suffix so admission can count cache keys without
    // allocating roots, expanded paths, or potentially large error strings.
    pub(crate) fn hook_suffix<'a>(&self, hook: &'a str) -> Result<&'a str, HookPathError> {
        let suffix = if hook == "." {
            ""
        } else if hook.starts_with('/') {
            let rest = hook
                .strip_prefix("/user/")
                .and_then(|path| path.strip_prefix(self.name.as_str()))
                .ok_or(HookPathError::Outside)?;
            if rest.is_empty() {
                ""
            } else {
                rest.strip_prefix('/').ok_or(HookPathError::Outside)?
            }
        } else {
            hook
        };
        if !suffix.is_empty()
            && suffix
                .split('/')
                .any(|segment| !valid_path_segment(segment))
        {
            return Err(HookPathError::Invalid);
        }
        if hook.is_empty() || hook.ends_with('/') {
            return Err(HookPathError::Empty);
        }
        Ok(suffix)
    }

    /// A lossless TOML payload shared by local gateways and the reconciler.
    pub fn deployment(
        &self,
        version: &str,
    ) -> Result<parrot_remote::admin_v2::ComponentDeploy, String> {
        self.deployment_with_limits(version, &ApplicationLimits::default())
    }

    /// Build a wire payload using the caller's host policy before cloning or encoding.
    pub fn deployment_with_limits(
        &self,
        version: &str,
        limits: &ApplicationLimits,
    ) -> Result<parrot_remote::admin_v2::ComponentDeploy, String> {
        use parrot_remote::admin_v2::{AdminInstancePolicy, ComponentDeploy};
        limits.check_component(self).map_err(|e| e.to_string())?;
        limits
            .check_metadata(&(self, version))
            .map_err(|e| e.to_string())?;
        if !valid_path_segment(&self.name) {
            return Err("invalid deployment component name".into());
        }
        if !self.engine.accepts(&self.artifact) {
            return Err("deployment artifact is incompatible with component engine".into());
        }
        let count = self.instances.instance_count();
        if count == 0 || count > u32::MAX as usize {
            return Err("instance count must fit 1..=u32::MAX".into());
        }
        if !is_valid_semver(version) {
            return Err("invalid deployment version: expected semver".into());
        }
        let mut remaining = limits.max_metadata_bytes;
        let artifact = self.artifact.encode_admin(limits, &mut remaining)?;
        let config = self
            .config
            .as_ref()
            .map(|config| crate::encoding::table(config, &mut remaining).map(String::into_bytes))
            .transpose()?;
        let payload = ComponentDeploy {
            name: self.name.clone(),
            version: version.into(),
            artifact,
            instances: match self.instances {
                InstancePolicy::Singleton | InstancePolicy::Ephemeral => {
                    AdminInstancePolicy::Singleton
                }
                InstancePolicy::Pool(count) => AdminInstancePolicy::Pool { count },
                InstancePolicy::Sharded(count) => AdminInstancePolicy::Sharded { count },
            },
            config,
        };
        limits.check_metadata(&payload).map_err(|e| e.to_string())?;
        Ok(payload)
    }
}

/// 引擎目标（09 §2.2：六引擎枚举——Lite* 网关进程自备产物）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineKind {
    Parrot,
    Akka,
    Ray,
    Erlang,
    LiteTs,
    LiteCpp,
}

impl EngineKind {
    /// 能承载的 artifact 形态（匹配表——A1 行为规约 ③）。
    pub fn accepts(&self, a: &ArtifactRef) -> bool {
        match (self, a) {
            (EngineKind::Parrot, ArtifactRef::Props { .. })
            | (EngineKind::Parrot, ArtifactRef::Wasm { .. })
            | (EngineKind::Parrot, ArtifactRef::Dylib { .. }) => true,
            (EngineKind::Akka, ArtifactRef::Jvm { .. }) => true,
            (EngineKind::Ray, ArtifactRef::PyModule { .. }) => true,
            (EngineKind::Erlang, ArtifactRef::Beam { .. }) => true,
            // Lite*：网关进程自备——无 artifact 声明（Any 占位容忍）
            (EngineKind::LiteTs, _) | (EngineKind::LiteCpp, _) => true,
            _ => false,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Parrot => "parrot",
            EngineKind::Akka => "akka",
            EngineKind::Ray => "ray",
            EngineKind::Erlang => "erlang",
            EngineKind::LiteTs => "lite-ts",
            EngineKind::LiteCpp => "lite-cpp",
        }
    }
}

/// 部署产物引用（09 §2.2 六形态原样）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ArtifactRef {
    /// parrot：编译期工厂名（host 使用 inventory ThreadPropsFactory）。
    Props { factory: String },
    /// parrot：wasm component（wasmtime）。
    Wasm { digest: String, uri: String },
    /// parrot：动态库（09 §4.3 四步卸载协议）。
    Dylib {
        digest: String,
        uri: String,
        abi: u32,
    },
    /// akka 网关：child-first URLClassLoader 加载。
    ///
    /// R3：`uri` 指向 app 构建产物（jar 路径——file:// 形态）；缺省时
    /// coords 兼容旧形态（坐标/内联 uri）。
    Jvm {
        main_class: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        coords: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    /// ray 网关：python 模块（ray job API working_dir）。
    ///
    /// R3：`uri` 指向 app 源码目录（working_dir——file:// 形态）；缺省时
    /// 沿 module 相对发现（旧形态）。
    PyModule {
        module: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_env: Option<toml::value::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    /// erlang 网关：OTP app（code:load_abs 热加载）。
    ///
    /// R3：`uri` 指向 app 构建产物目录（beam 所在——file:// 形态）；
    /// 缺省时沿网关 artifact 目录发现（旧形态）。
    Beam {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
}

impl ArtifactRef {
    /// 形态名（错误消息/golden vector 标签用）。
    pub fn kind(&self) -> &'static str {
        match self {
            ArtifactRef::Props { .. } => "props",
            ArtifactRef::Wasm { .. } => "wasm",
            ArtifactRef::Dylib { .. } => "dylib",
            ArtifactRef::Jvm { .. } => "jvm",
            ArtifactRef::PyModule { .. } => "pymodule",
            ArtifactRef::Beam { .. } => "beam",
        }
    }

    /// Convert to the admin-v2 representation, reporting invalid runtime_env.
    pub fn into_admin(self) -> Result<parrot_remote::admin_v2::AdminArtifactRef, String> {
        self.try_into()
    }

    /// Borrowed conversion with an explicit host policy; None and an empty table
    /// remain distinct. Runtime environments must be TOML tables.
    pub fn to_admin_with_limits(
        &self,
        limits: &ApplicationLimits,
    ) -> Result<parrot_remote::admin_v2::AdminArtifactRef, String> {
        limits.check_artifact(self).map_err(|e| e.to_string())?;
        let mut remaining = limits.max_metadata_bytes;
        let result = self.encode_admin(limits, &mut remaining)?;
        limits.check_metadata(&result).map_err(|e| e.to_string())?;
        Ok(result)
    }

    fn encode_admin(
        &self,
        limits: &ApplicationLimits,
        remaining: &mut u64,
    ) -> Result<parrot_remote::admin_v2::AdminArtifactRef, String> {
        use parrot_remote::admin_v2::AdminArtifactRef;
        Ok(match self {
            Self::Props { factory } => AdminArtifactRef::Props {
                factory: factory.clone(),
            },
            Self::Wasm { digest, uri } => AdminArtifactRef::Wasm {
                digest: digest.clone(),
                uri: uri.clone(),
            },
            Self::Dylib { digest, uri, abi } => AdminArtifactRef::Dylib {
                digest: digest.clone(),
                uri: uri.clone(),
                abi: *abi,
            },
            Self::Jvm {
                main_class,
                coords,
                uri,
            } => AdminArtifactRef::Jvm {
                main_class: main_class.clone(),
                coords: coords.clone(),
                uri: uri.clone(),
            },
            Self::PyModule {
                module,
                runtime_env,
                uri,
            } => {
                let runtime_env = runtime_env
                    .as_ref()
                    .map(|value| {
                        let table = value.as_table().ok_or("runtime_env must be a TOML table")?;
                        let text = crate::encoding::runtime_env(
                            table,
                            remaining,
                            limits.max_source_bytes,
                        )?;
                        Ok::<_, String>(text)
                    })
                    .transpose()?;
                AdminArtifactRef::PyModule {
                    module: module.clone(),
                    runtime_env,
                    uri: uri.clone(),
                }
            }
            Self::Beam { app, uri } => AdminArtifactRef::Beam {
                app: app.clone(),
                uri: uri.clone(),
            },
        })
    }
}

/// 实例策略（09 §2.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum InstancePolicy {
    #[default]
    Singleton,
    Sharded(usize),
    Pool(usize),
    Ephemeral,
}

impl InstancePolicy {
    /// 该策略的实例数（Ephemeral 按需——规划期记 1）。
    pub fn instance_count(&self) -> usize {
        match self {
            InstancePolicy::Singleton | InstancePolicy::Ephemeral => 1,
            InstancePolicy::Sharded(n) | InstancePolicy::Pool(n) => *n,
        }
    }
}

/// 放置约束（09 §5.3——复用 topology 三模式语义；此处独立建模保
/// parrot-app 对 parrot-remote 拓扑细节的零依赖面）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementConstraint {
    /// 拓扑角色（"hub"/"border"/"directory"/"normal"——字符串形态，方言侧解释）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 反亲和：同组件实例不共置。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub anti_affinity: bool,
}

impl PlacementConstraint {
    /// 节点名粗匹配（role/realm/label 出现在节点 id 即可——E1 首版
    /// 规则；空约束恒真）。
    pub fn matches(&self, node: &str) -> bool {
        [
            self.role.as_deref(),
            self.realm.as_deref(),
            self.label.as_deref(),
        ]
        .into_iter()
        .flatten()
        .all(|k| node.contains(k))
    }
}

/// 升级策略（09 §6.1 三策略）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum UpgradePolicy {
    /// 原地换实例：drain → 新实例起 → 路由原子切 → 旧实例停。
    HotSwap { drain_timeout_ms: u64 },
    /// 按分片逐个 HotSwap（Pool/Sharded 组件）。
    Rolling { max_surge: u8 },
    /// 破坏性变更：全停 → 状态迁移（版本化快照 N 代保留）→ 全起。
    Recreate { state_snapshots: u8 },
}

impl Default for UpgradePolicy {
    fn default() -> Self {
        UpgradePolicy::HotSwap {
            drain_timeout_ms: 5_000,
        }
    }
}

/// 组件钩子（路径引用——宿主发 ParrotAppHook 消息并等回执）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentHooks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_drain: Option<String>,
}

/// 组件间静态连接（09 §2.2 WireSpec）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireSpec {
    /// Source component and its local connection alias, e.g. "crawler:/user/next".
    pub from: String,
    /// Target component and concrete instance/child path, e.g. "index:/user/index".
    pub to: String,
    /// Requested transport policy: empty inherits the host; "lan" is supported
    /// by local assembly. "wan" is validated but rejected there until durable
    /// per-connection transport policy is implemented (never silently ignored).
    #[serde(default)]
    pub qos: String,
}

/// Parse a component-qualified absolute actor/connection path without aliases,
/// URI authorities, percent decoding or path normalization.
pub fn wire_endpoint(value: &str) -> Result<(&str, &str), String> {
    parse_wire_endpoint(value).map_err(|error| error.to_string())
}

enum WireEndpointError<'a> {
    MissingSeparator(&'a str),
    Invalid(&'a str),
}
impl std::fmt::Display for WireEndpointError<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSeparator(value) => write!(
                formatter,
                "wire endpoint requires component:/path: {value:?}"
            ),
            Self::Invalid(value) => write!(formatter, "invalid wire endpoint {value:?}"),
        }
    }
}

fn parse_wire_endpoint(value: &str) -> Result<(&str, &str), WireEndpointError<'_>> {
    let (component, path) = value
        .split_once(':')
        .ok_or(WireEndpointError::MissingSeparator(value))?;
    if !valid_path_segment(component)
        || !path.starts_with("/user/")
        || !path[1..].split('/').all(valid_path_segment)
    {
        return Err(WireEndpointError::Invalid(value));
    }
    Ok((component, path))
}

impl ComponentSpec {
    /// Concrete instance roots. Connections to a pool must select an explicit
    /// member; this API does not invent a load-balancing or shard-key policy.
    pub fn instance_paths(&self) -> Result<Vec<String>, AdmissionError> {
        self.instance_paths_with_limits(&ApplicationLimits::default())
    }
    /// Explicit host policy, also applied by planning and assembly. No array is
    /// reserved until count, configuration and expanded bytes have been checked.
    pub fn instance_paths_with_limits(
        &self,
        limits: &ApplicationLimits,
    ) -> Result<Vec<String>, AdmissionError> {
        limits.check_component(self)?;
        let mut paths = Vec::new();
        paths
            .try_reserve_exact(self.instances.instance_count())
            .map_err(|_| AdmissionError::Allocation("instance paths"))?;
        match self.instances {
            InstancePolicy::Singleton | InstancePolicy::Ephemeral => {
                paths.push(format!("/user/{}", self.name))
            }
            _ => paths.extend(
                (0..self.instances.instance_count()).map(|i| format!("/user/{}-{i}", self.name)),
            ),
        }
        Ok(paths)
    }
    pub fn owns_wire_path(&self, path: &str) -> bool {
        let Some(tail) = path.strip_prefix("/user/") else {
            return false;
        };
        let root = tail.split('/').next().unwrap_or("");
        match self.instances {
            InstancePolicy::Singleton | InstancePolicy::Ephemeral => root == self.name,
            _ => root
                .strip_prefix(&self.name)
                .and_then(|suffix| suffix.strip_prefix('-'))
                .and_then(|index| {
                    index
                        .parse::<usize>()
                        .ok()
                        .filter(|value| value.to_string() == index)
                })
                .is_some_and(|index| index < self.instances.instance_count()),
        }
    }
}

// ============================================================================
// 校验器（A1 行为规约 ①：全项通过才可进 Planner；错误驱动测试的全部分支源）
// ============================================================================

/// 校验错误（逐项返回——不 panic）。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ManifestError {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("duplicate component name: {0}")]
    DuplicateComponent(String),
    #[error("component {comp:?} depends on unknown {dep:?}")]
    UnknownDependency { comp: String, dep: String },
    #[error("dependency cycle: {0:?}")]
    CycleDetected(Vec<String>),
    #[error("wiring unreachable: {from:?} -> {to:?}")]
    WiringUnreachable { from: String, to: String },
    #[error("invalid wiring: {0}")]
    BadWiring(String),
    #[error("invalid mirror: {0}")]
    BadMirror(String),
    #[error("bad semver: {0:?}")]
    BadSemver(String),
    #[error("component {comp:?} (engine {engine:?}) cannot use artifact {artifact:?}")]
    EngineArtifactMismatch {
        comp: String,
        engine: EngineKind,
        artifact: String,
    },
    #[error("components list is empty")]
    EmptyComponents,
    #[error("invalid component name: {0:?}")]
    BadComponentName(String),
    #[error("invalid instances for component {comp}: {count}")]
    BadInstances { comp: String, count: usize },
    #[error("invalid hook: {0}")]
    BadHook(String),
    #[error("invalid cloud proxy component: {0}")]
    BadCloudProxy(String),
}

pub(crate) fn valid_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// Validate SemVer 2.0.0 syntax without allocating or imposing integer bounds.
/// This checks grammar only; it does not compare version precedence.
pub fn is_valid_semver(v: &str) -> bool {
    fn identifiers(value: &str, prerelease: bool) -> bool {
        value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                // Only purely numeric prerelease identifiers forbid leading zeroes.
                && !(prerelease
                    && part.len() > 1
                    && part.starts_with('0')
                    && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
    }
    // 剥 build metadata
    let core = match v.split_once('+') {
        Some((c, build)) if identifiers(build, false) => c,
        Some(_) => return false,
        None => v,
    };
    // 剥 prerelease
    let nums = match core.split_once('-') {
        Some((c, pre)) if identifiers(pre, true) => c,
        Some(_) => return false,
        None => core,
    };
    let mut n = 0;
    for part in nums.split('.') {
        if part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()) {
            return false;
        }
        if part.len() > 1 && part.starts_with('0') {
            return false; // 前导零非法
        }
        n += 1;
    }
    n == 3
}

/// 全量校验（A1 规约：返回逐项错误）。
pub fn validate(m: &AppManifest) -> Result<(), Vec<ManifestError>> {
    validate_with_limits(m, &ApplicationLimits::default())
}

// Write only admitted diagnostic chunks. Debug escaping can amplify input text;
// formatting a complete String before charging it would defeat early rejection.
fn diagnostic_text(
    message: impl std::fmt::Display,
    total: &mut u64,
    limit: u64,
) -> Result<String, AdmissionError> {
    use std::fmt::Write;
    crate::limits::add(
        "diagnostic bytes",
        total,
        std::mem::size_of::<ManifestError>() as u64,
        limit,
    )?;
    struct Writer<'a> {
        text: String,
        total: &'a mut u64,
        limit: u64,
        failure: Option<AdmissionError>,
    }
    impl Write for Writer<'_> {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            if let Err(error) = crate::limits::add(
                "diagnostic bytes",
                self.total,
                text.len() as u64,
                self.limit,
            ) {
                self.failure = Some(error);
                return Err(std::fmt::Error);
            }
            self.text.push_str(text);
            Ok(())
        }
    }
    let mut writer = Writer {
        text: String::new(),
        total,
        limit,
        failure: None,
    };
    let result = write!(&mut writer, "{message}");
    if let Some(error) = writer.failure {
        return Err(error);
    }
    result.map_err(|_| AdmissionError::Serialization("diagnostic formatting failed".into()))?;
    Ok(writer.text)
}

pub fn validate_with_limits(
    m: &AppManifest,
    limits: &ApplicationLimits,
) -> Result<(), Vec<ManifestError>> {
    limits
        .check_manifest(m)
        .map_err(|e| vec![ManifestError::Admission(e)])?;
    let mut errs = Vec::new();
    let mut diagnostic_bytes = 0;
    // Check retained diagnostic cost before cloning attacker-controlled text.
    // In particular, an unknown-dependency fanout must not copy a long
    // component name once per edge without a cumulative budget.
    macro_rules! report {
        ($bytes:expr, $error:expr) => {{
            let charge =
                ($bytes as u64).saturating_add(std::mem::size_of::<ManifestError>() as u64);
            if let Err(error) = crate::limits::add(
                "diagnostic bytes",
                &mut diagnostic_bytes,
                charge,
                limits.max_diagnostic_bytes,
            ) {
                errs.push(ManifestError::Admission(error));
                return Err(errs);
            }
            errs.push($error);
        }};
    }
    macro_rules! report_text {
        ($variant:path, $message:expr) => {{
            match diagnostic_text($message, &mut diagnostic_bytes, limits.max_diagnostic_bytes) {
                Ok(message) => errs.push($variant(message)),
                Err(error) => {
                    errs.push(ManifestError::Admission(error));
                    return Err(errs);
                }
            }
        }};
    }

    // ① app 级 semver
    if !is_valid_semver(&m.version) {
        report!(m.version.len(), ManifestError::BadSemver(m.version.clone()));
    }

    // ② 空组件表
    if m.components.is_empty() {
        report!(0usize, ManifestError::EmptyComponents);
        return Err(errs); // 后续检查无意义
    }

    // ③ 组件重名
    let mut seen = std::collections::BTreeSet::new();
    for c in &m.components {
        if !valid_path_segment(&c.name) {
            report!(
                c.name.len(),
                ManifestError::BadComponentName(c.name.clone())
            );
        }
        let count = c.instances.instance_count();
        if matches!(&c.artifact, ArtifactRef::Props { factory } if factory == crate::durable::CLOUD_PROXY_FACTORY)
        {
            if count != 1 || c.engine != EngineKind::Parrot {
                report!(
                    c.name.len() + 64,
                    ManifestError::BadCloudProxy(format!(
                        "{} requires one Parrot instance",
                        c.name
                    ))
                );
            }
            if let Err(error) = crate::durable::CloudProxyConfig::from_table(c.config.as_ref()) {
                report!(
                    c.name.len() + error.len() + 2,
                    ManifestError::BadCloudProxy(format!("{}: {error}", c.name))
                );
            }
        }
        if count == 0 || count > u32::MAX as usize {
            report!(
                c.name.len(),
                ManifestError::BadInstances {
                    comp: c.name.clone(),
                    count,
                }
            );
        }
        for path in [&c.hooks.on_start, &c.hooks.on_stop, &c.hooks.on_drain]
            .into_iter()
            .flatten()
        {
            if let Err(error) = c.hook_suffix(path) {
                report_text!(
                    ManifestError::BadHook,
                    HookDiagnostic {
                        error,
                        hook: path,
                        component: &c.name
                    }
                );
            }
        }
        if !seen.insert(c.name.as_str()) {
            report!(
                c.name.len(),
                ManifestError::DuplicateComponent(c.name.clone())
            );
        }
    }

    // ④ 依赖缺失
    for c in &m.components {
        for d in &c.deps {
            if !seen.contains(d.as_str()) {
                report!(
                    c.name.len() + d.len(),
                    ManifestError::UnknownDependency {
                        comp: c.name.clone(),
                        dep: d.clone(),
                    }
                );
            }
        }
    }

    // ⑤ 依赖环（DFS 三色标记 + 环路径回放）
    if let Some(cycle) = find_cycle(m) {
        report!(
            cycle
                .iter()
                .map(|s| s.len() + std::mem::size_of::<String>())
                .sum::<usize>(),
            ManifestError::CycleDetected(cycle.into_iter().map(str::to_owned).collect())
        );
    }

    // ⑥ engine-artifact 匹配表（含 R2 alt_artifact 备选形态）
    for c in &m.components {
        if !c.engine.accepts(&c.artifact) {
            report!(
                c.name.len() + 32,
                ManifestError::EngineArtifactMismatch {
                    comp: c.name.clone(),
                    engine: c.engine,
                    artifact: c.artifact.kind().to_string(),
                }
            );
        }
        if let Some(alt) = &c.alt_artifact {
            if !c.engine.accepts(alt) {
                report!(
                    c.name.len() + 32,
                    ManifestError::EngineArtifactMismatch {
                        comp: c.name.clone(),
                        engine: c.engine,
                        artifact: format!("alt:{}", alt.kind()),
                    }
                );
            }
        }
    }

    // Reject malformed endpoints and ambiguous source aliases before planning.
    let mut sources = std::collections::HashSet::new();
    for w in &m.wiring {
        let endpoints = parse_wire_endpoint(&w.from)
            .and_then(|from| parse_wire_endpoint(&w.to).map(|to| (from, to)));
        match endpoints {
            Err(error) => report_text!(ManifestError::BadWiring, error),
            Ok(((source, _), (target, path))) => {
                if !seen.contains(source) || !seen.contains(target) {
                    report!(
                        w.from.len() + w.to.len(),
                        ManifestError::WiringUnreachable {
                            from: w.from.clone(),
                            to: w.to.clone(),
                        }
                    );
                } else if !m.component(target).unwrap().owns_wire_path(path) {
                    report!(
                        w.to.len() * 2 + 128,
                        ManifestError::BadWiring(format!(
                            "target {} is outside the declared instances of {target}",
                            w.to
                        ))
                    );
                }
            }
        }
        if !sources.insert(w.from.as_str()) {
            report!(
                w.from.len() + 64,
                ManifestError::BadWiring(format!("duplicate source alias {}", w.from))
            );
        }
        if !matches!(w.qos.as_str(), "" | "lan" | "wan") {
            report!(
                w.qos.len() * 10 + 64,
                ManifestError::BadWiring(format!("unknown QoS {:?}", w.qos))
            );
        }
    }

    if let Err(error) = crate::mirror::validate(m) {
        report!(error.len(), ManifestError::BadMirror(error));
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

/// Iterative three-color DFS: deterministic cycle replay without call-stack
/// depth proportional to a manifest's dependency chain.
fn find_cycle(m: &AppManifest) -> Option<Vec<&str>> {
    let deps: BTreeMap<&str, &[String]> = m
        .components
        .iter()
        .map(|c| (c.name.as_str(), c.deps.as_slice()))
        .collect();
    let mut color: BTreeMap<&str, u8> = deps.keys().map(|&key| (key, 0)).collect();
    for &root in deps.keys() {
        if color[root] != 0 {
            continue;
        }
        let mut stack = vec![(root, 0usize)];
        color.insert(root, 1);
        while let Some((node, next)) = stack.last_mut() {
            let edges = deps[node];
            if *next == edges.len() {
                color.insert(*node, 2);
                stack.pop();
                continue;
            }
            let target = edges[*next].as_str();
            *next += 1;
            match color.get(target).copied() {
                Some(1) => {
                    let start = stack.iter().position(|(node, _)| *node == target).unwrap();
                    return Some(
                        stack[start..]
                            .iter()
                            .map(|(node, _)| *node)
                            .chain(std::iter::once(target))
                            .collect(),
                    );
                }
                Some(0) => {
                    color.insert(target, 1);
                    stack.push((target, 0));
                }
                _ => {} // Unknown dependencies are reported separately.
            }
        }
    }
    None
}

// ============================================================================
// TOML 双形态（A1 规约 ②：roundtrip 字节稳定）
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum AppLoadError {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("toml ser: {0}")]
    Ser(#[from] toml::ser::Error),
}

impl AppManifest {
    /// 校验（自由函数包装——orchestrator submit 路径）。
    pub fn validate_or_err(&self) -> Result<(), String> {
        crate::manifest::validate(self).map_err(|e| format!("{e:?}"))
    }

    /// 序列化为 TOML 文本（roundtrip 字节稳定的规范序：结构体字段序固定）。
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        self.to_toml_with_limits(&ApplicationLimits::default())
    }

    /// Admit the model before serialization and bound generated source bytes.
    /// Uses the same canonical bytes as TOML serialization of admitted models;
    /// semantic graph validation remains a separate operation.
    pub fn to_toml_with_limits(
        &self,
        limits: &ApplicationLimits,
    ) -> Result<String, toml::ser::Error> {
        use serde::ser::Error;
        limits
            .check_manifest(self)
            .map_err(toml::ser::Error::custom)?;
        crate::encoding::canonical(self, limits.max_source_bytes)
    }

    /// Parse with default source/model admission. Semantic graph validation is
    /// still performed by validate/plan/assemble, including for programmatic input.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        Self::from_toml_with_limits(s, &ApplicationLimits::default())
    }
    pub fn from_toml_with_limits(
        s: &str,
        limits: &ApplicationLimits,
    ) -> Result<Self, toml::de::Error> {
        use serde::de::Error;
        crate::limits::check("source bytes", s.len() as u64, limits.max_source_bytes)
            .map_err(toml::de::Error::custom)?;
        let manifest = toml::from_str(s)?;
        limits
            .check_manifest(&manifest)
            .map_err(toml::de::Error::custom)?;
        Ok(manifest)
    }
    /// Load a regular file, allowing symlinks to regular files. Unix opens do
    /// not wait for a FIFO writer or acquire a controlling terminal. Read at
    /// most source limit + 1 bytes, including if the file grows while reading.
    pub fn from_file(path: &Path) -> Result<Self, AppLoadError> {
        Self::from_file_with_limits(path, &ApplicationLimits::default())
    }
    pub fn from_file_with_limits(
        path: &Path,
        limits: &ApplicationLimits,
    ) -> Result<Self, AppLoadError> {
        use std::io::Read;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "manifest source must be a regular file",
            )
            .into());
        }
        let mut bytes = Vec::new();
        file.take(limits.max_source_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        crate::limits::check("source bytes", bytes.len() as u64, limits.max_source_bytes)?;
        let text = String::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self::from_toml_with_limits(&text, limits)?)
    }

    /// 组件名 → spec 快查。
    pub fn component(&self, name: &str) -> Option<&ComponentSpec> {
        self.components.iter().find(|c| c.name == name)
    }
}

/// 组件级 config 快查便捷（含 overlay 合并语义由 assemble 阶段实现——
/// 此处仅提供组件自身 config 表的只读访问）。
pub fn component_config_flat(c: &ComponentSpec) -> BTreeMap<String, String> {
    // 一层字符串值展平（hooks/config 的运维可读视图——`parrot app status` 用）。
    let mut out = BTreeMap::new();
    if let Some(t) = &c.config {
        for (k, v) in t {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

// ============================================================================
// 测试（§5.1 Manifest 25+——校验全分支 + roundtrip 字节一致）
// ============================================================================

// ─────────────────────────────────────────────────────────────
// BD-1：AdminArtifactRef ↔ ArtifactRef 转换（serde 同形互锁）
// ─────────────────────────────────────────────────────────────

impl TryFrom<ArtifactRef> for parrot_remote::admin_v2::AdminArtifactRef {
    type Error = String;
    fn try_from(a: ArtifactRef) -> Result<Self, Self::Error> {
        let result = a.to_admin_with_limits(&ApplicationLimits::default());
        if let ArtifactRef::PyModule {
            runtime_env: Some(value),
            ..
        } = a
        {
            crate::encoding::drop_value(value);
        }
        result
    }
}

impl TryFrom<parrot_remote::admin_v2::AdminArtifactRef> for ArtifactRef {
    type Error = String;
    fn try_from(a: parrot_remote::admin_v2::AdminArtifactRef) -> Result<Self, Self::Error> {
        Self::from_admin_with_limits(a, &ApplicationLimits::default())
    }
}

impl ArtifactRef {
    /// Bound the wire model and runtime_env text before parsing. The TOML parser
    /// retains its own nesting protection; decoded values also consume host limits.
    pub fn from_admin_with_limits(
        a: parrot_remote::admin_v2::AdminArtifactRef,
        limits: &ApplicationLimits,
    ) -> Result<Self, String> {
        use parrot_remote::admin_v2::AdminArtifactRef;
        limits.check_metadata(&a).map_err(|e| e.to_string())?;
        let result = match a {
            AdminArtifactRef::Props { factory } => ArtifactRef::Props { factory },
            AdminArtifactRef::Wasm { digest, uri } => ArtifactRef::Wasm { digest, uri },
            AdminArtifactRef::Dylib { digest, uri, abi } => ArtifactRef::Dylib { digest, uri, abi },
            AdminArtifactRef::Jvm {
                main_class,
                coords,
                uri,
            } => ArtifactRef::Jvm {
                main_class,
                coords,
                uri,
            },
            AdminArtifactRef::PyModule {
                module,
                runtime_env,
                uri,
            } => {
                let runtime_env = runtime_env
                    .map(|text| {
                        crate::limits::check(
                            "source bytes",
                            text.len() as u64,
                            limits.max_source_bytes,
                        )
                        .map_err(|e| e.to_string())?;
                        toml::from_str::<toml::Table>(&text)
                            .map(toml::Value::Table)
                            .map_err(|e| {
                                format!(
                                    "invalid runtime_env TOML: {}",
                                    e.message().chars().take(160).collect::<String>()
                                )
                            })
                    })
                    .transpose()?;
                ArtifactRef::PyModule {
                    module,
                    runtime_env,
                    uri,
                }
            }
            AdminArtifactRef::Beam { app, uri } => ArtifactRef::Beam { app, uri },
        };
        if let Err(error) = limits.check_artifact(&result) {
            if let ArtifactRef::PyModule {
                runtime_env: Some(value),
                ..
            } = result
            {
                crate::encoding::drop_value(value);
            }
            return Err(error.to_string());
        }
        Ok(result)
    }
}

impl From<InstancePolicy> for parrot_remote::admin_v2::AdminInstancePolicy {
    fn from(p: InstancePolicy) -> Self {
        use parrot_remote::admin_v2::AdminInstancePolicy;
        match p {
            InstancePolicy::Singleton => AdminInstancePolicy::Singleton,
            InstancePolicy::Pool(count) => AdminInstancePolicy::Pool { count },
            InstancePolicy::Sharded(count) => AdminInstancePolicy::Sharded { count },
            // Ephemeral 按需实例——wire 上与 Singleton 同形（count=1；
            // 差异语义留驻本地 manifest 层）
            InstancePolicy::Ephemeral => AdminInstancePolicy::Singleton,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props_comp(name: &str) -> ComponentSpec {
        ComponentSpec {
            name: name.into(),
            engine: EngineKind::Parrot,
            artifact: ArtifactRef::Props {
                factory: format!("app.{name}"),
            },
            alt_artifact: None,
            instances: InstancePolicy::Singleton,
            placement: PlacementConstraint::default(),
            upgrade: UpgradePolicy::default(),
            deps: vec![],
            config: None,
            hooks: ComponentHooks::default(),
        }
    }

    fn valid_manifest() -> AppManifest {
        AppManifest {
            name: "demo".into(),
            version: "1.0.0".into(),
            components: vec![props_comp("a"), props_comp("b")],
            wiring: vec![WireSpec {
                from: "a:/user/out".into(),
                to: "b:/user/b/in".into(),
                qos: "lan".into(),
            }],
            mirrors: Vec::new(),
            config_overlay: None,
        }
    }

    #[test]
    fn wiring_rejects_malformed_paths_alias_conflicts_and_foreign_targets() {
        for from in [
            "a",
            "a:",
            "a:user/out",
            "a:/user/",
            "a:/user//out",
            "a:/user/../b",
            "a:/user/%62",
            "a:/user/out:extra",
            "a:/user/out?x",
            "a:/system/out",
        ] {
            let mut manifest = valid_manifest();
            manifest.wiring[0].from = from.into();
            assert!(validate(&manifest).is_err(), "accepted {from:?}");
        }
        let mut manifest = valid_manifest();
        manifest.wiring.push(manifest.wiring[0].clone());
        assert!(validate(&manifest).is_err());
        for to in ["b:/user/a", "b:/user/bad", "b:/user/b-0", "b:/user/b/../a"] {
            let mut manifest = valid_manifest();
            manifest.wiring[0].to = to.into();
            assert!(validate(&manifest).is_err(), "accepted {to:?}");
        }
    }

    #[test]
    fn pool_wiring_requires_a_canonical_concrete_member() {
        let mut manifest = valid_manifest();
        manifest.components[1].instances = InstancePolicy::Pool(2);
        for path in ["/user/b-0", "/user/b-1/child"] {
            manifest.wiring[0].to = format!("b:{path}");
            assert!(validate(&manifest).is_ok(), "rejected {path}");
        }
        for path in [
            "/user/b",
            "/user/b-2",
            "/user/b-01",
            "/user/b-+1",
            "/user/b-18446744073709551616",
        ] {
            manifest.wiring[0].to = format!("b:{path}");
            assert!(validate(&manifest).is_err(), "accepted {path}");
        }
        // Ownership checks must not allocate one path per untrusted count.
        manifest.components[1].instances = InstancePolicy::Pool(usize::MAX);
        assert!(manifest.components[1].owns_wire_path("/user/b-1/child"));
        assert!(!manifest.components[1].owns_wire_path("/user/b"));
    }

    #[test]
    fn wiring_qos_has_no_silent_typo_fallback() {
        for qos in ["", "lan", "wan"] {
            let mut manifest = valid_manifest();
            manifest.wiring[0].qos = qos.into();
            assert!(validate(&manifest).is_ok());
        }
        for qos in ["LAN", " laN", "durable", "none"] {
            let mut manifest = valid_manifest();
            manifest.wiring[0].qos = qos.into();
            assert!(validate(&manifest).is_err());
        }
    }

    // ── semver 文法（8）─────────────────────────────────────────

    #[test]
    fn semver_valid_forms() {
        for v in [
            "1.0.0",
            "0.1.0",
            "10.20.30",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-x.7.z.92",
            "1.0.0+build.5",
            "2.1.0-rc.1+meta",
        ] {
            assert!(is_valid_semver(v), "should be valid: {v}");
        }
    }

    #[test]
    fn semver_invalid_forms() {
        for v in [
            "1.0", "1", "v1.0.0", "1.0.0.0", "01.0.0", "1.0.0-", "1.0.0+", "a.b.c", "", "1..0",
            "1.0.x",
        ] {
            assert!(!is_valid_semver(v), "should be invalid: {v}");
        }
    }

    // ── 校验全分支（14）────────────────────────────────────────

    #[test]
    fn validate_ok() {
        assert_eq!(validate(&valid_manifest()), Ok(()));
    }

    #[test]
    fn validate_bad_app_semver() {
        let mut m = valid_manifest();
        m.version = "not-semver".into();
        let errs = validate(&m).unwrap_err();
        assert!(matches!(errs[0], ManifestError::BadSemver(_)));
    }

    #[test]
    fn validate_empty_components() {
        let m = AppManifest {
            name: "x".into(),
            version: "1.0.0".into(),
            components: vec![],
            wiring: vec![],
            mirrors: Vec::new(),
            config_overlay: None,
        };
        assert_eq!(validate(&m), Err(vec![ManifestError::EmptyComponents]));
    }

    #[test]
    fn validate_duplicate_component() {
        let mut m = valid_manifest();
        m.components.push(props_comp("a"));
        let errs = validate(&m).unwrap_err();
        assert_eq!(errs[0], ManifestError::DuplicateComponent("a".into()));
    }

    #[test]
    fn validate_unknown_dependency() {
        let mut m = valid_manifest();
        m.components[0].deps = vec!["ghost".into()];
        let errs = validate(&m).unwrap_err();
        assert_eq!(
            errs[0],
            ManifestError::UnknownDependency {
                comp: "a".into(),
                dep: "ghost".into()
            }
        );
    }

    #[test]
    fn validate_cycle_direct() {
        let mut m = valid_manifest();
        m.components[0].deps = vec!["b".into()];
        m.components[1].deps = vec!["a".into()];
        let errs = validate(&m).unwrap_err();
        match &errs[0] {
            ManifestError::CycleDetected(c) => {
                assert_eq!(c.first(), c.last()); // 闭环
                assert_eq!(c.len(), 3); // a→b→a 或 b→a→b
            }
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn validate_cycle_self() {
        let mut m = valid_manifest();
        m.components[0].deps = vec!["a".into()];
        let errs = validate(&m).unwrap_err();
        assert!(matches!(&errs[0], ManifestError::CycleDetected(c) if c.len() == 2));
    }

    #[test]
    fn validate_cycle_three_nodes() {
        let mut m = valid_manifest();
        m.components.push(props_comp("c"));
        m.components[0].deps = vec!["c".into()];
        m.components[2].deps = vec!["b".into()];
        m.components[1].deps = vec!["a".into()];
        let errs = validate(&m).unwrap_err();
        match &errs[0] {
            ManifestError::CycleDetected(c) => assert_eq!(c.len(), 4),
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn validate_engine_artifact_matrix() {
        // 全组合穷举（6 引擎 × 6 形态）
        let engines = [
            EngineKind::Parrot,
            EngineKind::Akka,
            EngineKind::Ray,
            EngineKind::Erlang,
            EngineKind::LiteTs,
            EngineKind::LiteCpp,
        ];
        let artifacts = [
            ArtifactRef::Props {
                factory: "f".into(),
            },
            ArtifactRef::Wasm {
                digest: "d".into(),
                uri: "file:///x.wasm".into(),
            },
            ArtifactRef::Dylib {
                digest: "d".into(),
                uri: "file:///x.so".into(),
                abi: 1,
            },
            ArtifactRef::Jvm {
                main_class: "M".into(),
                coords: None,
                uri: None,
            },
            ArtifactRef::PyModule {
                module: "m".into(),
                runtime_env: None,
                uri: None,
            },
            ArtifactRef::Beam {
                app: "a".into(),
                uri: None,
            },
        ];
        let mut matched = 0;
        for e in engines {
            for a in &artifacts {
                let mut m = valid_manifest();
                m.components[0].engine = e;
                m.components[0].artifact = a.clone();
                let ok = validate(&m).is_ok();
                assert_eq!(ok, e.accepts(a), "{e:?} × {}", a.kind());
                if ok {
                    matched += 1;
                }
            }
        }
        // parrot 3 + akka 1 + ray 1 + erlang 1 + lite 12（Lite* 容纳一切）
        assert_eq!(matched, 3 + 1 + 1 + 1 + 12);
    }

    #[test]
    fn validate_artifact_mismatch_message() {
        let mut m = valid_manifest();
        m.components[0].engine = EngineKind::Erlang;
        m.components[0].artifact = ArtifactRef::Props {
            factory: "f".into(),
        };
        let errs = validate(&m).unwrap_err();
        match &errs[0] {
            ManifestError::EngineArtifactMismatch {
                comp,
                engine,
                artifact,
            } => {
                assert_eq!(comp, "a");
                assert_eq!(*engine, EngineKind::Erlang);
                assert_eq!(artifact, "props");
            }
            other => panic!("expected mismatch, got {other:?}"),
        }
    }

    #[test]
    fn validate_wiring_unreachable_from() {
        let mut m = valid_manifest();
        m.wiring.push(WireSpec {
            from: "ghost:/user/x".into(),
            to: "b:/user/b/in".into(),
            qos: String::new(),
        });
        let errs = validate(&m).unwrap_err();
        assert!(matches!(errs[0], ManifestError::WiringUnreachable { .. }));
    }

    #[test]
    fn validate_wiring_unreachable_to() {
        let mut m = valid_manifest();
        m.wiring.push(WireSpec {
            from: "a:/user/x".into(),
            to: "missing:/user/in".into(),
            qos: String::new(),
        });
        let errs = validate(&m).unwrap_err();
        assert!(matches!(errs[0], ManifestError::WiringUnreachable { .. }));
    }

    #[test]
    fn validate_accumulates_multiple_errors() {
        let mut m = valid_manifest();
        m.version = "bad".into();
        m.components[1].name = "a".into(); // 重名 + deps 引用 b 变未知
        let errs = validate(&m).unwrap_err();
        assert!(errs.len() >= 2, "accumulated: {errs:?}");
    }

    #[test]
    fn validate_deps_allow_diamond() {
        let mut m = valid_manifest();
        m.components.push(props_comp("c"));
        m.components[1].deps = vec!["a".into()];
        m.components[2].deps = vec!["a".into()];
        assert_eq!(validate(&m), Ok(()));
    }

    // ── TOML 双形态（6）────────────────────────────────────────

    #[test]
    fn toml_roundtrip_byte_stable() {
        let m = valid_manifest();
        let s1 = m.to_toml().unwrap();
        let m2 = AppManifest::from_toml(&s1).unwrap();
        let s2 = m2.to_toml().unwrap();
        assert_eq!(s1, s2, "roundtrip byte-stable");
    }

    #[test]
    fn toml_roundtrip_all_artifact_kinds() {
        let m = AppManifest {
            name: "full".into(),
            version: "2.3.4-rc.1".into(),
            components: vec![
                ComponentSpec {
                    name: "p".into(),
                    engine: EngineKind::Parrot,
                    artifact: ArtifactRef::Wasm {
                        digest: "abc".into(),
                        uri: "file:///a.wasm".into(),
                    },
                    alt_artifact: None,
                    instances: InstancePolicy::Sharded(4),
                    placement: PlacementConstraint {
                        role: Some("hub".into()),
                        realm: None,
                        label: Some("edge".into()),
                        anti_affinity: true,
                    },
                    upgrade: UpgradePolicy::Rolling { max_surge: 2 },
                    deps: vec![],
                    config: Some(
                        [("k".to_string(), toml::Value::from("v"))]
                            .into_iter()
                            .collect(),
                    ),
                    hooks: ComponentHooks {
                        on_start: Some("/user/p/start".into()),
                        on_stop: None,
                        on_drain: Some("/user/p/drain".into()),
                    },
                },
                ComponentSpec {
                    name: "j".into(),
                    engine: EngineKind::Akka,
                    artifact: ArtifactRef::Jvm {
                        main_class: "Main".into(),
                        coords: Some("g:a:1".into()),
                        uri: None,
                    },
                    alt_artifact: None,
                    instances: InstancePolicy::Pool(3),
                    placement: Default::default(),
                    upgrade: UpgradePolicy::Recreate { state_snapshots: 2 },
                    deps: vec!["p".into()],
                    config: None,
                    hooks: Default::default(),
                },
                ComponentSpec {
                    name: "py".into(),
                    engine: EngineKind::Ray,
                    artifact: ArtifactRef::PyModule {
                        module: "m".into(),
                        runtime_env: Some(toml::Value::from(true)),
                        uri: None,
                    },
                    alt_artifact: None,
                    instances: InstancePolicy::Ephemeral,
                    placement: Default::default(),
                    upgrade: UpgradePolicy::HotSwap {
                        drain_timeout_ms: 500,
                    },
                    deps: vec![],
                    config: None,
                    hooks: Default::default(),
                },
                ComponentSpec {
                    name: "erl".into(),
                    engine: EngineKind::Erlang,
                    artifact: ArtifactRef::Beam {
                        app: "frontier".into(),
                        uri: None,
                    },
                    alt_artifact: None,
                    instances: InstancePolicy::Singleton,
                    placement: Default::default(),
                    upgrade: UpgradePolicy::default(),
                    deps: vec![],
                    config: None,
                    hooks: Default::default(),
                },
            ],
            wiring: vec![],
            mirrors: Vec::new(),
            config_overlay: Some(
                [("x".to_string(), toml::Value::from(1))]
                    .into_iter()
                    .collect(),
            ),
        };
        let s1 = m.to_toml().unwrap();
        let m2: AppManifest = toml::from_str(&s1).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn toml_human_readable_form() {
        let m = valid_manifest();
        let s = m.to_toml().unwrap();
        assert!(s.contains("name = \"demo\""));
        assert!(s.contains("[[components]]"));
        assert!(s.contains("factory = \"app.a\""));
    }

    #[test]
    fn toml_parse_rejects_unknown_field() {
        let s = valid_manifest()
            .to_toml()
            .unwrap()
            .replace("name = \"demo\"", "name = \"demo\"\nunknown_top = 1");
        assert!(AppManifest::from_toml(&s).is_err());
    }

    #[test]
    fn toml_defaults_minimal() {
        let s = r#"
name = "m"
version = "0.0.1"

[[components]]
name = "a"
engine = "parrot"

[components.artifact]
Props = { factory = "f" }
"#;
        let m = AppManifest::from_toml(s).unwrap();
        assert_eq!(m.components[0].instances, InstancePolicy::Singleton);
        assert_eq!(m.components[0].upgrade, UpgradePolicy::default());
        assert!(m.components[0].deps.is_empty());
        assert_eq!(m.components[0].hooks, ComponentHooks::default());
    }

    #[test]
    fn from_file_missing() {
        let err = AppManifest::from_file(Path::new("/nonexistent/x.app.toml")).unwrap_err();
        assert!(matches!(err, AppLoadError::Io(_)));
    }

    #[test]
    fn from_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("parrot-app-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.app.toml");
        std::fs::write(&p, valid_manifest().to_toml().unwrap()).unwrap();
        let m = AppManifest::from_file(&p).unwrap();
        assert_eq!(m.name, "demo");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 辅助（2）──────────────────────────────────────────────

    #[test]
    fn component_lookup() {
        let m = valid_manifest();
        assert!(m.component("a").is_some());
        assert!(m.component("zzz").is_none());
    }

    #[test]
    fn instance_policy_counts() {
        assert_eq!(InstancePolicy::Singleton.instance_count(), 1);
        assert_eq!(InstancePolicy::Ephemeral.instance_count(), 1);
        assert_eq!(InstancePolicy::Sharded(8).instance_count(), 8);
        assert_eq!(InstancePolicy::Pool(4).instance_count(), 4);
    }

    #[test]
    fn component_config_flat_strings() {
        let mut c = props_comp("a");
        c.config = Some(
            [
                ("s".to_string(), toml::Value::from("v")),
                ("n".to_string(), toml::Value::from(3)),
            ]
            .into_iter()
            .collect(),
        );
        let flat = component_config_flat(&c);
        assert_eq!(flat.get("s"), Some(&"v".to_string()));
        assert!(!flat.contains_key("n")); // 非字符串不进扁平视图
    }

    #[test]
    fn engine_kind_str_roundtrip() {
        #[derive(serde::Deserialize)]
        struct EngineWrap {
            value: EngineKind,
        }
        for (k, s) in [
            (EngineKind::Parrot, "parrot"),
            (EngineKind::Akka, "akka"),
            (EngineKind::Ray, "ray"),
            (EngineKind::Erlang, "erlang"),
            (EngineKind::LiteTs, "lite-ts"),
            (EngineKind::LiteCpp, "lite-cpp"),
        ] {
            assert_eq!(k.as_str(), s);
            let e: EngineWrap = toml::from_str(&format!("value = \"{s}\"\n")).unwrap();
            assert_eq!(e.value, k);
        }
        // 未知引擎名拒绝
        assert!(toml::from_str::<EngineWrap>("value = \"golang\"\n").is_err());
    }
    // ── BD-1：AdminArtifactRef 同形互锁 ──────────────────────
    #[test]
    fn bd1_artifact_ref_roundtrip_all_kinds() {
        use parrot_remote::admin_v2::AdminArtifactRef;
        let arts = vec![
            ArtifactRef::Props {
                factory: "f".into(),
            },
            ArtifactRef::Beam {
                app: "a".into(),
                uri: None,
            },
            ArtifactRef::PyModule {
                module: "m".into(),
                runtime_env: None,
                uri: None,
            },
            ArtifactRef::Jvm {
                main_class: "M".into(),
                coords: None,
                uri: None,
            },
            ArtifactRef::Wasm {
                digest: "d".into(),
                uri: "u".into(),
            },
            ArtifactRef::Dylib {
                digest: "d".into(),
                uri: "u".into(),
                abi: 1,
            },
        ];
        for a in arts {
            let wire: AdminArtifactRef = a.clone().try_into().unwrap();
            let back: ArtifactRef = wire.try_into().unwrap();
            assert_eq!(back, a);
        }
    }

    #[test]
    fn bd1_instance_policy_maps() {
        use parrot_remote::admin_v2::AdminInstancePolicy;
        assert_eq!(
            AdminInstancePolicy::from(InstancePolicy::Singleton),
            AdminInstancePolicy::Singleton
        );
        assert_eq!(
            AdminInstancePolicy::from(InstancePolicy::Pool(3)),
            AdminInstancePolicy::Pool { count: 3 }
        );
    }

    #[test]
    fn bd1_pymodule_runtime_env_text_roundtrip() {
        use parrot_remote::admin_v2::AdminArtifactRef;
        let orig = ArtifactRef::PyModule {
            module: "jobs".into(),
            runtime_env: Some(toml::Value::Table(
                [(
                    "pip".to_string(),
                    toml::Value::Array(vec![toml::Value::from("requests")]),
                )]
                .into_iter()
                .collect(),
            )),
            uri: None,
        };
        let wire: AdminArtifactRef = orig.clone().try_into().unwrap();
        let back: ArtifactRef = wire.try_into().unwrap();
        assert_eq!(back, orig, "toml 文本互转保真");
    }
}

#[cfg(test)]
mod r5_tests {
    use super::*;

    /// R2/R3：alt_artifact 校验 + uri 字段 roundtrip + 新形态 manifest 解析。
    #[test]
    fn alt_artifact_mismatch_rejected() {
        let m = AppManifest {
            name: "t".into(),
            version: "1.0.0".into(),
            components: vec![ComponentSpec {
                name: "hub".into(),
                engine: EngineKind::Parrot,
                artifact: ArtifactRef::Dylib {
                    digest: "sha256:a".into(),
                    uri: "file://d.dylib".into(),
                    abi: 1,
                },
                // Beam 不是 parrot 引擎可接受的备选 → 报错
                alt_artifact: Some(ArtifactRef::Beam {
                    app: "x".into(),
                    uri: None,
                }),
                instances: InstancePolicy::Singleton,
                placement: Default::default(),
                upgrade: Default::default(),
                deps: vec![],
                config: None,
                hooks: Default::default(),
            }],
            wiring: vec![],
            mirrors: Vec::new(),
            config_overlay: Default::default(),
        };
        let errs = validate(&m).unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e,
            ManifestError::EngineArtifactMismatch { artifact, .. } if artifact.starts_with("alt:")
        )));
    }

    #[test]
    fn crawler_app_new_form_parses() {
        // Standard crawler manifest defaults to an embedded factory; artifact
        // execution and trust are exercised by the real application matrix.
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/crawler-lab/crawler.app.toml");
        let src = std::fs::read_to_string(&p).unwrap();
        let m: AppManifest = toml::from_str(&src).unwrap();
        assert_eq!(m.version, "2.0.0");
        let crawler = m.components.iter().find(|c| c.name == "crawler").unwrap();
        // Explicit CLI selection provides verified Dylib/Wasm artifacts.
        assert_eq!(
            crawler.artifact,
            ArtifactRef::Props {
                factory: "crawler.business".into()
            }
        );
        assert!(crawler.alt_artifact.is_none());
        // 三方言组件 uri 指向 app 内构建产物
        let f = m.components.iter().find(|c| c.name == "frontier").unwrap();
        assert!(
            matches!(&f.artifact, ArtifactRef::Beam { uri: Some(u), .. } if u.contains("erlang/"))
        );
        let i = m.components.iter().find(|c| c.name == "index").unwrap();
        assert!(
            matches!(&i.artifact, ArtifactRef::PyModule { uri: Some(u), .. } if u.contains("python/"))
        );
        let s = m.components.iter().find(|c| c.name == "search").unwrap();
        assert!(matches!(&s.artifact, ArtifactRef::Jvm { uri: Some(u), .. } if u.contains(".jar")));
        validate(&m).unwrap();
    }

    #[test]
    fn artifact_uri_roundtrip_toml() {
        // R3：uri 字段 TOML 双形态（有/无）
        let toml_src = r#"
            name = "t"
            version = "1.0.0"
            [[components]]
            name = "f"
            engine = "erlang"
            artifact = { Beam = { app = "frontier", uri = "file://erlang/" } }
        "#;
        let m: AppManifest = toml::from_str(toml_src).unwrap();
        let out = toml::to_string(&m).unwrap();
        let m2: AppManifest = toml::from_str(&out).unwrap();
        assert_eq!(m, m2);
        // uri 保留
        assert!(
            matches!(m.components[0].artifact, ArtifactRef::Beam { ref uri, .. } if uri.is_some())
        );
    }
}
