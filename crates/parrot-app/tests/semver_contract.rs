//! SemVer 2.0.0 grammar and the public application consumers of that grammar.
use parrot_app::{
    manifest::{is_valid_semver, ManifestError},
    orchestrator::{AppSupervisor, DesiredStateStore, FakeClock, MemStateStore},
    planner::{plan, LocalTopology, PlanError},
    AppManifest, ApplicationLimits, ArtifactRef, ComponentSpec, EngineKind,
};
use std::sync::Arc;

const INVALID_IDENTIFIERS: &[&str] = &[
    "1.0.0-alpha..1",
    "1.0.0-.alpha",
    "1.0.0-alpha.",
    "1.0.0-01",
    "1.0.0-alpha.00+build.01",
    "1.0.0+.",
    "1.0.0+.build",
    "1.0.0+build.",
    "1.0.0+build..1",
];

fn manifest(version: &str) -> AppManifest {
    AppManifest {
        name: "semver-contract".into(),
        version: version.into(),
        components: vec![ComponentSpec {
            name: "worker".into(),
            engine: EngineKind::Akka,
            artifact: ArtifactRef::Jvm {
                main_class: "test.Main".into(),
                coords: None,
                uri: None,
            },
            alt_artifact: None,
            instances: Default::default(),
            placement: Default::default(),
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

#[test]
fn semver_rejects_empty_prerelease_identifiers() {
    for suffix in [".", ".alpha", "alpha.", "alpha..1", "alpha...beta"] {
        for build in ["", "+build.01"] {
            let version = format!("1.0.0-{suffix}{build}");
            assert!(!is_valid_semver(&version), "accepted {version:?}");
        }
    }
}

#[test]
fn semver_rejects_empty_build_identifiers() {
    for suffix in [".", ".build", "build.", "build..1", "build...meta"] {
        for core in ["1.0.0", "1.0.0-alpha.0"] {
            let version = format!("{core}+{suffix}");
            assert!(!is_valid_semver(&version), "accepted {version:?}");
        }
    }
}

#[test]
fn semver_rejects_numeric_prerelease_leading_zeroes() {
    for identifier in ["00", "01", "0001", "0123456789012345678901234567890"] {
        for prefix in ["", "alpha."] {
            let version = format!("1.0.0-{prefix}{identifier}+build.01");
            assert!(!is_valid_semver(&version), "accepted {version:?}");
        }
    }
}

#[test]
fn semver_accepts_build_zeroes_and_non_numeric_prerelease() {
    for version in [
        "0.0.0-0",
        "1.0.0-alpha.0",
        "1.0.0-01a.00z.-01.0-1",
        "1.0.0--",
        "1.0.0+00.01.0001",
        "1.0.0-alpha.0+00.01.a-b.-",
        "1.0.0+build-with-hyphens",
    ] {
        assert!(is_valid_semver(version), "rejected {version:?}");
    }
}

#[test]
fn semver_grammar_does_not_impose_a_machine_integer_limit() {
    let number = "9".repeat(4096);
    for version in [
        format!("{number}.0.0"),
        format!("0.{number}.0"),
        format!("0.0.{number}"),
        format!("1.0.0-{number}"),
        format!("1.0.0+0{number}"),
    ] {
        assert!(is_valid_semver(&version));
        assert!(parrot_app::validate(&manifest(&version)).is_ok());
    }
}

#[test]
fn semver_rejects_invalid_core_delimiters_and_non_ascii() {
    for version in [
        "",
        "1",
        "1.0",
        "1.0.0.0",
        "01.0.0",
        "1.01.0",
        "1.0.01",
        "v1.0.0",
        "1.0.0-",
        "1.0.0+",
        "1.0.0+a+b",
        "1.0.0-a_b",
        "1.0.0+a_b",
        "1.0.0-ä",
        "1.0.0+α",
        "１.0.0",
        " 1.0.0",
        "1.0.0\n",
        "1.0.0-a\0b",
        "1.0.0+a/b",
    ] {
        assert!(!is_valid_semver(version), "accepted {version:?}");
    }
}

#[test]
fn manifest_semantics_reject_bad_versions_after_loading() {
    for version in INVALID_IDENTIFIERS {
        let source = manifest(version).to_toml().unwrap();
        // Loading checks source and model budgets; semantic validation is separate.
        let loaded = AppManifest::from_toml(&source).unwrap();
        let errors = parrot_app::validate(&loaded).expect_err("invalid version was accepted");
        assert_eq!(errors, vec![ManifestError::BadSemver((*version).into())]);
        assert_eq!(loaded.version, *version);
    }
}

#[test]
fn planner_rejects_bad_versions_before_consulting_topology() {
    use parrot_app::planner::{CandidateNode, TopologyView};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Topology(AtomicUsize);
    impl TopologyView for Topology {
        fn candidates(&self, placement: &parrot_app::PlacementConstraint) -> Vec<CandidateNode> {
            self.0.fetch_add(1, Ordering::Relaxed);
            LocalTopology.candidates(placement)
        }
        fn self_is(&self, role: Option<&str>) -> bool {
            self.0.fetch_add(1, Ordering::Relaxed);
            LocalTopology.self_is(role)
        }
    }
    for version in INVALID_IDENTIFIERS {
        let topology = Topology(AtomicUsize::new(0));
        assert!(matches!(
            plan(&manifest(version), &topology),
            Err(PlanError::Manifest(errors)) if errors == vec![ManifestError::BadSemver((*version).into())]
        ));
        assert_eq!(topology.0.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn deployment_rejects_bad_versions_on_both_policy_entrypoints() {
    let spec = &manifest("1.0.0").components[0];
    for version in INVALID_IDENTIFIERS {
        for result in [
            spec.deployment(version),
            spec.deployment_with_limits(version, &ApplicationLimits::default()),
        ] {
            assert_eq!(
                result.unwrap_err(),
                "invalid deployment version: expected semver"
            );
        }
    }
}

#[test]
fn supervisor_rejects_invalid_bootstrap_without_persisting() {
    for version in INVALID_IDENTIFIERS {
        let store = Arc::new(MemStateStore::new());
        let error = AppSupervisor::new(
            manifest(version),
            store.clone(),
            Arc::new(FakeClock::new(0)),
        )
        .err()
        .expect("invalid bootstrap was accepted");
        assert!(error.contains("BadSemver"), "{error}");
        assert!(store.latest().unwrap().is_none());
    }
}

#[test]
fn supervisor_rejects_invalid_submission_without_advancing_state() {
    let store = Arc::new(MemStateStore::new());
    let mut supervisor = AppSupervisor::new(
        manifest("1.0.0"),
        store.clone(),
        Arc::new(FakeClock::new(0)),
    )
    .unwrap();
    for version in INVALID_IDENTIFIERS {
        let error = supervisor
            .submit(manifest(version))
            .expect_err("invalid submit was accepted");
        assert!(error.contains("BadSemver"), "{error}");
        assert_eq!(supervisor.desired(), &manifest("1.0.0"));
        let stored = store.latest().unwrap().unwrap();
        assert_eq!(stored.revision, 1);
        assert_eq!(stored.manifest, manifest("1.0.0"));
    }
    supervisor.submit(manifest("1.0.1-0+build.01")).unwrap();
    let stored = store.latest().unwrap().unwrap();
    assert_eq!(stored.revision, 2);
    assert_eq!(stored.manifest.version, "1.0.1-0+build.01");
}

#[test]
fn stored_invalid_versions_are_rejected_on_restore() {
    use parrot_app::orchestrator::DesiredStateRecord;
    struct Stored(DesiredStateRecord);
    impl DesiredStateStore for Stored {
        fn latest(&self) -> Result<Option<DesiredStateRecord>, String> {
            Ok(Some(self.0.clone()))
        }
        fn commit(&self, _: Option<u64>, _: &DesiredStateRecord) -> Result<(), String> {
            panic!("restore must not rewrite stored state");
        }
    }
    for version in INVALID_IDENTIFIERS {
        let store = Arc::new(Stored(DesiredStateRecord {
            revision: 1,
            manifest: manifest(version),
            pending: Default::default(),
            mirrors: Default::default(),
        }));
        let error = AppSupervisor::new(manifest("1.0.0"), store, Arc::new(FakeClock::new(0)))
            .err()
            .expect("invalid stored version was accepted");
        assert!(error.contains("BadSemver"), "{error}");
    }
}

#[test]
fn valid_versions_roundtrip_and_are_used_verbatim() {
    for version in ["1.0.0-alpha.0+build.01", "1.0.0-01a", "1.0.0--+00", "0.0.0"] {
        let m = manifest(version);
        assert!(parrot_app::validate(&m).is_ok());
        assert_eq!(AppManifest::from_toml(&m.to_toml().unwrap()).unwrap(), m);
        assert_eq!(plan(&m, &LocalTopology).unwrap().order.len(), 1);
        assert_eq!(
            m.components[0].deployment(version).unwrap().version,
            version
        );
        let store = Arc::new(MemStateStore::new());
        let supervisor =
            AppSupervisor::new(m.clone(), store.clone(), Arc::new(FakeClock::new(0))).unwrap();
        assert_eq!(supervisor.desired(), &m);
        assert_eq!(store.latest().unwrap().unwrap().manifest, m);
    }
}

#[cfg(feature = "host")]
#[tokio::test]
async fn gateway_rejects_bad_versions_before_runtime_preparation() {
    use parrot_app::{
        host::{HostLookup, ProcessGatewayFactory},
        DeployError, GatewayFactory,
    };
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    let remote = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("semver-contract", None),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    let factory = ProcessGatewayFactory::new(Default::default(), remote.clone());
    let mut errors = Vec::new();
    for version in INVALID_IDENTIFIERS {
        errors.push(
            factory
                .start_gateway(
                    &manifest(version).components[0],
                    &["/user/worker".into()],
                    version,
                )
                .await,
        );
    }
    let children = factory.procs().child_count();
    let budget = factory.procs().budget().in_use();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    for error in errors {
        assert!(
            matches!(error, Err(DeployError::Config(message)) if message == "invalid deployment version: expected semver")
        );
    }
    assert_eq!(children, 0);
    assert_eq!(budget, 0);
}
