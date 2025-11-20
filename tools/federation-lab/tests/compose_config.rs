use federation_lab::composegen::{ComposeGenConfig, Output, MAX_COMPOSE_CONTAINERS};

#[test]
fn every_public_entry_rejects_invalid_configuration_before_arithmetic_or_allocation() {
    let base = ComposeGenConfig::fifty_clusters();
    let mut cases = Vec::new();
    for (clusters, nodes_per_cluster, fold) in [
        (0, 1, 1),
        (1, 0, 1),
        (1, 1, 0),
        (1, 1, 5),
        (usize::MAX, 1, 1),
        (1, usize::MAX, 1),
        (2, usize::MAX, 4),
        (MAX_COMPOSE_CONTAINERS + 1, 1, 1),
        (58_537, 1, 4),
    ] {
        cases.push(ComposeGenConfig {
            clusters,
            nodes_per_cluster,
            fold,
            ..base.clone()
        });
    }
    for realm in [
        "",
        "a/b",
        "a: b\ninjected: true",
        "-a",
        "a-",
        "UPPER",
        "a b",
    ] {
        cases.push(ComposeGenConfig {
            realm: realm.into(),
            ..base.clone()
        });
    }
    cases.push(ComposeGenConfig {
        realm: "a".repeat(33),
        ..base.clone()
    });
    cases.push(ComposeGenConfig {
        output: Output::File("".into()),
        ..base
    });
    for config in cases {
        assert!(config.stats().is_err(), "{config:?}");
        assert!(config.containers_per_cluster().is_err(), "{config:?}");
        assert!(config.total_containers().is_err(), "{config:?}");
        assert!(config.total_logical_nodes().is_err(), "{config:?}");
        assert!(config.rr_sessions().is_err(), "{config:?}");
        assert!(config.containers().is_err(), "{config:?}");
        assert!(config.render_yaml().is_err(), "{config:?}");
        assert!(config.write_out().is_err(), "{config:?}");
    }
}

#[test]
fn one_cluster_and_nondivisible_folding_conserve_nodes_without_phantom_sessions() {
    let config = ComposeGenConfig {
        clusters: 1,
        nodes_per_cluster: 9,
        fold: 4,
        ..ComposeGenConfig::fifty_clusters()
    };
    let stats = config.stats().unwrap();
    assert_eq!(stats.containers, 3);
    assert_eq!(stats.logical_nodes, 9);
    assert_eq!(stats.rr_sessions, 0);
    let containers = config.containers().unwrap();
    assert_eq!(
        containers
            .iter()
            .map(|item| item.logical_nodes)
            .collect::<Vec<_>>(),
        vec![4, 4, 1]
    );
    assert_eq!(containers.iter().filter(|item| item.is_hub).count(), 1);
}

#[test]
fn declared_container_budget_includes_the_exact_boundary() {
    let config = ComposeGenConfig {
        clusters: MAX_COMPOSE_CONTAINERS,
        nodes_per_cluster: 1,
        fold: 1,
        ..ComposeGenConfig::fifty_clusters()
    };
    assert_eq!(config.stats().unwrap().containers, MAX_COMPOSE_CONTAINERS);
    let containers = config.containers().unwrap();
    assert_eq!(containers.len(), MAX_COMPOSE_CONTAINERS);
    assert_eq!(containers.last().unwrap().port, Some(16_999));
}
