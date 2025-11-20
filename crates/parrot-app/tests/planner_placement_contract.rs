use parrot_app::{
    planner::{plan_with_limits, CandidateNode, PlanError, TopologyView},
    AdmissionError, AppManifest, ApplicationLimits, ArtifactRef, ComponentSpec, EngineKind,
    InstancePolicy, PlacementConstraint,
};

struct Topology(Vec<CandidateNode>);
impl TopologyView for Topology {
    fn candidates(&self, _: &PlacementConstraint) -> Vec<CandidateNode> {
        self.0.clone()
    }
    fn self_is(&self, _: Option<&str>) -> bool {
        true
    }
}

fn node(id: &str, load: f32) -> CandidateNode {
    CandidateNode {
        node_id: id.into(),
        role: "normal".into(),
        labels: vec![],
        load,
    }
}

fn manifest(policy: InstancePolicy, anti_affinity: bool) -> AppManifest {
    AppManifest {
        name: "placement-contract".into(),
        version: "1.0.0".into(),
        components: vec![ComponentSpec {
            name: "worker".into(),
            engine: EngineKind::Parrot,
            artifact: ArtifactRef::Props {
                factory: "worker".into(),
            },
            alt_artifact: None,
            instances: policy,
            placement: PlacementConstraint {
                anti_affinity,
                ..Default::default()
            },
            upgrade: Default::default(),
            deps: vec![],
            config: None,
            hooks: Default::default(),
        }],
        wiring: vec![],
        mirrors: vec![],
        config_overlay: None,
    }
}

fn unplaceable(error: PlanError, expected: &str) {
    match error {
        PlanError::Unplaceable { comp, reason } => {
            assert_eq!(comp, "worker");
            assert!(reason.contains(expected), "{reason}");
        }
        other => panic!("expected placement rejection, got {other:?}"),
    }
}

#[test]
fn anti_affinity_never_silently_reuses_an_insufficient_host_set() {
    let topology = Topology(vec![node("first", 0.1), node("second", 0.2)]);
    for policy in [InstancePolicy::Pool(3), InstancePolicy::Sharded(3)] {
        unplaceable(
            plan_with_limits(&manifest(policy, true), &topology, &Default::default())
                .expect_err("co-located anti-affinity plan accepted"),
            "anti_affinity",
        );
    }
}

#[test]
fn duplicate_host_identity_is_not_additional_capacity() {
    for anti_affinity in [false, true] {
        for load in [0.1, 0.9] {
            let topology = Topology(vec![node("same-host", 0.1), node("same-host", load)]);
            unplaceable(
                plan_with_limits(
                    &manifest(InstancePolicy::Pool(2), anti_affinity),
                    &topology,
                    &Default::default(),
                )
                .expect_err("ambiguous duplicate host accepted"),
                "duplicate candidate node",
            );
        }
    }
}

#[test]
fn empty_or_control_character_host_identity_is_rejected_even_when_not_selected() {
    for id in ["", "bad\0node", "bad\nnode", "bad\rnode", "bad\u{7f}node"] {
        let topology = Topology(vec![node("good", 0.0), node(id, 1.0)]);
        unplaceable(
            plan_with_limits(
                &manifest(InstancePolicy::Singleton, false),
                &topology,
                &Default::default(),
            )
            .expect_err("invalid host identity accepted"),
            "invalid candidate node",
        );
    }
}

#[test]
fn equal_load_host_selection_is_stable_across_provider_enumeration_order() {
    for ids in [
        ["c", "b", "a"],
        ["a", "c", "b"],
        ["b", "a", "c"],
        ["b", "c", "a"],
        ["c", "a", "b"],
        ["a", "b", "c"],
    ] {
        let topology = Topology(ids.iter().map(|id| node(id, 0.5)).collect());
        for (policy, anti_affinity, expected) in [
            (InstancePolicy::Singleton, false, vec!["a"]),
            (InstancePolicy::Pool(2), true, vec!["a", "b"]),
        ] {
            let plan = plan_with_limits(
                &manifest(policy, anti_affinity),
                &topology,
                &Default::default(),
            )
            .unwrap();
            assert_eq!(plan.order[0].nodes, expected, "provider order {ids:?}");
        }
    }
}

#[test]
fn signed_zero_loads_use_the_same_identity_tie_break() {
    for loads in [[0.0, -0.0], [-0.0, 0.0]] {
        let topology = Topology(vec![node("z", loads[0]), node("a", loads[1])]);
        let plan = plan_with_limits(
            &manifest(InstancePolicy::Singleton, false),
            &topology,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(plan.order[0].nodes, ["a"]);
    }
}

#[test]
fn sufficient_anti_affinity_assigns_one_distinct_host_per_instance() {
    let topology = Topology(vec![
        node("third", 1.0),
        node("first", 0.0),
        node("second", 0.5),
    ]);
    for policy in [InstancePolicy::Pool(3), InstancePolicy::Sharded(3)] {
        let plan =
            plan_with_limits(&manifest(policy, true), &topology, &Default::default()).unwrap();
        let component = &plan.order[0];
        assert_eq!(component.nodes, ["first", "second", "third"]);
        assert_eq!(
            component.shard_plan.as_ref().unwrap(),
            &["/user/worker-0", "/user/worker-1", "/user/worker-2"]
        );
        assert!(plan.warnings.is_empty());
    }
}

#[test]
fn co_location_without_anti_affinity_keeps_one_selected_host() {
    let topology = Topology(vec![node("busy", 0.9), node("idle", 0.1)]);
    let plan = plan_with_limits(
        &manifest(InstancePolicy::Pool(4), false),
        &topology,
        &Default::default(),
    )
    .unwrap();
    assert_eq!(plan.order[0].nodes, ["idle"]);
    assert_eq!(plan.order[0].shard_plan.as_ref().unwrap().len(), 4);
}

#[test]
fn empty_topology_remains_a_placement_error() {
    unplaceable(
        plan_with_limits(
            &manifest(InstancePolicy::Singleton, false),
            &Topology(vec![]),
            &Default::default(),
        )
        .unwrap_err(),
        "no candidate node",
    );
}

#[test]
fn exact_path_and_distinct_host_budget_is_accepted_and_one_less_is_rejected() {
    let manifest = manifest(InstancePolicy::Sharded(2), true);
    let topology = Topology(vec![node("alpha", 0.1), node("beta", 0.2)]);
    let plan = plan_with_limits(&manifest, &topology, &Default::default()).unwrap();
    let component = &plan.order[0];
    let exact: usize = component
        .shard_plan
        .as_ref()
        .unwrap()
        .iter()
        .chain(component.nodes.iter())
        .map(|s| s.len() + std::mem::size_of::<String>())
        .sum();
    let limits = ApplicationLimits {
        max_expanded_bytes: exact as u64,
        ..Default::default()
    };
    plan_with_limits(&manifest, &topology, &limits).unwrap();
    assert!(matches!(
        plan_with_limits(
            &manifest,
            &topology,
            &ApplicationLimits {
                max_expanded_bytes: exact as u64 - 1,
                ..limits
            },
        ),
        Err(PlanError::Admission(AdmissionError::Exceeded {
            resource: "expanded bytes",
            ..
        }))
    ));
}

#[test]
fn candidate_count_admission_precedes_identity_validation() {
    let manifest = manifest(InstancePolicy::Singleton, false);
    let topology = Topology(vec![node("", 0.1), node("", 0.1)]);
    assert!(matches!(
        plan_with_limits(
            &manifest,
            &topology,
            &ApplicationLimits {
                max_candidates: 1,
                ..Default::default()
            },
        ),
        Err(PlanError::Admission(AdmissionError::Exceeded {
            resource: "topology candidates",
            ..
        }))
    ));
}
