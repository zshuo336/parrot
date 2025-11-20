use parrot_api::types::BoxedActorRef;
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
    planner::{plan, CandidateNode, TopologyView},
    AppManifest, ArtifactRef, ComponentSpec, EngineKind, InstancePolicy, PlacementConstraint,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Topology;
impl TopologyView for Topology {
    fn candidates(&self, _: &PlacementConstraint) -> Vec<CandidateNode> {
        ["first", "second"]
            .into_iter()
            .map(|id| CandidateNode {
                node_id: id.into(),
                role: "normal".into(),
                labels: vec![],
                load: 0.0,
            })
            .collect()
    }
    fn self_is(&self, _: Option<&str>) -> bool {
        true
    }
}

#[derive(Default)]
struct Host(AtomicUsize);
#[async_trait::async_trait]
impl ParrotSpawner for Host {
    fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(DeployError::Engine("configuration probe invoked".into()))
    }
    async fn spawn_props(
        &self,
        _: &str,
        _: &[String],
        _: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("spawn must follow successful host configuration")
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("no actors were started")
    }
}
#[async_trait::async_trait]
impl GatewayFactory for Host {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        panic!("gateway configuration must follow successful spawner configuration")
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("no gateway may start")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("no gateway was started")
    }
}

fn spec(name: &str, instances: InstancePolicy, anti_affinity: bool) -> ComponentSpec {
    ComponentSpec {
        name: name.into(),
        engine: EngineKind::Parrot,
        artifact: ArtifactRef::Props {
            factory: "placement-probe".into(),
        },
        alt_artifact: None,
        instances,
        placement: PlacementConstraint {
            anti_affinity,
            ..Default::default()
        },
        upgrade: Default::default(),
        deps: vec![],
        config: None,
        hooks: Default::default(),
    }
}

fn manifest(components: Vec<ComponentSpec>) -> AppManifest {
    AppManifest {
        name: "local-placement".into(),
        version: "1.0.0".into(),
        components,
        wiring: vec![],
        mirrors: vec![],
        config_overlay: None,
    }
}

async fn rejected(manifest: &AppManifest, plan: &parrot_app::planner::Plan) {
    let host = Host::default();
    let deployer = LocalDeployer {
        topology: &Topology,
        parrot_spawner: &host,
        gateway_factory: &host,
    };
    let mut context = AssemblingContext::new();
    let result = context
        .assemble(plan, &deployer, manifest, &Default::default())
        .await;
    assert!(
        matches!(&result, Err(DeployError::Config(reason)) if reason.contains("anti_affinity")),
        "{result:?}"
    );
    assert_eq!(host.0.load(Ordering::SeqCst), 0);
    assert!(context.started().is_empty());
}

#[tokio::test]
async fn local_assembly_rejects_distributed_anti_affinity_before_configuring_a_host() {
    for policy in [InstancePolicy::Pool(2), InstancePolicy::Sharded(2)] {
        let manifest = manifest(vec![spec("worker", policy, true)]);
        let plan = plan(&manifest, &Topology).unwrap();
        assert_eq!(plan.order[0].nodes, ["first", "second"]);
        rejected(&manifest, &plan).await;
    }
}

#[tokio::test]
async fn caller_modified_node_lists_cannot_bypass_local_anti_affinity_admission() {
    let manifest = manifest(vec![spec("worker", InstancePolicy::Pool(2), true)]);
    let original = plan(&manifest, &Topology).unwrap();
    for nodes in [vec![], vec!["local".into()], vec!["local".into(); 2]] {
        let mut plan = original.clone();
        plan.order[0].nodes = nodes;
        rejected(&manifest, &plan).await;
    }
}

#[tokio::test]
async fn all_components_are_preflighted_before_any_host_configuration() {
    let manifest = manifest(vec![
        spec("a", InstancePolicy::Singleton, false),
        spec("z", InstancePolicy::Pool(2), true),
    ]);
    let plan = plan(&manifest, &Topology).unwrap();
    assert_eq!(plan.order[0].spec.name, "a");
    rejected(&manifest, &plan).await;
}

#[tokio::test]
async fn co_location_and_single_instance_requests_reach_host_configuration() {
    for (policy, anti_affinity) in [
        (InstancePolicy::Pool(2), false),
        (InstancePolicy::Singleton, true),
        (InstancePolicy::Pool(1), true),
        (InstancePolicy::Ephemeral, true),
    ] {
        let manifest = manifest(vec![spec("worker", policy, anti_affinity)]);
        let plan = plan(&manifest, &Topology).unwrap();
        let host = Host::default();
        let deployer = LocalDeployer {
            topology: &Topology,
            parrot_spawner: &host,
            gateway_factory: &host,
        };
        let mut context = AssemblingContext::new();
        let result = context
            .assemble(&plan, &deployer, &manifest, &Default::default())
            .await;
        assert!(
            matches!(&result, Err(DeployError::Engine(reason)) if reason == "configuration probe invoked"),
            "{result:?}"
        );
        assert_eq!(host.0.load(Ordering::SeqCst), 1);
        assert!(context.started().is_empty());
    }
}
