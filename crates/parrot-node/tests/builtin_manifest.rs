//! G2/M3（DEV_09 §3.7）：parrot-node builtin_app.toml Manifest 化测试。
//!
//! env 语义等价断言（PARROT_ACTORS → overlay 过滤）。

use parrot_node::builtin_manifest_components_from;

#[test]
fn default_all_four_components() {
    let all = builtin_manifest_components_from(None);
    assert_eq!(all, vec!["echo", "counter", "kv", "slow"]);
}

#[test]
fn filter_subset_preserves_manifest_order() {
    let got = builtin_manifest_components_from(Some("kv,echo"));
    // Manifest 声明序（echo 在 kv 前——过滤不重排）
    assert_eq!(got, vec!["echo", "kv"]);
}

#[test]
fn filter_single() {
    assert_eq!(
        builtin_manifest_components_from(Some(" counter ")),
        vec!["counter"]
    );
}

#[test]
fn filter_empty_string_means_none() {
    // 旧语义：PARROT_ACTORS=""（显式置空）→ 不起任何内置 actor
    assert!(builtin_manifest_components_from(Some("")).is_empty());
    assert!(builtin_manifest_components_from(Some("  ")).is_empty());
}

#[test]
fn filter_unknown_names_ignored() {
    assert_eq!(
        builtin_manifest_components_from(Some("echo,nope")),
        vec!["echo"]
    );
}

#[test]
fn builtin_manifest_validates() {
    let toml = include_str!("../builtin_app.toml");
    let m = parrot_app::manifest::AppManifest::from_toml(toml).expect("parse");
    parrot_app::manifest::validate(&m).expect("validate");
    assert_eq!(m.name, "parrot-node-builtin");
}

#[test]
fn all_components_are_parrot_engine_props() {
    let toml = include_str!("../builtin_app.toml");
    let m = parrot_app::manifest::AppManifest::from_toml(toml).unwrap();
    for c in &m.components {
        assert_eq!(c.engine.as_str(), "parrot", "{} 引擎漂移", c.name);
        assert!(
            matches!(&c.artifact, parrot_app::manifest::ArtifactRef::Props { .. }),
            "{} 非 Props 形态",
            c.name
        );
    }
}
