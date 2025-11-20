use parrot_app::{ArtifactRef, ComponentSpec, EngineKind, InstancePolicy};

pub fn component() -> ComponentSpec {
    ComponentSpec {
        name: "gateway-input".into(),
        engine: EngineKind::Akka,
        artifact: ArtifactRef::Jvm {
            main_class: "test.Main".into(),
            coords: None,
            uri: None,
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

pub fn nested_table(depth: usize) -> toml::Table {
    assert!(depth >= 2);
    let mut value = toml::Value::Integer(1);
    for _ in 2..depth {
        value = toml::Value::Array(vec![value]);
    }
    toml::Table::from_iter([("value".into(), value)])
}

pub fn budget<T>(result: Result<T, String>, resource: &str) {
    let error = result.err().expect("over-budget request was accepted");
    assert!(
        error.contains("budget exceeded") && error.contains(resource),
        "wrong error for {resource}: {} bytes, prefix {:?}",
        error.len(),
        error.chars().take(160).collect::<String>()
    );
}

#[cfg(feature = "host")]
pub async fn host() -> (
    std::sync::Arc<parrot::thread::system::ThreadActorSystem>,
    std::sync::Arc<parrot_remote::RemoteActorSystem>,
    parrot_app::host::ProcessGatewayFactory,
) {
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    let remote = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("gateway-input", None),
        std::sync::Arc::new(parrot_app::host::HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    let factory = parrot_app::host::ProcessGatewayFactory::new(Default::default(), remote.clone());
    (engine, remote, factory)
}

#[cfg(feature = "host")]
pub fn host_budget(
    result: Result<Vec<parrot_api::types::BoxedActorRef>, parrot_app::DeployError>,
    resource: &str,
) {
    match result {
        Err(parrot_app::DeployError::Config(error)) => budget::<()>(Err(error), resource),
        Err(error) => panic!("wrong error kind: {error}"),
        Ok(_) => panic!("over-budget gateway request was accepted"),
    }
}
