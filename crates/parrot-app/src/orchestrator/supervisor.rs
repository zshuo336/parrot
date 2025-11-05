//! E1（DEV_09 §3.5）：AppSupervisor——desired vs observed 调和。
//!
//! 职责：
//! - desired：`AppManifest`（提交即持久化——[`DesiredStateStore`] trait，
//!   Raft 持久化由宿主接线；测试注入内存实现）
//! - observed：组件状态报告（E3 HealthWatch 喂入 / `observe` 手动喂）
//! - `reconcile_once`：单步调和（diff → 动作清单——不依赖定时器，
//!   测试逐 diff 形态驱动；幂等：收敛后再次调和产生空动作）
//!
//! 时钟纪律（09 施工注 8）：所有时间经注入 `Clock`（Raft 头注释明令
//! 禁止系统时间差判任期——同规）。

pub mod clock {
    //! 注入时钟（测试确定性——禁止裸 SystemTime）。

    use std::time::{Duration, SystemTime};

    /// 时钟抽象（SystemTime 直读与 fake 双形态）。
    pub trait Clock: Send + Sync {
        fn now_unix_ms(&self) -> u64;
    }

    /// 生产时钟。
    #[derive(Debug, Clone, Copy, Default)]
    pub struct SystemClock;

    impl Clock for SystemClock {
        fn now_unix_ms(&self) -> u64 {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }
    }

    /// 测试假钟（手动步进）。
    #[derive(Debug, Default)]
    pub struct FakeClock {
        pub ms: std::sync::atomic::AtomicU64,
    }

    impl FakeClock {
        pub fn new(start_ms: u64) -> Self {
            Self {
                ms: std::sync::atomic::AtomicU64::new(start_ms),
            }
        }
        pub fn advance(&self, d: Duration) {
            self.ms
                .fetch_add(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl Clock for FakeClock {
        fn now_unix_ms(&self) -> u64 {
            self.ms.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
}

pub use clock::{Clock, FakeClock, SystemClock};

use crate::manifest::{AppManifest, ComponentSpec, InstancePolicy, UpgradePolicy};
use parrot_remote::admin_v2::{ComponentDeploy, ComponentStateReport};
use std::collections::BTreeMap;
use std::sync::Arc;

pub use super::state::{DesiredStateRecord, DesiredStateStore, MemStateStore, PendingUpgrade};

/// 调和动作（diff 产物——宿主按序执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// 部署组件实例（目标节点方言执行）。
    Deploy { comp: String, node: String },
    /// 排空组件（升级前置 / 移除前置）。
    Drain { comp: String },
    /// 停组件（移除终态）。
    Stop { comp: String },
    /// 等待（在途变更未稳定——下轮再看）。
    Wait { comp: String, reason: String },
}

/// 单步调和报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub actions: Vec<ReconcileAction>,
    /// observed 与 desired 完全一致（无待执行动作且无在途变更）。
    pub converged: bool,
}

/// Placement metadata supplied by the authenticated host discovery layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedNode {
    pub engine: crate::EngineKind,
    pub role: String,
    pub realm: String,
    pub labels: Vec<String>,
}

/// observed 视图（E3 喂入 / 手动注入）。
#[derive(Debug, Clone, Default)]
pub struct ObservedState {
    /// Complete component reports (parrot: /user/comp). In network mode a
    /// missing entry is unknown; an empty entry with matching node provenance
    /// records one successful status query that found no instances.
    pub components: BTreeMap<String, Vec<ComponentStateReport>>,
    /// Authenticated node source of each complete component cohort.
    pub component_nodes: BTreeMap<String, String>,
    /// 存活节点（placement 过滤数据源；空 = 不过滤——本地单机形态）。
    pub live_nodes: Vec<String>,
    /// True after an actual links snapshot, including an empty snapshot.
    pub links_known: bool,
    pub node_metadata: BTreeMap<String, ObservedNode>,
}

impl ObservedState {
    /// 组件在 observed 的版本集合（空 = 未部署）。
    pub fn versions_of(&self, comp: &str) -> Vec<String> {
        self.components
            .get(comp)
            .map(|rs| rs.iter().map(|r| r.version.clone()).collect())
            .unwrap_or_default()
    }

    /// 组件是否有任一实例处于 running。
    pub fn any_running(&self, comp: &str) -> bool {
        self.components
            .get(comp)
            .is_some_and(|rs| rs.iter().any(|r| r.state == "running"))
    }

    /// 组件实例数。
    pub fn instance_count(&self, comp: &str) -> usize {
        self.components.get(comp).map(|v| v.len()).unwrap_or(0)
    }
}

/// 期望实例数（InstancePolicy 语义投影）。
pub fn desired_instances(p: &InstancePolicy) -> usize {
    p.instance_count()
}

/// 组件默认目标节点（placement 后的单节点形态：引擎方言节点名约定）。
pub fn default_node(spec: &ComponentSpec) -> String {
    format!("{}-gw-1", spec.engine.as_str())
}

/// AppSupervisor（desired/observed 差分调和——无内部循环，测试逐形态驱动）。
pub struct AppSupervisor {
    limits: crate::limits::ApplicationLimits,
    pub(super) state: DesiredStateRecord,
    mirror_executor: bool,
    store: Arc<dyn DesiredStateStore>,
    /// 注入时钟（时间纪律——协调/超时判定数据源；E1 首版未消费，
    /// 保留为协议位：RolloutTracker 超时与调和周期将读取）。
    #[allow(dead_code)]
    clock: Arc<dyn Clock>,
}

// ComponentDeploy has no application policy ownership/receipt protocol yet.
// Local AssemblingContext supports mirrors; cluster reconciliation must reject
// them before persistence instead of reporting component-only convergence.
fn require_mirror_executor(manifest: &AppManifest) -> Result<(), String> {
    if manifest.mirrors.is_empty() {
        Ok(())
    } else {
        Err("cluster mirror reconciliation is not implemented; use local application assembly for mirror policies".into())
    }
}

impl AppSupervisor {
    pub fn new(
        manifest: AppManifest,
        store: Arc<dyn DesiredStateStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, String> {
        Self::new_with_limits(
            manifest,
            store,
            clock,
            crate::limits::ApplicationLimits::default(),
        )
    }
    /// The same host policy applies to initial desired state and later submits.
    pub fn new_with_limits(
        manifest: AppManifest,
        store: Arc<dyn DesiredStateStore>,
        clock: Arc<dyn Clock>,
        limits: crate::limits::ApplicationLimits,
    ) -> Result<Self, String> {
        Self::with_executor(manifest, store, clock, limits, false)
    }
    pub(super) fn with_executor(
        manifest: AppManifest,
        store: Arc<dyn DesiredStateStore>,
        clock: Arc<dyn Clock>,
        limits: crate::ApplicationLimits,
        mirror_executor: bool,
    ) -> Result<Self, String> {
        let state = match store.latest()? {
            Some(state) => {
                if state.manifest.name != manifest.name {
                    return Err("stored application identity differs from bootstrap".into());
                }
                state.validate(&limits)?;
                if !mirror_executor {
                    require_mirror_executor(&state.manifest)?;
                    if !state.mirrors.is_empty() {
                        return Err("mirror retirement requires MirrorSupervisor".into());
                    }
                }
                state
            }
            None => {
                let state = DesiredStateRecord {
                    mirrors: Default::default(),
                    revision: 1,
                    manifest,
                    pending: BTreeMap::new(),
                };
                state.validate(&limits)?;
                if !mirror_executor {
                    require_mirror_executor(&state.manifest)?;
                    if !state.mirrors.is_empty() {
                        return Err("mirror retirement requires MirrorSupervisor".into());
                    }
                }
                store.commit(None, &state)?;
                state
            }
        };
        Ok(Self {
            mirror_executor,
            limits,
            state,
            store,
            clock,
        })
    }

    /// Commit complete desired state and pending rollout ownership atomically.
    pub fn submit(&mut self, m: AppManifest) -> Result<(), String> {
        crate::manifest::validate_with_limits(&m, &self.limits).map_err(|e| format!("{e:?}"))?;
        if !self.mirror_executor {
            require_mirror_executor(&m)?;
        }
        if m.name != self.state.manifest.name {
            return Err("cannot change application identity".into());
        }
        self.require_current()?;
        if canonical(&m)? == canonical(&self.state.manifest)? {
            return Ok(());
        }
        let revision = self.next_revision()?;
        let mut pending = self.state.pending.clone();
        pending.retain(|name, _| m.component(name).is_some());
        let global_change = m.version != self.state.manifest.version
            || m.config_overlay.as_ref().map(canonical).transpose()?
                != self
                    .state
                    .manifest
                    .config_overlay
                    .as_ref()
                    .map(canonical)
                    .transpose()?
            || m.wiring != self.state.manifest.wiring;
        for component in &m.components {
            if let Some(old) = self.state.manifest.component(&component.name) {
                if global_change || canonical(component)? != canonical(old)? {
                    pending.insert(
                        component.name.clone(),
                        PendingUpgrade {
                            generation: revision,
                            policy: component.upgrade.clone(),
                        },
                    );
                }
            }
        }
        let next = DesiredStateRecord {
            mirrors: self.state.mirrors.clone(),
            revision,
            manifest: m,
            pending,
        };
        next.validate(&self.limits)?;
        if !self.mirror_executor {
            require_mirror_executor(&next.manifest)?;
            if !next.mirrors.is_empty() {
                return Err("mirror retirement requires MirrorSupervisor".into());
            }
        }
        self.store.commit(Some(self.state.revision), &next)?;
        self.state = next;
        Ok(())
    }

    pub fn desired(&self) -> &AppManifest {
        &self.state.manifest
    }
    pub fn revision(&self) -> u64 {
        self.state.revision
    }
    pub fn upgrade_in_flight(&self, comp: &str) -> Option<&UpgradePolicy> {
        self.state.pending.get(comp).map(|p| &p.policy)
    }
    pub fn upgrade_generation(&self, comp: &str) -> Option<u64> {
        self.state.pending.get(comp).map(|p| p.generation)
    }
    /// The executor must acknowledge the generation it actually deployed.
    /// Late completions cannot clear newer work. Completion is itself persisted.
    pub fn finish_upgrade(&mut self, comp: &str, generation: u64) -> Result<bool, String> {
        self.require_current()?;
        if self.upgrade_generation(comp) != Some(generation) {
            return Ok(false);
        }
        let mut next = self.state.clone();
        next.revision = self.next_revision()?;
        next.pending.remove(comp);
        self.store.commit(Some(self.state.revision), &next)?;
        self.state = next;
        Ok(true)
    }
    pub fn refresh(&mut self) -> Result<(), String> {
        let next = self
            .store
            .latest()?
            .ok_or("committed desired state disappeared")?;
        if next.manifest.name != self.state.manifest.name || next.revision < self.state.revision {
            return Err("invalid desired state recovery identity or revision".into());
        }
        next.validate(&self.limits)?;
        if !self.mirror_executor {
            require_mirror_executor(&next.manifest)?;
            if !next.mirrors.is_empty() {
                return Err("mirror retirement requires MirrorSupervisor".into());
            }
        }
        self.state = next;
        Ok(())
    }
    fn next_revision(&self) -> Result<u64, String> {
        self.state
            .revision
            .checked_add(1)
            .ok_or_else(|| "desired revision exhausted".into())
    }
    pub(super) fn require_current(&self) -> Result<(), String> {
        let revision = self
            .store
            .current_revision()?
            .ok_or("committed desired state disappeared")?;
        if revision != self.state.revision {
            return Err("desired state revision conflict; refresh required".into());
        }
        Ok(())
    }

    /// 调和一步：desired vs observed diff → 动作清单。
    ///
    /// diff 五形态（E1 测试义务）：
    /// 1. 缺失（observed 无）→ Deploy
    /// 2. 多余（desired 无）→ Drain → Stop（两步——先排空）
    /// 3. 版本不符 → 升级路径（in_flight 时 Wait 交 RolloutTracker；
    ///    否则 Drain+Deploy 同步重建）
    /// 4. 实例数不符 → Deploy（补齐）/ Drain 缺口
    /// 5. 状态退化（非 running 且应有）→ Wait（监督自愈预期）
    pub fn reconcile_once(&self, observed: &ObservedState) -> ReconcileReport {
        if !self.state.manifest.mirrors.is_empty() || !self.state.mirrors.is_empty() {
            return ReconcileReport {
                actions: vec![ReconcileAction::Wait {
                    comp: self.state.manifest.name.clone(),
                    reason: "mirror executor receipt required".into(),
                }],
                converged: false,
            };
        }
        self.reconcile_components(observed)
    }
    pub(super) fn commit_mirrors(
        &mut self,
        mirrors: BTreeMap<String, super::mirror::MirrorIntent>,
    ) -> Result<(), String> {
        self.require_current()?;
        let mut next = self.state.clone();
        next.revision = self.next_revision()?;
        next.mirrors = mirrors;
        next.validate(&self.limits)?;
        self.store.commit(Some(self.state.revision), &next)?;
        self.state = next;
        Ok(())
    }
    pub(super) fn reconcile_components(&self, observed: &ObservedState) -> ReconcileReport {
        if let Err(reason) = self
            .require_current()
            .and_then(|_| check_observed(observed, &self.limits))
        {
            return ReconcileReport {
                actions: vec![ReconcileAction::Wait {
                    comp: self.state.manifest.name.clone(),
                    reason,
                }],
                converged: false,
            };
        }
        let mut actions = Vec::new();
        for spec in &self.state.manifest.components {
            if self.state.pending.contains_key(&spec.name) {
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "upgrade in flight".into(),
                });
                continue;
            }
            if spec.deps.iter().any(|name| {
                self.state.pending.contains_key(name)
                    || !Self::healthy_observation(
                        self.state
                            .manifest
                            .component(name)
                            .expect("validated dependency"),
                        &self.state.manifest.version,
                        observed,
                    )
            }) {
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "dependencies not ready".into(),
                });
                continue;
            }
            let reports = observed
                .components
                .get(&spec.name)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut unique = std::collections::BTreeSet::new();
            if reports
                .iter()
                .any(|r| !owned_root(&spec.name, &r.path) || !unique.insert(r.path.as_str()))
            {
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "ambiguous or foreign instance reports".into(),
                });
                continue;
            }
            let Some(node) = Self::node_for(spec, observed) else {
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "no eligible live node".into(),
                });
                continue;
            };
            // No observation is not a negative status receipt. In particular,
            // link loss and generation invalidation deliberately erase stale
            // cohorts; they must not authorize a duplicate deployment elsewhere.
            // An empty report is useful only for the node we intend to mutate.
            if observed.links_known
                && reports.is_empty()
                && (!observed.components.contains_key(&spec.name)
                    || observed.component_nodes.get(&spec.name) != Some(&node))
            {
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "target component absence has not been observed".into(),
                });
                continue;
            }
            if observed.links_known
                && !reports.is_empty()
                && !observed
                    .component_nodes
                    .get(&spec.name)
                    .is_some_and(|node| Self::eligible_node(spec, node, observed))
            {
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "component source node is missing, offline or ineligible".into(),
                });
                continue;
            }
            if Self::healthy_observation(spec, &self.state.manifest.version, observed) {
                continue;
            }
            if reports
                .iter()
                .any(|r| !matches!(r.state.as_str(), "running" | "failed" | "stopped"))
            {
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "instances transitioning".into(),
                });
                continue;
            }

            // The component admin protocol deploys a complete policy, not one
            // missing index. Retire the old cohort before replacing a partial,
            // failed, scaled or wrong-version cohort.
            if !reports.is_empty() {
                actions.push(ReconcileAction::Drain {
                    comp: spec.name.clone(),
                });
                actions.push(ReconcileAction::Stop {
                    comp: spec.name.clone(),
                });
            }
            actions.push(ReconcileAction::Deploy {
                comp: spec.name.clone(),
                node,
            });
        }
        for (comp, reports) in &observed.components {
            if self.state.manifest.component(comp).is_none() && !reports.is_empty() {
                if reports.iter().any(|r| !owned_root(comp, &r.path))
                    || (observed.links_known
                        && !observed
                            .component_nodes
                            .get(comp)
                            .is_some_and(|node| observed.live_nodes.contains(node)))
                {
                    actions.push(ReconcileAction::Wait {
                        comp: comp.clone(),
                        reason: "foreign removal reports".into(),
                    });
                } else {
                    actions.push(ReconcileAction::Drain { comp: comp.clone() });
                    actions.push(ReconcileAction::Stop { comp: comp.clone() });
                }
            }
        }
        ReconcileReport {
            converged: actions.is_empty(),
            actions,
        }
    }

    /// placement 解析目标节点（live_nodes 空 → 默认节点名）。
    fn node_for(spec: &ComponentSpec, observed: &ObservedState) -> Option<String> {
        let wants = &spec.placement;
        // One component-wide Deploy cannot spread a pool across hosts. Never
        // silently degrade an anti-affinity request to one host.
        if wants.anti_affinity && spec.instances.instance_count() > 1 {
            return None;
        }
        if observed.live_nodes.is_empty() && !observed.links_known && *wants == Default::default() {
            return Some(default_node(spec));
        }
        observed
            .live_nodes
            .iter()
            .filter(|node| Self::eligible_node(spec, node, observed))
            .min()
            .cloned()
    }
    fn eligible_node(spec: &ComponentSpec, node: &str, observed: &ObservedState) -> bool {
        let wants = &spec.placement;
        observed.live_nodes.iter().any(|n| n == node)
            && if let Some(metadata) = observed.node_metadata.get(node) {
                metadata.engine == spec.engine
                    && wants.role.as_ref().is_none_or(|v| v == &metadata.role)
                    && wants.realm.as_ref().is_none_or(|v| v == &metadata.realm)
                    && wants
                        .label
                        .as_ref()
                        .is_none_or(|v| metadata.labels.contains(v))
            } else {
                *wants == Default::default()
                    && node.starts_with(&format!("{}-", spec.engine.as_str()))
            }
    }
    pub(super) fn healthy_observation(
        spec: &ComponentSpec,
        version: &str,
        observed: &ObservedState,
    ) -> bool {
        if observed.links_known
            && spec.placement.anti_affinity
            && desired_instances(&spec.instances) > 1
        {
            return false;
        }
        healthy(
            spec,
            version,
            observed
                .components
                .get(&spec.name)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        ) && (!observed.links_known
            || observed
                .component_nodes
                .get(&spec.name)
                .is_some_and(|node| Self::eligible_node(spec, node, observed)))
    }

    /// ComponentDeploy 生成（Deploy 动作的载荷——宿主下发 admin-v2）。
    pub fn deploy_payload(&self, spec: &ComponentSpec) -> Result<ComponentDeploy, String> {
        spec.deployment(&self.state.manifest.version)
    }
}

fn canonical(value: &impl serde::Serialize) -> Result<String, String> {
    toml::to_string(value).map_err(|e| e.to_string())
}
pub(super) fn owned_root(comp: &str, path: &str) -> bool {
    let Some(root) = path.strip_prefix("/user/") else {
        return false;
    };
    if !crate::manifest::valid_path_segment(comp) {
        return false;
    }
    if root == comp {
        return true;
    }
    root.strip_prefix(comp)
        .and_then(|s| s.strip_prefix('-'))
        .is_some_and(|s| {
            !s.is_empty()
                && (s == "0" || !s.starts_with('0'))
                && s.bytes().all(|b| b.is_ascii_digit())
                && s.parse::<u32>().is_ok()
        })
}
fn healthy(spec: &ComponentSpec, version: &str, reports: &[ComponentStateReport]) -> bool {
    if reports.len() != desired_instances(&spec.instances) {
        return false;
    }
    let mut paths = std::collections::BTreeSet::new();
    reports.iter().all(|r| {
        r.state == "running"
            && r.version == version
            && owned_root(&spec.name, &r.path)
            && !r.path[6..].contains('/')
            && spec.owns_wire_path(&r.path)
            && paths.insert(r.path.as_str())
    })
}
fn check_observed(
    observed: &ObservedState,
    limits: &crate::ApplicationLimits,
) -> Result<(), String> {
    use crate::limits::{add, check};
    let check_all = || -> Result<(), crate::AdmissionError> {
        check(
            "observed components",
            observed.components.len() as u64,
            limits.max_components,
        )?;
        check(
            "observed live nodes",
            observed.live_nodes.len() as u64,
            limits.max_candidates,
        )?;
        let (mut rows, mut bytes) = (0, 0);
        for (name, reports) in &observed.components {
            add(
                "observed instances",
                &mut rows,
                reports.len() as u64,
                limits.max_instances,
            )?;
            add(
                "observed bytes",
                &mut bytes,
                name.len() as u64,
                limits.max_metadata_bytes,
            )?;
            for r in reports {
                add(
                    "observed bytes",
                    &mut bytes,
                    (r.path.len()
                        + r.state.len()
                        + r.version.len()
                        + std::mem::size_of::<ComponentStateReport>()) as u64,
                    limits.max_metadata_bytes,
                )?;
            }
        }
        check(
            "observed node metadata",
            observed.node_metadata.len() as u64,
            limits.max_candidates,
        )?;
        for (node, metadata) in &observed.node_metadata {
            for value in std::iter::once(node)
                .chain([&metadata.role, &metadata.realm])
                .chain(metadata.labels.iter())
            {
                add(
                    "observed bytes",
                    &mut bytes,
                    (value.len() + std::mem::size_of::<String>()) as u64,
                    limits.max_metadata_bytes,
                )?;
            }
        }
        check(
            "observed component sources",
            observed.component_nodes.len() as u64,
            limits.max_components,
        )?;
        for (comp, node) in &observed.component_nodes {
            add(
                "observed bytes",
                &mut bytes,
                (comp.len() + node.len() + 2 * std::mem::size_of::<String>()) as u64,
                limits.max_metadata_bytes,
            )?;
        }
        for node in &observed.live_nodes {
            add(
                "observed bytes",
                &mut bytes,
                node.len() as u64,
                limits.max_metadata_bytes,
            )?;
        }
        Ok(())
    };
    check_all().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ArtifactRef;
    use crate::manifest::EngineKind;

    fn manifest(name: &str, comps: Vec<ComponentSpec>) -> AppManifest {
        AppManifest {
            name: name.into(),
            version: "1.0.0".into(),
            components: comps,
            wiring: vec![],
            mirrors: Vec::new(),
            config_overlay: None,
        }
    }

    fn spec(name: &str) -> ComponentSpec {
        ComponentSpec {
            name: name.into(),
            engine: EngineKind::Parrot,
            artifact: ArtifactRef::Props {
                factory: format!("f-{name}"),
            },
            alt_artifact: None,
            instances: InstancePolicy::Singleton,
            placement: Default::default(),
            upgrade: Default::default(),
            deps: vec![],
            config: None,
            hooks: Default::default(),
        }
    }

    fn sup(manifest: AppManifest) -> AppSupervisor {
        AppSupervisor::new(
            manifest,
            Arc::new(MemStateStore::new()),
            Arc::new(FakeClock::new(1_000)),
        )
        .unwrap()
    }

    fn report(comp: &str, state: &str, version: &str) -> ObservedState {
        let mut o = ObservedState::default();
        o.components.insert(
            comp.into(),
            vec![ComponentStateReport {
                path: format!("/user/{comp}"),
                state: state.into(),
                version: version.into(),
            }],
        );
        o
    }

    // ── diff 五形态 ──

    #[test]
    fn diff_missing_deploys() {
        let s = sup(manifest("app", vec![spec("a")]));
        let r = s.reconcile_once(&ObservedState::default());
        assert_eq!(
            r.actions,
            vec![ReconcileAction::Deploy {
                comp: "a".into(),
                node: "parrot-gw-1".into()
            }]
        );
        assert!(!r.converged);
    }

    #[test]
    fn diff_converged_empty_actions() {
        let s = sup(manifest("app", vec![spec("a")]));
        let r = s.reconcile_once(&report("a", "running", "1.0.0"));
        assert!(r.actions.is_empty());
        assert!(r.converged);
    }

    #[test]
    fn diff_extra_drains_then_stops() {
        let s = sup(manifest("app", vec![spec("a")]));
        let mut o = report("orphan", "running", "1.0.0");
        o.components
            .extend(report("a", "running", "1.0.0").components);
        let r = s.reconcile_once(&o);
        assert_eq!(
            r.actions,
            vec![
                ReconcileAction::Drain {
                    comp: "orphan".into()
                },
                ReconcileAction::Stop {
                    comp: "orphan".into()
                },
            ]
        );
        assert!(!r.converged);
    }

    #[test]
    fn diff_version_mismatch_recreates() {
        let s = sup(manifest("app", vec![spec("a")]));
        let r = s.reconcile_once(&report("a", "running", "2"));
        assert_eq!(
            r.actions,
            vec![
                ReconcileAction::Drain { comp: "a".into() },
                ReconcileAction::Stop { comp: "a".into() },
                ReconcileAction::Deploy {
                    comp: "a".into(),
                    node: "parrot-gw-1".into()
                },
            ]
        );
    }

    #[test]
    fn diff_not_running_waits() {
        let s = sup(manifest("app", vec![spec("a")]));
        let r = s.reconcile_once(&report("a", "starting", "1.0.0"));
        assert!(matches!(r.actions[0], ReconcileAction::Wait { .. }));
        assert!(!r.converged);
    }

    // ── 实例数形态 ──

    #[test]
    fn diff_pool_scale_up_and_down() {
        let mut pool = spec("p");
        pool.instances = InstancePolicy::Pool(3);
        let s = sup(manifest("app", vec![pool]));
        // 1/3 → Deploy 补
        let mut o = ObservedState::default();
        o.components.insert(
            "p".into(),
            (0..1)
                .map(|i| ComponentStateReport {
                    path: format!("/user/p-{i}"),
                    state: "running".into(),
                    version: "1.0.0".into(),
                })
                .collect(),
        );
        let r = s.reconcile_once(&o);
        assert_eq!(
            r.actions,
            vec![
                ReconcileAction::Drain { comp: "p".into() },
                ReconcileAction::Stop { comp: "p".into() },
                ReconcileAction::Deploy {
                    comp: "p".into(),
                    node: "parrot-gw-1".into()
                },
            ]
        );
        // 4/3 → Drain 减
        let mut o4 = ObservedState::default();
        o4.components.insert(
            "p".into(),
            (0..4)
                .map(|i| ComponentStateReport {
                    path: format!("/user/p-{i}"),
                    state: "running".into(),
                    version: "1.0.0".into(),
                })
                .collect(),
        );
        let r4 = s.reconcile_once(&o4);
        assert_eq!(
            r4.actions,
            vec![
                ReconcileAction::Drain { comp: "p".into() },
                ReconcileAction::Stop { comp: "p".into() },
                ReconcileAction::Deploy {
                    comp: "p".into(),
                    node: "parrot-gw-1".into()
                }
            ]
        );
    }

    // ── 幂等 ──

    #[test]
    fn reconcile_idempotent_when_converged() {
        let s = sup(manifest("app", vec![spec("a"), spec("b")]));
        let o = {
            let mut o = report("a", "running", "1.0.0");
            o.components.insert(
                "b".into(),
                vec![ComponentStateReport {
                    path: "/user/b".into(),
                    state: "running".into(),
                    version: "1.0.0".into(),
                }],
            );
            o
        };
        let r1 = s.reconcile_once(&o);
        let r2 = s.reconcile_once(&o);
        assert_eq!(r1, r2);
        assert!(r1.converged);
    }

    // ── submit / 持久化 ──

    #[test]
    fn submit_persists_and_increments_rev() {
        let store = Arc::new(MemStateStore::new());
        let mut s = AppSupervisor::new(
            manifest("app", vec![spec("a")]),
            store.clone(),
            Arc::new(FakeClock::new(0)),
        )
        .unwrap();
        let mut v2 = spec("a");
        v2.artifact = ArtifactRef::Props {
            factory: "f-a-v2".into(),
        };
        s.submit(manifest("app", vec![v2])).unwrap();
        let committed = store.latest().unwrap().unwrap();
        assert_eq!(committed.revision, 2); // bootstrap is the first durable commit
        assert_eq!(committed.manifest.components.len(), 1);
        // 版本变化组件已登记在途
        assert!(s.upgrade_in_flight("a").is_some());
    }

    #[test]
    fn submit_invalid_manifest_rejected() {
        let mut s = sup(manifest("app", vec![spec("a")]));
        let bad = AppManifest {
            name: "app".into(),
            version: "not-semver".into(), // 校验失败
            components: vec![],
            wiring: vec![],
            mirrors: Vec::new(),
            config_overlay: None,
        };
        assert!(s.submit(bad).is_err());
    }

    #[test]
    fn in_flight_upgrade_waits() {
        let mut s = sup(manifest("app", vec![spec("a")]));
        let mut v2 = spec("a");
        v2.artifact = ArtifactRef::Props {
            factory: "v2".into(),
        };
        s.submit(manifest("app", vec![v2])).unwrap();
        let r = s.reconcile_once(&report("a", "running", "1.0.0"));
        assert!(matches!(r.actions[0], ReconcileAction::Wait { .. }));
        // 完成 → 清记 → 下轮走重建弧（旧版本报文触发）
        assert!(s
            .finish_upgrade("a", s.upgrade_generation("a").unwrap())
            .unwrap());
        let r2 = s.reconcile_once(&report("a", "running", "0"));
        assert!(matches!(r2.actions[0], ReconcileAction::Drain { .. }));
    }

    // ── placement / 载荷 ──

    #[test]
    fn node_respects_live_nodes() {
        let s = sup(manifest("app", vec![spec("a")]));
        let o = ObservedState {
            live_nodes: vec!["parrot-gw-2".into(), "akka-gw-1".into()],
            ..Default::default()
        };
        let r = s.reconcile_once(&o);
        match &r.actions[0] {
            ReconcileAction::Deploy { node, .. } => assert_eq!(node, "parrot-gw-2"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn deploy_payload_maps_fields() {
        let mut pool = spec("p");
        pool.instances = InstancePolicy::Sharded(4);
        let d = sup(manifest("app", vec![pool.clone()]))
            .deploy_payload(&pool)
            .unwrap();
        assert_eq!(d.name, "p");
        assert!(matches!(
            d.instances,
            parrot_remote::admin_v2::AdminInstancePolicy::Sharded { count: 4 }
        ));
        assert_eq!(
            d.artifact,
            parrot_remote::admin_v2::AdminArtifactRef::Props {
                factory: "f-p".into()
            }
        );
    }

    // ── E1 补充（凑 20+：边界形态）──

    #[test]
    fn ephemeral_counts_one() {
        let mut e = spec("e");
        e.instances = InstancePolicy::Ephemeral;
        assert_eq!(desired_instances(&e.instances), 1);
        let s = sup(manifest("app", vec![e]));
        let r = s.reconcile_once(&ObservedState::default());
        assert!(matches!(r.actions[0], ReconcileAction::Deploy { .. }));
    }

    #[test]
    fn empty_desired_rejected_at_initialization_and_submit() {
        let store = Arc::new(MemStateStore::new());
        assert!(AppSupervisor::new(
            manifest("app", vec![]),
            store.clone(),
            Arc::new(FakeClock::new(0))
        )
        .is_err());
        let mut s = sup(manifest("app", vec![spec("a")]));
        assert!(s.submit(manifest("app", vec![])).is_err());
        assert_eq!(s.desired().components.len(), 1);
        assert!(store.latest().unwrap().is_none());
    }

    #[test]
    fn missing_and_extra_in_same_pass() {
        // 形态 1+2 并存：desired{a} observed{b} → Deploy a + Drain/Stop b
        let s = sup(manifest("app", vec![spec("a")]));
        let o = report("b", "running", "1.0.0");
        let r = s.reconcile_once(&o);
        assert_eq!(r.actions.len(), 3);
        assert!(matches!(&r.actions[0], ReconcileAction::Deploy { comp, .. } if comp == "a"));
        assert!(matches!(&r.actions[1], ReconcileAction::Drain { comp } if comp == "b"));
        assert!(matches!(&r.actions[2], ReconcileAction::Stop { comp } if comp == "b"));
    }

    #[test]
    fn multi_component_deterministic_order() {
        // desired 组件序即动作序（确定性——同输入同输出）
        let s = sup(manifest("app", vec![spec("z"), spec("a"), spec("m")]));
        let r1 = s.reconcile_once(&ObservedState::default());
        let r2 = s.reconcile_once(&ObservedState::default());
        assert_eq!(r1, r2);
        let names: Vec<&str> = r1
            .actions
            .iter()
            .map(|a| match a {
                ReconcileAction::Deploy { comp, .. } => comp.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["z", "a", "m"]);
    }

    #[test]
    fn upgrade_policy_registered_on_submit() {
        let mut s = sup(manifest("app", vec![spec("a")]));
        let mut v2 = spec("a");
        v2.upgrade = UpgradePolicy::Rolling { max_surge: 2 };
        v2.artifact = ArtifactRef::Props {
            factory: "v2".into(),
        };
        s.submit(manifest("app", vec![v2])).unwrap();
        assert!(matches!(
            s.upgrade_in_flight("a"),
            Some(UpgradePolicy::Rolling { max_surge: 2 })
        ));
    }

    #[test]
    fn unchanged_component_not_marked_in_flight() {
        let mut s = sup(manifest("app", vec![spec("a"), spec("b")]));
        // 只换 a；b 不动
        let mut v2 = spec("a");
        v2.artifact = ArtifactRef::Props {
            factory: "v2".into(),
        };
        let b = spec("b");
        s.submit(manifest("app", vec![v2, b])).unwrap();
        assert!(s.upgrade_in_flight("a").is_some());
        assert!(s.upgrade_in_flight("b").is_none());
    }

    #[test]
    fn deploy_payload_all_artifact_kinds() {
        let mk = |a: ArtifactRef| ComponentSpec {
            name: "c".into(),
            engine: if matches!(a, ArtifactRef::Beam { .. }) {
                EngineKind::Erlang
            } else {
                EngineKind::Parrot
            },
            artifact: a,
            alt_artifact: None,
            ..spec("c")
        };
        use parrot_remote::admin_v2::AdminArtifactRef as A;
        let cases = vec![
            (
                mk(ArtifactRef::Wasm {
                    digest: "d".into(),
                    uri: "u".into(),
                }),
                A::Wasm {
                    digest: "d".into(),
                    uri: "u".into(),
                },
            ),
            (
                mk(ArtifactRef::Beam {
                    app: "app1".into(),
                    uri: None,
                }),
                A::Beam {
                    app: "app1".into(),
                    uri: None,
                },
            ),
        ];
        for (spec, want) in cases {
            assert_eq!(
                sup(manifest("app", vec![spec.clone()]))
                    .deploy_payload(&spec)
                    .unwrap()
                    .artifact,
                want
            );
        }
    }

    #[test]
    fn observed_helpers() {
        let o = report("a", "running", "9");
        assert_eq!(o.instance_count("a"), 1);
        assert!(o.any_running("a"));
        assert_eq!(o.versions_of("a"), vec!["9".to_string()]);
        assert_eq!(o.instance_count("missing"), 0);
        assert!(!o.any_running("missing"));
    }

    #[test]
    fn clock_injected_fake() {
        let c = FakeClock::new(100);
        assert_eq!(c.now_unix_ms(), 100);
        c.advance(std::time::Duration::from_millis(50));
        assert_eq!(c.now_unix_ms(), 150);
    }

    #[test]
    fn store_roundtrip() {
        let st = MemStateStore::new();
        assert!(st.latest().unwrap().is_none());
        let mut state = DesiredStateRecord {
            mirrors: Default::default(),
            revision: 1,
            manifest: manifest("app", vec![spec("a")]),
            pending: BTreeMap::new(),
        };
        st.commit(None, &state).unwrap();
        state.revision = 2;
        state.manifest.version = "2.0.0".into();
        st.commit(Some(1), &state).unwrap();
        let latest = st.latest().unwrap().unwrap();
        assert_eq!(
            (latest.revision, latest.manifest.version.as_str()),
            (2, "2.0.0")
        );
    }
    #[test]
    fn deployment_preserves_nested_component_configuration() {
        let mut component = spec("configured");
        component.config = Some(
            toml::from_str("limit = 7\nlabel = '中文'\n[nested]\nenabled = true\nitems = [1, 2]")
                .unwrap(),
        );
        let deployment = sup(manifest("app", vec![component.clone()]))
            .deploy_payload(&component)
            .unwrap();
        let bytes = deployment
            .config
            .expect("component config must reach the executor");
        let actual: toml::Table = toml::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(Some(actual), component.config);
    }
}
