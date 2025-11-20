use parrot_app::{
    assemble::effective_config,
    planner::{CandidateNode, LocalNodeTopology, TopologyView},
    AppManifest, PlacementConstraint,
};

fn topology(realm: Option<&str>) -> LocalNodeTopology {
    LocalNodeTopology::new(
        CandidateNode {
            node_id: "operator-node".into(),
            role: "hub".into(),
            labels: vec!["gpu".into(), "ssd".into()],
            load: 0.25,
        },
        realm.map(Into::into),
    )
}

#[test]
fn explicit_local_metadata_matches_each_constraint_conjunctively() {
    let topology = topology(Some("west"));
    for role in [None, Some("hub"), Some("hub-backup"), Some("normal")] {
        for realm in [None, Some("west"), Some("west-2")] {
            for label in [
                None,
                Some("gpu"),
                Some("ssd"),
                Some("gp"),
                Some("gpu-extra"),
            ] {
                let c = PlacementConstraint {
                    role: role.map(Into::into),
                    realm: realm.map(Into::into),
                    label: label.map(Into::into),
                    anti_affinity: false,
                };
                let expected = role.is_none_or(|v| v == "hub")
                    && realm.is_none_or(|v| v == "west")
                    && label.is_none_or(|v| v == "gpu" || v == "ssd");
                assert_eq!(topology.self_matches(&c), expected, "{c:?}");
                let candidates = topology.candidates(&c);
                assert_eq!(candidates.len(), usize::from(expected), "{c:?}");
                if expected {
                    assert_eq!(candidates[0].node_id, "operator-node");
                    assert_eq!(candidates[0].load, 0.25);
                }
            }
        }
    }
}

#[test]
fn missing_realm_is_distinct_from_an_explicit_empty_realm() {
    let c = PlacementConstraint {
        realm: Some(String::new()),
        ..Default::default()
    };
    assert!(!topology(None).self_matches(&c));
    assert!(topology(Some("")).self_matches(&c));
}

#[test]
fn resolved_local_metadata_tracks_all_supported_roles_and_actual_identity() {
    for role in ["normal", "HUB", "border", "directory"] {
        let config = parrot_config::ParrotConfig::builder()
            .remote_node_id("configured-node")
            .remote_topology_role(role)
            .build()
            .unwrap();
        let topology = LocalNodeTopology::from_resolved(&config);
        assert!(topology.self_is(None));
        assert!(topology.self_is(Some(&role.to_ascii_lowercase())));
        assert!(!topology.self_is(Some("unrelated")));
        let candidates = topology.candidates(&Default::default());
        assert_eq!(candidates[0].node_id, "configured-node");
        assert_eq!(candidates[0].role, role.to_ascii_lowercase());
        assert!(candidates[0].labels.is_empty());
        assert!(!topology.self_matches(&PlacementConstraint {
            realm: Some("configured-node".into()),
            ..Default::default()
        }));
    }
    let resolved = parrot_config::ParrotConfig::default().build().unwrap();
    assert_eq!(
        LocalNodeTopology::from_resolved(&resolved).candidates(&Default::default())[0].node_id,
        parrot_remote::RemoteConfig::from_resolved(&resolved)
            .unwrap()
            .node_id
    );
}

#[test]
fn manifest_overlay_role_wins_before_local_topology_is_created() {
    let manifest = AppManifest::from_toml(
        "name = 'overlay'\nversion = '1.0.0'\ncomponents = []\n[config_overlay.remote.node]\nnode_id = 'overlay-node'\ntopology_role = 'hub'",
    ).unwrap();
    let base = parrot_config::ParrotConfig::builder()
        .remote_node_id("base-node")
        .remote_topology_role("normal");
    let resolved = effective_config(&manifest, &base).unwrap();
    let topology = LocalNodeTopology::from_resolved(&resolved);
    assert!(topology.self_is(Some("hub")));
    assert!(!topology.self_is(Some("normal")));
    assert_eq!(
        topology.candidates(&Default::default())[0].node_id,
        "overlay-node"
    );
}

#[test]
fn legacy_provider_default_requires_positive_local_role_and_known_dimensions() {
    struct Legacy;
    impl TopologyView for Legacy {
        fn candidates(&self, _: &PlacementConstraint) -> Vec<CandidateNode> {
            panic!("local admission must not query remote candidates")
        }
        fn self_is(&self, role: Option<&str>) -> bool {
            role == Some("hub")
        }
    }
    let mut c = PlacementConstraint {
        role: Some("hub".into()),
        ..Default::default()
    };
    assert!(Legacy.self_matches(&c));
    c.realm = Some("west".into());
    assert!(!Legacy.self_matches(&c));
    c.realm = None;
    c.label = Some("gpu".into());
    assert!(!Legacy.self_matches(&c));
    assert!(!Legacy.self_matches(&Default::default()));
}
