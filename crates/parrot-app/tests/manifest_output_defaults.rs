#[path = "support/manifest_output.rs"]
mod support;

use parrot_app::{orchestrator::DesiredStateRecord, ArtifactRef, EngineKind};
use support::*;

fn record(manifest: parrot_app::AppManifest) -> DesiredStateRecord {
    DesiredStateRecord {
        revision: 1,
        manifest,
        pending: Default::default(),
        mirrors: Default::default(),
    }
}

#[test]
fn output_checks_component_depth_before_recursive_serialization() {
    let mut m = manifest();
    m.components[0].config = Some(nested_table(65));
    budget(m.to_toml(), "config depth");
}

#[test]
fn output_checks_overlay_depth_before_recursive_serialization() {
    let mut m = manifest();
    m.config_overlay = Some(nested_table(65));
    budget(m.to_toml(), "config depth");
}

#[test]
fn output_checks_main_and_alternate_runtime_environments() {
    for alternate in [false, true] {
        let mut m = manifest();
        let artifact = ArtifactRef::PyModule {
            module: "jobs".into(),
            uri: None,
            runtime_env: Some(toml::Value::Table(nested_table(65))),
        };
        if alternate {
            m.components[0].alt_artifact = Some(artifact);
        } else {
            m.components[0].engine = EngineKind::Ray;
            m.components[0].artifact = artifact;
        }
        budget(m.to_toml(), "config depth");
    }
}

#[test]
fn output_checks_model_counts_before_serialization() {
    let mut m = manifest();
    m.components = vec![m.components[0].clone(); 1025];
    budget(m.to_toml(), "components");
    m.components.truncate(1);
    m.components[0].deps = vec!["dependency".into(); 16_385];
    budget(m.to_toml(), "dependencies");
}

#[test]
fn output_checks_model_metadata_before_serialization() {
    let mut m = manifest();
    m.name = "x".repeat(16 * 1024 * 1024);
    budget(m.to_toml(), "metadata bytes");
}

#[test]
fn output_limits_generated_source_independently_of_model_bytes() {
    let m = repeated_headers();
    assert!(serde_json::to_vec(&m).unwrap().len() < 64 * 1024);
    parrot_app::validate(&m).unwrap();
    budget(m.to_toml(), "source bytes");
}

#[test]
fn desired_record_serde_uses_default_output_admission() {
    let mut m = manifest();
    m.components[0].config = Some(nested_table(65));
    budget(serde_json::to_vec(&record(m)), "config depth");
}

#[cfg(unix)]
#[test]
fn failed_output_admission_does_not_publish_or_poison_disk_owner() {
    use parrot_app::orchestrator::{DesiredStateStore, FileStateStore};
    let directory = tempfile::tempdir().unwrap();
    let store = FileStateStore::open(directory.path(), Default::default()).unwrap();
    budget(
        store.commit(None, &record(repeated_headers())),
        "source bytes",
    );
    assert_eq!(store.current_revision().unwrap(), None);
    assert!(!directory.path().join("desired.state").exists());
    assert!(!directory.path().join("desired.next").exists());
    store.commit(None, &record(manifest())).unwrap();
    drop(store);
    let reopened = FileStateStore::open(directory.path(), Default::default()).unwrap();
    assert_eq!(reopened.current_revision().unwrap(), Some(1));
    assert_eq!(reopened.latest().unwrap().unwrap().manifest, manifest());
}

#[test]
fn canonical_output_preserves_legacy_bytes_and_special_values() {
    let fixtures = [
        "",
        "value=1\n",
        "empty={}\narrays=[{a=1},{b={c=true}}]\n",
        "when=2026-10-15T00:00:00Z\nvalue=nan\npos=inf\nneg=-inf\nzero=-0.0\n",
        "\"\" = {\"dot.key\"={empty={}}, \"空\"=[{}, {a=true}]}\nmixed=[1,{x=2},[3,4]]\n",
        "text='''line one\nline two'''\nquote='a\"b'\n",
    ];
    for config in fixtures {
        let mut m = manifest();
        m.components[0].config = Some(toml::from_str(config).unwrap());
        m.config_overlay = Some(toml::from_str(config).unwrap());
        let expected = toml::to_string(&m).unwrap();
        assert_eq!(m.to_toml().unwrap(), expected);
        let loaded = parrot_app::AppManifest::from_toml(&expected).unwrap();
        assert_eq!(loaded.to_toml().unwrap(), expected);
    }
}

#[test]
fn desired_record_serde_keeps_legacy_manifest_text_and_field_shape() {
    let mut m = manifest();
    m.components[0].config =
        Some(toml::from_str("value=nan\nzero=-0.0\nwhen=2026-10-15T00:00:00Z").unwrap());
    let expected = toml::to_string(&m).unwrap();
    let bytes = serde_json::to_vec(&record(m)).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 4);
    assert_eq!(value["manifest"], expected);
    let restored: DesiredStateRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored.manifest.to_toml().unwrap(), expected);
}
