#[path = "support/manifest_output.rs"]
mod support;

use parrot_app::{
    orchestrator::{AppSupervisor, DesiredStateStore, MemStateStore, SystemClock},
    ApplicationLimits,
};
use std::sync::Arc;
use support::*;

fn supervisor(limits: ApplicationLimits) -> (AppSupervisor, Arc<MemStateStore>) {
    let store = Arc::new(MemStateStore::with_limits(limits));
    let owner =
        AppSupervisor::new_with_limits(manifest(), store.clone(), Arc::new(SystemClock), limits)
            .unwrap();
    (owner, store)
}

#[test]
fn expanded_submit_is_rejected_before_revision_or_rollout_changes() {
    let (mut owner, store) = supervisor(ApplicationLimits::default());
    let mut valid = manifest();
    valid.components[0].artifact = parrot_app::ArtifactRef::Props {
        factory: "test.changed".into(),
    };
    owner.submit(valid.clone()).unwrap();
    assert_eq!(owner.upgrade_generation("worker"), Some(2));
    let invalid = repeated_headers();
    assert!(toml::to_string(&invalid).unwrap().len() > 4 * 1024 * 1024);
    let rejected = owner.submit(invalid);
    let persisted = store.latest().unwrap().unwrap();
    let unchanged = owner.revision() == 2
        && owner.upgrade_generation("worker") == Some(2)
        && persisted.revision == 2
        && persisted.pending["worker"].generation == 2
        && toml::to_string(&persisted.manifest).unwrap() == toml::to_string(&valid).unwrap();
    budget(rejected, "source bytes");
    assert!(
        unchanged,
        "rejected submit changed durable or in-memory ownership"
    );
    valid.version = "1.0.1".into();
    owner.submit(valid.clone()).unwrap();
    assert_eq!(owner.revision(), 3);
    assert_eq!(owner.upgrade_generation("worker"), Some(3));
    owner.submit(valid).unwrap();
    assert_eq!(owner.revision(), 3);
    assert_eq!(store.current_revision().unwrap(), Some(3));
}

#[test]
fn explicit_source_policy_accepts_exact_bytes_and_rejects_one_less() {
    let expanded = repeated_headers();
    let bytes = toml::to_string(&expanded).unwrap().len() as u64;
    let (mut exact, store) = supervisor(ApplicationLimits {
        max_source_bytes: bytes,
        ..Default::default()
    });
    exact.submit(expanded.clone()).unwrap();
    assert_eq!(exact.revision(), 2);
    exact.submit(expanded.clone()).unwrap();
    assert_eq!(exact.revision(), 2);
    assert_eq!(store.current_revision().unwrap(), Some(2));
    let (mut short, store) = supervisor(ApplicationLimits {
        max_source_bytes: bytes - 1,
        ..Default::default()
    });
    budget(short.submit(expanded), "source bytes");
    assert_eq!(short.revision(), 1);
    assert_eq!(store.current_revision().unwrap(), Some(1));
    assert_eq!(short.upgrade_generation("worker"), None);
}

#[test]
fn custom_model_policy_survives_submit_comparison() {
    let mut m = manifest();
    m.components[0].config = Some(nested_table(65));
    let (mut explicit, _) = supervisor(ApplicationLimits {
        max_config_depth: 65,
        ..Default::default()
    });
    explicit.submit(m.clone()).unwrap();
    explicit.submit(m.clone()).unwrap();
    assert_eq!(explicit.revision(), 2);
    let (mut default, _) = supervisor(ApplicationLimits::default());
    let error = default.submit(m).unwrap_err();
    assert!(error.contains("config depth"), "{error}");
    assert_eq!(default.revision(), 1);
}

#[test]
fn comparison_preserves_special_values_and_component_rollout_ownership() {
    let mut m = manifest();
    let mut sibling = m.components[0].clone();
    sibling.name = "sibling".into();
    m.components.push(sibling);
    let store = Arc::new(MemStateStore::default());
    let mut owner = AppSupervisor::new(m.clone(), store.clone(), Arc::new(SystemClock)).unwrap();
    m.components[0].config = Some(
        toml::from_str(
            "nan=nan\npositive=inf\nnegative=-inf\nzero=-0.0\nwhen=2026-11-13T00:00:00Z\nempty={}",
        )
        .unwrap(),
    );
    owner.submit(m.clone()).unwrap();
    assert_eq!(owner.revision(), 2);
    assert_eq!(owner.upgrade_generation("worker"), Some(2));
    assert_eq!(owner.upgrade_generation("sibling"), None);
    owner.submit(m.clone()).unwrap();
    assert_eq!(owner.revision(), 2, "NaN must not invent a new rollout");
    m.components[0]
        .config
        .as_mut()
        .unwrap()
        .insert("zero".into(), toml::Value::Float(0.0));
    owner.submit(m.clone()).unwrap();
    assert_eq!(
        owner.revision(),
        3,
        "signed zero is part of canonical identity"
    );
    assert_eq!(owner.upgrade_generation("worker"), Some(3));
    assert_eq!(owner.upgrade_generation("sibling"), None);
    // An explicit empty overlay is distinct from no overlay, even though it
    // has no values. Both components must receive the new global generation.
    m.config_overlay = Some(toml::Table::new());
    owner.submit(m.clone()).unwrap();
    assert_eq!(owner.revision(), 4);
    assert_eq!(owner.upgrade_generation("worker"), Some(4));
    assert_eq!(owner.upgrade_generation("sibling"), Some(4));
    owner.submit(m).unwrap();
    assert_eq!(store.current_revision().unwrap(), Some(4));
}
