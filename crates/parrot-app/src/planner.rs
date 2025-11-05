//! Planner（DEV_09 §3.1 A2）：依赖 DAG 拓扑排序 + placement 过滤。
//!
//! 行为规约：
//! ① Kahn 算法确定性排序（同输入同序——入度归零批次内按组件名 BTreeSet 序出队）；
//! ② deps 缺失报 `UnknownDependency` 带组件名（validate 已报——plan 复核防御）；
//! ③ placement 无候选 → `Unplaceable` 带原因串；
//! ④ Sharded(n)/Pool(n) 生成 n 个路径（`/user/{name}-{i}`）。

use crate::limits::{add, check, AdmissionError, ApplicationLimits};
use std::collections::{BTreeMap, BTreeSet};

use crate::manifest::{
    validate_with_limits, AppManifest, ComponentSpec, InstancePolicy, ManifestError,
    PlacementConstraint,
};

/// 拓扑视图（测试替身友好——真实实现查 SWIM/Directory）。
pub trait TopologyView: Send + Sync {
    /// 约束下的候选节点集（真实实现：拓扑角色/realm 过滤 + 负载排序）。
    fn candidates(&self, c: &PlacementConstraint) -> Vec<CandidateNode>;
    /// 本节点是否满足角色（本地部署过滤用）。
    fn self_is(&self, role: Option<&str>) -> bool;
}

/// 候选节点描述。
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateNode {
    pub node_id: String,
    pub role: String,
    pub labels: Vec<String>,
    /// 负载水位 `[0,1]`（选节点时低者优先）。
    pub load: f32,
}

/// 空拓扑（本地单机形态：一切约束在本机满足）。
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalTopology;

impl TopologyView for LocalTopology {
    fn candidates(&self, _c: &PlacementConstraint) -> Vec<CandidateNode> {
        vec![CandidateNode {
            node_id: "local".into(),
            role: "normal".into(),
            labels: vec![],
            load: 0.0,
        }]
    }

    fn self_is(&self, role: Option<&str>) -> bool {
        role.is_none_or(|r| r == "normal")
    }
}

/// 规划产物。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Plan {
    /// 依赖序装配序列（Kahn 确定性）。
    pub order: Vec<PlannedComponent>,
    /// 非致命告警（如 anti_affinity 无多候选降级）。
    pub warnings: Vec<String>,
}

/// 单组件规划结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlannedComponent {
    pub spec: ComponentSpec,
    /// Sharded/Pool 的实例路径分配（None = Singleton 单路径）。
    pub shard_plan: Option<Vec<String>>,
    /// placement 选中的节点（首期单候选；Sharded 多实例逐节点分配）。
    pub nodes: Vec<String>,
}

/// 规划错误。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("manifest invalid: {0:?}")]
    Manifest(Vec<ManifestError>),
    #[error("component {comp:?} unplaceable: {reason}")]
    Unplaceable { comp: String, reason: String },
}

/// 规划入口（A2 规约签名）。
pub fn plan(m: &AppManifest, topology: &dyn TopologyView) -> Result<Plan, PlanError> {
    plan_with_limits(m, topology, &ApplicationLimits::default())
}

pub fn plan_with_limits(
    m: &AppManifest,
    topology: &dyn TopologyView,
    limits: &ApplicationLimits,
) -> Result<Plan, PlanError> {
    validate_with_limits(m, limits).map_err(PlanError::Manifest)?;
    let mut expanded = 0;
    for component in &m.components {
        add(
            "expanded bytes",
            &mut expanded,
            limits.path_bytes(component)?,
            limits.max_expanded_bytes,
        )?;
    }

    let mut out = Plan::default();

    // Kahn：入度表 + 邻接表（确定性——BTreeMap 键序）
    let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for c in &m.components {
        indegree.entry(c.name.as_str()).or_insert(0);
        for d in &c.deps {
            *indegree.entry(c.name.as_str()).or_insert(0) += 1;
            dependents
                .entry(d.as_str())
                .or_default()
                .push(c.name.as_str());
        }
    }

    let by_name: BTreeMap<&str, &ComponentSpec> =
        m.components.iter().map(|c| (c.name.as_str(), c)).collect();

    // 就绪集（入度 0——BTreeSet 保名字序确定性）
    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&k, _)| k)
        .collect();
    let mut emitted = 0usize;

    while let Some(&name) = ready.iter().next() {
        ready.remove(&name);
        let c = by_name[name];
        out.order.push(plan_component(
            c,
            topology,
            &mut out.warnings,
            limits,
            &mut expanded,
        )?);
        emitted += 1;
        for &dep in dependents.get(name).into_iter().flatten() {
            let e = indegree.get_mut(dep).expect("indegree entry exists");
            *e -= 1;
            if *e == 0 {
                ready.insert(dep);
            }
        }
    }

    // validate 已确保无环——此处为不变式防御
    debug_assert_eq!(
        emitted,
        m.components.len(),
        "Kahn emitted all (no cycle — validate pre-checked)"
    );
    limits.check_plan(&out)?;
    Ok(out)
}

/// 单组件规划：placement 过滤 + 实例路径分配。
fn plan_component(
    c: &ComponentSpec,
    topology: &dyn TopologyView,
    warnings: &mut Vec<String>,
    limits: &ApplicationLimits,
    expanded: &mut u64,
) -> Result<PlannedComponent, PlanError> {
    let mut cands = topology.candidates(&c.placement);
    check(
        "topology candidates",
        cands.len() as u64,
        limits.max_candidates,
    )?;
    let mut bytes = 0;
    for node in &cands {
        add(
            "candidate bytes",
            &mut bytes,
            std::mem::size_of::<CandidateNode>() as u64,
            limits.max_candidate_bytes,
        )?;
        for text in std::iter::once(&node.node_id)
            .chain(std::iter::once(&node.role))
            .chain(node.labels.iter())
        {
            add(
                "candidate bytes",
                &mut bytes,
                text.len() as u64 + std::mem::size_of::<String>() as u64,
                limits.max_candidate_bytes,
            )?;
        }
        if !node.load.is_finite() || !(0.0..=1.0).contains(&node.load) {
            return Err(PlanError::Unplaceable {
                comp: c.name.clone(),
                reason: "candidate load must be finite and in 0..=1".into(),
            });
        }
    }
    if cands.is_empty() {
        return Err(PlanError::Unplaceable {
            comp: c.name.clone(),
            reason: format!(
                "no candidate node for constraint (role={:?}, realm={:?}, label={:?})",
                c.placement.role, c.placement.realm, c.placement.label
            ),
        });
    }
    // 低负载优先（placement 规约：SWIM 元数据过滤后选水位最低）
    cands.sort_by(|a, b| a.load.total_cmp(&b.load));
    let n = c.instances.instance_count();
    // Admit all selected node clones before allocating paths or assignments.
    if c.placement.anti_affinity && n > 1 {
        for (i, node) in cands.iter().enumerate().take(n) {
            let copies = n / cands.len() + usize::from(i < n % cands.len());
            let charge = (node.node_id.len() as u64 + std::mem::size_of::<String>() as u64)
                .checked_mul(copies as u64)
                .ok_or(AdmissionError::Overflow("expanded bytes"))?;
            add(
                "expanded bytes",
                expanded,
                charge,
                limits.max_expanded_bytes,
            )?;
        }
    } else {
        add(
            "expanded bytes",
            expanded,
            cands[0].node_id.len() as u64 + std::mem::size_of::<String>() as u64,
            limits.max_expanded_bytes,
        )?;
    }
    let shard_plan = match c.instances {
        InstancePolicy::Sharded(_) | InstancePolicy::Pool(_) => {
            Some(c.instance_paths_with_limits(limits)?)
        }
        _ => None,
    };

    // 节点分配：anti_affinity 时逐实例轮转不同节点；否则首选节点
    let nodes = if c.placement.anti_affinity && n > 1 {
        if cands.len() < n {
            warnings.push(format!(
                "component {:?}: anti_affinity requested {} nodes but only {} candidates — reusing nodes",
                c.name, n, cands.len()
            ));
        }
        (0..n)
            .map(|i| cands[i % cands.len()].node_id.clone())
            .collect()
    } else {
        vec![cands[0].node_id.clone()]
    };

    Ok(PlannedComponent {
        spec: c.clone(),
        shard_plan,
        nodes,
    })
}

// ============================================================================
// 测试（§5.1 Planner 15+）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ArtifactRef, ComponentHooks, EngineKind, UpgradePolicy, WireSpec};

    fn comp(name: &str, deps: &[&str]) -> ComponentSpec {
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
            deps: deps.iter().map(|s| s.to_string()).collect(),
            config: None,
            hooks: ComponentHooks::default(),
        }
    }

    fn manifest(comps: Vec<ComponentSpec>) -> AppManifest {
        AppManifest {
            name: "t".into(),
            version: "1.0.0".into(),
            components: comps,
            wiring: vec![],
            mirrors: Vec::new(),
            config_overlay: None,
        }
    }

    /// 固定拓扑替身：给定候选集（可控 placement 匹配）。
    struct FixedTopology(Vec<CandidateNode>);
    impl TopologyView for FixedTopology {
        fn candidates(&self, c: &PlacementConstraint) -> Vec<CandidateNode> {
            self.0
                .iter()
                .filter(|n| c.role.as_deref().is_none_or(|r| n.role == r))
                .filter(|_n| c.realm.is_none()) // 替身：realm 无映射即不匹配
                .filter(|n| {
                    c.label
                        .as_deref()
                        .is_none_or(|l| n.labels.iter().any(|x| x == l))
                })
                .cloned()
                .collect()
        }
        fn self_is(&self, role: Option<&str>) -> bool {
            role.is_none_or(|r| self.0.iter().any(|n| n.role == r))
        }
    }

    #[test]
    fn plan_linear_chain() {
        let m = manifest(vec![comp("c", &["b"]), comp("a", &[]), comp("b", &["a"])]);
        let p = plan(&m, &LocalTopology).unwrap();
        let names: Vec<&str> = p.order.iter().map(|c| c.spec.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn plan_diamond_deps() {
        let m = manifest(vec![
            comp("d", &["b", "c"]),
            comp("b", &["a"]),
            comp("c", &["a"]),
            comp("a", &[]),
        ]);
        let p = plan(&m, &LocalTopology).unwrap();
        let names: Vec<&str> = p.order.iter().map(|c| c.spec.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn plan_deterministic_same_input_same_order() {
        // 构造同层多就绪节点：b/c/d 均只依赖 a——出队序必须是名字序
        let m = manifest(vec![
            comp("d", &["a"]),
            comp("c", &["a"]),
            comp("b", &["a"]),
            comp("a", &[]),
            comp("e", &["b", "c", "d"]),
        ]);
        let p1 = plan(&m, &LocalTopology).unwrap();
        let p2 = plan(&m, &LocalTopology).unwrap();
        let names: Vec<&str> = p1.order.iter().map(|c| c.spec.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d", "e"], "BTreeSet 名字序锁定");
        let names2: Vec<&str> = p2.order.iter().map(|c| c.spec.name.as_str()).collect();
        assert_eq!(names, names2);
    }

    #[test]
    fn plan_independent_components_name_order() {
        let m = manifest(vec![comp("z", &[]), comp("m", &[]), comp("a", &[])]);
        let p = plan(&m, &LocalTopology).unwrap();
        let names: Vec<&str> = p.order.iter().map(|c| c.spec.name.as_str()).collect();
        assert_eq!(names, vec!["a", "m", "z"]);
    }

    #[test]
    fn plan_rejects_invalid_manifest() {
        let mut m = manifest(vec![comp("a", &["ghost"])]);
        m.version = "bad".into();
        let err = plan(&m, &LocalTopology).unwrap_err();
        match err {
            PlanError::Manifest(errs) => assert!(errs.len() >= 2),
            other => panic!("expected manifest error, got {other:?}"),
        }
    }

    #[test]
    fn plan_unknown_dependency_reported_via_manifest() {
        let m = manifest(vec![comp("a", &["missing"])]);
        let err = plan(&m, &LocalTopology).unwrap_err();
        match err {
            PlanError::Manifest(errs) => {
                assert!(
                    matches!(&errs[0], ManifestError::UnknownDependency { comp, dep } if comp == &"a".to_string() && dep == &"missing".to_string())
                );
            }
            other => panic!("expected manifest error, got {other:?}"),
        }
    }

    #[test]
    fn plan_unplaceable_no_candidates() {
        let mut m = manifest(vec![comp("a", &[])]);
        m.components[0].placement.role = Some("hub".into());
        let err = plan(&m, &FixedTopology(vec![])).unwrap_err();
        match err {
            PlanError::Unplaceable { comp, reason } => {
                assert_eq!(comp, "a");
                assert!(reason.contains("no candidate node"), "{reason}");
                assert!(reason.contains("hub"), "{reason}");
            }
            other => panic!("expected unplaceable, got {other:?}"),
        }
    }

    #[test]
    fn plan_placement_role_filter() {
        let topo = FixedTopology(vec![
            CandidateNode {
                node_id: "n1".into(),
                role: "normal".into(),
                labels: vec![],
                load: 0.9,
            },
            CandidateNode {
                node_id: "n2".into(),
                role: "hub".into(),
                labels: vec![],
                load: 0.1,
            },
        ]);
        let mut m = manifest(vec![comp("a", &[])]);
        m.components[0].placement.role = Some("hub".into());
        let p = plan(&m, &topo).unwrap();
        assert_eq!(p.order[0].nodes, vec!["n2"]);
    }

    #[test]
    fn plan_placement_label_filter() {
        let topo = FixedTopology(vec![
            CandidateNode {
                node_id: "edge-1".into(),
                role: "normal".into(),
                labels: vec!["edge".into()],
                load: 0.5,
            },
            CandidateNode {
                node_id: "core-1".into(),
                role: "normal".into(),
                labels: vec!["core".into()],
                load: 0.1,
            },
        ]);
        let mut m = manifest(vec![comp("a", &[])]);
        m.components[0].placement.label = Some("edge".into());
        let p = plan(&m, &topo).unwrap();
        assert_eq!(p.order[0].nodes, vec!["edge-1"]);
    }

    #[test]
    fn plan_load_balanced_pick_lowest() {
        let topo = FixedTopology(vec![
            CandidateNode {
                node_id: "busy".into(),
                role: "normal".into(),
                labels: vec![],
                load: 0.95,
            },
            CandidateNode {
                node_id: "idle".into(),
                role: "normal".into(),
                labels: vec![],
                load: 0.05,
            },
        ]);
        let m = manifest(vec![comp("a", &[])]);
        let p = plan(&m, &topo).unwrap();
        assert_eq!(p.order[0].nodes, vec!["idle"]);
    }

    #[test]
    fn plan_sharded_paths() {
        let mut m = manifest(vec![comp("a", &[])]);
        m.components[0].instances = InstancePolicy::Sharded(4);
        let p = plan(&m, &LocalTopology).unwrap();
        assert_eq!(
            p.order[0].shard_plan.as_deref().unwrap(),
            &["/user/a-0", "/user/a-1", "/user/a-2", "/user/a-3"][..]
        );
    }

    #[test]
    fn plan_pool_paths() {
        let mut m = manifest(vec![comp("w", &[])]);
        m.components[0].instances = InstancePolicy::Pool(3);
        let p = plan(&m, &LocalTopology).unwrap();
        assert_eq!(p.order[0].shard_plan.as_deref().unwrap().len(), 3);
    }

    #[test]
    fn plan_singleton_no_shard_plan() {
        let m = manifest(vec![comp("a", &[])]);
        let p = plan(&m, &LocalTopology).unwrap();
        assert!(p.order[0].shard_plan.is_none());
    }

    #[test]
    fn plan_anti_affinity_spreads_nodes() {
        let topo = FixedTopology(vec![
            CandidateNode {
                node_id: "n1".into(),
                role: "normal".into(),
                labels: vec![],
                load: 0.1,
            },
            CandidateNode {
                node_id: "n2".into(),
                role: "normal".into(),
                labels: vec![],
                load: 0.2,
            },
        ]);
        let mut m = manifest(vec![comp("a", &[])]);
        m.components[0].instances = InstancePolicy::Sharded(4);
        m.components[0].placement.anti_affinity = true;
        let p = plan(&m, &topo).unwrap();
        assert_eq!(p.order[0].nodes, vec!["n1", "n2", "n1", "n2"]);
        assert_eq!(p.warnings.len(), 1, "4 实例 2 候选降级告警");
    }

    #[test]
    fn plan_anti_affinity_enough_nodes_no_warning() {
        let topo = FixedTopology(vec![
            CandidateNode {
                node_id: "n1".into(),
                role: "normal".into(),
                labels: vec![],
                load: 0.1,
            },
            CandidateNode {
                node_id: "n2".into(),
                role: "normal".into(),
                labels: vec![],
                load: 0.2,
            },
        ]);
        let mut m = manifest(vec![comp("a", &[])]);
        m.components[0].instances = InstancePolicy::Pool(2);
        m.components[0].placement.anti_affinity = true;
        let p = plan(&m, &topo).unwrap();
        assert_eq!(p.order[0].nodes, vec!["n1", "n2"]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn valid_wiring_preserves_dependency_order() {
        let m = AppManifest {
            name: "w".into(),
            version: "1.0.0".into(),
            components: vec![comp("a", &[]), comp("b", &["a"])],
            wiring: vec![WireSpec {
                from: "a:/user/out".into(),
                to: "b:/user/b/in".into(),
                qos: "lan".into(),
            }],
            mirrors: Vec::new(),
            config_overlay: None,
        };
        let p = plan(&m, &LocalTopology).unwrap();
        assert_eq!(
            p.order
                .iter()
                .map(|c| c.spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn invalid_wiring_is_rejected_before_topology_queries() {
        struct Untouched;
        impl TopologyView for Untouched {
            fn candidates(&self, _: &PlacementConstraint) -> Vec<CandidateNode> {
                panic!("invalid manifest reached topology");
            }
            fn self_is(&self, _: Option<&str>) -> bool {
                panic!("invalid manifest reached topology");
            }
        }
        let mut m = manifest(vec![comp("a", &[]), comp("b", &["a"])]);
        m.wiring.push(WireSpec {
            from: "a:/user/out".into(),
            to: "b:/user/other".into(),
            qos: "lan".into(),
        });
        assert!(matches!(plan(&m, &Untouched), Err(PlanError::Manifest(_))));
    }

    #[test]
    fn plan_deep_chain_100() {
        let comps: Vec<ComponentSpec> = (0..100)
            .map(|i| {
                let deps = if i == 0 {
                    vec![]
                } else {
                    vec![format!("c{:03}", i - 1)]
                };
                comp(
                    &format!("c{i:03}"),
                    &deps.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                )
            })
            .collect();
        let p = plan(&manifest(comps), &LocalTopology).unwrap();
        assert_eq!(p.order.len(), 100);
        assert_eq!(p.order[99].spec.name, "c099");
    }

    #[test]
    fn excessive_instance_request_is_rejected_before_topology_or_expansion() {
        struct Untouched;
        impl TopologyView for Untouched {
            fn candidates(&self, _: &PlacementConstraint) -> Vec<CandidateNode> {
                panic!("unbounded instance request reached topology before admission");
            }
            fn self_is(&self, _: Option<&str>) -> bool {
                panic!("unbounded instance request reached topology before admission");
            }
        }
        let mut m = manifest(vec![comp("huge", &[])]);
        m.components[0].instances = InstancePolicy::Pool(u32::MAX as usize);
        assert!(plan(&m, &Untouched).is_err());
    }

    #[test]
    fn local_topology_self_is() {
        assert!(LocalTopology.self_is(None));
        assert!(LocalTopology.self_is(Some("normal")));
        assert!(!LocalTopology.self_is(Some("hub")));
    }
}
