use parrot_remote::{
    raft::{ManualClock, Role},
    roles::directory::{DirCmd, DirQuery, DirectoryShards},
};
use std::sync::Arc;

fn directory() -> DirectoryShards {
    let clock = Arc::new(ManualClock::new());
    let mut d = DirectoryShards::new(4, clock.clone());
    clock.advance(1400);
    for s in d.shards.values_mut() {
        s.raft.tick();
    }
    d
}
fn node_on(d: &DirectoryShards, shard: &str) -> String {
    (0..10_000)
        .map(|n| format!("node-{n}"))
        .find(|n| d.route(n) == Some(shard))
        .unwrap()
}
fn write(d: &mut DirectoryShards, cmd: DirCmd) {
    d.propose(&cmd).unwrap();
    d.apply_committed_all().unwrap();
}

#[test]
fn border_reassignment_has_one_owner_even_when_nodes_hash_to_different_shards() {
    let mut d = directory();
    let ids: Vec<_> = d.shards.keys().cloned().collect();
    let first = node_on(&d, &ids[0]);
    let second = node_on(&d, &ids[1]);
    assert_ne!(d.route(&first), d.route(&second));
    let prefix = "parrot://region/";
    for node in [first.clone(), second.clone()] {
        write(
            &mut d,
            DirCmd::BorderDeclare {
                prefix: prefix.into(),
                node,
            },
        );
    }
    let copies = d
        .shards
        .values()
        .filter(|s| s.sm.borders.contains_key(prefix))
        .count();
    assert_eq!(copies, 1, "a prefix must have one authoritative shard");
    assert_eq!(
        d.query(&DirQuery::BorderFor {
            prefix: prefix.into()
        })
        .unwrap()
        .endpoints,
        [format!("parrot://{second}")]
    );
    write(&mut d, DirCmd::Remove { node: first });
    assert_eq!(
        d.query(&DirQuery::BorderFor {
            prefix: prefix.into()
        })
        .unwrap()
        .endpoints,
        [format!("parrot://{second}")],
        "removing previous owner must preserve reassignment"
    );
    write(&mut d, DirCmd::Remove { node: second });
    assert!(d
        .query(&DirQuery::BorderFor {
            prefix: prefix.into()
        })
        .is_none());
}

#[test]
fn border_lookup_selects_global_longest_prefix_and_advances_query_version() {
    let mut d = directory();
    // Deliberately make the broad match be the FIRST HashMap result. This is
    // a deterministic adversarial fixture, not a probabilistic repeated test.
    let ids: Vec<_> = d.shards.keys().cloned().collect();
    let broad_node = node_on(&d, &ids[0]);
    let narrow_node = node_on(&d, &ids[1]);
    let broad = (0..10_000)
        .map(|i| format!("parrot://r{i}/"))
        .find(|p| d.route(&format!("border:{p}")) == Some(ids[0].as_str()))
        .unwrap();
    let narrow = (0..10_000)
        .map(|i| format!("{broad}c{i}/"))
        .find(|p| d.route(&format!("border:{p}")) == Some(ids[1].as_str()))
        .unwrap();
    write(
        &mut d,
        DirCmd::BorderDeclare {
            prefix: broad,
            node: broad_node.clone(),
        },
    );
    for i in 0..10 {
        write(
            &mut d,
            DirCmd::Upsert {
                node: broad_node.clone(),
                endpoints: vec![i.to_string()],
            },
        );
    }
    let query = DirQuery::BorderFor {
        prefix: format!("{narrow}leaf"),
    };
    let before = d.query(&query).unwrap();
    write(
        &mut d,
        DirCmd::BorderDeclare {
            prefix: narrow,
            node: narrow_node.clone(),
        },
    );
    assert_eq!(
        d.shards
            .values()
            .filter(|s| !s.sm.borders.is_empty())
            .count(),
        2
    );
    let after = d.query(&query).unwrap();
    assert_eq!(after.endpoints, [format!("parrot://{narrow_node}")]);
    assert!(
        after.version > before.version,
        "a more specific route must not carry an older query version"
    );
}

#[test]
fn removal_cleans_cross_shard_key_members_without_resurrecting_on_reregister() {
    let mut d = directory();
    let ids: Vec<_> = d.shards.keys().cloned().collect();
    let removed = node_on(&d, &ids[0]);
    let survivor = node_on(&d, &ids[1]);
    let key = (0..10_000)
        .map(|i| format!("service-{i}"))
        .find(|k| d.route(&format!("key:{k}")) != d.route(&removed))
        .unwrap();
    write(
        &mut d,
        DirCmd::Upsert {
            node: removed.clone(),
            endpoints: vec!["old".into()],
        },
    );
    write(
        &mut d,
        DirCmd::KeyAggregate {
            key: key.clone(),
            nodes: vec![removed.clone(), survivor.clone()],
        },
    );
    write(
        &mut d,
        DirCmd::Remove {
            node: removed.clone(),
        },
    );
    assert!(
        d.shards
            .values()
            .all(|s| s.sm.key_index.values().all(|ns| !ns.contains(&removed))),
        "references must be removed in their authoritative Raft state, not hidden by the query"
    );
    let query = DirQuery::NodesForKey { key };
    assert_eq!(
        d.query(&query).unwrap().endpoints,
        [format!("parrot://{survivor}")]
    );
    write(
        &mut d,
        DirCmd::Upsert {
            node: removed,
            endpoints: vec!["new".into()],
        },
    );
    assert_eq!(
        d.query(&query).unwrap().endpoints,
        [format!("parrot://{survivor}")]
    );
}

#[test]
fn removal_rejects_before_any_log_append_when_a_participant_is_not_leader() {
    let mut d = directory();
    let ids: Vec<_> = d.shards.keys().cloned().collect();
    let removed = node_on(&d, &ids[0]);
    d.shards.get_mut(&ids[1]).unwrap().raft.role = Role::Follower;
    d.shards.get_mut(&ids[1]).unwrap().raft.leader_hint = None;
    let before: std::collections::HashMap<_, _> = d
        .shards
        .iter()
        .map(|(id, s)| (id.clone(), s.raft.log.clone()))
        .collect();
    assert!(matches!(d.propose(&DirCmd::Remove { node: removed }),
        Err(parrot_remote::roles::directory::DirectoryWriteError::NotLeader { shard, leader: None }) if shard == ids[1]));
    for (id, s) in &d.shards {
        assert_eq!(
            s.raft.log, before[id],
            "no partial admission on preflight failure"
        );
    }
}

#[test]
fn missing_configured_shard_returns_error_instead_of_panicking_or_partial_removal() {
    let mut d = directory();
    let ids: Vec<_> = d.shards.keys().cloned().collect();
    let node = node_on(&d, &ids[0]);
    d.shards.remove(&ids[0]);
    assert!(d
        .propose(&DirCmd::Upsert {
            node: node.clone(),
            endpoints: vec![]
        })
        .is_err());
    assert!(
        matches!(d.propose(&DirCmd::Remove { node }), Err(parrot_remote::roles::directory::DirectoryWriteError::Unavailable(shard)) if shard == ids[0])
    );
    assert!(d.shards.values().all(|s| s.raft.log.is_empty()));
}

// Drive the real Raft message exchange for one two-member group. The fixture
// holds back these messages to test independent commitment, rather than setting
// commit_index or pretending one received response commits every shard.
fn replicated_group(d: &mut DirectoryShards, id: &str) -> parrot_remote::raft::RaftNode {
    use parrot_remote::raft::{RaftNode, RaftRpc};
    let clock = Arc::new(ManualClock::new());
    let peer = format!("peer-{id}");
    let mut leader = RaftNode::new(id, vec![peer.clone()], clock.clone());
    let mut follower = RaftNode::new(peer, vec![id.into()], clock.clone());
    clock.advance(1400);
    leader.tick();
    for (_, rpc) in leader.outbox() {
        follower.step(id.into(), rpc);
    }
    for (_, rpc) in follower.outbox() {
        assert!(matches!(
            rpc,
            RaftRpc::RequestVoteResp {
                vote_granted: true,
                ..
            }
        ));
        leader.step(follower.id.clone(), rpc);
    }
    assert_eq!(leader.role, Role::Leader);
    d.shards.get_mut(id).unwrap().raft = leader;
    follower
}
fn replicate(d: &mut DirectoryShards, id: &str, follower: &mut parrot_remote::raft::RaftNode) {
    for _ in 0..32 {
        let leader = &mut d.shards.get_mut(id).unwrap().raft;
        let rpc = leader.outbox();
        if rpc.is_empty() {
            return;
        }
        for (_, rpc) in rpc {
            follower.step(id.into(), rpc);
        }
        for (_, rpc) in follower.outbox() {
            leader.step(follower.id.clone(), rpc);
        }
    }
    panic!("replication did not quiesce");
}

#[test]
fn removal_receipt_waits_for_every_real_raft_group_and_application_is_separate() {
    use parrot_remote::roles::directory::DirectoryProposalStatus as Status;
    let mut d = directory();
    let ids: Vec<_> = d.shards.keys().cloned().collect();
    let blocked = ids[1].clone();
    let node = node_on(&d, &ids[0]);
    let key = (0..10_000)
        .map(|i| format!("service-{i}"))
        .find(|k| d.route(&format!("key:{k}")) == Some(blocked.as_str()))
        .unwrap();
    let mut follower = replicated_group(&mut d, &blocked);
    write(
        &mut d,
        DirCmd::Upsert {
            node: node.clone(),
            endpoints: vec!["endpoint".into()],
        },
    );
    d.propose(&DirCmd::KeyAggregate {
        key: key.clone(),
        nodes: vec![node.clone()],
    })
    .unwrap();
    replicate(&mut d, &blocked, &mut follower);
    d.apply_committed_all().unwrap();
    let query = DirQuery::NodesForKey { key };
    assert!(d.query(&query).is_some());
    let receipt = d.propose(&DirCmd::Remove { node: node.clone() }).unwrap();
    assert_eq!(receipt.positions().len(), 4);
    assert_eq!(d.proposal_status(&receipt), Status::Pending);
    d.apply_committed_all().unwrap();
    assert!(d
        .query(&DirQuery::Resolve {
            prefix_or_node: node
        })
        .is_none());
    assert!(
        d.query(&query).is_some(),
        "uncommitted shard must not mutate its business state"
    );
    assert_eq!(d.proposal_status(&receipt), Status::Pending);
    replicate(&mut d, &blocked, &mut follower);
    assert_eq!(d.proposal_status(&receipt), Status::Committed);
    assert!(d.query(&query).is_some(), "commit is not application");
    d.apply_committed_all().unwrap();
    assert!(
        d.query(&query).is_none(),
        "last member removal cleans the empty service record"
    );
    let saved = d.shards.remove(&blocked).unwrap();
    assert_eq!(d.proposal_status(&receipt), Status::Unavailable);
    d.shards.insert(blocked, saved);
    assert_eq!(d.proposal_status(&receipt), Status::Committed);
}

#[test]
fn overwritten_log_position_does_not_complete_original_receipt() {
    use parrot_remote::{
        raft::{LogEntry, RaftRpc},
        roles::directory::DirectoryProposalStatus as Status,
    };
    let mut d = directory();
    let id = d.shards.keys().next().unwrap().clone();
    let node = node_on(&d, &id);
    let _follower = replicated_group(&mut d, &id);
    let original = DirCmd::Upsert {
        node: node.clone(),
        endpoints: vec!["original".into()],
    };
    let receipt = d.propose(&original).unwrap();
    assert_eq!(d.proposal_status(&receipt), Status::Pending);
    let replacement = bincode::serde::encode_to_vec(
        DirCmd::Upsert {
            node,
            endpoints: vec!["replacement".into()],
        },
        bincode::config::standard(),
    )
    .unwrap();
    let raft = &mut d.shards.get_mut(&id).unwrap().raft;
    let peer = raft.peers[0].clone();
    raft.step(
        peer.clone(),
        RaftRpc::AppendEntries {
            term: 2,
            leader: peer,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![LogEntry {
                term: 2,
                index: 1,
                cmd: replacement,
            }],
            leader_commit: 1,
        },
    );
    assert_eq!(raft.commit_index, 1);
    assert_eq!(d.proposal_status(&receipt), Status::Conflict);
}

#[test]
fn truncated_participant_makes_the_entire_receipt_conflict() {
    use parrot_remote::{
        raft::{LogEntry, RaftRpc},
        roles::directory::DirectoryProposalStatus as Status,
    };
    let mut d = directory();
    let id = d.shards.keys().min().unwrap().clone();
    let _follower = replicated_group(&mut d, &id);
    let command = DirCmd::Remove {
        node: "discarded".into(),
    };
    d.propose(&command).unwrap();
    let receipt = d.propose(&command).unwrap();
    assert_eq!(d.proposal_status(&receipt), Status::Pending);
    // All other groups have committed. One replaced participant is sufficient
    // for Conflict; multi-group acceptance must not hide a discarded suffix.
    let raft = &mut d.shards.get_mut(&id).unwrap().raft;
    let peer = raft.peers[0].clone();
    raft.step(
        peer.clone(),
        RaftRpc::AppendEntries {
            term: 2,
            leader: peer,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![LogEntry {
                term: 2,
                index: 1,
                cmd: DirCmd::Remove {
                    node: "replacement".into(),
                }
                .encode()
                .unwrap(),
            }],
            leader_commit: 1,
        },
    );
    assert_eq!(raft.log.len(), 1);
    assert_eq!(d.proposal_status(&receipt), Status::Conflict);
}

#[test]
fn receipt_is_scoped_to_its_directory_instance_even_with_identical_logs() {
    use parrot_remote::roles::directory::DirectoryProposalStatus as Status;
    let mut first = directory();
    let mut second = directory();
    let cmd = DirCmd::Remove {
        node: "same".into(),
    };
    let one = first.propose(&cmd).unwrap();
    let two = second.propose(&cmd).unwrap();
    assert_eq!(one.positions(), two.positions());
    assert_eq!(first.proposal_status(&one), Status::Committed);
    assert_eq!(second.proposal_status(&two), Status::Committed);
    assert_eq!(second.proposal_status(&one), Status::Unavailable);
    assert_eq!(first.proposal_status(&two), Status::Unavailable);
}

#[test]
fn foreign_shard_records_cannot_supply_authoritative_lookup_results() {
    let mut d = directory();
    let prefix = "parrot://foreign/";
    let key = "service";
    let border_owner = d.route(&format!("border:{prefix}")).unwrap().to_string();
    let key_owner = d.route(&format!("key:{key}")).unwrap().to_string();
    for (id, s) in &mut d.shards {
        if id != &border_owner {
            s.sm.borders.insert(prefix.into(), "wrong".into());
        }
        if id != &key_owner {
            s.sm.key_index.insert(key.into(), vec!["wrong".into()]);
        }
    }
    assert!(d
        .query(&DirQuery::BorderFor {
            prefix: prefix.into()
        })
        .is_none());
    assert!(d
        .query(&DirQuery::NodesForKey { key: key.into() })
        .is_none());
}

#[test]
fn checked_queries_distinguish_absence_unavailable_shards_and_version_exhaustion() {
    use parrot_remote::roles::directory::DirectoryReadError as Error;
    let query = DirQuery::BorderFor {
        prefix: "parrot://region/leaf".into(),
    };
    let empty = DirectoryShards::new(0, Arc::new(ManualClock::new()));
    assert_eq!(empty.query_checked(&query), Err(Error::NoShards));
    let mut d = directory();
    assert_eq!(d.query_checked(&query), Ok(None));
    write(
        &mut d,
        DirCmd::BorderDeclare {
            prefix: "parrot://region/".into(),
            node: "border".into(),
        },
    );
    assert!(d.query_checked(&query).unwrap().is_some());
    let ids: Vec<_> = d.shards.keys().cloned().collect();
    let saved = d.shards.remove(&ids[0]).unwrap();
    assert_eq!(
        d.query_checked(&query),
        Err(Error::Unavailable(ids[0].clone()))
    );
    assert!(d.query(&query).is_none()); // Convenience view also fails closed.
    let node = node_on(&d, &ids[0]);
    assert_eq!(
        d.query_checked(&DirQuery::Resolve {
            prefix_or_node: node
        }),
        Err(Error::Unavailable(ids[0].clone()))
    );
    d.shards.insert(ids[0].clone(), saved);
    d.shards.get_mut(&ids[0]).unwrap().sm.version = u64::MAX;
    d.shards.get_mut(&ids[1]).unwrap().sm.version = 1;
    assert_eq!(d.query_checked(&query), Err(Error::VersionOverflow));
}
