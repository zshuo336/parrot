use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    directory: PathBuf,
    manifest: AppManifest,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "websearch-host-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        // Preparation validates presence only; these fixtures deliberately do
        // not require a built gateway or a developer's Maven cache.
        for name in [
            "frontier.erl",
            "runtime/tools/parrot-obs/gateway_worker.py",
            "runtime/tools/parrot-obs/ray_gateway.py",
            "runtime/interop/python/parrot_protocol/__init__.py",
            "runtime/interop/python/parrot_protocol/ray_gw.py",
            "runtime/interop/jvm/target/parrot-protocol-jvm-0.1.0.jar",
            "runtime/interop/jvm/target/dependency.jar",
            "runtime/interop/erlang/parrot_gw.erl",
            "runtime/interop/erlang/parrot_component_runtime.erl",
            "runtime/interop/erlang/parrot_beam_snapshot.erl",
            "runtime/interop/erlang/parrot_trace.erl",
            "runtime/interop/erlang/parrot_beam_guest.erl",
            "runtime/interop/erlang/parrot_beam_host.erl",
        ] {
            let path = directory.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "fixture").unwrap();
        }
        std::fs::write(
            directory.join("runtime/interop/jvm/target/cp.txt"),
            "dependency.jar",
        )
        .unwrap();
        let mut manifest: AppManifest =
            toml::from_str(include_str!("../websearch.app.toml")).unwrap();
        for component in &mut manifest.components {
            match &mut component.artifact {
                ArtifactRef::Beam { uri, .. }
                | ArtifactRef::PyModule { uri, .. }
                | ArtifactRef::Jvm { uri, .. } => {
                    // Preflight needs local paths, not a functioning gateway or compiled artifact.
                    *uri = Some(directory.to_string_lossy().into_owned());
                }
                _ => {}
            }
        }
        Self {
            directory,
            manifest,
        }
    }
    fn settings(&mut self, value: serde_json::Value) {
        self.manifest
            .components
            .iter_mut()
            .find(|c| c.name == "crawler")
            .unwrap()
            .config = Some(
            [("settings".into(), toml::Value::String(value.to_string()))]
                .into_iter()
                .collect(),
        );
    }
    fn prepare(&self, args: &[&str]) -> Result<Prepared, Error> {
        let path = self.directory.join("application.toml");
        std::fs::write(&path, toml::to_string(&self.manifest).unwrap()).unwrap();
        let mut options = cli::parse(args.iter().map(|s| s.to_string()))?.unwrap();
        options.runtime_root = Some(self.directory.join("runtime"));
        prepare(&path, options)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn source_only_beam_compiles_but_existing_bundle_preserves_sibling_modules() {
    let fixture = Fixture::new();
    let source = fixture.prepare(&[]).unwrap();
    assert_eq!(
        source.beam_sources.get("frontier"),
        Some(&fixture.directory.join("frontier.erl"))
    );
    std::fs::write(fixture.directory.join("frontier.beam"), "compiled").unwrap();
    assert!(fixture.prepare(&[]).unwrap().beam_sources.is_empty());
    std::fs::remove_file(fixture.directory.join("frontier.beam")).unwrap();
    std::fs::remove_file(fixture.directory.join("frontier.erl")).unwrap();
    assert!(fixture
        .prepare(&[])
        .err()
        .unwrap()
        .to_string()
        .contains("source or compiled artifact"));
    assert!(fixture.prepare(&["--serve-only"]).is_ok());
}

#[test]
fn manifest_defaults_and_explicit_zero_depth_port_overrides_are_distinct() {
    let mut fixture = Fixture::new();
    fixture.settings(
        serde_json::json!({"pages":7,"sites":3,"maxdepth":4,"port":9090,"data":"owned",
        "index_data":"index","seeds":["https://example.test/page"]}),
    );
    let original = fixture.prepare(&[]).unwrap();
    assert_eq!(original.options.pages, 7);
    assert_eq!(original.options.sites_target, 3);
    assert_eq!(original.options.web_port, 9090);
    assert_eq!(original.options.data_dir, fixture.directory.join("owned"));
    assert_eq!(
        original.options.index_data,
        Some(
            fixture
                .directory
                .join("index")
                .to_string_lossy()
                .into_owned()
        )
    );
    let overridden = fixture
        .prepare(&[
            "--pages",
            "2",
            "--maxdepth",
            "0",
            "--port",
            "0",
            "--crawl-origin",
            "http://127.0.0.1:1234",
            "http://127.0.0.1:1234/root",
        ])
        .unwrap();
    assert_eq!(overridden.options.pages, 2);
    assert_eq!(overridden.options.max_depth, 0);
    assert_eq!(overridden.options.web_port, 0);
    assert_eq!(overridden.options.seeds, ["http://127.0.0.1:1234/root"]);
    assert_eq!(
        overridden.options.crawl_origins[0].as_str(),
        "http://127.0.0.1:1234/"
    );
    assert!(!fixture.directory.join("owned").exists());
    assert!(!fixture.directory.join("index").exists());
}

#[test]
fn explicit_origin_replaces_manifest_seeds_and_serve_prunes_crawl_dependencies() {
    let mut fixture = Fixture::new();
    fixture.settings(serde_json::json!({"seeds":["https://example.test/page"]}));
    let crawl = fixture
        .prepare(&["--crawl-origin", "http://127.0.0.1:1234"])
        .unwrap();
    assert_eq!(crawl.options.seeds, ["http://127.0.0.1:1234/"]);
    let serve = fixture.prepare(&["--serve-only"]).unwrap();
    assert!(serve.options.keep_data && serve.options.seeds.is_empty());
    assert_eq!(
        serve
            .manifest
            .components
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["search", "crawler"]
    );
    assert_eq!(
        serve.manifest.component("search").unwrap().deps,
        Vec::<String>::new()
    );
    assert_eq!(
        serve.manifest.component("crawler").unwrap().deps,
        ["search"]
    );
    assert_eq!(serve.manifest.wiring.len(), 1);
}

#[test]
fn invalid_business_settings_cannot_be_silently_defaulted_or_normalized() {
    for value in [
        serde_json::json!({"data":""}),
        serde_json::json!({"index_data":""}),
        serde_json::json!({"pages":0}),
        serde_json::json!({"unknown":true}),
        serde_json::json!({"sites":0}),
        serde_json::json!({"maxdepth":65536}),
        serde_json::json!({"seeds":["file:///tmp/x"]}),
    ] {
        let mut fixture = Fixture::new();
        fixture.settings(value.clone());
        assert!(fixture.prepare(&[]).is_err(), "accepted {value}");
    }
}

#[test]
fn conflicting_index_selection_requires_explicit_override() {
    let mut fixture = Fixture::new();
    fixture.settings(serde_json::json!({"index_data":"a"}));
    fixture
        .manifest
        .components
        .iter_mut()
        .find(|c| c.name == "search")
        .unwrap()
        .config = Some(
        [("data_dir".into(), toml::Value::String("b".into()))]
            .into_iter()
            .collect(),
    );
    assert!(fixture.prepare(&[]).is_err());
    let chosen = fixture
        .prepare(&["--index-data", "/explicit/index"])
        .unwrap();
    assert_eq!(
        chosen.options.index_data.as_deref(),
        Some("/explicit/index")
    );
}

#[test]
fn wrong_wire_missing_wire_and_executable_dylib_are_rejected() {
    let mut fixture = Fixture::new();
    fixture.manifest.wiring[2].to = "tokenizer:/user/tokenizer".into();
    assert!(fixture.prepare(&[]).is_err());
    fixture.manifest.wiring.pop();
    assert!(fixture.prepare(&[]).is_err());
    let mut fixture = Fixture::new();
    fixture
        .manifest
        .components
        .iter_mut()
        .find(|c| c.name == "crawler")
        .unwrap()
        .artifact = ArtifactRef::Dylib {
        uri: "target/debug/websearch".into(),
        abi: 1,
        digest: "sha256:".to_owned() + &"0".repeat(64),
    };
    assert!(fixture
        .prepare(&[])
        .err()
        .unwrap()
        .to_string()
        .contains("executable is not a Dylib"));
}

#[test]
fn framework_overlay_and_direct_mode_conflicts_fail_preflight() {
    let mut fixture = Fixture::new();
    assert!(fixture.prepare(&["jvm=127.0.0.1:1234"]).is_err());
    assert!(fixture.prepare(&["--node-root", "/tmp/artifacts"]).is_err());
    fixture.manifest.config_overlay = Some(
        [("unknown_field".into(), toml::Value::Integer(1))]
            .into_iter()
            .collect(),
    );
    assert!(fixture.prepare(&[]).is_err());
    assert!(!fixture.directory.join("data").exists());
}

#[test]
fn serve_only_does_not_require_unused_crawl_artifacts() {
    let mut fixture = Fixture::new();
    if let ArtifactRef::Beam { uri, .. } = &mut fixture.manifest.components[0].artifact {
        *uri = Some("absent-erlang".into());
    }
    if let ArtifactRef::PyModule { uri, .. } = &mut fixture.manifest.components[1].artifact {
        *uri = Some("absent-python".into());
    }
    assert!(fixture.prepare(&[]).is_err());
    assert!(fixture.prepare(&["--serve-only"]).is_ok());
}

#[tokio::test]
async fn crawler_factory_validates_settings_before_spawning() {
    let engine = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    let bad = Settings {
        pages: 0,
        ..Settings::default()
    }
    .table()
    .unwrap();
    let result = spawn(engine.clone(), "/user/invalid".into(), Some(bad)).await;
    assert!(result.is_err());
    engine.shutdown_internal().await.unwrap();
}

#[test]
fn business_settings_preserve_host_gateway_policy_and_cannot_override_it() {
    let flags = cli::Options {
        gateway_process_limit: 3,
        ..Default::default()
    };
    let settings = Settings::from_options(&flags);
    assert_eq!(settings.options(&flags).unwrap().gateway_process_limit, 3);
    let mut unauthorized = serde_json::to_value(&settings).unwrap();
    unauthorized["gateway_process_limit"] = 999.into();
    assert!(serde_json::from_value::<Settings>(unauthorized).is_err());
}
