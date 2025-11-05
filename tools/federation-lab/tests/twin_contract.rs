use federation_lab::twin::{
    DigitalTwin, TraversalReport, TwinConfig, VerifyOutcome, BATCH_SIZE, MAX_MODEL_NODES,
};
use parrot_remote::roles::directory::{DirCmd, DirQuery};
use std::collections::HashSet;
use std::net::SocketAddr;

#[test]
fn distinct_nodes_have_distinct_endpoints() {
    let twin = DigitalTwin::build(TwinConfig::default()).unwrap();
    let first = twin.expected_of("parrot://fed/c0/n0").unwrap();
    let later = twin.expected_of("parrot://fed/c0/n256").unwrap();
    assert_ne!(first.endpoint, later.endpoint);
}

#[test]
fn every_endpoint_is_a_valid_socket_address() {
    let twin = DigitalTwin::build(TwinConfig::default()).unwrap();
    for address in ["parrot://fed/c0/n0", "parrot://fed/c49/n19999"] {
        let endpoint = twin.expected_of(address).unwrap().endpoint;
        endpoint
            .strip_prefix("tcp://")
            .unwrap()
            .parse::<SocketAddr>()
            .expect("model endpoints must be parseable by the transport");
    }
}

fn report(visited: u64, correct: u64, unreachable: u64) -> TraversalReport {
    TraversalReport {
        visited,
        correct,
        unreachable,
        mismatch: 0,
        elapsed_ms: 0,
        elapsed_ns: 0,
        materialized_bytes: None,
        peak_materialized_nodes: 0,
    }
}

#[test]
fn empty_traversal_does_not_pass() {
    assert!(!report(0, 0, 0).passed(0));
}

#[test]
fn unreachable_or_unaccounted_visits_do_not_pass() {
    assert!(!report(2, 1, 1).passed(2));
    assert!(!report(2, 1, 0).passed(2));
    assert!(!report(2, 3, 0).passed(2));
    assert!(report(2, 2, 0).passed(2));
    let mut incorrect = report(2, 2, 0);
    incorrect.mismatch = 1;
    assert!(!incorrect.passed(2));
}

#[test]
fn golden_mapping_and_inverse_include_octet_and_cluster_boundaries() {
    let twin = DigitalTwin::build(TwinConfig::default()).unwrap();
    for (addr, endpoint) in [
        ("parrot://fed/c0/n0", "tcp://10.0.0.1:7"),
        ("parrot://fed/c0/n255", "tcp://10.0.1.0:7"),
        ("parrot://fed/c0/n256", "tcp://10.0.1.1:7"),
        ("parrot://fed/c1/n0", "tcp://10.0.78.33:7"),
        ("parrot://fed/c49/n19999", "tcp://10.15.66.64:7"),
    ] {
        assert_eq!(twin.expected_of(addr).unwrap().endpoint, endpoint);
        assert_eq!(twin.address_for_endpoint(endpoint).as_deref(), Some(addr));
    }
}

#[test]
fn full_irregular_space_has_no_collisions_and_roundtrips() {
    let cfg = TwinConfig {
        clusters: 7,
        nodes_per_cluster: 333,
        ..Default::default()
    };
    let twin = DigitalTwin::build(cfg.clone()).unwrap();
    let mut endpoints = HashSet::new();
    for idx in 0..cfg.total_nodes().unwrap() {
        let address = cfg.address_of(idx).unwrap();
        let expected = twin.expected_of(&address).unwrap();
        assert!(
            endpoints.insert(expected.endpoint.clone()),
            "collision at {address}"
        );
        assert_eq!(twin.address_for_endpoint(&expected.endpoint), Some(address));
        assert_eq!(expected.border, format!("parrot://fed/c{}", idx / 333));
    }
    assert_eq!(endpoints.len(), 2331);
    assert!(cfg.address_of(2331).is_none());
    assert!(cfg.address_of(u64::MAX).is_none());
}

#[test]
fn invalid_configuration_is_rejected_before_allocating_or_dividing() {
    let mut invalid = vec![];
    for (clusters, nodes, shards) in [
        (0, 1, 1),
        (1, 0, 1),
        (1, 1, 0),
        (1, 1, 257),
        (usize::MAX, 2, 1),
        (1, MAX_MODEL_NODES as usize + 1, 1),
    ] {
        invalid.push(TwinConfig {
            clusters,
            nodes_per_cluster: nodes,
            directory_shards: shards,
            ..Default::default()
        });
    }
    for realm in ["", "a/b", "a?b", "a b", "a%2f", "..", "雪"] {
        invalid.push(TwinConfig {
            realm: realm.into(),
            ..Default::default()
        });
    }
    for cfg in invalid {
        assert!(cfg.total_nodes().is_err(), "accepted {cfg:?}");
        assert!(cfg.address_of(0).is_none());
        assert!(DigitalTwin::build(cfg).is_err());
    }
    let cfg = TwinConfig {
        clusters: 1,
        nodes_per_cluster: MAX_MODEL_NODES as usize,
        ..Default::default()
    };
    let last = cfg.address_of(MAX_MODEL_NODES - 1).unwrap();
    let twin = DigitalTwin::build(cfg).unwrap();
    assert_eq!(
        twin.expected_of(&last).unwrap().endpoint,
        "tcp://10.255.255.254:7"
    );
}

#[test]
fn malformed_addresses_and_non_model_endpoints_are_rejected() {
    let twin = DigitalTwin::build(TwinConfig::default()).unwrap();
    let directory = twin.populate_directory(&[0]).unwrap();
    for addr in [
        "",
        "parrot://",
        "parrot://fed",
        "parrot://fed/",
        "parrot://fed/c",
        "parrot://fed/c/",
        "parrot://fed/cX/n1",
        "parrot://fed/c1/nX",
        "parrot://fed/c1/n1/extra",
        "parrot://fed/c-1/n1",
        "parrot://fed/c01/n1",
        "parrot://fed/c1/n01",
        "PARROT://fed/c1/n1",
        "parrot://FED/c1/n1",
        "parrot://fed/c1/n1 ",
        " parrot://fed/c1/n1",
        "parrot:///c1/n1",
        "parrot://fed//n1",
        "parrot://fed/c50/n0",
        "parrot://fed/c0/n20000",
    ] {
        assert_eq!(
            twin.verify(addr, &directory),
            VerifyOutcome::OutOfRealm,
            "{addr}"
        );
    }
    for endpoint in [
        "",
        "tcp://10.0.0:7",
        "tcp://10.0.0.0:7",
        "tcp://10.0.0.1:8",
        "udp://10.0.0.1:7",
        "tcp://11.0.0.1:7",
        "tcp://10.255.255.255:7",
        "tcp://[::1]:7",
        "tcp://10.15.66.65:7",
    ] {
        assert!(twin.address_for_endpoint(endpoint).is_none(), "{endpoint}");
    }
}

#[test]
fn verify_observes_committed_directory_state_and_detects_corruption() {
    let twin = DigitalTwin::build(TwinConfig::default()).unwrap();
    let mut directory = twin.populate_directory(&[256]).unwrap();
    let addr = "parrot://fed/c0/n256";
    assert_eq!(twin.verify(addr, &directory), VerifyOutcome::Correct);
    directory
        .propose(&DirCmd::Upsert {
            node: "fed/c0/n256".into(),
            endpoints: vec!["tcp://10.0.0.1:7".into()],
        })
        .unwrap();
    assert_eq!(directory.apply_committed_all().unwrap(), 1);
    assert_eq!(twin.verify(addr, &directory), VerifyOutcome::Mismatch);
    directory
        .propose(&DirCmd::Remove {
            node: "fed/c0/n256".into(),
        })
        .unwrap();
    directory.apply_committed_all().unwrap();
    assert_eq!(twin.verify(addr, &directory), VerifyOutcome::Mismatch);
    assert_eq!(
        twin.verify("parrot://fed/c0/n257", &directory),
        VerifyOutcome::Mismatch
    );
}

#[test]
fn wrong_shard_and_bad_border_are_detected() {
    let twin = DigitalTwin::build(TwinConfig::default()).unwrap();
    let mut directory = twin.populate_directory(&[256]).unwrap();
    let key = "fed/c0/n256";
    let owner = directory.route(key).unwrap().to_owned();
    let entry = directory
        .shards
        .get_mut(&owner)
        .unwrap()
        .sm
        .nodes
        .remove(key)
        .unwrap();
    directory
        .shards
        .iter_mut()
        .find(|(id, _)| **id != owner)
        .unwrap()
        .1
        .sm
        .nodes
        .insert(key.into(), entry);
    assert_eq!(
        twin.verify("parrot://fed/c0/n256", &directory),
        VerifyOutcome::Mismatch
    );
    let mut directory = twin.populate_directory(&[256]).unwrap();
    for shard in directory.shards.values_mut() {
        for node in shard.sm.borders.values_mut() {
            *node = "fed/c1/n0".into();
        }
    }
    assert_eq!(
        twin.verify("parrot://fed/c0/n256", &directory),
        VerifyOutcome::Mismatch
    );
    let mut directory = twin.populate_directory(&[256]).unwrap();
    directory
        .propose(&DirCmd::Remove {
            node: "fed/c0/n0".into(),
        })
        .unwrap();
    directory.apply_committed_all().unwrap();
    assert_eq!(
        twin.verify("parrot://fed/c0/n256", &directory),
        VerifyOutcome::Mismatch
    );
}

#[test]
fn cluster_borders_and_endpoints_do_not_alias_across_digit_boundaries() {
    let twin = DigitalTwin::build(TwinConfig {
        clusters: 11,
        nodes_per_cluster: 2,
        directory_shards: 4,
        ..Default::default()
    })
    .unwrap();
    let directory = twin.populate_directory(&[3, 21]).unwrap();
    for cluster in [1, 10] {
        let addr = format!("parrot://fed/c{cluster}/n1");
        assert_eq!(twin.verify(&addr, &directory), VerifyOutcome::Correct);
        assert_eq!(
            directory
                .query(&DirQuery::BorderFor { prefix: addr })
                .unwrap()
                .endpoints,
            [format!("parrot://fed/c{cluster}/n0")]
        );
    }
}

#[test]
fn traversal_uses_bounded_real_directory_batches() {
    let twin = DigitalTwin::build(TwinConfig {
        clusters: 4,
        nodes_per_cluster: 1_000,
        directory_shards: 8,
        ..Default::default()
    })
    .unwrap();
    let report = twin.traverse_all().unwrap();
    assert!(report.passed(4_000), "{report:?}");
    assert!(report.peak_materialized_nodes > 0);
    assert!(report.peak_materialized_nodes <= 2 * BATCH_SIZE);
    assert_eq!(report.materialized_bytes, None);
    assert!(report.elapsed_ns > 0);
}

#[test]
fn million_space_sampling_has_explicit_coverage_and_nonempty_gate() {
    let twin = DigitalTwin::build(TwinConfig::default()).unwrap();
    let report = twin.traverse_sampled(10_000).unwrap();
    assert!(report.passed(10_000), "{report:?}");
    assert!(!report.passed(1_000_000));
    assert!(report.peak_materialized_nodes <= 2 * BATCH_SIZE);
    assert!(twin.traverse_sampled(0).is_err());
    assert!(twin.traverse_sampled(1_000_001).is_err());
    assert!(twin.traverse_sampled(1).unwrap().passed(1));
    assert!(twin.populate_directory(&vec![0; BATCH_SIZE + 1]).is_err());
    assert!(twin.populate_directory(&[1_000_000]).is_err());
}
