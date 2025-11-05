use parrot_api::receptionist::{ReceptionistEvent, ReceptionistKey};
use parrot_remote::receptionist::{Receptionist, ReceptionistEntry, ReceptionistSync};
use tokio::sync::mpsc::error::TryRecvError;

fn key() -> ReceptionistKey {
    ReceptionistKey::new("edge/service").unwrap()
}

fn registered(path: &str) -> ReceptionistEvent {
    ReceptionistEvent::Registered {
        key: key(),
        remote_path: path.into(),
    }
}

fn unregistered(path: &str) -> ReceptionistEvent {
    ReceptionistEvent::Unregistered {
        key: key(),
        remote_path: path.into(),
    }
}

fn entry(node: &str, path: &str) -> ReceptionistEntry {
    ReceptionistEntry {
        key: key(),
        remote_path: path.into(),
        node: node.into(),
    }
}

#[test]
fn extreme_live_budgets_preserve_snapshot_and_live_events_without_panicking() {
    let maximum = tokio::sync::Semaphore::MAX_PERMITS;
    for initial in [0, 3] {
        for budget in [0, 1, 64, maximum - 1, maximum, maximum + 1, usize::MAX] {
            let mut table = Receptionist::new();
            for index in 0..initial {
                table.register("local", &key(), &format!("/user/initial-{index}"));
            }
            let mut stream = table.subscribe(&key(), budget);
            assert!(stream.max_capacity() <= maximum);
            assert!(stream.max_capacity() > initial);
            for index in 0..initial {
                assert_eq!(
                    stream.try_recv().unwrap(),
                    registered(&format!("/user/initial-{index}"))
                );
            }
            assert_eq!(stream.try_recv(), Err(TryRecvError::Empty));
            let event = table.register("local", &key(), "/user/live");
            table.broadcast(&event);
            assert_eq!(stream.try_recv().unwrap(), registered("/user/live"));
            let event = table.deregister(&key(), "/user/live");
            table.broadcast(&event);
            assert_eq!(stream.try_recv().unwrap(), unregistered("/user/live"));
            assert_eq!(stream.try_recv(), Err(TryRecvError::Empty));
            drop(table);
            assert_eq!(stream.try_recv(), Err(TryRecvError::Disconnected));
        }
    }
}

#[test]
fn owner_snapshot_transitions_match_independent_set_model() {
    use std::collections::BTreeSet;
    type View = BTreeSet<(String, String)>;
    let view = |entries: &[ReceptionistEntry]| -> View {
        entries
            .iter()
            .map(|entry| (entry.key.as_str().into(), entry.remote_path.clone()))
            .collect()
    };
    let entries = |mask: u8| -> Vec<ReceptionistEntry> {
        (0..4)
            .filter(|index| mask & (1 << index) != 0)
            .map(|index| ReceptionistEntry {
                key: ReceptionistKey::new(format!("edge/service-{}", index % 2)).unwrap(),
                remote_path: format!("parrot://n/user/{index}"),
                node: "n".into(),
            })
            .collect()
    };
    let other = ReceptionistEntry {
        key: ReceptionistKey::new("edge/service-0").unwrap(),
        remote_path: "parrot://other/user/permanent".into(),
        node: "other".into(),
    };
    for initial in 0..16 {
        for next in 0..16 {
            for final_mask in 0..16 {
                let mut table = Receptionist::new();
                table.register(&other.node, &other.key, &other.remote_path);
                let mut previous = View::new();
                for mask in [initial, next, final_mask] {
                    let wanted = entries(mask);
                    let current = view(&wanted);
                    let expected: BTreeSet<_> = previous
                        .difference(&current)
                        .map(|(key, path)| (false, key.clone(), path.clone()))
                        .chain(
                            current
                                .difference(&previous)
                                .map(|(key, path)| (true, key.clone(), path.clone())),
                        )
                        .collect();
                    let sync = ReceptionistSync {
                        from: "n".into(),
                        entries: wanted.iter().rev().chain(wanted.iter()).cloned().collect(),
                    };
                    let events = table.merge_sync(&sync);
                    let actual: BTreeSet<_> = events
                        .iter()
                        .map(|event| match event {
                            ReceptionistEvent::Registered { key, remote_path } => {
                                (true, key.as_str().to_owned(), remote_path.clone())
                            }
                            ReceptionistEvent::Unregistered { key, remote_path } => {
                                (false, key.as_str().to_owned(), remote_path.clone())
                            }
                        })
                        .collect();
                    assert_eq!(
                        actual.len(),
                        events.len(),
                        "a duplicate input must not duplicate events"
                    );
                    assert_eq!(
                        actual, expected,
                        "transition {initial}/{next}/{final_mask} at {mask}"
                    );
                    let snapshot = table.snapshot("n");
                    assert_eq!(snapshot.len(), current.len());
                    assert_eq!(view(&snapshot), current);
                    assert_eq!(
                        table.snapshot("other").as_slice(),
                        std::slice::from_ref(&other)
                    );
                    assert!(
                        table.merge_sync(&sync).is_empty(),
                        "replay must be idempotent"
                    );
                    previous = current;
                }
            }
        }
    }
}

#[test]
fn large_duplicate_snapshots_keep_exact_state_and_report_merge_cost() {
    use std::{collections::BTreeSet, time::Instant};
    for size in [1024, 4096, 16384] {
        for repetition in 0..3 {
            let mut table = Receptionist::new();
            let entries: Vec<_> = (0..size)
                .map(|index| entry("n", &format!("parrot://n/user/{index}")))
                .collect();
            let expected: BTreeSet<_> = entries
                .iter()
                .map(|entry| entry.remote_path.clone())
                .collect();
            let sync = ReceptionistSync {
                from: "n".into(),
                entries: entries
                    .iter()
                    .chain(entries.iter().rev())
                    .cloned()
                    .collect(),
            };
            let start = Instant::now();
            let events = table.merge_sync(&sync);
            let elapsed = start.elapsed();
            eprintln!(
                "MERGE_SAMPLE entries={size} repetition={repetition} nanoseconds={}",
                elapsed.as_nanos()
            );
            assert_eq!(events.len(), size);
            assert!(events
                .iter()
                .all(|event| matches!(event, ReceptionistEvent::Registered { .. })));
            let snapshot = table.snapshot("n");
            assert_eq!(snapshot.len(), size);
            assert_eq!(
                snapshot
                    .iter()
                    .map(|entry| entry.remote_path.clone())
                    .collect::<BTreeSet<_>>(),
                expected
            );
            assert!(table.merge_sync(&sync).is_empty());
            let removed = table.merge_sync(&ReceptionistSync {
                from: "n".into(),
                entries: Vec::new(),
            });
            let paths: BTreeSet<_> = removed
                .iter()
                .map(|event| match event {
                    ReceptionistEvent::Unregistered {
                        key: actual,
                        remote_path,
                    } => {
                        assert_eq!(*actual, key());
                        remote_path.clone()
                    }
                    other => panic!("empty replacement must only withdraw: {other:?}"),
                })
                .collect();
            assert_eq!(removed.len(), size);
            assert_eq!(paths, expected);
            assert!(table.snapshot("n").is_empty());
        }
    }
}

#[test]
fn complete_snapshot_precedes_live_changes_even_with_small_buffer() {
    let mut table = Receptionist::new();
    let paths = [
        "parrot://n/user/a",
        "parrot://n/user/b",
        "parrot://n/user/c",
    ];
    for path in paths {
        table.register("n", &key(), path);
    }
    let mut stream = table.subscribe(&key(), 1);
    let live = table.deregister(&key(), paths[1]);
    table.broadcast(&live);
    for path in paths {
        assert_eq!(stream.try_recv(), Ok(registered(path)));
    }
    assert_eq!(stream.try_recv(), Ok(unregistered(paths[1])));
    assert_eq!(stream.try_recv(), Err(TryRecvError::Empty));
    let live = table.register("n", &key(), "parrot://n/user/d");
    table.broadcast(&live);
    assert_eq!(stream.try_recv(), Ok(registered("parrot://n/user/d")));
}

#[test]
fn zero_live_buffer_has_one_slot_and_a_complete_snapshot() {
    let mut table = Receptionist::new();
    table.register("n", &key(), "parrot://n/user/a");
    table.register("n", &key(), "parrot://n/user/b");
    let mut stream = table.subscribe(&key(), 0);
    let live = table.register("n", &key(), "parrot://n/user/c");
    table.broadcast(&live);
    for path in [
        "parrot://n/user/a",
        "parrot://n/user/b",
        "parrot://n/user/c",
    ] {
        assert_eq!(stream.try_recv(), Ok(registered(path)));
    }
    assert_eq!(stream.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn snapshot_removal_identifies_each_path_and_preserves_other_nodes() {
    let mut table = Receptionist::new();
    for (node, path) in [
        ("n", "parrot://n/user/a"),
        ("n", "parrot://n/user/b"),
        ("other", "parrot://other/user/a"),
    ] {
        table.register(node, &key(), path);
    }
    let mut stream = table.subscribe(&key(), 4);
    for _ in 0..3 {
        assert!(matches!(
            stream.try_recv(),
            Ok(ReceptionistEvent::Registered { .. })
        ));
    }
    let sync = ReceptionistSync {
        from: "n".into(),
        entries: vec![],
    };
    let events = table.merge_sync(&sync);
    assert_eq!(
        events,
        vec![
            unregistered("parrot://n/user/a"),
            unregistered("parrot://n/user/b")
        ]
    );
    for event in &events {
        table.broadcast(event);
    }
    for event in events {
        assert_eq!(stream.try_recv(), Ok(event));
    }
    assert_eq!(stream.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(
        table.snapshot("other"),
        vec![entry("other", "parrot://other/user/a")]
    );
    assert!(table.snapshot("n").is_empty());
    assert!(table.merge_sync(&sync).is_empty());
}

#[test]
fn replacement_emits_only_actual_changes_and_duplicate_replay_is_silent() {
    let mut table = Receptionist::new();
    table.register("n", &key(), "parrot://n/user/a");
    table.register("n", &key(), "parrot://n/user/b");
    let mut sync = ReceptionistSync {
        from: "n".into(),
        entries: vec![
            entry("n", "parrot://n/user/b"),
            entry("n", "parrot://n/user/c"),
            entry("n", "parrot://n/user/c"),
        ],
    };
    assert_eq!(
        table.merge_sync(&sync),
        vec![
            unregistered("parrot://n/user/a"),
            registered("parrot://n/user/c")
        ]
    );
    sync.entries.reverse();
    assert!(table.merge_sync(&sync).is_empty());
    assert_eq!(table.lookup(&key()).len(), 2);
}

#[test]
fn mixed_node_snapshot_is_rejected_before_any_mutation() {
    let mut table = Receptionist::new();
    table.register("n", &key(), "parrot://n/user/original");
    let sync = ReceptionistSync {
        from: "n".into(),
        entries: vec![
            entry("n", "parrot://n/user/new"),
            entry("other", "parrot://other/user/injected"),
        ],
    };
    assert!(table.merge_sync(&sync).is_empty());
    assert_eq!(
        table.snapshot("n"),
        vec![entry("n", "parrot://n/user/original")]
    );
    assert!(table.snapshot("other").is_empty());
}

#[test]
fn key_decoding_rejects_constructor_invalid_values_in_json_and_wire() {
    for invalid in ["edge/invalid key", "edge/*", "edge/#"] {
        let json = serde_json::to_string(invalid).unwrap();
        assert!(serde_json::from_str::<ReceptionistKey>(&json).is_err());
        let wire = bincode::serde::encode_to_vec(invalid, bincode::config::standard()).unwrap();
        assert!(bincode::serde::decode_from_slice::<ReceptionistKey, _>(
            &wire,
            bincode::config::standard()
        )
        .is_err());
        let sync = serde_json::json!({
            "from": "n",
            "entries": [{"key": invalid, "node": "n", "remote_path": "parrot://n/user/a"}]
        });
        assert!(serde_json::from_value::<ReceptionistSync>(sync).is_err());
    }
}

#[test]
fn valid_key_preserves_string_json_and_binary_wire_compatibility() {
    let expected = key();
    assert_eq!(
        serde_json::to_string(&expected).unwrap(),
        "\"edge/service\""
    );
    assert_eq!(
        serde_json::from_str::<ReceptionistKey>("\"edge/service\"").unwrap(),
        expected
    );
    let bytes =
        bincode::serde::encode_to_vec(expected.as_str(), bincode::config::standard()).unwrap();
    assert_eq!(
        bincode::serde::encode_to_vec(&expected, bincode::config::standard()).unwrap(),
        bytes
    );
    let (decoded, read) = bincode::serde::decode_from_slice::<ReceptionistKey, _>(
        &bytes,
        bincode::config::standard(),
    )
    .unwrap();
    assert_eq!(decoded, expected);
    assert_eq!(read, bytes.len());
}

#[test]
fn full_subscriber_closes_after_its_prefix_and_healthy_subscriber_continues() {
    let mut table = Receptionist::new();
    let mut slow = table.subscribe(&key(), 1);
    let mut healthy = table.subscribe(&key(), 1);
    let first = table.register("n", &key(), "parrot://n/user/a");
    table.broadcast(&first);
    assert_eq!(healthy.try_recv(), Ok(first.clone()));
    let second = table.register("n", &key(), "parrot://n/user/b");
    table.broadcast(&second);
    assert_eq!(slow.try_recv(), Ok(first));
    assert_eq!(slow.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(healthy.try_recv(), Ok(second));
    let third = table.deregister(&key(), "parrot://n/user/a");
    table.broadcast(&third);
    assert_eq!(healthy.try_recv(), Ok(third));
    assert_eq!(healthy.try_recv(), Err(TryRecvError::Empty));
    let mut recovered = table.subscribe(&key(), 1);
    assert_eq!(recovered.try_recv(), Ok(registered("parrot://n/user/b")));
    assert_eq!(recovered.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn authorized_facade_replays_more_than_its_default_live_capacity() {
    use parrot_api::receptionist::ReceptionistGateway;
    use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};

    let rules = AclRules::from_json(r#"{"roles":{"n":["edge"]},"default_deny":true}"#).unwrap();
    let facade = ReceptionistFacade::new("n").with_acl(rules);
    for index in 0..65 {
        facade
            .register(key(), format!("parrot://n/user/{index}"))
            .await
            .unwrap();
    }
    let mut stream = facade.subscribe(key()).await.unwrap();
    facade
        .deregister(key(), "parrot://n/user/64".into())
        .await
        .unwrap();
    for index in 0..65 {
        assert_eq!(
            stream.try_recv(),
            Ok(registered(&format!("parrot://n/user/{index}")))
        );
    }
    assert_eq!(stream.try_recv(), Ok(unregistered("parrot://n/user/64")));
    assert_eq!(stream.try_recv(), Err(TryRecvError::Empty));
}
