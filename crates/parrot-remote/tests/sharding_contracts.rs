use parrot_remote::sharding::{
    entity_key_of, EntityLease, EntityStatus, HashRing, HashRingError, ShardCoordinator,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

fn nodes() -> Vec<String> {
    vec!["a".into(), "b".into(), "c".into()]
}

fn activate(coordinator: &ShardCoordinator, key: &str) -> EntityLease {
    let lease = coordinator
        .begin_activation(key)
        .expect("reserve generation");
    assert_eq!(coordinator.entities()[key].status, EntityStatus::Activating);
    assert!(coordinator.activated(&lease));
    lease
}

#[test]
fn removal_reports_exactly_the_affected_keys_and_preserves_other_states() {
    let coordinator = ShardCoordinator::new(&nodes());
    let mut before = BTreeMap::new();
    for index in 0..100 {
        let key = format!("entity-{index}");
        let lease = activate(&coordinator, &key);
        before.insert(key, lease.node().to_owned());
    }
    let expected: BTreeSet<_> = before
        .iter()
        .filter(|(_, node)| node.as_str() == "a")
        .map(|(key, _)| key.clone())
        .collect();
    assert!(!expected.is_empty());
    let moves = coordinator.rebalance(&["b".into(), "c".into()]).unwrap();
    assert_eq!(moves.len(), expected.len());
    assert_eq!(
        moves
            .iter()
            .map(|item| item.owner.key().to_owned())
            .collect::<BTreeSet<_>>(),
        expected
    );
    let after = coordinator.entities();
    assert_eq!(after.len(), before.len());
    for (key, owner) in before {
        let state = &after[&key];
        assert_eq!(state.node, owner);
        if expected.contains(&key) {
            assert_eq!(state.status, EntityStatus::Migrating);
            let item = moves.iter().find(|item| item.owner.key() == key).unwrap();
            assert_eq!(item.owner.node(), owner);
            assert_eq!(coordinator.route(&key), item.target);
        } else {
            assert_eq!(state.node, owner);
            assert_eq!(state.status, EntityStatus::Active);
        }
    }
}

#[test]
fn an_empty_membership_withdraws_every_active_entity() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let lease = activate(&coordinator, "key");
    let moves = coordinator.rebalance(&[]).unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].owner, lease);
    assert_eq!(moves[0].target, None);
    assert!(coordinator.begin_activation("key").is_none());
    assert_eq!(coordinator.route("key"), None);
    assert_eq!(
        coordinator.entities()["key"].status,
        EntityStatus::Migrating
    );
}

#[test]
fn membership_changes_do_not_resurrect_passivated_entities() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let lease = activate(&coordinator, "key");
    assert!(coordinator.passivated(&lease));
    let moves = coordinator.rebalance(&["b".into()]).unwrap();
    assert!(moves.is_empty());
    assert_eq!(
        coordinator.entities()["key"].status,
        EntityStatus::Passivated
    );
    assert_eq!(coordinator.entities()["key"].node, "a");
}

#[test]
fn activation_cannot_register_a_foreign_nodes_lease() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let foreign = ShardCoordinator::new(&["b".into()])
        .begin_activation("key")
        .unwrap();
    assert!(!coordinator.activated(&foreign));
    assert!(coordinator.entities().is_empty());
}

#[test]
fn migration_keeps_the_actual_owner_until_retirement_is_confirmed() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let lease = activate(&coordinator, "key");
    let moves = coordinator.rebalance(&["b".into()]).unwrap();
    assert_eq!(moves[0].owner, lease);
    assert_eq!(coordinator.route("key").as_deref(), Some("b"));
    assert_eq!(
        coordinator.entities()["key"].status,
        EntityStatus::Migrating
    );
    assert_eq!(coordinator.entities()["key"].node, "a");
}

#[test]
fn removing_a_node_moves_only_its_keys() {
    let original = HashRing::new(&nodes(), 256);
    let shrunk = HashRing::new(&["b".into(), "c".into()], 256);
    let mut affected = 0;
    for index in 0..5000 {
        let key = format!("key-{index}");
        let before = original.node(&key).unwrap();
        let after = shrunk.node(&key).unwrap();
        if before == "a" {
            affected += 1;
            assert_ne!(after, "a");
        } else {
            assert_eq!(before, after);
        }
    }
    assert!(affected > 0 && affected < 5000);
}

#[test]
fn membership_order_and_duplicates_do_not_change_placement() {
    let original = HashRing::new(&nodes(), 256);
    let reordered = HashRing::new(&["c".into(), "a".into(), "b".into(), "a".into()], 256);
    for index in 0..5000 {
        let key = format!("key-{index}");
        assert_eq!(original.node(&key), reordered.node(&key));
    }
    assert_eq!(original.nodes(), reordered.nodes());
}

#[test]
fn adding_a_node_moves_keys_only_to_the_added_member() {
    let original = HashRing::new(&nodes(), 256);
    let expanded = HashRing::new(&["a".into(), "b".into(), "c".into(), "d".into()], 256);
    let mut moved = 0;
    for index in 0..5000 {
        let key = format!("addition-{index}");
        let before = original.node(&key).unwrap();
        let after = expanded.node(&key).unwrap();
        if after == "d" {
            moved += 1;
        } else {
            assert_eq!(before, after);
        }
    }
    assert!(moved > 0 && moved < 5000);
}

#[test]
fn entity_address_limits_include_scheme_node_and_utf8_bytes() {
    let coordinator = ShardCoordinator::new(&["node".into()]);
    let prefix = "parrot://node/user/entity-";
    let key = "k".repeat(4096 - prefix.len());
    let address = format!("{prefix}{key}");
    assert_eq!(address.len(), 4096);
    assert_eq!(entity_key_of(&address), Some(key.as_str()));
    assert_eq!(coordinator.route(&key).as_deref(), Some("node"));
    assert!(coordinator.begin_activation(&key).is_some());
    let oversized = key + "x";
    assert!(entity_key_of(&format!("{prefix}{oversized}")).is_none());
    assert!(coordinator.route(&oversized).is_none());
    assert!(coordinator.begin_activation(&oversized).is_none());
    // A multi-byte key is budgeted by bytes and remains literal when parsed.
    let utf8 = "\u{00e9}".repeat((4096 - prefix.len()) / 2);
    assert_eq!(coordinator.route(&utf8).as_deref(), Some("node"));
    assert_eq!(
        entity_key_of(&format!("{prefix}{utf8}")),
        Some(utf8.as_str())
    );
    assert!(coordinator.route(&(utf8 + "\u{00e9}")).is_none());
}

#[test]
fn entity_paths_reject_wrong_roots_empty_keys_and_nested_tails() {
    for path in [
        "/other/entity-key",
        "/user/entity-",
        "/user/entity-a/child",
        "/user/entity-a/entity-b",
        "relative/entity-key",
        "//user/entity-key",
        "/user/entity-key?query",
        "/user/entity-key#fragment",
        "/user/entity-\nkey",
        "parrot:///user/entity-key",
        "other://node/user/entity-key",
    ] {
        assert_eq!(entity_key_of(path), None, "{path:?}");
    }
}

#[test]
fn entity_paths_preserve_valid_local_and_qualified_literal_keys() {
    for (path, key) in [
        ("/user/entity-sensor-42", "sensor-42"),
        ("parrot://node/user/entity-x1", "x1"),
        ("/user/entity-%2f", "%2f"),
    ] {
        assert_eq!(entity_key_of(path), Some(key));
    }
}

#[test]
fn placement_vectors_preserve_directory_and_ring_routing() {
    let mut digest = Sha256::new();
    for count in [0, 1, 3, 8] {
        let nodes: Vec<_> = (0..count).map(|index| format!("dir-s{index}")).collect();
        let ring = HashRing::new(&nodes, 256);
        let directory = parrot_remote::roles::directory::DirectoryShards::new(
            count,
            std::sync::Arc::new(parrot_remote::raft::RealClock::default()),
        );
        for index in 0..2048 {
            let key = format!("route-{index}");
            let holder = ring.node(&key);
            assert_eq!(holder, directory.route(&key));
            if let Some(holder) = holder {
                assert!(nodes.iter().any(|node| node == holder));
            } else {
                assert_eq!(count, 0);
            }
            digest.update(format!("{count}:{key}:{holder:?}\n").as_bytes());
        }
    }
    assert_eq!(
        format!("{:x}", digest.finalize()),
        "a6421b9e8a4e63034889b01cf1aff4642bb18ea474600f79e1e3a7efc0fd4b2f"
    );
}

#[test]
fn reservations_block_duplicate_spawn_until_actual_retirement() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let pending = coordinator.begin_activation("key").unwrap();
    assert!(coordinator.begin_activation("key").is_none());
    assert!(!coordinator.forget_passivated(&pending));
    assert_eq!(
        coordinator.rebalance(&["b".into()]).unwrap()[0].owner,
        pending
    );
    assert!(!coordinator.activated(&pending));
    assert!(coordinator.begin_activation("key").is_none());
    assert!(coordinator.passivated(&pending));
    let replacement = activate(&coordinator, "key");
    assert_eq!(replacement.node(), "b");
    assert_ne!(replacement, pending);
    assert!(!coordinator.passivated(&pending));
    assert!(!coordinator.activated(&pending));
    assert!(!coordinator.forget_passivated(&pending));
    assert_eq!(coordinator.entities()["key"].status, EntityStatus::Active);
}

#[test]
fn same_node_reactivation_rejects_aba_and_foreign_callbacks() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let old = activate(&coordinator, "key");
    let retained = old.clone();
    assert_eq!(retained, old);
    assert!(coordinator.passivated(&old));
    assert!(coordinator.passivated(&retained));
    assert!(!coordinator.activated(&old));
    let current = activate(&coordinator, "key");
    let foreign = activate(&ShardCoordinator::new(&["a".into()]), "key");
    for stale in [&old, &retained, &foreign] {
        assert_ne!(current, *stale);
        assert!(!coordinator.passivated(stale));
        assert!(!coordinator.activated(stale));
        assert!(!coordinator.forget_passivated(stale));
    }
    assert!(coordinator.activated(&current.clone()));
    assert_eq!(coordinator.entities()["key"].status, EntityStatus::Active);
}

#[test]
fn forgetting_requires_retirement_and_cannot_remove_a_later_generation() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let old = activate(&coordinator, "key");
    assert!(!coordinator.forget_passivated(&old));
    assert!(coordinator.passivated(&old));
    assert!(coordinator.forget_passivated(&old));
    assert!(!coordinator.forget_passivated(&old));
    assert!(coordinator.entities().is_empty());
    let current = activate(&coordinator, "key");
    assert!(!coordinator.passivated(&old));
    assert!(!coordinator.forget_passivated(&old));
    assert!(coordinator.passivated(&current));
    let foreign = activate(&ShardCoordinator::new(&["a".into()]), "key");
    for stale in [&old, &foreign] {
        assert!(!coordinator.forget_passivated(stale));
        assert_eq!(
            coordinator.entities()["key"].status,
            EntityStatus::Passivated
        );
    }
    assert!(coordinator.forget_passivated(&current));
}

#[test]
fn draining_generation_does_not_revive_and_pending_targets_track_membership() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let old = activate(&coordinator, "key");
    for (members, target) in [
        (vec!["b".into()], Some("b")),
        (vec![], None),
        (vec!["c".into()], Some("c")),
        (vec!["a".into()], Some("a")),
    ] {
        let moves = coordinator.rebalance(&members).unwrap();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].owner, old);
        assert_eq!(moves[0].target.as_deref(), target);
        assert_eq!(coordinator.entities()["key"].node, "a");
        assert_eq!(
            coordinator.entities()["key"].status,
            EntityStatus::Migrating
        );
        assert!(!coordinator.activated(&old));
        assert!(coordinator.begin_activation("key").is_none());
    }
    assert!(coordinator.passivated(&old));
    assert!(coordinator.rebalance(&["a".into()]).unwrap().is_empty());
    let current = activate(&coordinator, "key");
    assert_ne!(old, current);
}

#[test]
fn unavailable_or_invalid_destinations_never_reserve_entities() {
    let empty = ShardCoordinator::new(&[]);
    assert!(empty.route("valid").is_none());
    assert!(empty.begin_activation("valid").is_none());
    let coordinator = ShardCoordinator::new(&["a".into()]);
    for key in [
        "",
        "a/b",
        "?query",
        "#fragment",
        "\n",
        "\0",
        &"k".repeat(4096),
    ] {
        assert!(coordinator.route(key).is_none(), "{key:?}");
        assert!(coordinator.begin_activation(key).is_none(), "{key:?}");
    }
    assert!(coordinator.entities().is_empty());
    let literal = activate(&coordinator, "%2f");
    assert_eq!(literal.key(), "%2f");
    let long_node = "n".repeat(4090);
    let long = ShardCoordinator::new(&[long_node]);
    assert!(long.route("k").is_none());
    assert!(long.begin_activation("k").is_none());
}

#[test]
fn invalid_membership_leaves_placement_and_entity_states_intact() {
    let coordinator = ShardCoordinator::new(&["a".into()]);
    let lease = activate(&coordinator, "key");
    let before = coordinator.entities();
    for invalid in ["", "bad/node", "a b", "bad@host", "a?b", "a#b", "a\0b"] {
        assert_eq!(
            coordinator.rebalance(&["b".into(), invalid.into()]),
            Err(HashRingError::InvalidNode { index: 1 })
        );
        assert_eq!(coordinator.entities(), before);
        assert_eq!(coordinator.route("key").as_deref(), Some("a"));
        assert!(coordinator.activated(&lease));
        assert!(matches!(
            ShardCoordinator::try_new(&[invalid.into()]),
            Err(HashRingError::InvalidNode { index: 0 })
        ));
    }
}

#[test]
fn ring_rejects_invalid_configuration_and_unrepresentable_capacity() {
    assert!(matches!(
        HashRing::try_new(&["a".into()], 0),
        Err(HashRingError::ZeroVirtualNodes)
    ));
    assert!(matches!(
        HashRing::try_new(&["a".into()], usize::MAX),
        Err(HashRingError::Capacity)
    ));
    assert!(matches!(
        HashRing::try_new(&["a".into(), "b".into()], usize::MAX),
        Err(HashRingError::Capacity)
    ));
    for replication in [0, 1, usize::MAX] {
        let ring = HashRing::try_new(&[], replication).unwrap();
        assert_eq!(ring.node("key"), None);
        assert!(ring.nodes().is_empty());
    }
    assert!(matches!(
        HashRing::try_new(&["n".repeat(65536)], 1),
        Err(HashRingError::InvalidNode { index: 0 })
    ));
}

#[test]
fn concurrent_reservations_admit_exactly_one_generation() {
    let coordinator = std::sync::Arc::new(ShardCoordinator::new(&["a".into()]));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let coordinator = coordinator.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                coordinator.begin_activation("key")
            })
        })
        .collect();
    let leases: Vec<_> = handles
        .into_iter()
        .filter_map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(leases.len(), 1);
    assert_eq!(coordinator.entities().len(), 1);
    assert_eq!(
        coordinator.entities()["key"].status,
        EntityStatus::Activating
    );
    assert!(coordinator.activated(&leases[0]));
}

#[test]
fn concurrent_rebalances_return_their_own_atomic_membership_targets() {
    let coordinator = std::sync::Arc::new(ShardCoordinator::new(&["a".into()]));
    let leases: Vec<_> = (0..64)
        .map(|i| activate(&coordinator, &format!("key-{i}")))
        .collect();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let coordinator = coordinator.clone();
            let barrier = barrier.clone();
            let leases = leases.clone();
            std::thread::spawn(move || {
                let node = format!("replacement-{i}");
                barrier.wait();
                for _ in 0..64 {
                    let moves = coordinator.rebalance(std::slice::from_ref(&node)).unwrap();
                    assert_eq!(moves.len(), leases.len());
                    assert!(moves
                        .windows(2)
                        .all(|pair| pair[0].owner.key() < pair[1].owner.key()));
                    for item in moves {
                        assert_eq!(item.target.as_ref(), Some(&node));
                        assert_eq!(item.owner.node(), "a");
                        assert!(leases.contains(&item.owner));
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(coordinator
        .entities()
        .values()
        .all(|state| state.node == "a" && state.status == EntityStatus::Migrating));
}
