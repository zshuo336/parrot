#![cfg(feature = "host")]

#[path = "support/host_inputs.rs"]
mod support;

use parrot::thread::system::ThreadActorSystem;
use parrot_app::{
    assemble::{DeployError, ParrotSpawner},
    host::{HostLookup, ThreadPropsSpawner},
    manifest::ArtifactRef,
    ApplicationLimits,
};
use std::sync::Arc;
use support::*;

#[tokio::test]
async fn instances_are_counted_before_cleanup_reservations() {
    let engine = ThreadActorSystem::shared(Default::default());
    let base = ThreadPropsSpawner::new(engine.clone());
    for props in [false, true] {
        let paths = vec![path(), path()];
        let limits = ApplicationLimits {
            max_instances: 1,
            ..Default::default()
        };
        rejected(
            request(
                &base.clone().with_application_limits(limits),
                &paths,
                None,
                props,
            )
            .await,
            "instances",
        );
        assert!(paths.iter().all(|path| invocations(path) == 0));
        reaches_factory(
            &base.clone().with_application_limits(ApplicationLimits {
                max_instances: 2,
                ..limits
            }),
            &paths,
            None,
            props,
        )
        .await;
        assert_eq!(
            invocations(&paths[1]),
            0,
            "first factory failure must stop the pool"
        );
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn path_budget_charges_all_utf8_bytes_and_string_headers() {
    let engine = ThreadActorSystem::shared(Default::default());
    let base = ThreadPropsSpawner::new(engine.clone());
    for props in [false, true] {
        let paths = vec![
            format!("{}-\u{4e2d}", path()),
            format!("{}-\u{6587}", path()),
        ];
        let exact = paths
            .iter()
            .map(|path| path.len() + std::mem::size_of::<String>())
            .sum::<usize>() as u64;
        let limits = ApplicationLimits {
            max_expanded_bytes: exact - 1,
            ..Default::default()
        };
        rejected(
            request(
                &base.clone().with_application_limits(limits),
                &paths,
                None,
                props,
            )
            .await,
            "expanded bytes",
        );
        assert!(paths.iter().all(|path| invocations(path) == 0));
        reaches_factory(
            &base.clone().with_application_limits(ApplicationLimits {
                max_expanded_bytes: exact,
                ..limits
            }),
            &paths,
            None,
            props,
        )
        .await;
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn config_depth_includes_table_array_and_leaf() {
    let engine = ThreadActorSystem::shared(Default::default());
    let base = ThreadPropsSpawner::new(engine.clone());
    let config = nested_table(2); // table -> array -> integer: depth three
    for props in [false, true] {
        let paths = vec![path()];
        let limits = ApplicationLimits {
            max_config_depth: 2,
            ..Default::default()
        };
        rejected(
            request(
                &base.clone().with_application_limits(limits),
                &paths,
                Some(&config),
                props,
            )
            .await,
            "config depth",
        );
        assert_eq!(invocations(&paths[0]), 0);
        reaches_factory(
            &base.clone().with_application_limits(ApplicationLimits {
                max_config_depth: 3,
                ..limits
            }),
            &paths,
            Some(&config),
            props,
        )
        .await;
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn config_values_share_one_budget_across_sibling_containers() {
    let engine = ThreadActorSystem::shared(Default::default());
    let base = ThreadPropsSpawner::new(engine.clone());
    let config = toml::Table::from_iter([
        (
            "a".into(),
            toml::Value::Array(vec![toml::Value::Integer(1)]),
        ),
        (
            "b".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "c".into(),
                toml::Value::Integer(2),
            )])),
        ),
    ]); // root table, two containers, two leaves: five values
    for props in [false, true] {
        let paths = vec![path()];
        let limits = ApplicationLimits {
            max_config_values: 4,
            ..Default::default()
        };
        rejected(
            request(
                &base.clone().with_application_limits(limits),
                &paths,
                Some(&config),
                props,
            )
            .await,
            "config values",
        );
        assert_eq!(invocations(&paths[0]), 0);
        reaches_factory(
            &base.clone().with_application_limits(ApplicationLimits {
                max_config_values: 5,
                ..limits
            }),
            &paths,
            Some(&config),
            props,
        )
        .await;
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn metadata_matches_fixed_json_vector_in_both_public_entries() {
    let engine = ThreadActorSystem::shared(Default::default());
    let base = ThreadPropsSpawner::new(engine.clone());
    let paths = vec!["/user/metadata".into()];
    let config = toml::Table::from_iter([("value".into(), toml::Value::String("\n\"\\".into()))]);
    let artifact = ArtifactRef::Props {
        factory: FACTORY.into(),
    };
    let vector = r#"[{"Props":{"factory":"parrot.host-input-probe"}},["/user/metadata"],{"value":"\n\"\\"}]"#;
    assert_eq!(
        serde_json::to_string(&(&artifact, &paths, Some(&config))).unwrap(),
        vector
    );
    let exact = vector.len() as u64;
    for (index, props) in [false, true].into_iter().enumerate() {
        let limits = ApplicationLimits {
            max_metadata_bytes: exact - 1,
            ..Default::default()
        };
        rejected(
            request(
                &base.clone().with_application_limits(limits),
                &paths,
                Some(&config),
                props,
            )
            .await,
            "metadata bytes",
        );
        assert_eq!(invocations(&paths[0]), index);
        let result = request(
            &base.clone().with_application_limits(ApplicationLimits {
                max_metadata_bytes: exact,
                ..limits
            }),
            &paths,
            Some(&config),
            props,
        )
        .await;
        assert!(
            matches!(result, Err(DeployError::Engine(message)) if message.contains("host input probe invoked"))
        );
        assert_eq!(invocations(&paths[0]), index + 1);
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn absent_config_is_distinct_from_an_empty_table() {
    let engine = ThreadActorSystem::shared(Default::default());
    let base = ThreadPropsSpawner::new(engine.clone()).with_application_limits(ApplicationLimits {
        max_config_values: 0,
        ..Default::default()
    });
    for props in [false, true] {
        let paths = vec![path()];
        rejected(
            request(&base, &paths, Some(&toml::Table::new()), props).await,
            "config values",
        );
        assert_eq!(invocations(&paths[0]), 0);
        reaches_factory(&base, &paths, None, props).await;
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn explicit_limits_can_raise_defaults_without_changing_existing_clones() {
    let engine = ThreadActorSystem::shared(Default::default());
    let original = ThreadPropsSpawner::new(engine.clone());
    let relaxed = original.clone().with_application_limits(ApplicationLimits {
        max_config_depth: 66,
        ..Default::default()
    });
    let strict = relaxed.clone().with_application_limits(ApplicationLimits {
        max_config_depth: 1,
        ..Default::default()
    });
    let config = nested_table(65);
    for props in [false, true] {
        let paths = vec![path()];
        rejected(
            request(&original, &paths, Some(&config), props).await,
            "config depth",
        );
        rejected(
            request(&strict, &paths, Some(&config), props).await,
            "config depth",
        );
        assert_eq!(invocations(&paths[0]), 0);
        reaches_factory(&relaxed, &paths, Some(&config), props).await;
    }
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn python_runtime_env_is_bounded_before_serialization_or_runtime_rejection() {
    let engine = ThreadActorSystem::shared(Default::default());
    let base = ThreadPropsSpawner::new(engine.clone());
    let artifact = ArtifactRef::PyModule {
        module: "test".into(),
        uri: None,
        runtime_env: Some(toml::Value::Array(vec![toml::Value::Integer(1)])),
    };
    let config = toml::Table::from_iter([("x".into(), toml::Value::Integer(1))]);
    let paths = vec![path()];
    let limits = ApplicationLimits {
        max_config_values: 3,
        max_config_depth: 2,
        ..Default::default()
    };
    rejected(
        base.clone()
            .with_application_limits(limits)
            .spawn_artifact(&artifact, &paths, Some(&config))
            .await,
        "config values",
    );
    rejected(
        base.clone()
            .with_application_limits(ApplicationLimits {
                max_config_values: 4,
                max_config_depth: 1,
                ..limits
            })
            .spawn_artifact(&artifact, &paths, Some(&config))
            .await,
        "config depth",
    );
    let result = base
        .with_application_limits(ApplicationLimits {
            max_config_values: 4,
            ..limits
        })
        .spawn_artifact(&artifact, &paths, Some(&config))
        .await;
    assert!(matches!(result, Err(DeployError::Artifact(message)) if message.contains("pymodule")));
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn remote_constructor_applies_defaults_and_custom_limits() {
    let engine = ThreadActorSystem::shared(Default::default());
    let remote = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("host-input", None),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    let base = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let config = nested_table(65);
    for props in [false, true] {
        let paths = vec![path()];
        rejected(
            request(&base, &paths, Some(&config), props).await,
            "config depth",
        );
        assert_eq!(invocations(&paths[0]), 0);
        reaches_factory(
            &base.clone().with_application_limits(ApplicationLimits {
                max_config_depth: 66,
                ..Default::default()
            }),
            &paths,
            Some(&config),
            props,
        )
        .await;
    }
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cleanup_capacity_remains_independent_from_input_policy() {
    let engine = ThreadActorSystem::shared(Default::default());
    let base = ThreadPropsSpawner::new(engine.clone());
    let paths: Vec<_> = (0..4097).map(|_| path()).collect();
    for props in [false, true] {
        let result = request(
            &base.clone().with_application_limits(ApplicationLimits {
                max_instances: 5000,
                ..Default::default()
            }),
            &paths,
            None,
            props,
        )
        .await;
        assert!(
            matches!(result, Err(DeployError::Engine(message)) if message.contains("cleanup ownership limit"))
        );
        rejected(
            request(
                &base.clone().with_application_limits(ApplicationLimits {
                    max_instances: 4096,
                    ..Default::default()
                }),
                &paths,
                None,
                props,
            )
            .await,
            "instances",
        );
    }
    assert!(paths.iter().all(|path| invocations(path) == 0));
    reaches_factory(&base, &[path()], None, false).await;
    engine.shutdown_internal().await.unwrap();
}
