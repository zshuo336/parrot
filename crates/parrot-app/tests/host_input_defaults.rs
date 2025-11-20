#![cfg(feature = "host")]

#[path = "support/host_inputs.rs"]
mod support;

use parrot::thread::system::ThreadActorSystem;
use parrot_app::{assemble::ParrotSpawner, host::ThreadPropsSpawner, manifest::ArtifactRef};
use support::*;

async fn reject_then_progress(config: &toml::Table, resource: &str) {
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    for props in [false, true] {
        let path = path();
        rejected(
            request(&spawner, std::slice::from_ref(&path), Some(config), props).await,
            resource,
        );
        assert_eq!(
            invocations(&path),
            0,
            "rejected request executed application code"
        );
        reaches_factory(&spawner, &[path], None, props).await;
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn direct_config_depth_is_checked_before_factory_execution() {
    reject_then_progress(&nested_table(65), "config depth").await;
}

#[tokio::test]
async fn direct_config_values_include_containers_and_leaves() {
    let config = toml::Table::from_iter([(
        "value".into(),
        toml::Value::Array(vec![toml::Value::Integer(1); 131_071]),
    )]);
    reject_then_progress(&config, "config values").await;
}

#[tokio::test]
async fn direct_config_metadata_is_checked_before_factory_execution() {
    let config = toml::Table::from_iter([(
        "value".into(),
        toml::Value::String("x".repeat(16 * 1024 * 1024)),
    )]);
    reject_then_progress(&config, "metadata bytes").await;
}

#[tokio::test]
async fn direct_path_bytes_are_checked_before_factory_execution() {
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let long = format!("/{}", "x".repeat(16 * 1024 * 1024));
    for props in [false, true] {
        rejected(
            request(&spawner, std::slice::from_ref(&long), None, props).await,
            "expanded bytes",
        );
    }
    assert_eq!(invocations(&long), 0);
    reaches_factory(&spawner, &[path()], None, false).await;
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn direct_artifact_metadata_is_bounded_before_factory_lookup() {
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let artifact = ArtifactRef::Props {
        factory: "x".repeat(16 * 1024 * 1024),
    };
    rejected(
        spawner.spawn_artifact(&artifact, &[path()], None).await,
        "metadata bytes",
    );
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn oversized_request_is_rejected_before_artifact_uri_validation() {
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let artifact = ArtifactRef::Wasm {
        digest: "invalid".into(),
        uri: "invalid".into(),
    };
    rejected(
        spawner
            .spawn_artifact(&artifact, &[path()], Some(&nested_table(65)))
            .await,
        "config depth",
    );
    engine.shutdown_internal().await.unwrap();
}
