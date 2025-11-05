//! Resource policy is enforced before topology, allocation and runtime side effects.
use parrot_app::limits::{AdmissionError, ApplicationLimits};
use parrot_app::manifest::{
    validate_with_limits, AppLoadError, AppManifest, ArtifactRef, ComponentSpec, EngineKind,
    InstancePolicy, ManifestError, PlacementConstraint, WireSpec,
};
use parrot_app::planner::{plan_with_limits, CandidateNode, LocalTopology, TopologyView};

fn spec(name: &str, count: usize) -> ComponentSpec {
    ComponentSpec {
        name: name.into(),
        engine: EngineKind::Parrot,
        artifact: ArtifactRef::Props {
            factory: "test.echo".into(),
        },
        alt_artifact: None,
        instances: InstancePolicy::Pool(count),
        placement: Default::default(),
        upgrade: Default::default(),
        deps: vec![],
        config: None,
        hooks: Default::default(),
    }
}
fn manifest(components: Vec<ComponentSpec>) -> AppManifest {
    AppManifest {
        name: "admission".into(),
        version: "1.0.0".into(),
        components,
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    }
}
fn exceeded(error: AdmissionError, resource: &str) {
    assert!(
        matches!(&error, AdmissionError::Exceeded { resource: got, .. } if *got == resource),
        "{error:?}"
    );
}

#[test]
fn instance_budget_is_inclusive_and_cumulative() {
    let limits = ApplicationLimits {
        max_instances: 5,
        ..Default::default()
    };
    let mut m = manifest(vec![spec("a", 2), spec("b", 3)]);
    assert!(plan_with_limits(&m, &LocalTopology, &limits).is_ok());
    m.components[1].instances = InstancePolicy::Pool(4);
    exceeded(limits.check_manifest(&m).unwrap_err(), "instances");
    exceeded(
        spec("single", 6)
            .instance_paths_with_limits(&limits)
            .unwrap_err(),
        "instances",
    );
    assert!(spec("single", 5)
        .instance_paths_with_limits(&limits)
        .is_ok());
    exceeded(
        spec("huge", usize::MAX).instance_paths().unwrap_err(),
        "instances",
    );
}

#[test]
fn expanded_paths_charge_utf8_and_decimal_digit_boundaries() {
    for count in [1, 9, 10, 11, 99, 100, 101] {
        let component = spec("中文", count);
        let paths = component.instance_paths().unwrap();
        let exact = paths
            .iter()
            .map(|p| p.len() + std::mem::size_of::<String>())
            .sum::<usize>() as u64;
        let mut limits = ApplicationLimits {
            max_expanded_bytes: exact,
            ..Default::default()
        };
        assert_eq!(
            component.instance_paths_with_limits(&limits).unwrap(),
            paths
        );
        limits.max_expanded_bytes -= 1;
        exceeded(
            component.instance_paths_with_limits(&limits).unwrap_err(),
            "expanded bytes",
        );
    }
    let component = spec(&"x".repeat(4096), 5000);
    exceeded(component.instance_paths().unwrap_err(), "expanded bytes");
}

#[test]
fn components_dependencies_and_wires_have_independent_cumulative_limits() {
    let mut a = spec("a", 1);
    a.deps = vec!["b".into()];
    let mut c = spec("c", 1);
    c.deps = vec!["b".into()];
    let mut m = manifest(vec![a, spec("b", 1), c]);
    m.wiring = vec![
        WireSpec {
            from: "a:/user/a/query".into(),
            to: "b:/user/b-0".into(),
            qos: "lan".into(),
        },
        WireSpec {
            from: "c:/user/c/query".into(),
            to: "b:/user/b-0".into(),
            qos: "lan".into(),
        },
    ];
    let limits = ApplicationLimits {
        max_components: 3,
        max_dependencies: 2,
        max_wires: 2,
        ..Default::default()
    };
    validate_with_limits(&m, &limits).unwrap();
    exceeded(
        ApplicationLimits {
            max_components: 2,
            ..limits
        }
        .check_manifest(&m)
        .unwrap_err(),
        "components",
    );
    exceeded(
        ApplicationLimits {
            max_dependencies: 1,
            ..limits
        }
        .check_manifest(&m)
        .unwrap_err(),
        "dependencies",
    );
    exceeded(
        ApplicationLimits {
            max_wires: 1,
            ..limits
        }
        .check_manifest(&m)
        .unwrap_err(),
        "wires",
    );
}

#[test]
fn source_limit_precedes_parsing_and_bounds_sparse_file_reads() {
    let m = manifest(vec![spec("a", 1)]);
    let source = m.to_toml().unwrap();
    let limits = ApplicationLimits {
        max_source_bytes: source.len() as u64,
        ..Default::default()
    };
    assert_eq!(
        AppManifest::from_toml_with_limits(&source, &limits).unwrap(),
        m
    );
    let small = ApplicationLimits {
        max_source_bytes: limits.max_source_bytes - 1,
        ..limits
    };
    assert!(AppManifest::from_toml_with_limits(&source, &small)
        .unwrap_err()
        .to_string()
        .contains("source bytes"));
    assert!(
        AppManifest::from_toml_with_limits(&"!".repeat(source.len() + 1), &limits)
            .unwrap_err()
            .to_string()
            .contains("source bytes")
    );
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), &source).unwrap();
    assert_eq!(
        AppManifest::from_file_with_limits(file.path(), &limits).unwrap(),
        m
    );
    file.as_file().set_len(1 << 34).unwrap(); // 16 GiB sparse file, never allocate/read its full length.
    match AppManifest::from_file_with_limits(file.path(), &limits).unwrap_err() {
        AppLoadError::Admission(AdmissionError::Exceeded {
            resource: "source bytes",
            requested,
            limit,
        }) => {
            assert_eq!(requested, limit + 1);
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn metadata_budget_counts_escaped_model_without_reencoding_a_buffer() {
    let mut m = manifest(vec![spec("a", 1)]);
    m.components[0].config = Some(toml::Table::from_iter([(
        "text".into(),
        toml::Value::String("\n\"中文".repeat(50)),
    )]));
    let bytes = serde_json::to_vec(&m).unwrap().len() as u64;
    let limits = ApplicationLimits {
        max_metadata_bytes: bytes,
        ..Default::default()
    };
    limits.check_manifest(&m).unwrap();
    exceeded(
        ApplicationLimits {
            max_metadata_bytes: bytes - 1,
            ..limits
        }
        .check_manifest(&m)
        .unwrap_err(),
        "metadata bytes",
    );
}

#[test]
fn config_depth_and_total_values_include_containers_and_overlay() {
    let mut m = manifest(vec![spec("a", 1), spec("b", 1)]);
    for c in &mut m.components {
        c.config = Some(toml::from_str("items = [1, 2]").unwrap()); // table + array + two leaves
    }
    m.config_overlay = Some(toml::Table::new()); // another container
    let limits = ApplicationLimits {
        max_config_depth: 3,
        max_config_values: 9,
        ..Default::default()
    };
    limits.check_manifest(&m).unwrap();
    exceeded(
        ApplicationLimits {
            max_config_values: 8,
            ..limits
        }
        .check_manifest(&m)
        .unwrap_err(),
        "config values",
    );
    exceeded(
        ApplicationLimits {
            max_config_depth: 2,
            ..limits
        }
        .check_manifest(&m)
        .unwrap_err(),
        "config depth",
    );
    let mut value = toml::Value::Integer(1);
    for _ in 0..64 {
        value = toml::Value::Array(vec![value]);
    }
    m.components[0].config = Some(toml::Table::from_iter([("deep".into(), value)]));
    exceeded(
        ApplicationLimits::default().check_manifest(&m).unwrap_err(),
        "config depth",
    );
}

#[test]
fn runtime_env_is_charged_for_primary_and_alternative_artifacts() {
    let mut c = spec("py", 1);
    c.engine = EngineKind::Ray;
    let artifact = ArtifactRef::PyModule {
        module: "fixture".into(),
        uri: None,
        runtime_env: Some(toml::Value::Array(vec![toml::Value::Integer(1)])),
    };
    c.artifact = artifact.clone();
    c.alt_artifact = Some(artifact);
    let m = manifest(vec![c]);
    let limits = ApplicationLimits {
        max_config_values: 4,
        ..Default::default()
    };
    limits.check_manifest(&m).unwrap();
    exceeded(
        ApplicationLimits {
            max_config_values: 3,
            ..limits
        }
        .check_manifest(&m)
        .unwrap_err(),
        "config values",
    );
}

#[test]
fn deep_dependency_graph_uses_heap_stack_and_replays_real_cycle() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let mut m = manifest(
                (0..1000)
                    .map(|i| {
                        let mut c = spec(&format!("n{i:04}"), 1);
                        if i < 999 {
                            c.deps.push(format!("n{:04}", i + 1));
                        }
                        c
                    })
                    .collect(),
            );
            let limits = ApplicationLimits::default();
            validate_with_limits(&m, &limits).unwrap();
            let planned = plan_with_limits(&m, &LocalTopology, &limits).unwrap();
            assert_eq!(planned.order[0].spec.name, "n0999");
            m.components[999].deps.push("n0997".into());
            let errors = validate_with_limits(&m, &limits).unwrap_err();
            assert_eq!(
                errors,
                vec![ManifestError::CycleDetected(vec![
                    "n0997".into(),
                    "n0998".into(),
                    "n0999".into(),
                    "n0997".into()
                ])]
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

struct Candidates(Vec<CandidateNode>);
impl TopologyView for Candidates {
    fn candidates(&self, _: &PlacementConstraint) -> Vec<CandidateNode> {
        self.0.clone()
    }
    fn self_is(&self, _: Option<&str>) -> bool {
        true
    }
}
fn node(name: &str, load: f32) -> CandidateNode {
    CandidateNode {
        node_id: name.into(),
        role: "normal".into(),
        labels: vec![],
        load,
    }
}

#[test]
fn topology_limits_apply_before_sorting_and_selected_node_cloning() {
    let mut c = spec("a", 3);
    c.placement.anti_affinity = true;
    let m = manifest(vec![c]);
    let candidates = Candidates(vec![node("one", 0.4), node("two", 0.2)]);
    let limits = ApplicationLimits {
        max_candidates: 2,
        ..Default::default()
    };
    let p = plan_with_limits(&m, &candidates, &limits).unwrap();
    assert_eq!(p.order[0].nodes, ["two", "one", "two"]);
    assert!(plan_with_limits(
        &m,
        &candidates,
        &ApplicationLimits {
            max_candidates: 1,
            ..limits
        }
    )
    .unwrap_err()
    .to_string()
    .contains("topology candidates"));
    let candidate_bytes = candidates
        .0
        .iter()
        .map(|n| {
            std::mem::size_of::<CandidateNode>()
                + n.node_id.len()
                + n.role.len()
                + 2 * std::mem::size_of::<String>()
        })
        .sum::<usize>() as u64;
    plan_with_limits(
        &m,
        &candidates,
        &ApplicationLimits {
            max_candidate_bytes: candidate_bytes,
            ..limits
        },
    )
    .unwrap();
    assert!(plan_with_limits(
        &m,
        &candidates,
        &ApplicationLimits {
            max_candidate_bytes: candidate_bytes - 1,
            ..limits
        }
    )
    .unwrap_err()
    .to_string()
    .contains("candidate bytes"));
    let expanded_bytes = p.order[0]
        .shard_plan
        .as_ref()
        .unwrap()
        .iter()
        .chain(p.order[0].nodes.iter())
        .map(|s| s.len() + std::mem::size_of::<String>())
        .sum::<usize>() as u64;
    plan_with_limits(
        &m,
        &candidates,
        &ApplicationLimits {
            max_expanded_bytes: expanded_bytes,
            ..limits
        },
    )
    .unwrap();
    assert!(plan_with_limits(
        &m,
        &candidates,
        &ApplicationLimits {
            max_expanded_bytes: expanded_bytes - 1,
            ..limits
        }
    )
    .unwrap_err()
    .to_string()
    .contains("expanded bytes"));
}

#[test]
fn topology_rejects_nan_infinite_and_out_of_range_loads() {
    let m = manifest(vec![spec("a", 1)]);
    for load in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.01] {
        assert!(plan_with_limits(
            &m,
            &Candidates(vec![node("bad", load)]),
            &Default::default()
        )
        .unwrap_err()
        .to_string()
        .contains("candidate load"));
    }
    for load in [0.0, 1.0] {
        plan_with_limits(
            &m,
            &Candidates(vec![node("good", load)]),
            &Default::default(),
        )
        .unwrap();
    }
}

#[test]
fn mutable_plan_rechecks_supplied_strings_counts_and_aggregate_config() {
    let mut m = manifest(vec![spec("a", 1), spec("b", 1)]);
    for c in &mut m.components {
        c.config = Some(toml::from_str("value = 1").unwrap());
    }
    let p = plan_with_limits(&m, &LocalTopology, &Default::default()).unwrap();
    exceeded(
        ApplicationLimits {
            max_config_values: 3,
            ..Default::default()
        }
        .check_plan(&p)
        .unwrap_err(),
        "config values",
    );
    for change in 0..5 {
        let mut p = p.clone();
        match change {
            0 => p.order[0].nodes = vec!["x".repeat(300)],
            1 => p.order[0].shard_plan = Some(vec!["/user/".repeat(60)]),
            2 => p.order[0].nodes = vec!["x".into(); 4],
            3 => p.order[0].shard_plan = Some(vec!["x".into(); 4]),
            _ => p.order[0].spec.instances = InstancePolicy::Pool(4),
        }
        let limits = ApplicationLimits {
            max_expanded_bytes: 256,
            max_instances: 3,
            ..Default::default()
        };
        assert!(limits.check_plan(&p).is_err(), "forgery {change}");
    }
}

// Every host callback is a side effect sentinel, including configuration.
struct NoSideEffects;
use parrot_api::types::BoxedActorRef;
use parrot_app::assemble::{
    AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner,
};
#[async_trait::async_trait]
impl ParrotSpawner for NoSideEffects {
    fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        panic!("configured before admission")
    }
    async fn spawn_props(
        &self,
        _: &str,
        _: &[String],
        _: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("spawned before admission")
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("stopped before admission")
    }
}
#[async_trait::async_trait]
impl GatewayFactory for NoSideEffects {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        panic!("gateway configured before admission")
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("gateway started before admission")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("gateway stopped before admission")
    }
}

#[tokio::test]
async fn assembly_rejects_loose_plan_and_mutated_plan_before_any_host_callback() {
    let m = manifest(vec![spec("a", 3)]);
    let original = plan_with_limits(&m, &LocalTopology, &Default::default()).unwrap();
    let host = NoSideEffects;
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        parrot_spawner: &host,
        gateway_factory: &host,
    };
    for change in 0..5 {
        let mut p = original.clone();
        let limits = if change == 0 {
            ApplicationLimits {
                max_instances: 2,
                ..Default::default()
            }
        } else {
            match change {
                1 => p.order[0].nodes = vec!["x".repeat(512)],
                2 => p.order[0].shard_plan = Some(vec!["x".repeat(512)]),
                3 => p.order[0].spec.instances = InstancePolicy::Pool(99999),
                _ => p.order[0].spec.config = Some(toml::from_str("secret = true").unwrap()),
            }
            ApplicationLimits {
                max_expanded_bytes: 512,
                ..Default::default()
            }
        };
        let mut context = AssemblingContext::new_with_limits(limits);
        assert!(
            context
                .assemble(&p, &deployer, &m, &Default::default())
                .await
                .is_err(),
            "change {change}"
        );
        assert!(context.started().is_empty());
    }
}

use parrot_app::orchestrator::supervisor::{AppSupervisor, DesiredStateStore, FakeClock};
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct RecordingStore {
    attempts: Mutex<Vec<u64>>,
    fail: std::sync::atomic::AtomicBool,
    inner: parrot_app::orchestrator::MemStateStore,
}
impl DesiredStateStore for RecordingStore {
    fn commit(
        &self,
        expected: Option<u64>,
        state: &parrot_app::orchestrator::DesiredStateRecord,
    ) -> Result<(), String> {
        self.attempts.lock().unwrap().push(state.revision);
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err("injected durable write failure".into())
        } else {
            self.inner.commit(expected, state)
        }
    }
    fn latest(&self) -> Result<Option<parrot_app::orchestrator::DesiredStateRecord>, String> {
        self.inner.latest()
    }
}

#[test]
fn supervisor_rejects_initial_and_submitted_over_budget_state_without_persistence() {
    let store = Arc::new(RecordingStore::default());
    let limits = ApplicationLimits {
        max_instances: 2,
        ..Default::default()
    };
    let too_large = manifest(vec![spec("a", 3)]);
    assert!(AppSupervisor::new_with_limits(
        too_large.clone(),
        store.clone(),
        Arc::new(FakeClock::new(0)),
        limits
    )
    .is_err());
    let initial = manifest(vec![spec("a", 1)]);
    let mut supervisor = AppSupervisor::new_with_limits(
        initial.clone(),
        store.clone(),
        Arc::new(FakeClock::new(0)),
        limits,
    )
    .unwrap();
    assert!(supervisor.submit(too_large).is_err());
    assert_eq!(supervisor.desired(), &initial);
    assert!(supervisor.upgrade_in_flight("a").is_none());
    assert_eq!(*store.attempts.lock().unwrap(), [1]); // only validated bootstrap committed
    supervisor.submit(manifest(vec![spec("a", 2)])).unwrap();
    assert_eq!(*store.attempts.lock().unwrap(), [1, 2]);
}

#[test]
fn failed_persistence_keeps_revision_desired_and_upgrade_state_unchanged() {
    use std::sync::atomic::Ordering;
    let store = Arc::new(RecordingStore::default());
    let initial = manifest(vec![spec("a", 1)]);
    let mut supervisor =
        AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0))).unwrap();
    store.fail.store(true, Ordering::SeqCst);
    let update = manifest(vec![spec("a", 2)]);
    assert!(supervisor.submit(update.clone()).is_err());
    assert_eq!(supervisor.desired(), &initial);
    assert!(supervisor.upgrade_in_flight("a").is_none());
    store.fail.store(false, Ordering::SeqCst);
    supervisor.submit(update.clone()).unwrap();
    assert_eq!(supervisor.desired(), &update);
    assert!(supervisor.upgrade_in_flight("a").is_some());
    assert_eq!(*store.attempts.lock().unwrap(), [1, 2, 2]);
}

#[test]
fn unknown_dependency_fanout_diagnostics_remain_bounded() {
    let mut c = spec(&"x".repeat(10_000), 1);
    c.deps = (0..128).map(|i| format!("missing{i}")).collect();
    let m = manifest(vec![c]);
    let errors = validate_with_limits(&m, &Default::default()).unwrap_err();
    assert!(
        errors.len() <= 8,
        "diagnostic fanout copied a 10KB component name {} times",
        errors.len()
    );
    assert!(matches!(
        errors.last(),
        Some(ManifestError::Admission(AdmissionError::Exceeded {
            resource: "diagnostic bytes",
            ..
        }))
    ));
    let one = ApplicationLimits {
        max_diagnostic_bytes: 0,
        ..Default::default()
    };
    let errors = validate_with_limits(&m, &one).unwrap_err();
    assert_eq!(errors.len(), 1); // only the fixed-size budget diagnostic
                                 // A tiny diagnostic budget cannot reject an otherwise valid manifest.
    validate_with_limits(&manifest(vec![spec("valid", 1)]), &one).unwrap();
}
