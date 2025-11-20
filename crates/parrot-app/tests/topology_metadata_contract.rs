use parrot_api::types::BoxedActorRef;
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
    planner::{
        plan, plan_with_limits, CandidateNode, LocalTopology, Plan, PlanError, TopologyView,
    },
    AppManifest, ApplicationLimits, ArtifactRef, ComponentSpec, EngineKind, InstancePolicy,
    PlacementConstraint,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Topology(Vec<CandidateNode>);
impl TopologyView for Topology {
    fn candidates(&self, _: &PlacementConstraint) -> Vec<CandidateNode> {
        self.0.clone()
    }
    fn self_is(&self, role: Option<&str>) -> bool {
        role.is_none_or(|role| role == "normal")
    }
}

fn node(id: &str, role: &str, labels: &[&str], load: f32) -> CandidateNode {
    CandidateNode {
        node_id: id.into(),
        role: role.into(),
        labels: labels.iter().map(|label| (*label).into()).collect(),
        load,
    }
}

fn constraint(role: Option<&str>, realm: Option<&str>, label: Option<&str>) -> PlacementConstraint {
    PlacementConstraint {
        role: role.map(Into::into),
        realm: realm.map(Into::into),
        label: label.map(Into::into),
        anti_affinity: false,
    }
}

fn manifest(placement: PlacementConstraint) -> AppManifest {
    AppManifest {
        name: "metadata-contract".into(),
        version: "1.0.0".into(),
        components: vec![ComponentSpec {
            name: "worker".into(),
            engine: EngineKind::Parrot,
            artifact: ArtifactRef::Props {
                factory: "metadata-probe".into(),
            },
            alt_artifact: None,
            instances: InstancePolicy::Singleton,
            placement,
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

fn unplaceable(result: Result<Plan, PlanError>) {
    assert!(
        matches!(&result, Err(PlanError::Unplaceable { comp, .. }) if comp == "worker"),
        "{result:?}"
    );
}

#[test]
fn default_local_topology_cannot_invent_role_realm_or_labels() {
    for placement in [
        constraint(Some("hub"), None, None),
        constraint(Some("border"), None, None),
        constraint(Some("directory"), None, None),
        constraint(None, Some("local"), None),
        constraint(None, None, Some("local")),
        constraint(Some("normal"), Some("local"), Some("local")),
    ] {
        assert!(
            LocalTopology.candidates(&placement).is_empty(),
            "{placement:?}"
        );
        unplaceable(plan(&manifest(placement), &LocalTopology));
    }
}

#[test]
fn default_local_topology_preserves_normal_and_unconstrained_placement() {
    for role in [None, Some("normal")] {
        let mut manifest = manifest(constraint(role, None, None));
        manifest.components[0].instances = InstancePolicy::Pool(2);
        let plan = plan(&manifest, &LocalTopology).unwrap();
        assert_eq!(plan.order[0].nodes, ["local"]);
        assert_eq!(plan.order[0].shard_plan.as_ref().unwrap().len(), 2);
    }
}

#[test]
fn planner_checks_exact_role_and_label_before_ranking() {
    let mut nodes = vec![
        node("cheap", "normal", &["gpu"], 0.0),
        node("role-prefix", "hub-backup", &["gpu"], 0.1),
        node("label-prefix", "hub", &["gpu-extra"], 0.2),
        node("target", "hub", &["ssd", "gpu"], 0.9),
    ];
    let manifest = manifest(constraint(Some("hub"), None, Some("gpu")));
    for _ in 0..2 {
        let plan = plan(&manifest, &Topology(nodes.clone())).unwrap();
        assert_eq!(plan.order[0].nodes, ["target"]);
        nodes.reverse();
    }
}

#[test]
fn planner_rejects_a_provider_with_no_matching_metadata() {
    for (role, labels) in [("normal", vec!["gpu"]), ("hub", vec!["gpu-extra"])] {
        unplaceable(plan(
            &manifest(constraint(Some("hub"), None, Some("gpu"))),
            &Topology(vec![node("hub-gpu-in-name-only", role, &labels, 0.0)]),
        ));
    }
}

#[test]
fn metadata_filtering_cannot_hide_candidate_budget_or_identity_errors() {
    let manifest = manifest(constraint(Some("hub"), None, None));
    let topology = Topology(vec![
        node("other", "normal", &[], 0.0),
        node("target", "hub", &[], 1.0),
    ]);
    let limits = ApplicationLimits {
        max_candidates: 1,
        ..Default::default()
    };
    assert!(matches!(
        plan_with_limits(&manifest, &topology, &limits),
        Err(PlanError::Admission(_))
    ));
    let invalid = Topology(vec![
        node("", "normal", &[], 0.0),
        node("target", "hub", &[], 1.0),
    ]);
    unplaceable(plan(&manifest, &invalid));
}

#[test]
fn anti_affinity_capacity_counts_only_matching_nodes() {
    let mut manifest = manifest(constraint(Some("hub"), None, Some("gpu")));
    manifest.components[0].instances = InstancePolicy::Pool(2);
    manifest.components[0].placement.anti_affinity = true;
    unplaceable(plan(
        &manifest,
        &Topology(vec![
            node("first", "hub", &["gpu"], 0.0),
            node("second", "normal", &["gpu"], 0.1),
        ]),
    ));
}

#[derive(Default)]
struct Host(AtomicUsize);
#[async_trait::async_trait]
impl ParrotSpawner for Host {
    fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(DeployError::Engine("metadata configuration probe".into()))
    }
    async fn spawn_props(
        &self,
        _: &str,
        _: &[String],
        _: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("configuration probe never spawns")
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("configuration probe owns no actors")
    }
}
#[async_trait::async_trait]
impl GatewayFactory for Host {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        panic!("gateway configuration follows successful spawner configuration")
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("configuration probe never starts gateways")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("configuration probe owns no gateways")
    }
}

async fn assemble(
    manifest: &AppManifest,
    plan: &Plan,
    topology: &dyn TopologyView,
    accepted: bool,
) {
    let host = Host::default();
    let deployer = LocalDeployer {
        topology,
        parrot_spawner: &host,
        gateway_factory: &host,
    };
    let mut context = AssemblingContext::new();
    let result = context
        .assemble(plan, &deployer, manifest, &Default::default())
        .await;
    if accepted {
        assert!(
            matches!(&result, Err(DeployError::Engine(reason)) if reason == "metadata configuration probe"),
            "{result:?}"
        );
        assert_eq!(host.0.load(Ordering::SeqCst), 1);
    } else {
        assert!(
            matches!(&result, Err(DeployError::Config(reason)) if reason.contains("placement")),
            "{result:?}"
        );
        assert_eq!(host.0.load(Ordering::SeqCst), 0);
    }
    assert!(context.started().is_empty());
}

#[tokio::test]
async fn local_assembly_checks_its_own_role_even_when_planner_has_a_matching_peer() {
    let manifest = manifest(constraint(Some("hub"), None, None));
    let topology = Topology(vec![node("peer", "hub", &[], 0.0)]);
    let original = plan(&manifest, &topology).unwrap();
    for nodes in [vec![], vec!["local".into()], vec!["peer".into()]] {
        let mut plan = original.clone();
        plan.order[0].nodes = nodes;
        assemble(&manifest, &plan, &topology, false).await;
    }
}

#[tokio::test]
async fn local_assembly_cannot_assume_unknown_realm_or_labels_match() {
    let topology = Topology(vec![node("peer", "normal", &["gpu"], 0.0)]);
    for placement in [
        constraint(None, Some("west"), None),
        constraint(Some("normal"), None, Some("gpu")),
        constraint(Some("normal"), Some("west"), Some("gpu")),
    ] {
        let manifest = manifest(placement);
        let plan = plan(&manifest, &topology).unwrap();
        assemble(&manifest, &plan, &LocalTopology, false).await;
        assemble(&manifest, &plan, &topology, false).await;
    }
}

#[tokio::test]
async fn placement_of_every_component_is_preflighted_before_configuration() {
    let mut manifest = manifest(Default::default());
    let mut last = manifest.components[0].clone();
    last.name = "z-last".into();
    last.placement.role = Some("hub".into());
    manifest.components.push(last);
    let topology = Topology(vec![node("peer", "hub", &[], 0.0)]);
    let plan = plan(&manifest, &topology).unwrap();
    assert_eq!(plan.order[0].spec.name, "worker");
    assemble(&manifest, &plan, &topology, false).await;
}

#[tokio::test]
async fn legacy_topologies_can_still_deploy_normal_and_unconstrained_components() {
    let topology = Topology(vec![node("peer", "normal", &[], 0.0)]);
    for role in [None, Some("normal")] {
        let manifest = manifest(constraint(role, None, None));
        let plan = plan(&manifest, &topology).unwrap();
        assemble(&manifest, &plan, &topology, true).await;
        assemble(&manifest, &plan, &LocalTopology, true).await;
    }
}
