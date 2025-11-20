use parrot_remote::{
    raft::{ManualClock, RaftRpc, Role},
    roles::directory::{
        directory_command_digest,
        ownership::{OwnershipCommand, OwnershipOutcome, OwnershipStatus},
        DirCmd, DirQuery, DirectoryRequestStatus, DurableDirectory, DurableDirectoryStatus,
    },
};
use std::sync::Arc;

fn put(node: &str, endpoint: &str) -> DirCmd {
    DirCmd::Upsert {
        node: node.into(),
        endpoints: vec![endpoint.into()],
    }
}
fn query(node: &str) -> DirQuery {
    DirQuery::Resolve {
        prefix_or_node: node.into(),
    }
}
fn open(path: &std::path::Path, clock: &Arc<ManualClock>) -> DurableDirectory {
    DurableDirectory::open(path, "directory-snapshots", "a", vec![], clock.clone()).unwrap()
}
fn single(path: &std::path::Path) -> (Arc<ManualClock>, DurableDirectory) {
    let clock = Arc::new(ManualClock::new());
    let mut node = open(path, &clock);
    clock.advance(1400);
    node.tick().unwrap();
    assert_eq!(node.role(), Role::Leader);
    (clock, node)
}
fn applied(status: &OwnershipStatus) -> (u64, &OwnershipOutcome) {
    let OwnershipStatus::Applied { index, outcome, .. } = status else {
        panic!("{status:?}");
    };
    (*index, outcome)
}

#[test]
fn checkpoint_preserves_business_views_persistent_ids_and_live_receipts() {
    let data = tempfile::tempdir().unwrap();
    let (clock, mut node) = single(data.path());
    let command = put("worker", "mem://worker");
    let digest = directory_command_digest(&command).unwrap();
    let status = node.submit_request("client", "upsert", &command).unwrap();
    let receipt = node
        .propose(&DirCmd::BorderDeclare {
            prefix: "/jobs".into(),
            node: "worker".into(),
        })
        .unwrap();
    node.propose(&DirCmd::KeyAggregate {
        key: "service".into(),
        nodes: vec!["worker".into()],
    })
    .unwrap();
    let before = node.state_local().unwrap().clone();
    assert_eq!(node.compact().unwrap(), 3);
    assert_eq!(node.compact().unwrap(), 3);
    assert_eq!(
        node.proposal_status(&receipt).unwrap(),
        DurableDirectoryStatus::Applied
    );
    assert_eq!(
        node.submit_request("client", "upsert", &command).unwrap(),
        status
    );
    assert_eq!(
        node.submit_request("client", "upsert", &put("worker", "different"))
            .unwrap(),
        DirectoryRequestStatus::Conflict
    );
    assert_eq!(node.applied_index(), 3);
    let disk = std::fs::read(data.path().join("raft.state")).unwrap();
    for _ in 0..50 {
        let mut read = node.begin_linear_read(&query("worker")).unwrap();
        let result = node.poll_linear_read(&mut read).unwrap().unwrap();
        assert_eq!(result.entry, before.query(&query("worker")));
        assert_eq!(result.barrier_index, 3);
    }
    assert_eq!(
        std::fs::read(data.path().join("raft.state")).unwrap(),
        disk,
        "compaction must not restore per-read disk writes"
    );
    drop(node);
    let mut node = open(data.path(), &clock);
    assert_eq!(node.snapshot_index(), 3);
    assert_eq!(node.applied_index(), 3);
    assert_eq!(node.state_local().unwrap(), &before);
    assert_eq!(
        node.request_status("client", "upsert", digest).unwrap(),
        status
    );
    assert_eq!(
        node.proposal_status(&receipt).unwrap(),
        DurableDirectoryStatus::Foreign
    );
    clock.advance(1400);
    node.tick().unwrap();
    let next = node.propose(&put("next", "mem://next")).unwrap();
    assert_eq!(next.index(), 4);
    assert_eq!(
        node.query_local(&query("next")).unwrap().unwrap().version,
        4
    );
    node.compact().unwrap();
    drop(node);
    let node = open(data.path(), &clock);
    assert_eq!(node.applied_index(), 4);
    assert_eq!(node.state_local().unwrap().version, 4);
}

#[test]
fn ownership_generations_values_and_historical_outcomes_survive_repeated_compaction() {
    let data = tempfile::tempdir().unwrap();
    let (clock, mut node) = single(data.path());
    let claim = OwnershipCommand::Claim {
        resource: "resource".into(),
        expected: None,
    };
    let claimed = node.submit_ownership("holder", "claim", &claim).unwrap();
    let generation = applied(&claimed).0;
    let write = OwnershipCommand::WriteValue {
        resource: "resource".into(),
        generation,
        expected_revision: None,
        value: vec![42; 4096],
    };
    let written = node.submit_ownership("holder", "write", &write).unwrap();
    let revision = applied(&written).0;
    let read = OwnershipCommand::ReadValue {
        resource: "resource".into(),
    };
    let historical = node.submit_ownership("observer", "read", &read).unwrap();
    let release = OwnershipCommand::Release {
        resource: "resource".into(),
        expected: generation,
    };
    let released = node
        .submit_ownership("holder", "release", &release)
        .unwrap();
    let released_generation = applied(&released).0;
    let conflict = node
        .submit_ownership("holder", "stale-write", &write)
        .unwrap();
    assert!(matches!(
        applied(&conflict).1,
        OwnershipOutcome::Rejected { .. }
    ));
    let next_claim = OwnershipCommand::Claim {
        resource: "resource".into(),
        expected: Some(released_generation),
    };
    let replacement = node
        .submit_ownership("replacement", "claim", &next_claim)
        .unwrap();
    let replacement_generation = applied(&replacement).0;
    let next_write = OwnershipCommand::WriteValue {
        resource: "resource".into(),
        generation: replacement_generation,
        expected_revision: Some(revision),
        value: b"new value".to_vec(),
    };
    let overwritten = node
        .submit_ownership("replacement", "write", &next_write)
        .unwrap();
    let saved = [
        ("holder", "claim", claim, claimed),
        ("holder", "write", write, written),
        ("observer", "read", read.clone(), historical.clone()),
        ("holder", "release", release, released),
        ("replacement", "claim", next_claim, replacement),
        ("replacement", "write", next_write, overwritten),
    ];
    let end = node.applied_index();
    node.compact().unwrap();
    drop(node);
    let mut node = open(data.path(), &clock);
    clock.advance(1400);
    node.tick().unwrap();
    for (client, id, command, status) in &saved {
        assert_eq!(
            node.ownership_status(client, id, command.digest().unwrap())
                .unwrap(),
            *status
        );
        assert_eq!(node.submit_ownership(client, id, command).unwrap(), *status);
    }
    assert_eq!(
        node.applied_index(),
        end,
        "retries of old IDs must not reapply after recovery"
    );
    assert_eq!(
        node.submit_ownership(
            "observer",
            "read",
            &OwnershipCommand::Observe {
                resource: "resource".into()
            }
        )
        .unwrap(),
        OwnershipStatus::Conflict
    );
    let current = node
        .submit_ownership("observer", "fresh-read", &read)
        .unwrap();
    let OwnershipOutcome::ValueObserved {
        ownership: Some(record),
        value: Some(value),
    } = applied(&current).1
    else {
        panic!("{current:?}");
    };
    assert_eq!(record.generation, replacement_generation);
    assert!(value.revision > revision);
    assert_eq!(value.value, b"new value"[..]);
    assert_ne!(current, historical);
    node.compact().unwrap();
    drop(node);
    let node = open(data.path(), &clock);
    assert_eq!(
        node.ownership_status("observer", "read", read.digest().unwrap())
            .unwrap(),
        historical
    );
    assert_eq!(
        node.ownership_status("holder", "stale-write", saved[1].2.digest().unwrap())
            .unwrap(),
        conflict
    );
}

#[test]
fn thousands_of_historical_reads_share_one_persisted_value_revision() {
    let data = tempfile::tempdir().unwrap();
    let (clock, mut node) = single(data.path());
    node.submit_ownership(
        "holder",
        "claim",
        &OwnershipCommand::Claim {
            resource: "resource".into(),
            expected: None,
        },
    )
    .unwrap();
    node.submit_ownership(
        "holder",
        "write",
        &OwnershipCommand::WriteValue {
            resource: "resource".into(),
            generation: 1,
            expected_revision: None,
            value: vec![7; 4096],
        },
    )
    .unwrap();
    let read = OwnershipCommand::ReadValue {
        resource: "resource".into(),
    };
    let mut saved = Vec::new();
    for i in 0..4200 {
        saved.push(
            node.submit_ownership("observer", &format!("read-{i}"), &read)
                .unwrap(),
        );
    }
    node.compact().unwrap();
    let size = std::fs::metadata(data.path().join("raft.state"))
        .unwrap()
        .len();
    assert!(
        size < 1024 * 1024,
        "historical values must be interned; actual checkpoint {size}"
    );
    drop(node);
    let node = open(data.path(), &clock);
    for (i, status) in saved.iter().enumerate() {
        assert_eq!(
            node.ownership_status("observer", &format!("read-{i}"), read.digest().unwrap())
                .unwrap(),
            *status
        );
    }
}

struct Group {
    dirs: Vec<tempfile::TempDir>,
    nodes: Vec<DurableDirectory>,
    clock: Arc<ManualClock>,
    offline: bool,
    snapshot_chunks: usize,
}
fn name(i: usize) -> String {
    ((b'a' + i as u8) as char).to_string()
}
impl Group {
    fn new() -> Self {
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let clock = Arc::new(ManualClock::new());
        let nodes = dirs
            .iter()
            .enumerate()
            .map(|(i, dir)| Self::open(dir.path(), i, &clock))
            .collect();
        let mut group = Self {
            dirs,
            nodes,
            clock,
            offline: true,
            snapshot_chunks: 0,
        };
        group.clock.advance(1400);
        group.nodes[0].tick().unwrap();
        group.pump();
        assert_eq!(group.nodes[0].role(), Role::Leader);
        group
    }
    fn open(path: &std::path::Path, i: usize, clock: &Arc<ManualClock>) -> DurableDirectory {
        DurableDirectory::open(
            path,
            "snapshot-group",
            name(i),
            (0..3).filter(|j| *j != i).map(name).collect(),
            clock.clone(),
        )
        .unwrap()
    }
    fn pump(&mut self) {
        for _ in 0..10_000 {
            let packets: Vec<_> = self
                .nodes
                .iter_mut()
                .enumerate()
                .flat_map(|(from, node)| {
                    node.outbox()
                        .unwrap()
                        .into_iter()
                        .map(move |(to, rpc)| (from, to, rpc))
                })
                .collect();
            if packets.is_empty() {
                return;
            }
            for (from, to, rpc) in packets {
                let to = (to.as_bytes()[0] - b'a') as usize;
                if self.offline && (from == 2 || to == 2) {
                    continue;
                }
                if let RaftRpc::InstallSnapshot { data, .. } = &rpc {
                    assert!(data.len() <= 64 * 1024);
                    self.snapshot_chunks += 1;
                }
                self.nodes[to].step(name(from), rpc).unwrap();
            }
        }
        panic!("snapshot transfer did not converge");
    }
    fn reopen(&mut self, i: usize) {
        drop(self.nodes.remove(i));
        self.nodes
            .insert(i, Self::open(self.dirs[i].path(), i, &self.clock));
    }
}

#[test]
fn offline_voter_installs_directory_checkpoint_tail_dedup_and_receipt_proof() {
    let mut group = Group::new();
    let big = DirCmd::Upsert {
        node: "big".into(),
        endpoints: vec!["x".repeat(400); 400],
    };
    let receipt = group.nodes[0].propose(&big).unwrap();
    group.pump();
    assert_eq!(
        group.nodes[0].proposal_status(&receipt).unwrap(),
        DurableDirectoryStatus::Applied
    );
    let request = put("request", "original");
    group.nodes[0]
        .submit_request("client", "stable", &request)
        .unwrap();
    group.pump();
    let claim = OwnershipCommand::Claim {
        resource: "resource".into(),
        expected: None,
    };
    group.nodes[0]
        .submit_ownership("holder", "claim", &claim)
        .unwrap();
    group.pump();
    let historical = group.nodes[0]
        .ownership_status("holder", "claim", claim.digest().unwrap())
        .unwrap();
    assert_eq!(group.nodes[0].compact().unwrap(), 3);
    group.nodes[0]
        .propose(&put("tail", "after-checkpoint"))
        .unwrap();
    group.pump();
    group.offline = false;
    group.clock.advance(200);
    group.nodes[0].tick().unwrap();
    group.pump();
    assert!(group.snapshot_chunks >= 3);
    assert_eq!(group.nodes[2].snapshot_index(), 3);
    for node in &group.nodes {
        assert_eq!(node.applied_index(), 4);
        assert_eq!(
            node.query_local(&query("big")).unwrap().unwrap().endpoints,
            vec!["x".repeat(400); 400]
        );
        assert_eq!(
            node.query_local(&query("tail")).unwrap().unwrap().version,
            3
        );
        assert!(matches!(
            node.request_status(
                "client",
                "stable",
                directory_command_digest(&request).unwrap()
            )
            .unwrap(),
            DirectoryRequestStatus::Applied { index: 2, .. }
        ));
        assert_eq!(
            node.ownership_status("holder", "claim", claim.digest().unwrap())
                .unwrap(),
            historical
        );
    }
    group.reopen(2);
    assert_eq!(group.nodes[2].applied_index(), 4);
    assert_eq!(
        group.nodes[2]
            .query_local(&query("tail"))
            .unwrap()
            .unwrap()
            .endpoints,
        ["after-checkpoint"]
    );
    assert_eq!(
        group.nodes[0].proposal_status(&receipt).unwrap(),
        DurableDirectoryStatus::Applied
    );
}

fn packet(index: u64, snapshot_term: u64, data: Vec<u8>) -> RaftRpc {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"parrot:raft:snapshot#v1");
    hash.update(index.to_le_bytes());
    hash.update(snapshot_term.to_le_bytes());
    hash.update(&data);
    RaftRpc::InstallSnapshot {
        term: 5,
        leader: "b".into(),
        index,
        snapshot_term,
        digest: hash.finalize().into(),
        offset: 0,
        total: data.len() as u64,
        data,
    }
}

#[test]
fn invalid_application_snapshot_is_rejected_before_adoption_and_recovery_cleanup() {
    let mut group = Group::new();
    group.nodes[0]
        .propose(&put("existing", "retained"))
        .unwrap();
    group.pump();
    assert!(group.nodes[0]
        .step("b".into(), packet(10, 4, b"opaque bytes".to_vec()))
        .is_err());
    assert!(!group.nodes[0].is_closed());
    assert_eq!(group.nodes[0].current_term(), 5);
    assert_eq!(group.nodes[0].snapshot_index(), 0);
    assert_eq!(group.nodes[0].applied_index(), 1);
    assert_eq!(
        group.nodes[0]
            .query_local(&query("existing"))
            .unwrap()
            .unwrap()
            .endpoints,
        ["retained"]
    );
    assert!(matches!(
        group.nodes[0].outbox().unwrap().as_slice(),
        [(
            _,
            RaftRpc::InstallSnapshotResp {
                installed: false,
                ..
            }
        )]
    ));
    group.reopen(0);
    assert_eq!(group.nodes[0].current_term(), 5);
    assert_eq!(group.nodes[0].applied_index(), 1);

    let data = tempfile::tempdir().unwrap();
    let (clock, mut node) = single(data.path());
    node.propose(&put("original", "value")).unwrap();
    drop(node);
    let mut raft = parrot_remote::raft::DurableRaft::open(
        data.path(),
        "directory-v1:directory-snapshots",
        "a",
        vec![],
        clock.clone(),
    )
    .unwrap();
    raft.committed().unwrap();
    raft.compact(1, b"invalid schema".to_vec()).unwrap();
    drop(raft);
    let mut before = std::fs::read(data.path().join("raft.state")).unwrap();
    before.push(0);
    std::fs::write(data.path().join("raft.state"), &before).unwrap();
    std::fs::write(data.path().join("raft.next"), b"preserve").unwrap();
    assert!(
        DurableDirectory::open(data.path(), "directory-snapshots", "a", vec![], clock).is_err()
    );
    assert_eq!(
        std::fs::read(data.path().join("raft.state")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(data.path().join("raft.next")).unwrap(),
        b"preserve"
    );
}

#[test]
fn received_checkpoint_cannot_invent_the_exact_fate_of_an_uncommitted_local_receipt() {
    let source = tempfile::tempdir().unwrap();
    let (clock, mut node) = single(source.path());
    node.propose(&put("winner", "one")).unwrap();
    node.propose(&put("winner", "two")).unwrap();
    node.compact().unwrap();
    drop(node);
    let raft = parrot_remote::raft::DurableRaft::open(
        source.path(),
        "directory-v1:directory-snapshots",
        "a",
        vec![],
        clock,
    )
    .unwrap();
    let snapshot = raft.snapshot().unwrap().clone();
    let mut group = Group::new();
    let receipt = group.nodes[0]
        .propose(&put("unconfirmed", "pending"))
        .unwrap();
    assert_eq!(
        group.nodes[0].proposal_status(&receipt).unwrap(),
        DurableDirectoryStatus::Pending
    );
    group.nodes[0]
        .step(
            "b".into(),
            packet(snapshot.index, snapshot.term, snapshot.data),
        )
        .unwrap();
    assert_eq!(
        group.nodes[0].proposal_status(&receipt).unwrap(),
        DurableDirectoryStatus::Compacted
    );
    assert_eq!(
        group.nodes[0].query_local(&query("unconfirmed")).unwrap(),
        None
    );
    assert_eq!(
        group.nodes[0]
            .query_local(&query("winner"))
            .unwrap()
            .unwrap()
            .endpoints,
        ["two"]
    );
}

#[test]
fn embedded_shard_receipts_address_the_retained_absolute_log() {
    use parrot_remote::roles::directory::{DirectoryProposalStatus, DirectoryShards};
    let clock = Arc::new(ManualClock::new());
    let mut shards = DirectoryShards::new(1, clock.clone());
    clock.advance(1400);
    for shard in shards.shards.values_mut() {
        shard.raft.tick();
    }
    let first = put("worker", "first");
    let old = shards.propose(&first).unwrap();
    assert_eq!(
        shards.proposal_status(&old),
        DirectoryProposalStatus::Committed
    );
    assert_eq!(shards.apply_committed_all().unwrap(), 1);
    // This embedded caller owns its single-command application snapshot codec.
    let raft = &mut shards.shards.get_mut("dir-s0").unwrap().raft;
    raft.compact(1, first.encode().unwrap()).unwrap();
    let next = shards.propose(&put("worker", "second")).unwrap();
    assert_eq!(next.positions()[0].index, 2);
    assert_eq!(
        shards.proposal_status(&next),
        DirectoryProposalStatus::Committed,
        "retained receipts must use an index relative to the compacted base"
    );
    assert_eq!(
        shards.proposal_status(&old),
        DirectoryProposalStatus::Unavailable,
        "an opaque embedded snapshot cannot prove an old command's identity"
    );
    assert_eq!(shards.apply_committed_all().unwrap(), 1);
    assert_eq!(
        shards.query(&query("worker")).unwrap().endpoints,
        ["second"]
    );
}

#[test]
fn embedded_replica_and_shards_require_explicit_snapshot_application() {
    use parrot_remote::{
        raft::{RaftNode, StateMachine},
        roles::directory::{DirectoryReplica, DirectoryShards},
    };
    let clock = Arc::new(ManualClock::new());
    let command = put("worker", "from-snapshot");
    let mut replica = DirectoryReplica::new("a", vec!["b".into()], clock.clone());
    replica
        .raft
        .step("b".into(), packet(1, 1, command.encode().unwrap()));
    assert!(
        replica.apply_committed().is_err(),
        "pending embedded snapshot must not look like a successful empty apply"
    );
    assert!(replica.raft.pending_snapshot().is_some());
    replica.sm.apply(&command.encode().unwrap()).unwrap();
    replica.raft.snapshot_applied(1, 1).unwrap();
    assert_eq!(replica.apply_committed().unwrap(), 0);
    assert_eq!(
        replica.query(&query("worker")).unwrap().endpoints,
        ["from-snapshot"]
    );

    let mut shards = DirectoryShards::new(2, clock.clone());
    clock.advance(1400);
    for shard in shards.shards.values_mut() {
        shard.raft.tick();
    }
    shards
        .shards
        .get_mut("dir-s0")
        .unwrap()
        .raft
        .propose(put("other", "pending").encode().unwrap())
        .unwrap();
    let shard = shards.shards.get_mut("dir-s1").unwrap();
    shard.raft = RaftNode::new("dir-s1", vec!["b".into()], clock);
    shard
        .raft
        .step("b".into(), packet(1, 1, command.encode().unwrap()));
    assert!(
        shards.apply_committed_all().is_err(),
        "one pending snapshot must reject the whole local preflight"
    );
    assert_eq!(shards.shards["dir-s0"].sm.version, 0);
    let shard = shards.shards.get_mut("dir-s1").unwrap();
    shard.sm.apply(&command.encode().unwrap()).unwrap();
    shard.raft.snapshot_applied(1, 1).unwrap();
    assert_eq!(shards.apply_committed_all().unwrap(), 1);
    assert_eq!(shards.shards["dir-s0"].sm.version, 1);
}

#[test]
fn exhausted_shard_rejects_admission_before_any_participant_appends() {
    use parrot_remote::{
        raft::{RaftNode, StateMachine},
        roles::directory::{DirectoryShards, DirectoryWriteError},
    };
    let clock = Arc::new(ManualClock::new());
    let mut shards = DirectoryShards::new(2, clock.clone());
    let command = put("worker", "from-snapshot");
    let exhausted = shards.shards.get_mut("dir-s1").unwrap();
    exhausted.raft = RaftNode::new("dir-s1", vec!["b".into()], clock.clone());
    exhausted
        .raft
        .step("b".into(), packet(u64::MAX, 1, command.encode().unwrap()));
    exhausted.sm.apply(&command.encode().unwrap()).unwrap();
    exhausted.raft.snapshot_applied(u64::MAX, 1).unwrap();
    clock.advance(1400);
    for shard in shards.shards.values_mut() {
        shard.raft.tick();
    }
    let exhausted = shards.shards.get_mut("dir-s1").unwrap();
    exhausted.raft.step(
        "b".into(),
        RaftRpc::RequestVoteResp {
            term: exhausted.raft.current_term,
            vote_granted: true,
        },
    );
    assert_eq!(exhausted.raft.role, Role::Leader);
    assert_eq!(exhausted.raft.last_log_index(), u64::MAX);
    assert_eq!(shards.shards["dir-s0"].raft.role, Role::Leader);
    assert_eq!(shards.shards["dir-s0"].raft.last_log_index(), 0);
    let result = shards.propose(&DirCmd::Remove {
        node: "worker".into(),
    });
    assert_eq!(
        shards.shards["dir-s0"].raft.last_log_index(),
        0,
        "admission failure must precede appending to the first participant"
    );
    assert!(matches!(result, Err(DirectoryWriteError::Unavailable(shard)) if shard == "dir-s1"));
    assert_eq!(shards.apply_committed_all().unwrap(), 0);
    assert_eq!(shards.shards["dir-s0"].sm.version, 0);
    assert_eq!(shards.shards["dir-s1"].sm.version, 1);
    assert!(shards.shards["dir-s1"].sm.query(&query("worker")).is_some());
}
