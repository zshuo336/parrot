#[path = "support/gateway_inputs.rs"]
mod support;

use parrot_app::{ApplicationLimits, ArtifactRef, EngineKind, InstancePolicy};
use parrot_remote::admin_v2::AdminArtifactRef;
use support::*;

fn python(value: Option<toml::Value>) -> ArtifactRef {
    ArtifactRef::PyModule {
        module: "jobs".into(),
        runtime_env: value,
        uri: None,
    }
}

#[test]
fn explicit_structure_limits_count_root_containers_and_alternate_environment() {
    let mut spec = component();
    spec.config = Some(nested_table(4));
    let limits = ApplicationLimits {
        max_config_depth: 4,
        max_config_values: 4,
        ..Default::default()
    };
    spec.deployment_with_limits("1.0.0", &limits).unwrap();
    budget(
        spec.deployment_with_limits(
            "1.0.0",
            &ApplicationLimits {
                max_config_depth: 3,
                ..limits
            },
        ),
        "config depth",
    );
    spec.alt_artifact = Some(python(Some(toml::Value::Table(toml::Table::new()))));
    budget(
        spec.deployment_with_limits("1.0.0", &limits),
        "config values",
    );
    spec.deployment_with_limits(
        "1.0.0",
        &ApplicationLimits {
            max_config_values: 5,
            ..limits
        },
    )
    .unwrap();
}

#[test]
fn count_and_dependency_boundaries_do_not_replace_each_other() {
    let mut spec = component();
    let limits = ApplicationLimits {
        max_instances: 2,
        max_dependencies: 1,
        ..Default::default()
    };
    spec.instances = InstancePolicy::Pool(2);
    spec.deps = vec!["a".into()];
    spec.deployment_with_limits("1.0.0", &limits).unwrap();
    spec.deps.push("b".into());
    budget(
        spec.deployment_with_limits("1.0.0", &limits),
        "dependencies",
    );
    spec.deps.clear();
    spec.instances = InstancePolicy::Pool(3);
    budget(spec.deployment_with_limits("1.0.0", &limits), "instances");
    spec.instances = InstancePolicy::Pool(0);
    assert!(spec
        .deployment_with_limits("1.0.0", &limits)
        .unwrap_err()
        .contains("1..=u32::MAX"));
}

#[test]
fn deployment_checks_name_engine_and_version_before_encoding() {
    let mut spec = component();
    spec.name = "../invalid".into();
    assert!(spec
        .deployment("1.0.0")
        .unwrap_err()
        .contains("component name"));
    spec = component();
    spec.engine = EngineKind::Parrot;
    assert!(spec
        .deployment("1.0.0")
        .unwrap_err()
        .contains("incompatible"));
    assert!(component()
        .deployment("not-semver")
        .unwrap_err()
        .contains("version"));
    for value in [toml::Value::Integer(1), toml::Value::Array(vec![])] {
        assert!(python(Some(value))
            .into_admin()
            .unwrap_err()
            .contains("TOML table"));
    }
}

#[test]
fn version_has_its_own_share_of_the_model_budget() {
    let spec = component();
    let limits = ApplicationLimits {
        max_metadata_bytes: 512,
        ..Default::default()
    };
    spec.deployment_with_limits("1.0.0", &limits).unwrap();
    budget(
        spec.deployment_with_limits(&format!("1.0.0+{}", "a".repeat(512)), &limits),
        "metadata bytes",
    );
    budget(
        spec.deployment_with_limits(&"!".repeat(512), &limits),
        "metadata bytes",
    );
}

#[test]
fn artifact_metadata_matches_an_independent_json_vector() {
    let artifact = ArtifactRef::Props {
        factory: "x".into(),
    };
    let vector = r#"{"Props":{"factory":"x"}}"#;
    let limits = ApplicationLimits {
        max_metadata_bytes: vector.len() as u64,
        ..Default::default()
    };
    let wire = artifact.to_admin_with_limits(&limits).unwrap();
    assert_eq!(
        wire,
        AdminArtifactRef::Props {
            factory: "x".into()
        }
    );
    assert_eq!(
        ArtifactRef::from_admin_with_limits(wire.clone(), &limits).unwrap(),
        artifact
    );
    let small = ApplicationLimits {
        max_metadata_bytes: limits.max_metadata_bytes - 1,
        ..limits
    };
    budget(artifact.to_admin_with_limits(&small), "metadata bytes");
    budget(
        ArtifactRef::from_admin_with_limits(wire, &small),
        "metadata bytes",
    );
}

#[test]
fn runtime_environment_none_empty_table_and_complex_values_roundtrip() {
    let table: toml::Table = toml::from_str("label = '中文'\nescaped = \"line\\n\\t\\\"\\\\\"\nnumber = -2\nfloat = 1.25\nflag = true\ntime = 2026-11-13T12:00:00Z\nempty = []\n[\"key.with.dot\"]\nitems = [{a = 1}, {a = 2}]\n").unwrap();
    for value in [
        None,
        Some(toml::Value::Table(toml::Table::new())),
        Some(toml::Value::Table(table)),
    ] {
        let original = python(value);
        let wire = original.clone().into_admin().unwrap();
        let back = ArtifactRef::try_from(wire).unwrap();
        assert_eq!(back, original);
    }
}

#[test]
fn runtime_environment_text_boundary_is_checked_in_both_directions() {
    let artifact = python(Some(toml::Value::Table(toml::Table::from_iter([(
        "x".into(),
        toml::Value::Integer(1),
    )]))));
    let limits = ApplicationLimits {
        max_source_bytes: 6,
        ..Default::default()
    };
    let wire = artifact.to_admin_with_limits(&limits).unwrap();
    match &wire {
        AdminArtifactRef::PyModule {
            runtime_env: Some(text),
            ..
        } => assert_eq!(text, "x = 1\n"),
        _ => panic!("runtime_env missing"),
    }
    assert_eq!(
        ArtifactRef::from_admin_with_limits(wire.clone(), &limits).unwrap(),
        artifact
    );
    let small = ApplicationLimits {
        max_source_bytes: 5,
        ..limits
    };
    budget(artifact.to_admin_with_limits(&small), "source bytes");
    budget(
        ArtifactRef::from_admin_with_limits(wire, &small),
        "source bytes",
    );
}

#[test]
fn incoming_decoded_structure_is_counted_after_bounded_parsing() {
    let wire = AdminArtifactRef::PyModule {
        module: "jobs".into(),
        uri: None,
        runtime_env: Some("x = [[1]]\n".into()),
    };
    let limits = ApplicationLimits {
        max_config_depth: 4,
        max_config_values: 4,
        ..Default::default()
    };
    ArtifactRef::from_admin_with_limits(wire.clone(), &limits).unwrap();
    budget(
        ArtifactRef::from_admin_with_limits(
            wire.clone(),
            &ApplicationLimits {
                max_config_depth: 3,
                ..limits
            },
        ),
        "config depth",
    );
    budget(
        ArtifactRef::from_admin_with_limits(
            wire,
            &ApplicationLimits {
                max_config_values: 3,
                ..limits
            },
        ),
        "config values",
    );
}

#[test]
fn malformed_wire_text_is_rejected_before_parse_when_source_is_too_large() {
    let wire = AdminArtifactRef::PyModule {
        module: "jobs".into(),
        uri: None,
        runtime_env: Some("!".repeat(100)),
    };
    let limits = ApplicationLimits {
        max_source_bytes: 99,
        ..Default::default()
    };
    budget(
        ArtifactRef::from_admin_with_limits(wire, &limits),
        "source bytes",
    );
    let malformed = AdminArtifactRef::PyModule {
        module: "jobs".into(),
        uri: None,
        runtime_env: Some(format!("x = {}!", "a".repeat(100_000))),
    };
    let error = ArtifactRef::try_from(malformed).unwrap_err();
    assert!(error.starts_with("invalid runtime_env TOML:"));
    assert!(
        error.len() < 1024,
        "parse diagnostics must not reproduce input text"
    );
}

#[test]
fn encoding_and_wire_metadata_have_independent_limits() {
    let mut spec = component();
    spec.config = Some(toml::Table::from_iter([(
        "x".into(),
        toml::Value::String("a".repeat(200)),
    )]));
    // The input model fits 600 bytes, while config's wire Vec<u8> costs JSON
    // integers and separators. This must also be admitted before remote work.
    assert!(serde_json::to_vec(&(&spec, "1.0.0")).unwrap().len() <= 600);
    budget(
        spec.deployment_with_limits(
            "1.0.0",
            &ApplicationLimits {
                max_metadata_bytes: 600,
                ..Default::default()
            },
        ),
        "metadata bytes",
    );
    spec.deployment_with_limits(
        "1.0.0",
        &ApplicationLimits {
            max_metadata_bytes: 2048,
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn explicit_looser_policy_does_not_fall_back_to_defaults() {
    let mut spec = component();
    spec.config = Some(nested_table(65));
    let limits = ApplicationLimits {
        max_config_depth: 65,
        ..Default::default()
    };
    spec.deployment_with_limits("1.0.0", &limits).unwrap();
    budget(spec.deployment("1.0.0"), "config depth");
}

#[test]
fn supervisor_uses_its_own_policy_and_rechecks_public_payload_inputs() {
    use parrot_app::{
        orchestrator::{AppSupervisor, FakeClock, MemStateStore},
        AppManifest,
    };
    let initial = AppManifest {
        name: "gateway-policy".into(),
        version: "1.0.0".into(),
        components: vec![component()],
        wiring: vec![],
        mirrors: vec![],
        config_overlay: None,
    };
    let limits = ApplicationLimits {
        max_config_depth: 65,
        max_instances: 2,
        ..Default::default()
    };
    let supervisor = AppSupervisor::new_with_limits(
        initial,
        std::sync::Arc::new(MemStateStore::new()),
        std::sync::Arc::new(FakeClock::new(0)),
        limits,
    )
    .unwrap();
    let mut spec = component();
    spec.config = Some(nested_table(65));
    supervisor.deploy_payload(&spec).unwrap();
    spec.instances = InstancePolicy::Pool(3);
    budget(supervisor.deploy_payload(&spec), "instances");
}

#[test]
fn owned_deep_value_rejection_drops_iteratively_in_a_subprocess() {
    const FLAG: &str = "PARROT_GATEWAY_DEEP_DROP_CHILD";
    if std::env::var_os(FLAG).is_some() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let mut value = toml::Value::Integer(1);
                for _ in 0..20_000 {
                    value = toml::Value::Array(vec![value]);
                }
                budget(
                    AdminArtifactRef::try_from(python(Some(value))),
                    "config depth",
                );
            })
            .unwrap()
            .join()
            .unwrap();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "owned_deep_value_rejection_drops_iteratively_in_a_subprocess",
            "--nocapture",
        ])
        .env(FLAG, "1")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "deep rejection child: {status}");
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("deep rejection child exceeded deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(feature = "host")]
#[tokio::test]
async fn gateway_factory_clone_retains_its_own_policy() {
    use parrot_app::{DeployError, GatewayFactory};
    let (engine, remote, original) = host().await;
    let strict = original.clone().with_application_limits(ApplicationLimits {
        max_config_depth: 2,
        ..Default::default()
    });
    let relaxed = original.clone().with_application_limits(ApplicationLimits {
        max_config_depth: 65,
        ..Default::default()
    });
    let mut spec = component();
    spec.config = Some(nested_table(65));
    let paths = ["/user/gateway-input".into()];
    host_budget(
        strict.start_gateway(&spec, &paths, "1.0.0").await,
        "config depth",
    );
    host_budget(
        original.start_gateway(&spec, &paths, "1.0.0").await,
        "config depth",
    );
    assert!(
        matches!(relaxed.start_gateway(&spec, &paths, "1.0.0").await, Err(DeployError::Engine(e)) if e == "app not listening")
    );
    assert_eq!(original.procs().child_count(), 0);
    assert_eq!(original.procs().budget().in_use(), 0);
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[cfg(feature = "host")]
#[tokio::test]
async fn every_gateway_constructor_applies_limits_before_preparation() {
    use parrot_app::{host::ProcessGatewayFactory, GatewayFactory};
    let (engine, remote, original) = host().await;
    let routes = std::sync::Arc::default();
    let constructors = [
        original,
        ProcessGatewayFactory::new_with_routes(Default::default(), remote.clone(), routes),
        ProcessGatewayFactory::new_with_routes_and_budget(
            Default::default(),
            remote.clone(),
            Default::default(),
            Default::default(),
        ),
    ];
    let mut spec = component();
    spec.config = Some(nested_table(65));
    for factory in constructors {
        host_budget(
            factory
                .start_gateway(&spec, &["/user/gateway-input".into()], "1.0.0")
                .await,
            "config depth",
        );
    }
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[cfg(feature = "host")]
#[tokio::test]
async fn gateway_path_counts_bytes_headers_and_canonical_order_are_checked() {
    use parrot_app::{DeployError, GatewayFactory};
    let (engine, remote, original) = host().await;
    let mut spec = component();
    spec.instances = InstancePolicy::Pool(2);
    let paths = [
        "/user/gateway-input-0".into(),
        "/user/gateway-input-1".into(),
    ];
    let bytes = 2 * (21 + std::mem::size_of::<String>()) as u64;
    let factory = original.with_application_limits(ApplicationLimits {
        max_instances: 2,
        max_expanded_bytes: bytes,
        ..Default::default()
    });
    assert!(
        matches!(factory.start_gateway(&spec, &paths, "1.0.0").await, Err(DeployError::Engine(e)) if e == "app not listening")
    );
    host_budget(
        factory
            .clone()
            .with_application_limits(ApplicationLimits {
                max_expanded_bytes: bytes - 1,
                ..Default::default()
            })
            .start_gateway(&spec, &paths, "1.0.0")
            .await,
        "expanded bytes",
    );
    host_budget(
        factory
            .clone()
            .with_application_limits(ApplicationLimits {
                max_instances: 1,
                ..Default::default()
            })
            .start_gateway(&spec, &paths, "1.0.0")
            .await,
        "instances",
    );
    let reversed = [paths[1].clone(), paths[0].clone()];
    assert!(
        matches!(factory.start_gateway(&spec, &reversed, "1.0.0").await, Err(DeployError::Config(e)) if e.contains("instance paths"))
    );
    spec.instances = InstancePolicy::Sharded(2);
    assert!(
        matches!(factory.start_gateway(&spec, &paths, "1.0.0").await, Err(DeployError::Engine(e)) if e == "app not listening")
    );
    assert_eq!(factory.procs().child_count(), 0);
    assert_eq!(factory.procs().budget().in_use(), 0);
    spec.name = "网关".into();
    let unicode_paths = ["/user/网关-0".into(), "/user/网关-1".into()];
    let unicode_bytes = 2 * (14 + std::mem::size_of::<String>()) as u64;
    let unicode_factory = factory.with_application_limits(ApplicationLimits {
        max_expanded_bytes: unicode_bytes,
        ..Default::default()
    });
    assert!(
        matches!(unicode_factory.start_gateway(&spec, &unicode_paths, "1.0.0").await,
        Err(DeployError::Engine(e)) if e == "app not listening")
    );
    host_budget(
        unicode_factory
            .with_application_limits(ApplicationLimits {
                max_expanded_bytes: unicode_bytes - 1,
                ..Default::default()
            })
            .start_gateway(&spec, &unicode_paths, "1.0.0")
            .await,
        "expanded bytes",
    );
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}
