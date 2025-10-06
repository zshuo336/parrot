//! AppManifest 数据模型（DEV_09 §3.1 A1——09 §2.2 原样落地）。
//!
//! 应用定义与运行时执行解耦的单一事实源：Rust 类型（程序化构造/测试断言）
//! + TOML 文件（`*.app.toml`，运维可读可 diff），serde 互转。
//!
//! 分层铁律（09 §2.2）：本 crate 不依赖 `parrot` 主 crate——引擎访问经
//! parrot-remote 的 PropsFactory/facade 协议。

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
    /// 并入 parrot-config 文件层之前（Y3：overlay > 代码 > parrot.toml > 默认）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_overlay: Option<toml::value::Table>,
}

/// 组件声明（09 §2.2 ComponentSpec 原样）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ComponentSpec {
    pub name: String,
    pub engine: EngineKind,
    pub artifact: ArtifactRef,
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
pub enum ArtifactRef {
    /// parrot：编译期工厂名（inventory PropsFactory）。
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
    Jvm {
        main_class: String,
        coords: Option<String>,
    },
    /// ray 网关：python 模块（ray job API working_dir）。
    PyModule {
        module: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_env: Option<toml::value::Value>,
    },
    /// erlang 网关：OTP app（code:load_abs 热加载）。
    Beam { app: String },
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

    /// → admin-v2 镜像形态（E1 deploy 载荷生成用）。
    /// PyModule.runtime_env：TOML 值 → 文本（方言侧再解析）。
    pub fn into_admin(self) -> parrot_remote::admin_v2::AdminArtifactRef {
        use parrot_remote::admin_v2::AdminArtifactRef as A;
        match self {
            ArtifactRef::Props { factory } => A::Props { factory },
            ArtifactRef::Wasm { digest, uri } => A::Wasm { digest, uri },
            ArtifactRef::Dylib { digest, uri, abi } => A::Dylib { digest, uri, abi },
            ArtifactRef::Jvm { main_class, coords } => A::Jvm { main_class, coords },
            ArtifactRef::PyModule { module, runtime_env } => A::PyModule {
                module,
                runtime_env: runtime_env.map(|v| v.to_string()),
            },
            ArtifactRef::Beam { app } => A::Beam { app },
        }
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
        [self.role.as_deref(), self.realm.as_deref(), self.label.as_deref()]
            .into_iter()
            .flatten()
            .all(|k| node.contains(k))
    }
}

/// 升级策略（09 §6.1 三策略）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
pub struct WireSpec {
    /// "frontier:/user/next"（组件名:组件内路径）。
    pub from: String,
    /// "index:/user/build"。
    pub to: String,
    /// QoS 档位（复用 topology 语义："wan"/"lan"）。
    #[serde(default)]
    pub qos: String,
}

// ============================================================================
// 校验器（A1 行为规约 ①：全项通过才可进 Planner；错误驱动测试的全部分支源）
// ============================================================================

/// 校验错误（逐项返回——不 panic）。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ManifestError {
    #[error("duplicate component name: {0}")]
    DuplicateComponent(String),
    #[error("component {comp:?} depends on unknown {dep:?}")]
    UnknownDependency { comp: String, dep: String },
    #[error("dependency cycle: {0:?}")]
    CycleDetected(Vec<String>),
    #[error("wiring unreachable: {from:?} -> {to:?}")]
    WiringUnreachable { from: String, to: String },
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
}

/// 手写 semver 校验（MAJOR.MINOR.PATCH[-prerelease][+build]——省 semver 依赖）。
pub fn is_valid_semver(v: &str) -> bool {
    // 剥 build metadata
    let core = match v.split_once('+') {
        Some((c, build))
            if !build.is_empty()
                && build
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-') =>
        {
            c
        }
        Some(_) => return false,
        None => v,
    };
    // 剥 prerelease
    let nums = match core.split_once('-') {
        Some((c, pre))
            if !pre.is_empty()
                && pre
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.') =>
        {
            c
        }
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
    let mut errs = Vec::new();

    // ① app 级 semver
    if !is_valid_semver(&m.version) {
        errs.push(ManifestError::BadSemver(m.version.clone()));
    }

    // ② 空组件表
    if m.components.is_empty() {
        errs.push(ManifestError::EmptyComponents);
        return Err(errs); // 后续检查无意义
    }

    // ③ 组件重名
    let mut seen = std::collections::BTreeSet::new();
    for c in &m.components {
        if !seen.insert(&c.name) {
            errs.push(ManifestError::DuplicateComponent(c.name.clone()));
        }
    }

    // ④ 依赖缺失
    for c in &m.components {
        for d in &c.deps {
            if !seen.contains(d) {
                errs.push(ManifestError::UnknownDependency {
                    comp: c.name.clone(),
                    dep: d.clone(),
                });
            }
        }
    }

    // ⑤ 依赖环（DFS 三色标记 + 环路径回放）
    if let Some(cycle) = find_cycle(m) {
        errs.push(ManifestError::CycleDetected(cycle));
    }

    // ⑥ engine-artifact 匹配表
    for c in &m.components {
        if !c.engine.accepts(&c.artifact) {
            errs.push(ManifestError::EngineArtifactMismatch {
                comp: c.name.clone(),
                engine: c.engine,
                artifact: c.artifact.kind().to_string(),
            });
        }
    }

    // ⑦ wiring 可达性（from/to 的组件名必须已声明）
    for w in &m.wiring {
        let heads = [
            w.from.split(':').next().unwrap_or(""),
            w.to.split(':').next().unwrap_or(""),
        ];
        let bad = heads.iter().any(|head| !seen.contains(&head.to_string()));
        if bad {
            errs.push(ManifestError::WiringUnreachable {
                from: w.from.clone(),
                to: w.to.clone(),
            });
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

/// DFS 环检测（确定性：按组件名 BTreeMap 序访问；环路径回放）。
fn find_cycle(m: &AppManifest) -> Option<Vec<String>> {
    use std::collections::BTreeMap;
    let deps: BTreeMap<&str, Vec<&str>> = m
        .components
        .iter()
        .map(|c| (c.name.as_str(), c.deps.iter().map(|d| d.as_str()).collect()))
        .collect();

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }
    let mut color: BTreeMap<&str, Color> = deps.keys().map(|k| (*k, Color::White)).collect();
    let mut stack: Vec<&str> = Vec::new();

    fn dfs<'a>(
        u: &'a str,
        deps: &BTreeMap<&'a str, Vec<&'a str>>,
        color: &mut BTreeMap<&'a str, Color>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        color.insert(u, Color::Grey);
        stack.push(u);
        for &v in deps.get(u).into_iter().flatten() {
            if !deps.contains_key(v) {
                continue; // 未知依赖由 ④ 报
            }
            match color.get(v) {
                Some(Color::Grey) => {
                    // 回放环：栈中 v 起
                    let start = stack.iter().position(|&x| x == v).unwrap();
                    let cycle: Vec<String> = stack[start..]
                        .iter()
                        .map(|s| s.to_string())
                        .chain(std::iter::once(v.to_string()))
                        .collect();
                    return Some(cycle);
                }
                Some(Color::White) => {
                    if let Some(c) = dfs(v, deps, color, stack) {
                        return Some(c);
                    }
                }
                _ => {}
            }
        }
        stack.pop();
        color.insert(u, Color::Black);
        None
    }

    for &k in deps.keys() {
        if color.get(k) == Some(&Color::White) {
            if let Some(c) = dfs(k, &deps, &mut color, &mut stack) {
                return Some(c);
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
        toml::to_string(self)
    }

    /// 从 TOML 文本解析。
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// 从文件加载（`*.app.toml`）。
    pub fn from_file(path: &Path) -> Result<Self, AppLoadError> {
        let s = std::fs::read_to_string(path)?;
        Ok(Self::from_toml(&s)?)
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

impl From<ArtifactRef> for parrot_remote::admin_v2::AdminArtifactRef {
    fn from(a: ArtifactRef) -> Self {
        use parrot_remote::admin_v2::AdminArtifactRef;
        match a {
            ArtifactRef::Props { factory } => AdminArtifactRef::Props { factory },
            ArtifactRef::Wasm { digest, uri } => AdminArtifactRef::Wasm { digest, uri },
            ArtifactRef::Dylib { digest, uri, abi } => AdminArtifactRef::Dylib { digest, uri, abi },
            ArtifactRef::Jvm { main_class, coords } => AdminArtifactRef::Jvm { main_class, coords },
            ArtifactRef::PyModule {
                module,
                runtime_env,
            } => AdminArtifactRef::PyModule {
                module,
                // toml::Value → TOML 文本（wire 同形策略见 admin_v2.rs 文档）
                runtime_env: runtime_env.and_then(|v| toml::to_string(&v).ok()),
            },
            ArtifactRef::Beam { app } => AdminArtifactRef::Beam { app },
        }
    }
}

impl From<parrot_remote::admin_v2::AdminArtifactRef> for ArtifactRef {
    fn from(a: parrot_remote::admin_v2::AdminArtifactRef) -> Self {
        use parrot_remote::admin_v2::AdminArtifactRef;
        match a {
            AdminArtifactRef::Props { factory } => ArtifactRef::Props { factory },
            AdminArtifactRef::Wasm { digest, uri } => ArtifactRef::Wasm { digest, uri },
            AdminArtifactRef::Dylib { digest, uri, abi } => ArtifactRef::Dylib { digest, uri, abi },
            AdminArtifactRef::Jvm { main_class, coords } => ArtifactRef::Jvm { main_class, coords },
            AdminArtifactRef::PyModule {
                module,
                runtime_env,
            } => ArtifactRef::PyModule {
                module,
                // TOML 文本 → toml::Value（解析失败容忍 None——方言侧兜底）
                runtime_env: runtime_env.and_then(|s| toml::from_str(&s).ok()),
            },
            AdminArtifactRef::Beam { app } => ArtifactRef::Beam { app },
        }
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
                to: "b:/user/in".into(),
                qos: "lan".into(),
            }],
            config_overlay: None,
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
            },
            ArtifactRef::PyModule {
                module: "m".into(),
                runtime_env: None,
            },
            ArtifactRef::Beam { app: "a".into() },
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
            to: "b:/user/in".into(),
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
                    },
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
                    },
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
                    },
                    instances: InstancePolicy::Singleton,
                    placement: Default::default(),
                    upgrade: UpgradePolicy::default(),
                    deps: vec![],
                    config: None,
                    hooks: Default::default(),
                },
            ],
            wiring: vec![],
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
            ArtifactRef::Beam { app: "a".into() },
            ArtifactRef::PyModule {
                module: "m".into(),
                runtime_env: None,
            },
            ArtifactRef::Jvm {
                main_class: "M".into(),
                coords: None,
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
            let wire: AdminArtifactRef = a.clone().into();
            let back: ArtifactRef = wire.into();
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
        };
        let wire: AdminArtifactRef = orig.clone().into();
        let back: ArtifactRef = wire.into();
        assert_eq!(back, orig, "toml 文本互转保真");
    }
}
