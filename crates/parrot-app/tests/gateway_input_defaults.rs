#[path = "support/gateway_inputs.rs"]
mod support;

use parrot_app::{ArtifactRef, InstancePolicy};
use parrot_remote::admin_v2::AdminArtifactRef;
use support::*;

#[test]
fn payload_config_depth_is_checked_before_toml_encoding() {
    let mut spec = component();
    spec.config = Some(nested_table(65));
    budget(spec.deployment("1.0.0"), "config depth");
}

#[test]
fn payload_config_values_include_containers() {
    let mut spec = component();
    spec.config = Some(toml::Table::from_iter([(
        "value".into(),
        toml::Value::Array(vec![toml::Value::Integer(1); 131_071]),
    )]));
    budget(spec.deployment("1.0.0"), "config values");
}

#[test]
fn payload_metadata_is_checked_before_encoding() {
    let mut spec = component();
    spec.config = Some(toml::Table::from_iter([(
        "value".into(),
        toml::Value::String("x".repeat(16 * 1024 * 1024)),
    )]));
    budget(spec.deployment("1.0.0"), "metadata bytes");
}

#[test]
fn payload_instances_and_dependencies_have_independent_budgets() {
    let mut spec = component();
    spec.instances = InstancePolicy::Pool(16_385);
    budget(spec.deployment("1.0.0"), "instances");
    spec.instances = InstancePolicy::Singleton;
    spec.deps = vec!["dep".into(); 16_385];
    budget(spec.deployment("1.0.0"), "dependencies");
}

#[test]
fn payload_version_is_included_in_metadata_before_semver_errors() {
    let spec = component();
    budget(
        spec.deployment(&format!("1.0.0+{}", "x".repeat(16 * 1024 * 1024))),
        "metadata bytes",
    );
    budget(
        spec.deployment(&"!".repeat(16 * 1024 * 1024)),
        "metadata bytes",
    );
}

#[test]
fn deployment_never_silently_discards_unencodable_runtime_env() {
    let mut spec = component();
    spec.artifact = ArtifactRef::PyModule {
        module: "jobs".into(),
        uri: None,
        runtime_env: Some(toml::Value::Boolean(true)),
    };
    spec.engine = parrot_app::EngineKind::Ray;
    assert!(
        spec.deployment("1.0.0").is_err(),
        "invalid runtime_env was erased from a successful deployment"
    );
}

#[test]
fn outgoing_artifact_conversion_reports_invalid_runtime_env() {
    let artifact = ArtifactRef::PyModule {
        module: "jobs".into(),
        uri: None,
        runtime_env: Some(toml::Value::Boolean(true)),
    };
    assert!(
        AdminArtifactRef::try_from(artifact).is_err(),
        "conversion silently removed runtime_env"
    );
}

#[test]
fn incoming_artifact_conversion_reports_malformed_runtime_env() {
    let artifact = AdminArtifactRef::PyModule {
        module: "jobs".into(),
        uri: None,
        runtime_env: Some("not valid [toml".into()),
    };
    assert!(
        ArtifactRef::try_from(artifact).is_err(),
        "conversion silently removed malformed runtime_env"
    );
}

#[cfg(feature = "host")]
#[tokio::test]
async fn gateway_config_is_checked_before_runtime_preparation() {
    use parrot_app::GatewayFactory;
    let (engine, remote, factory) = host().await;
    let mut spec = component();
    spec.config = Some(nested_table(65));
    host_budget(
        factory
            .start_gateway(&spec, &["/user/gateway-input".into()], "1.0.0")
            .await,
        "config depth",
    );
    assert_eq!(factory.procs().budget().in_use(), 0);
    assert_eq!(factory.procs().child_count(), 0);
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[cfg(feature = "host")]
#[tokio::test]
async fn gateway_supplied_paths_are_counted_before_runtime_preparation() {
    use parrot_app::GatewayFactory;
    let (engine, remote, factory) = host().await;
    let paths = vec![format!("/{}", "x".repeat(16 * 1024 * 1024))];
    host_budget(
        factory.start_gateway(&component(), &paths, "1.0.0").await,
        "expanded bytes",
    );
    assert_eq!(factory.procs().budget().in_use(), 0);
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[cfg(feature = "host")]
#[tokio::test]
async fn gateway_path_mismatch_is_rejected_before_runtime_preparation() {
    use parrot_app::{DeployError, GatewayFactory};
    let (engine, remote, factory) = host().await;
    for paths in [
        vec![],
        vec!["/user/other".into()],
        vec!["/user/gateway-input".into(); 2],
    ] {
        let result = factory.start_gateway(&component(), &paths, "1.0.0").await;
        assert!(
            matches!(result, Err(DeployError::Config(message)) if message.contains("instance paths")),
            "path mismatch reached runtime preparation"
        );
    }
    assert_eq!(factory.procs().budget().in_use(), 0);
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}
