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

/// desired 状态持久化（E1：提交即持久——崩溃恢复 / Raft 共识由此承载）。
pub trait DesiredStateStore: Send + Sync {
    /// 追加新版本 desired（单调递增 rev）。
    fn persist(&self, rev: u64, m: &AppManifest) -> Result<(), String>;
    /// 读最新持久版本（None = 从未提交）。
    fn latest(&self) -> Result<Option<(u64, AppManifest)>, String>;
}

/// 内存实现（测试 / 单机非持久形态）。
#[derive(Default)]
pub struct MemStateStore {
    inner: std::sync::Mutex<BTreeMap<u64, AppManifest>>,
}

impl MemStateStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DesiredStateStore for MemStateStore {
    fn persist(&self, rev: u64, m: &AppManifest) -> Result<(), String> {
        self.inner.lock().unwrap().insert(rev, m.clone());
        Ok(())
    }
    fn latest(&self) -> Result<Option<(u64, AppManifest)>, String> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .next_back()
            .map(|(r, m)| (*r, m.clone())))
    }
}

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

/// observed 视图（E3 喂入 / 手动注入）。
#[derive(Debug, Clone, Default)]
pub struct ObservedState {
    /// path 前缀 → 实例报告（parrot: /user/comp；方言网关各自映射）。
    pub components: BTreeMap<String, Vec<ComponentStateReport>>,
    /// 存活节点（placement 过滤数据源；空 = 不过滤——本地单机形态）。
    pub live_nodes: Vec<String>,
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
    desired: AppManifest,
    store: Arc<dyn DesiredStateStore>,
    /// 注入时钟（时间纪律——协调/超时判定数据源；E1 首版未消费，
    /// 保留为协议位：RolloutTracker 超时与调和周期将读取）。
    #[allow(dead_code)]
    clock: Arc<dyn Clock>,
    /// 提交修订号（每次 submit 递增——持久化键）。
    rev: u64,
    /// 升级在途标记（RolloutTracker 驱动——Drain/Deploy 间隙的 Wait 来源）。
    in_flight: std::collections::BTreeMap<String, UpgradePolicy>,
}

impl AppSupervisor {
    pub fn new(
        manifest: AppManifest,
        store: Arc<dyn DesiredStateStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            desired: manifest,
            store,
            clock,
            rev: 0,
            in_flight: BTreeMap::new(),
        }
    }

    /// 新版本 desired（升级入口——提交即持久化）。
    pub fn submit(&mut self, m: AppManifest) -> Result<(), String> {
        m.validate_or_err()?;
        self.rev += 1;
        self.store.persist(self.rev, &m)?;
        // 在途升级登记（版本变化的组件——RolloutTracker 协同）
        for c in &m.components {
            if let Some(old) = self.desired.components.iter().find(|o| o.name == c.name) {
                if old.artifact != c.artifact || old.instances != c.instances {
                    self.in_flight.insert(c.name.clone(), c.upgrade.clone());
                }
            }
        }
        self.desired = m;
        Ok(())
    }

    /// 当前 desired（只读——测试断言 / RolloutTracker 输入）。
    pub fn desired(&self) -> &AppManifest {
        &self.desired
    }

    /// 在途升级策略（RolloutTracker 协同查询）。
    pub fn upgrade_in_flight(&self, comp: &str) -> Option<&UpgradePolicy> {
        self.in_flight.get(comp)
    }

    /// 升级完成清记（RolloutTracker Done 后调用）。
    pub fn finish_upgrade(&mut self, comp: &str) {
        self.in_flight.remove(comp);
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
        let mut actions = Vec::new();
        let mut converged = true;
        for spec in &self.desired.components {
            let node = Self::node_for(spec, observed);
            let want = desired_instances(&spec.instances);
            let have = observed.instance_count(&spec.name);
            let versions = observed.versions_of(&spec.name);
            if have == 0 {
                // 形态 1：缺失
                actions.push(ReconcileAction::Deploy {
                    comp: spec.name.clone(),
                    node,
                });
                converged = false;
                continue;
            }
            if self.in_flight.contains_key(&spec.name) {
                // 升级在途——RolloutTracker 驱动（reconcile 不抢跑）
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "upgrade in flight".into(),
                });
                converged = false;
                continue;
            }
            if !versions.iter().all(|v| v == "1") {
                // 形态 3：版本不符（desired 部署版本约定 "1"——
                // deploy_payload 同约定；无在途登记走同步重建弧）
                actions.push(ReconcileAction::Drain {
                    comp: spec.name.clone(),
                });
                actions.push(ReconcileAction::Deploy {
                    comp: spec.name.clone(),
                    node,
                });
                converged = false;
                continue;
            }
            if have < want {
                // 形态 4a：实例不足
                actions.push(ReconcileAction::Deploy {
                    comp: spec.name.clone(),
                    node,
                });
                converged = false;
            } else if have > want {
                // 形态 4b：实例过剩
                actions.push(ReconcileAction::Drain {
                    comp: spec.name.clone(),
                });
                converged = false;
            } else if !observed.any_running(&spec.name) {
                // 形态 5：全实例非 running（starting 等）——等监督
                actions.push(ReconcileAction::Wait {
                    comp: spec.name.clone(),
                    reason: "instances not running".into(),
                });
                converged = false;
            }
        }
        // 形态 2：多余组件（desired 无 / observed 有）
        for comp in observed.components.keys() {
            if !self.desired.components.iter().any(|c| &c.name == comp) {
                actions.push(ReconcileAction::Drain { comp: comp.clone() });
                actions.push(ReconcileAction::Stop { comp: comp.clone() });
                converged = false;
            }
        }
        ReconcileReport { actions, converged }
    }

    /// placement 解析目标节点（live_nodes 空 → 默认节点名）。
    fn node_for(spec: &ComponentSpec, observed: &ObservedState) -> String {
        let wants = &spec.placement;
        if observed.live_nodes.is_empty() {
            return default_node(spec);
        }
        // 首版规则：engine 匹配的首个存活节点（anti_affinity 由多组件
        // 展开时的宿主分配层处理——此处单节点选择）
        let engine_prefix = spec.engine.as_str();
        observed
            .live_nodes
            .iter()
            .find(|n| n.starts_with(engine_prefix) || wants.matches(n))
            .cloned()
            .unwrap_or_else(|| default_node(spec))
    }

    /// ComponentDeploy 生成（Deploy 动作的载荷——宿主下发 admin-v2）。
    pub fn deploy_payload(spec: &ComponentSpec) -> ComponentDeploy {
        ComponentDeploy {
            name: spec.name.clone(),
            version: "1".into(),
            artifact: spec.artifact.clone().into_admin(),
            instances: match &spec.instances {
                InstancePolicy::Singleton | InstancePolicy::Ephemeral => {
                    parrot_remote::admin_v2::AdminInstancePolicy::Singleton
                }
                InstancePolicy::Pool(n) => {
                    parrot_remote::admin_v2::AdminInstancePolicy::Pool { count: *n }
                }
                InstancePolicy::Sharded(n) => {
                    parrot_remote::admin_v2::AdminInstancePolicy::Sharded { count: *n }
                }
            },
            config: None,
        }
    }
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
        let r = s.reconcile_once(&report("a", "running", "1"));
        assert!(r.actions.is_empty());
        assert!(r.converged);
    }

    #[test]
    fn diff_extra_drains_then_stops() {
        let s = sup(manifest("app", vec![]));
        let o = report("orphan", "running", "1");
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
        let r = s.reconcile_once(&report("a", "starting", "1"));
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
                    version: "1".into(),
                })
                .collect(),
        );
        let r = s.reconcile_once(&o);
        assert_eq!(r.actions.len(), 1);
        assert!(matches!(r.actions[0], ReconcileAction::Deploy { .. }));
        // 4/3 → Drain 减
        let mut o4 = ObservedState::default();
        o4.components.insert(
            "p".into(),
            (0..4)
                .map(|i| ComponentStateReport {
                    path: format!("/user/p-{i}"),
                    state: "running".into(),
                    version: "1".into(),
                })
                .collect(),
        );
        let r4 = s.reconcile_once(&o4);
        assert_eq!(
            r4.actions,
            vec![ReconcileAction::Drain { comp: "p".into() }]
        );
    }

    // ── 幂等 ──

    #[test]
    fn reconcile_idempotent_when_converged() {
        let s = sup(manifest("app", vec![spec("a"), spec("b")]));
        let o = {
            let mut o = report("a", "running", "1");
            o.components.insert(
                "b".into(),
                vec![ComponentStateReport {
                    path: "/user/b".into(),
                    state: "running".into(),
                    version: "1".into(),
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
        );
        let mut v2 = spec("a");
        v2.artifact = ArtifactRef::Props {
            factory: "f-a-v2".into(),
        };
        s.submit(manifest("app", vec![v2])).unwrap();
        let (rev, m) = store.latest().unwrap().unwrap();
        assert_eq!(rev, 1);
        assert_eq!(m.components.len(), 1);
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
        let r = s.reconcile_once(&report("a", "running", "1"));
        assert!(matches!(r.actions[0], ReconcileAction::Wait { .. }));
        // 完成 → 清记 → 下轮走重建弧（旧版本报文触发）
        s.finish_upgrade("a");
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
        let d = AppSupervisor::deploy_payload(&pool);
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
    fn empty_desired_no_actions_on_empty_observed() {
        let s = sup(manifest("app", vec![]));
        let r = s.reconcile_once(&ObservedState::default());
        assert!(r.actions.is_empty());
        assert!(r.converged);
    }

    #[test]
    fn missing_and_extra_in_same_pass() {
        // 形态 1+2 并存：desired{a} observed{b} → Deploy a + Drain/Stop b
        let s = sup(manifest("app", vec![spec("a")]));
        let o = report("b", "running", "1");
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
            engine: EngineKind::Parrot,
            artifact: a,
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
                mk(ArtifactRef::Beam { app: "app1".into() }),
                A::Beam { app: "app1".into() },
            ),
        ];
        for (spec, want) in cases {
            assert_eq!(AppSupervisor::deploy_payload(&spec).artifact, want);
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
        st.persist(1, &manifest("app", vec![])).unwrap();
        st.persist(2, &manifest("app2", vec![])).unwrap();
        let (rev, m) = st.latest().unwrap().unwrap();
        assert_eq!((rev, m.name.as_str()), (2, "app2"));
    }
}
