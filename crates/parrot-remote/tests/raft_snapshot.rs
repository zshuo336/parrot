use parrot_remote::raft::{
    DurableRaft, LogEntry, ManualClock, RaftNode, RaftRpc, Role, SnapshotError,
};
use std::sync::Arc;

const CHUNK: usize = 64 * 1024;

struct Net {
    clock: Arc<ManualClock>,
    nodes: Vec<RaftNode>,
    offline: bool,
}
impl Net {
    fn new() -> Self {
        let clock = Arc::new(ManualClock::new());
        let nodes = (0..3)
            .map(|i| {
                RaftNode::new(
                    name(i),
                    (0..3).filter(|j| *j != i).map(name).collect(),
                    clock.clone(),
                )
            })
            .collect();
        let mut net = Self {
            clock,
            nodes,
            offline: true,
        };
        net.clock.advance(1400);
        net.nodes[0].tick();
        net.pump();
        assert_eq!(net.nodes[0].role, Role::Leader);
        for value in 1u64..=8 {
            net.nodes[0].propose(value.to_le_bytes().to_vec()).unwrap();
            net.pump();
        }
        assert_eq!(drain(&mut net.nodes[0]), (1..=8).collect::<Vec<_>>());
        assert_eq!(drain(&mut net.nodes[1]), (1..=8).collect::<Vec<_>>());
        assert_eq!(net.nodes[2].commit_index, 0);
        net
    }
    fn pump(&mut self) {
        for _ in 0..10_000 {
            let messages: Vec<_> = self
                .nodes
                .iter_mut()
                .enumerate()
                .flat_map(|(from, node)| {
                    node.outbox()
                        .into_iter()
                        .map(move |(to, rpc)| (from, to, rpc))
                })
                .collect();
            if messages.is_empty() {
                return;
            }
            for (from, to, rpc) in messages {
                let to = (to.as_bytes()[0] - b'a') as usize;
                if self.offline && (from == 2 || to == 2) {
                    continue;
                }
                self.nodes[to].step(name(from), rpc);
            }
        }
        panic!("snapshot protocol produced an unbounded response loop");
    }
    fn compact(&mut self, index: u64, size: usize) -> Vec<u8> {
        let state: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
        self.nodes[0].compact(index, state.clone()).unwrap();
        state
    }
    fn first_chunk(&mut self) -> RaftRpc {
        self.clock.advance(200);
        self.nodes[0].tick();
        self.nodes[0]
            .outbox()
            .into_iter()
            .find_map(|(to, rpc)| {
                (to == "c" && matches!(&rpc, RaftRpc::InstallSnapshot { .. })).then_some(rpc)
            })
            .unwrap()
    }
    fn receive_chunk(&mut self, chunk: RaftRpc) -> RaftRpc {
        self.nodes[2].step("a".into(), chunk);
        let messages = self.nodes[2].outbox();
        assert_eq!(messages.len(), 1);
        messages.into_iter().next().unwrap().1
    }
    fn next_chunk(&mut self, response: RaftRpc) -> RaftRpc {
        self.nodes[0].step("c".into(), response);
        self.nodes[0]
            .outbox()
            .into_iter()
            .find_map(|(to, rpc)| {
                (to == "c" && matches!(&rpc, RaftRpc::InstallSnapshot { .. })).then_some(rpc)
            })
            .unwrap()
    }
}
fn name(i: usize) -> String {
    ((b'a' + i as u8) as char).to_string()
}
fn drain(node: &mut RaftNode) -> Vec<u64> {
    node.committed()
        .into_iter()
        .map(|entry| u64::from_le_bytes(entry.cmd.try_into().unwrap()))
        .collect()
}
fn offset(rpc: &RaftRpc) -> u64 {
    match rpc {
        RaftRpc::InstallSnapshot { offset, .. } => *offset,
        _ => panic!("expected snapshot chunk"),
    }
}
fn response_offset(rpc: &RaftRpc) -> u64 {
    match rpc {
        RaftRpc::InstallSnapshotResp { next_offset, .. } => *next_offset,
        _ => panic!("expected snapshot response"),
    }
}

#[test]
fn compaction_preserves_absolute_positions_and_requires_application() {
    let clock = Arc::new(ManualClock::new());
    let mut node = RaftNode::new("a", vec![], clock.clone());
    clock.advance(1400);
    node.tick();
    for value in 1u64..=8 {
        node.propose(value.to_le_bytes().to_vec()).unwrap();
    }
    assert_eq!(node.compact(6, vec![1]), Err(SnapshotError::NotApplied));
    assert_eq!(node.snapshot_index(), 0);
    assert_eq!(drain(&mut node), (1..=8).collect::<Vec<_>>());
    for index in [0, 9] {
        assert_eq!(node.compact(index, vec![]), Err(SnapshotError::NotApplied));
    }
    assert_eq!(
        node.compact(6, vec![0; 16 * 1024 * 1024 + 1]),
        Err(SnapshotError::Capacity)
    );
    node.compact(6, b"state-six".to_vec()).unwrap();
    assert_eq!(node.snapshot_index(), 6);
    assert_eq!(node.snapshot_term(), 1);
    assert_eq!(node.last_log_index(), 8);
    assert_eq!(
        node.log.iter().map(|entry| entry.index).collect::<Vec<_>>(),
        [7, 8]
    );
    assert_eq!(node.compact(6, vec![]), Err(SnapshotError::NotApplied));
    assert_eq!(node.propose(9u64.to_le_bytes().to_vec()).unwrap(), 9);
    assert_eq!(drain(&mut node), [9]);
    node.compact(9, b"state-nine".to_vec()).unwrap();
    assert!(node.log.is_empty());
    assert_eq!(node.last_log_index(), 9);
    assert_eq!(node.last_log_term(), 1);
    assert_eq!(node.propose(10u64.to_le_bytes().to_vec()).unwrap(), 10);
    assert_eq!(drain(&mut node), [10]);
}

#[test]
fn lagging_voter_installs_chunks_before_applying_the_retained_tail() {
    let mut net = Net::new();
    let state = net.compact(6, 3 * CHUNK + 17);
    net.offline = false;
    net.clock.advance(200);
    net.nodes[0].tick();
    net.pump();
    let follower = &mut net.nodes[2];
    assert_eq!(follower.pending_snapshot().unwrap().data, state);
    assert_eq!(follower.snapshot_index(), 6);
    assert_eq!(follower.commit_index, 8);
    assert!(
        follower.committed().is_empty(),
        "tail cannot escape before the snapshot is applied"
    );
    assert_eq!(follower.snapshot_applied(5, 1), Err(SnapshotError::Invalid));
    assert_eq!(follower.snapshot_applied(6, 2), Err(SnapshotError::Invalid));
    follower.snapshot_applied(6, 1).unwrap();
    assert_eq!(drain(follower), [7, 8]);
    assert_eq!(follower.snapshot_applied(6, 1), Err(SnapshotError::Invalid));
    assert_eq!(
        net.nodes[0].propose(9u64.to_le_bytes().to_vec()).unwrap(),
        9
    );
    net.pump();
    assert_eq!(drain(&mut net.nodes[2]), [9]);
    assert_eq!(net.nodes[2].last_log_index(), 9);
}

#[test]
fn lost_and_duplicate_chunks_retry_the_same_bytes_without_early_installation() {
    let mut net = Net::new();
    let state = net.compact(6, 2 * CHUNK + 7);
    let first = net.first_chunk();
    let first_ack = net.receive_chunk(first.clone());
    assert_eq!(response_offset(&first_ack), CHUNK as u64);
    assert_eq!(
        net.nodes[2].commit_index, 0,
        "partial state cannot become committed"
    );
    assert!(net.nodes[2].pending_snapshot().is_none());
    assert_eq!(
        net.first_chunk(),
        first,
        "lost snapshot acknowledgments must retry identical chunks"
    );
    assert_eq!(net.receive_chunk(first.clone()), first_ack);
    let second = net.next_chunk(first_ack.clone());
    assert_eq!(offset(&second), CHUNK as u64);
    let second_ack = net.receive_chunk(second.clone());
    assert_eq!(
        net.receive_chunk(first),
        first_ack,
        "duplicate acknowledgments name only the duplicated chunk"
    );
    let third = net.next_chunk(second_ack);
    assert_eq!(offset(&third), 2 * CHUNK as u64);
    let final_ack = net.receive_chunk(third);
    assert!(matches!(
        &final_ack,
        RaftRpc::InstallSnapshotResp {
            installed: true,
            ..
        }
    ));
    assert_eq!(net.nodes[2].pending_snapshot().unwrap().data, state);
    net.nodes[0].step("c".into(), final_ack);
    let tail = net.nodes[0].outbox();
    assert!(tail.iter().any(|(to, rpc)| to == "c"
        && matches!(
            rpc,
            RaftRpc::AppendEntries {
                prev_log_index: 6,
                ..
            }
        )));
}

#[test]
fn an_old_reset_response_can_restart_a_transfer_already_installed_at_the_follower() {
    let mut net = Net::new();
    net.compact(6, 2 * CHUNK + 7);
    let first = net.first_chunk();
    let mut out_of_order = first.clone();
    if let RaftRpc::InstallSnapshot { offset, .. } = &mut out_of_order {
        *offset = CHUNK as u64;
    }
    let reset = net.receive_chunk(out_of_order);
    assert_eq!(response_offset(&reset), 0);
    let first_ack = net.receive_chunk(first.clone());
    let second = net.next_chunk(first_ack);
    let second_ack = net.receive_chunk(second);
    let last = net.next_chunk(second_ack);
    let _lost_final_ack = net.receive_chunk(last);
    net.nodes[0].step("c".into(), reset);
    assert_eq!(net.first_chunk(), first);
    let cached = net.receive_chunk(first);
    assert!(matches!(
        &cached,
        RaftRpc::InstallSnapshotResp {
            installed: true,
            ..
        }
    ));
    net.nodes[0].step("c".into(), cached);
    assert!(
        net.nodes[0].outbox().iter().any(|(to, rpc)| to == "c"
            && matches!(
                rpc,
                RaftRpc::AppendEntries {
                    prev_log_index: 6,
                    ..
                }
            )),
        "a matching cached installation must release the sender to its retained log"
    );
}

#[test]
fn replies_must_match_member_term_snapshot_identity_and_sent_progress() {
    let mut net = Net::new();
    net.compact(6, 3 * CHUNK);
    let first = net.first_chunk();
    let correct = net.receive_chunk(first.clone());
    let term = net.nodes[0].current_term;
    for which in 0..5 {
        let mut reply = correct.clone();
        if let RaftRpc::InstallSnapshotResp {
            term,
            index,
            digest,
            next_offset,
            ..
        } = &mut reply
        {
            match which {
                0 => *term = 0,
                1 => *index += 1,
                2 => digest[0] ^= 1,
                3 => *next_offset += CHUNK as u64,
                _ => *term += 10,
            }
        }
        net.nodes[0].step(if which == 4 { "outsider" } else { "c" }.into(), reply);
        assert_eq!(net.nodes[0].current_term, term);
        assert!(
            net.nodes[0].outbox().is_empty(),
            "unmatched snapshot replies must not advance transfer progress"
        );
    }
    assert_eq!(net.first_chunk(), first);
    assert_eq!(offset(&net.next_chunk(correct)), CHUNK as u64);
}

#[test]
fn stale_snapshot_cannot_replace_an_already_committed_prefix() {
    let mut net = Net::new();
    net.compact(6, 2 * CHUNK);
    let first = net.first_chunk();
    net.nodes[2].step(
        "a".into(),
        RaftRpc::AppendEntries {
            term: 1,
            leader: "a".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: (1u64..=8)
                .map(|index| LogEntry {
                    term: 1,
                    index,
                    cmd: index.to_le_bytes().to_vec(),
                })
                .collect(),
            leader_commit: 8,
        },
    );
    net.nodes[2].outbox();
    assert_eq!(drain(&mut net.nodes[2]), (1..=8).collect::<Vec<_>>());
    let response = net.receive_chunk(first);
    assert!(matches!(
        &response,
        RaftRpc::InstallSnapshotResp {
            installed: true,
            ..
        }
    ));
    assert_eq!(net.nodes[2].snapshot_index(), 0);
    assert!(net.nodes[2].pending_snapshot().is_none());
    assert_eq!(net.nodes[2].commit_index, 8);
    assert_eq!(net.nodes[2].log.len(), 8);
    assert!(net.nodes[2].committed().is_empty());
}

#[test]
fn a_new_snapshot_does_not_mix_bytes_with_an_older_partial_transfer() {
    let mut net = Net::new();
    net.compact(6, 2 * CHUNK + 17);
    let old = net.first_chunk();
    let old_ack = net.receive_chunk(old);
    let old_second = net.next_chunk(old_ack);
    let current = net.compact(8, 3 * CHUNK + 19);
    let first = net.first_chunk();
    let first_ack = net.receive_chunk(first);
    assert_eq!(response_offset(&net.receive_chunk(old_second)), 0);
    let mut next = net.next_chunk(first_ack);
    loop {
        let response = net.receive_chunk(next);
        if matches!(
            &response,
            RaftRpc::InstallSnapshotResp {
                installed: true,
                ..
            }
        ) {
            break;
        }
        next = net.next_chunk(response);
    }
    assert_eq!(net.nodes[2].pending_snapshot().unwrap().data, current);
    assert_eq!(net.nodes[2].snapshot_index(), 8);
}

#[test]
fn corrupt_or_inconsistent_chunks_never_install_application_state() {
    for duplicate in [false, true] {
        let mut net = Net::new();
        net.compact(6, CHUNK + 7);
        let first = net.first_chunk();
        let first_ack = net.receive_chunk(first.clone());
        let mut corrupt = if duplicate {
            first
        } else {
            net.next_chunk(first_ack)
        };
        if let RaftRpc::InstallSnapshot { data, .. } = &mut corrupt {
            data[0] ^= 1;
        }
        let rejected = net.receive_chunk(corrupt);
        assert!(matches!(
            &rejected,
            RaftRpc::InstallSnapshotResp {
                next_offset: 0,
                installed: false,
                ..
            }
        ));
        assert_eq!(net.nodes[2].commit_index, 0);
        assert_eq!(net.nodes[2].snapshot_index(), 0);
        assert!(net.nodes[2].pending_snapshot().is_none());
    }
}

#[test]
fn malformed_chunks_and_forged_leaders_cannot_change_a_fresh_voter() {
    let mut net = Net::new();
    net.compact(6, CHUNK);
    let first = net.first_chunk();
    for which in 0..10 {
        let mut invalid = first.clone();
        if let RaftRpc::InstallSnapshot {
            term,
            leader,
            index,
            snapshot_term,
            offset,
            total,
            data,
            ..
        } = &mut invalid
        {
            match which {
                0 => *term = 0,
                1 => *index = 0,
                2 => *snapshot_term = 0,
                3 => *snapshot_term = *term + 1,
                4 => *offset = u64::MAX,
                5 => *total = 16 * 1024 * 1024 + 1,
                6 => data.push(0),
                7 => data.clear(),
                8 => *leader = "b".into(),
                _ => *offset = 1,
            }
        }
        net.nodes[2].step("a".into(), invalid);
        assert_eq!(net.nodes[2].current_term, 0);
        assert_eq!(net.nodes[2].commit_index, 0);
        assert!(net.nodes[2].outbox().is_empty());
    }
}

#[test]
fn higher_term_reply_revokes_leadership_and_queued_snapshot_chunks() {
    let mut net = Net::new();
    net.compact(6, 3 * CHUNK);
    let first = net.first_chunk();
    let mut response = net.receive_chunk(first);
    if let RaftRpc::InstallSnapshotResp { term, .. } = &mut response {
        *term += 1;
    }
    net.nodes[0].step("c".into(), response);
    assert_eq!(net.nodes[0].role, Role::Follower);
    assert_eq!(net.nodes[0].current_term, 2);
    assert!(net.nodes[0].outbox().is_empty());
}

#[test]
fn empty_snapshot_and_matching_uncommitted_suffix_remain_recoverable() {
    let mut net = Net::new();
    net.compact(6, 0);
    let first = net.first_chunk();
    net.nodes[2].step(
        "a".into(),
        RaftRpc::AppendEntries {
            term: 1,
            leader: "a".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: (1u64..=9)
                .map(|index| LogEntry {
                    term: 1,
                    index,
                    cmd: index.to_le_bytes().to_vec(),
                })
                .collect(),
            leader_commit: 2,
        },
    );
    net.nodes[2].outbox();
    net.receive_chunk(first);
    assert_eq!(
        net.nodes[2]
            .log
            .iter()
            .map(|entry| entry.index)
            .collect::<Vec<_>>(),
        [7, 8, 9]
    );
    assert_eq!(net.nodes[2].commit_index, 6);
    assert!(net.nodes[2].pending_snapshot().unwrap().data.is_empty());
    assert!(net.nodes[2].committed().is_empty());
    net.nodes[2].snapshot_applied(6, 1).unwrap();
    assert!(
        net.nodes[2].committed().is_empty(),
        "uncommitted suffix must not be applied by installation"
    );
}

#[test]
fn snapshot_only_log_participates_in_freshness_checks_and_absolute_elections() {
    let mut net = Net::new();
    net.compact(8, 17);
    let first = net.first_chunk();
    net.receive_chunk(first);
    net.nodes[2].snapshot_applied(8, 1).unwrap();
    net.clock.advance(1400);
    net.nodes[2].tick();
    let votes = net.nodes[2].outbox();
    assert_eq!(votes.len(), 2);
    assert!(votes.iter().all(|(_, rpc)| matches!(
        rpc,
        RaftRpc::RequestVote {
            term: 2,
            last_log_index: 8,
            last_log_term: 1,
            ..
        }
    )));
    net.nodes[2].step(
        "b".into(),
        RaftRpc::RequestVoteResp {
            term: 2,
            vote_granted: true,
        },
    );
    net.nodes[2].outbox();
    assert_eq!(
        net.nodes[2].propose(9u64.to_le_bytes().to_vec()).unwrap(),
        9
    );
    assert_eq!(net.nodes[2].log[0].index, 9);
    assert_eq!(net.nodes[2].log[0].term, 2);
}

#[test]
fn durable_hosts_publish_installation_before_exposing_the_acknowledgment() {
    let data = tempfile::tempdir().unwrap();
    let mut owner = DurableRaft::open(
        data.path(),
        "snapshot-gate",
        "c",
        vec!["a".into(), "b".into()],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    let mut net = Net::new();
    let state = net.compact(6, 17);
    let first = net.first_chunk();
    owner.step("a".into(), first).unwrap();
    assert_eq!(owner.commit_index(), 6);
    assert!(owner.committed().unwrap().is_empty());
    assert!(matches!(
        owner.outbox().unwrap().as_slice(),
        [(
            _,
            RaftRpc::InstallSnapshotResp {
                installed: true,
                ..
            }
        )]
    ));
    drop(owner);
    let mut reopened = DurableRaft::open(
        data.path(),
        "snapshot-gate",
        "c",
        vec!["a".into(), "b".into()],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    let snapshot = reopened.pending_snapshot().unwrap().unwrap();
    assert_eq!((snapshot.index, snapshot.term), (6, 1));
    assert_eq!(snapshot.data, state);
    assert!(reopened.committed().unwrap().is_empty());
    reopened.snapshot_applied(6, 1).unwrap();
    assert!(reopened.pending_snapshot().unwrap().is_none());
}

#[test]
fn absolute_index_exhaustion_never_wraps_or_loops_snapshot_acknowledgments() {
    use sha2::{Digest, Sha256};
    let clock = Arc::new(ManualClock::new());
    let mut node = RaftNode::new("c", vec!["a".into(), "b".into()], clock.clone());
    let base = u64::MAX - 1;
    let mut hash = Sha256::new();
    hash.update(b"parrot:raft:snapshot#v1");
    hash.update(base.to_le_bytes());
    hash.update(1u64.to_le_bytes());
    node.step(
        "a".into(),
        RaftRpc::InstallSnapshot {
            term: 1,
            leader: "a".into(),
            index: base,
            snapshot_term: 1,
            digest: hash.finalize().into(),
            offset: 0,
            total: 0,
            data: vec![],
        },
    );
    node.outbox();
    node.snapshot_applied(base, 1).unwrap();
    clock.advance(1400);
    node.tick();
    node.outbox();
    node.step(
        "b".into(),
        RaftRpc::RequestVoteResp {
            term: 2,
            vote_granted: true,
        },
    );
    node.outbox();
    assert_eq!(node.propose(9u64.to_le_bytes().to_vec()).unwrap(), u64::MAX);
    node.outbox();
    node.step(
        "b".into(),
        RaftRpc::AppendEntriesResp {
            term: 2,
            success: true,
            match_index: u64::MAX,
        },
    );
    assert_eq!(drain(&mut node), [9]);
    assert!(
        node.propose(vec![10]).is_err(),
        "exhausted absolute indices must not wrap"
    );
    node.compact(u64::MAX, vec![]).unwrap();
    node.outbox();
    clock.advance(200);
    node.tick();
    let messages = node.outbox();
    assert!(messages.iter().any(|(peer, rpc)| peer == "b" && matches!(rpc, RaftRpc::AppendEntries { prev_log_index: u64::MAX, entries, .. } if entries.is_empty())));
    let (_, snapshot) = messages.into_iter().find(|(peer, _)| peer == "a").unwrap();
    let RaftRpc::InstallSnapshot { index, digest, .. } = snapshot else {
        panic!("expected terminal snapshot");
    };
    node.step(
        "a".into(),
        RaftRpc::InstallSnapshotResp {
            term: 2,
            index,
            digest,
            next_offset: 0,
            installed: true,
        },
    );
    let messages = node.outbox();
    assert!(!messages.is_empty());
    assert!(messages.iter().all(|(_, rpc)| matches!(rpc, RaftRpc::AppendEntries { prev_log_index: u64::MAX, entries, .. } if entries.is_empty())),
        "installed terminal index must produce a finite heartbeat, not another snapshot");
}
