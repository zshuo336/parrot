#[path = "support/manifest_output.rs"]
mod support;

use parrot_app::{orchestrator::DesiredStateRecord, ApplicationLimits};
use support::*;

fn record(manifest: parrot_app::AppManifest) -> DesiredStateRecord {
    DesiredStateRecord {
        revision: 1,
        manifest,
        pending: Default::default(),
        mirrors: Default::default(),
    }
}

fn legacy_record(manifest: &parrot_app::AppManifest) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "revision": 1,
        "manifest": toml::to_string(manifest).unwrap(),
        "pending": {},
        "mirrors": {},
    }))
    .unwrap()
}

#[test]
fn exact_utf8_source_boundary_includes_all_tables() {
    let mut m = manifest();
    m.components[0].config = Some(toml::from_str("text='中文'\n[deep]\nx=1").unwrap());
    let expected = toml::to_string(&m).unwrap();
    let mut limits = ApplicationLimits {
        max_source_bytes: expected.len() as u64,
        ..Default::default()
    };
    assert_eq!(m.to_toml_with_limits(&limits).unwrap(), expected);
    limits.max_source_bytes -= 1;
    budget(m.to_toml_with_limits(&limits), "source bytes");
    limits.max_source_bytes = 0;
    budget(m.to_toml_with_limits(&limits), "source bytes");
}

#[test]
fn explicit_model_limits_check_before_output_and_allow_relaxation() {
    let mut m = manifest();
    m.components[0].config = Some(nested_table(65));
    let mut limits = ApplicationLimits {
        max_config_depth: 65,
        ..Default::default()
    };
    assert_eq!(
        m.to_toml_with_limits(&limits).unwrap(),
        toml::to_string(&m).unwrap()
    );
    budget(m.to_toml(), "config depth");
    limits.max_config_depth = 64;
    budget(m.to_toml_with_limits(&limits), "config depth");
    let m = manifest();
    limits.max_components = 0;
    budget(m.to_toml_with_limits(&limits), "components");
    limits.max_components = 1;
    limits.max_metadata_bytes = 0;
    budget(m.to_toml_with_limits(&limits), "metadata bytes");
}

#[test]
fn aggregate_config_values_are_checked_across_fields() {
    let mut m = manifest();
    m.components[0].config = Some(toml::from_str("x=1").unwrap());
    m.config_overlay = m.components[0].config.clone();
    let mut limits = ApplicationLimits {
        max_config_values: 4,
        ..Default::default()
    };
    m.to_toml_with_limits(&limits).unwrap();
    limits.max_config_values = 3;
    budget(m.to_toml_with_limits(&limits), "config values");
}

#[test]
fn generated_header_expansion_obeys_explicit_source_policy() {
    let m = repeated_headers();
    let expected = toml::to_string(&m).unwrap();
    let limits = ApplicationLimits {
        max_source_bytes: expected.len() as u64,
        ..Default::default()
    };
    assert_eq!(m.to_toml_with_limits(&limits).unwrap(), expected);
    budget(m.to_toml(), "source bytes");
}

#[test]
fn output_admission_does_not_add_semantic_graph_validation() {
    let mut m = manifest();
    m.components[0].deps.push("missing".into());
    assert!(parrot_app::validate(&m).is_err());
    assert_eq!(m.to_toml().unwrap(), toml::to_string(&m).unwrap());
}

#[test]
fn canonical_bytes_match_legacy_across_nested_table_shapes() {
    let keys = ["simple", "", "dot.key", "quote\"key", "换行\n键"];
    let leaves = [
        "value=1",
        "value=[1,true,'x']",
        "value=[{},{}]",
        "value=[{x={y=2}}, {z=[1,{q=3}]}]",
        "value=[]",
    ];
    for key in keys {
        for leaf in leaves {
            for depth in 0..6 {
                let mut table: toml::Table = toml::from_str(leaf).unwrap();
                for _ in 0..depth {
                    table = toml::Table::from_iter([(key.into(), toml::Value::Table(table))]);
                }
                let mut m = manifest();
                m.components[0].config = Some(table.clone());
                m.config_overlay = Some(table);
                assert_eq!(m.to_toml().unwrap(), toml::to_string(&m).unwrap());
            }
        }
    }
}

#[test]
fn desired_record_default_decode_checks_manifest_source_and_model() {
    let mut m = manifest();
    m.components[0].config = Some(nested_table(65));
    budget(
        serde_json::from_slice::<DesiredStateRecord>(&legacy_record(&m)),
        "config depth",
    );
    budget(
        serde_json::from_slice::<DesiredStateRecord>(&legacy_record(&repeated_headers())),
        "source bytes",
    );
}

#[test]
fn desired_record_legacy_shape_unknown_duplicate_and_trailing_fields() {
    let mut value: serde_json::Value = serde_json::from_slice(&legacy_record(&manifest())).unwrap();
    value.as_object_mut().unwrap().remove("mirrors");
    let restored: DesiredStateRecord = serde_json::from_value(value.clone()).unwrap();
    assert!(restored.mirrors.is_empty());
    value["unknown"] = true.into();
    assert!(serde_json::from_value::<DesiredStateRecord>(value).is_err());
    let text = String::from_utf8(legacy_record(&manifest())).unwrap();
    let duplicate = text.replacen('{', "{\"revision\":1,", 1);
    assert!(serde_json::from_str::<DesiredStateRecord>(&duplicate).is_err());
    assert!(serde_json::from_str::<DesiredStateRecord>(&(text + "{}")).is_err());
}

#[test]
fn desired_record_keeps_its_public_serde_structure_name() {
    use serde::{de::Error, Deserialize, Deserializer};
    struct NamedFormat;
    impl<'de> Deserializer<'de> for NamedFormat {
        type Error = serde::de::value::Error;
        fn deserialize_any<V: serde::de::Visitor<'de>>(
            self,
            _: V,
        ) -> Result<V::Value, Self::Error> {
            Err(Self::Error::custom("expected a named structure"))
        }
        fn deserialize_struct<V: serde::de::Visitor<'de>>(
            self,
            name: &'static str,
            _: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            // Named formats can reject a changed type identity before fields.
            Err(Self::Error::custom(name))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct
            map enum identifier ignored_any
        }
    }
    assert_eq!(
        DesiredStateRecord::deserialize(NamedFormat)
            .unwrap_err()
            .to_string(),
        "DesiredStateRecord"
    );
}

#[cfg(unix)]
mod disk {
    use super::*;
    use parrot_app::orchestrator::{DesiredStateStore, FileStateStore};
    use sha2::{Digest, Sha256};

    fn write_snapshot(directory: &std::path::Path, payload: &[u8]) {
        let mut bytes = b"PDES01\0\0".to_vec();
        bytes.extend_from_slice(&Sha256::digest(payload));
        bytes.extend_from_slice(payload);
        std::fs::write(directory.join("desired.state"), bytes).unwrap();
    }

    #[test]
    fn explicit_disk_source_policy_survives_commit_reopen_and_legacy_load() {
        let m = repeated_headers();
        let limits = ApplicationLimits {
            max_source_bytes: toml::to_string(&m).unwrap().len() as u64,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let store = FileStateStore::open(dir.path(), limits).unwrap();
        store.commit(None, &record(m.clone())).unwrap();
        drop(store);
        let stored = std::fs::read(dir.path().join("desired.state")).unwrap();
        let legacy = legacy_record(&m);
        let value: serde_json::Value = serde_json::from_slice(&stored[40..]).unwrap();
        assert_eq!(value["manifest"], toml::to_string(&m).unwrap());
        let reopened = FileStateStore::open(dir.path(), limits).unwrap();
        assert_eq!(reopened.latest().unwrap().unwrap().manifest, m);
        drop(reopened);
        budget(
            FileStateStore::open(dir.path(), Default::default()),
            "source bytes",
        );
        assert_eq!(
            std::fs::read(dir.path().join("desired.state")).unwrap(),
            stored
        );
        write_snapshot(dir.path(), &legacy);
        let loaded = FileStateStore::open(dir.path(), limits).unwrap();
        assert_eq!(loaded.latest().unwrap().unwrap().manifest, m);
    }

    #[test]
    fn explicit_disk_model_policy_is_used_in_both_directions() {
        let mut m = manifest();
        m.components[0].config = Some(nested_table(65));
        let limits = ApplicationLimits {
            max_config_depth: 65,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let store = FileStateStore::open(dir.path(), limits).unwrap();
        store.commit(None, &record(m.clone())).unwrap();
        drop(store);
        let reopened = FileStateStore::open(dir.path(), limits).unwrap();
        assert_eq!(reopened.latest().unwrap().unwrap().manifest, m);
        drop(reopened);
        budget(
            FileStateStore::open(dir.path(), Default::default()),
            "config depth",
        );
    }

    #[test]
    fn legacy_disk_rejection_does_not_remove_abandoned_next() {
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(dir.path(), &legacy_record(&repeated_headers()));
        std::fs::write(dir.path().join("desired.next"), b"retain until admitted").unwrap();
        budget(
            FileStateStore::open(dir.path(), Default::default()),
            "source bytes",
        );
        assert_eq!(
            std::fs::read(dir.path().join("desired.next")).unwrap(),
            b"retain until admitted"
        );
    }

    #[test]
    fn checksum_valid_snapshot_still_rejects_trailing_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = legacy_record(&manifest());
        bytes.extend_from_slice(b"{}");
        write_snapshot(dir.path(), &bytes);
        let result = FileStateStore::open(dir.path(), Default::default());
        assert!(result.err().unwrap().contains("trailing"));
    }

    #[tokio::test]
    async fn mirror_reconcile_uses_explicit_output_policy() {
        use parrot_app::orchestrator::{MirrorSupervisor, ObservedState, SystemClock};
        use std::sync::Arc;
        struct Empty;
        #[async_trait::async_trait]
        impl parrot_remote::LocalLookup for Empty {
            async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
                None
            }
        }
        let m = repeated_headers();
        let limits = ApplicationLimits {
            max_source_bytes: toml::to_string(&m).unwrap().len() as u64,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let mut controller =
            MirrorSupervisor::open(dir.path(), m, Arc::new(SystemClock), limits).unwrap();
        let remote = parrot_remote::RemoteActorSystem::new(
            parrot_remote::RemoteConfig::mem("output-policy-mirror"),
            Arc::new(Empty),
        )
        .unwrap();
        controller
            .reconcile_once(&ObservedState::default(), &remote)
            .await
            .unwrap();
        drop(controller);
        MirrorSupervisor::open(dir.path(), manifest(), Arc::new(SystemClock), limits).unwrap();
        remote.shutdown().await.unwrap();
    }
}
