use parrot_remote::{
    raft::{ManualClock, StateMachine},
    roles::directory::{DirCmd, DirectoryReplica, DirectorySm},
};
use std::sync::Arc;
fn bytes(cmd: &DirCmd) -> Vec<u8> {
    bincode::serde::encode_to_vec(cmd, bincode::config::standard()).unwrap()
}
#[test]
fn trailing_bytes_never_apply_a_valid_prefix() {
    let mut sm = DirectorySm::default();
    let mut input = bytes(&DirCmd::Upsert {
        node: "n".into(),
        endpoints: vec!["tcp://n:7".into()],
    });
    input.push(0);
    assert!(sm.apply(&input).is_err());
    assert_eq!(
        sm,
        DirectorySm::default(),
        "invalid record must not change business state"
    );
}
#[test]
fn revision_exhaustion_is_rejected_without_panic_or_mutation() {
    let mut sm = DirectorySm {
        version: u64::MAX,
        ..Default::default()
    };
    let before = sm.clone();
    assert!(sm
        .apply(&bytes(&DirCmd::Remove { node: "n".into() }))
        .is_err());
    assert_eq!(sm, before);
}
#[test]
fn malformed_committed_batch_is_not_partially_applied() {
    let clock = Arc::new(ManualClock::new());
    let mut replica = DirectoryReplica::new("s", vec![], clock.clone());
    clock.advance(1400);
    replica.raft.tick();
    replica
        .raft
        .propose(bytes(&DirCmd::Upsert {
            node: "n".into(),
            endpoints: vec!["x".into()],
        }))
        .unwrap();
    replica.raft.propose(vec![255]).unwrap();
    assert!(replica.apply_committed().is_err());
    assert!(
        replica.apply_committed().is_err(),
        "failed batch remains pending"
    );
    assert_eq!(replica.raft.committed().len(), 2);
    assert_eq!(
        replica.sm,
        DirectorySm::default(),
        "preflight whole committed batch before applying any of it"
    );
}

#[test]
fn decoding_limits_reject_huge_declared_strings_collections_and_trailing_input() {
    // Remove variant 1 followed by a declared u64 string length with no bytes.
    let mut huge = vec![1, 253];
    huge.extend_from_slice(&u64::MAX.to_le_bytes());
    for input in [vec![], vec![255], huge, vec![0; 1024 * 1024 + 1]] {
        assert!(DirCmd::decode(&input).is_err());
    }
    let valid = DirCmd::KeyAggregate {
        key: "key".into(),
        nodes: (0..4096).map(|i| format!("n{i}")).collect(),
    };
    assert_eq!(DirCmd::decode(&valid.encode().unwrap()).unwrap(), valid);
    let oversized = DirCmd::KeyAggregate {
        key: "key".into(),
        nodes: vec!["n".into(); 4097],
    };
    assert!(oversized.encode().is_err());
    assert!(DirCmd::decode(&bytes(&oversized)).is_err());
    // Exactly the wire byte limit is valid, one extra byte is not. Both
    // use valid per-string and collection sizes, so this exercises total size.
    let mut endpoints = vec!["x".repeat(4096); 256];
    let raw = bytes(&DirCmd::Upsert {
        node: "n".into(),
        endpoints: endpoints.clone(),
    });
    let excess = raw.len() - 1024 * 1024;
    endpoints.last_mut().unwrap().truncate(4096 - excess);
    let boundary = DirCmd::Upsert {
        node: "n".into(),
        endpoints: endpoints.clone(),
    };
    assert_eq!(boundary.encode().unwrap().len(), 1024 * 1024);
    assert_eq!(
        DirCmd::decode(&boundary.encode().unwrap()).unwrap(),
        boundary
    );
    endpoints.last_mut().unwrap().push('x');
    let over = DirCmd::Upsert {
        node: "n".into(),
        endpoints,
    };
    assert!(over.encode().is_err());
    assert!(DirCmd::decode(&bytes(&over)).is_err());
    for node in ["".into(), "bad\nnode".into(), "n".repeat(1025)] {
        assert!(DirCmd::Remove { node }.encode().is_err());
    }
}

#[test]
fn invalid_typed_proposals_do_not_enter_any_directory_log() {
    use parrot_remote::roles::directory::DirectoryShards;
    let clock = Arc::new(ManualClock::new());
    let mut d = DirectoryShards::new(3, clock.clone());
    let mut solo = DirectoryReplica::new("s", vec![], clock.clone());
    clock.advance(1400);
    solo.raft.tick();
    for s in d.shards.values_mut() {
        s.raft.tick();
    }
    let invalid = DirCmd::Remove {
        node: "bad\0node".into(),
    };
    assert!(d.propose(&invalid).is_err());
    assert!(solo.propose(&invalid).is_err());
    assert!(solo.raft.log.is_empty());
    assert!(d.shards.values().all(|s| s.raft.log.is_empty()));
}

#[test]
fn bad_pending_command_in_one_shard_does_not_drain_or_mutate_any_local_group() {
    use parrot_remote::roles::directory::DirectoryShards;
    let clock = Arc::new(ManualClock::new());
    let mut d = DirectoryShards::new(3, clock.clone());
    clock.advance(1400);
    for s in d.shards.values_mut() {
        s.raft.tick();
    }
    let ids: Vec<_> = d.shards.keys().cloned().collect();
    for (i, id) in ids.iter().enumerate() {
        d.shards
            .get_mut(id)
            .unwrap()
            .raft
            .propose(if i == 1 {
                vec![255]
            } else {
                bytes(&DirCmd::Remove { node: "n".into() })
            })
            .unwrap();
    }
    assert!(d.apply_committed_all().is_err());
    assert!(d.apply_committed_all().is_err());
    for s in d.shards.values_mut() {
        assert_eq!(s.sm, DirectorySm::default());
        assert_eq!(s.raft.committed().len(), 1);
    }
}
