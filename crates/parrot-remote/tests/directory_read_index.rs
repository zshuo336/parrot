use parrot_remote::{
    raft::{DurableRaft, ManualClock, RaftRpc, RaftStorageError, Role},
    roles::directory::{DirCmd, DirQuery, DurableDirectory, DurableDirectoryError},
};
use std::sync::Arc;

struct Group {
    nodes: Vec<DurableDirectory>,
    clock: Arc<ManualClock>,
    _directories: Vec<tempfile::TempDir>,
}
fn name(i: usize) -> String {
    format!("n{i}")
}
fn query() -> DirQuery {
    DirQuery::Resolve {
        prefix_or_node: "worker".into(),
    }
}
impl Group {
    fn new(count: usize) -> Self {
        let clock = Arc::new(ManualClock::new());
        let directories: Vec<_> = (0..count).map(|_| tempfile::tempdir().unwrap()).collect();
        let nodes = directories
            .iter()
            .enumerate()
            .map(|(i, dir)| {
                DurableDirectory::open(
                    dir.path(),
                    "read-index",
                    name(i),
                    (0..count).filter(|j| *j != i).map(name).collect(),
                    clock.clone(),
                )
                .unwrap()
            })
            .collect();
        let mut group = Self {
            nodes,
            clock,
            _directories: directories,
        };
        group.clock.advance(1400);
        group.nodes[0].tick().unwrap();
        group.pump();
        assert_eq!(group.nodes[0].role(), Role::Leader);
        group.nodes[0]
            .propose(&DirCmd::Upsert {
                node: "worker".into(),
                endpoints: vec!["mem://worker".into()],
            })
            .unwrap();
        group.pump();
        group
    }
    fn pump(&mut self) {
        for _ in 0..50 {
            let mut messages = Vec::new();
            for (i, node) in self.nodes.iter_mut().enumerate() {
                for (target, rpc) in node.outbox().unwrap() {
                    messages.push((i, target, rpc));
                }
            }
            if messages.is_empty() {
                return;
            }
            for (from, target, rpc) in messages {
                let index = (0..self.nodes.len()).find(|i| name(*i) == target).unwrap();
                self.nodes[index].step(name(from), rpc).unwrap();
            }
        }
        panic!("model did not settle");
    }
    fn probes(&mut self) -> Vec<(String, RaftRpc)> {
        let messages = self.nodes[0].outbox().unwrap();
        assert!(!messages.is_empty());
        assert!(messages
            .iter()
            .all(|(_, rpc)| matches!(rpc, RaftRpc::ReadIndex { .. })));
        messages
    }
    fn response(&mut self, messages: &[(String, RaftRpc)], peer: usize) -> RaftRpc {
        let rpc = messages
            .iter()
            .find(|(target, _)| *target == name(peer))
            .unwrap()
            .1
            .clone();
        self.nodes[peer].step(name(0), rpc).unwrap();
        let messages = self.nodes[peer].outbox().unwrap();
        assert_eq!(messages.len(), 1);
        let (target, response) = messages.into_iter().next().unwrap();
        assert_eq!(target, name(0));
        assert!(matches!(response, RaftRpc::ReadIndexResp { .. }));
        response
    }
}

#[test]
fn fresh_context_cannot_reuse_a_prior_read_or_append_ack() {
    let mut group = Group::new(3);
    let mut first = group.nodes[0].begin_linear_read(&query()).unwrap();
    let probes = group.probes();
    let old = group.response(&probes, 1);
    group.nodes[0].step(name(1), old.clone()).unwrap();
    let first_result = group.nodes[0]
        .poll_linear_read(&mut first)
        .unwrap()
        .unwrap();
    let mut second = group.nodes[0].begin_linear_read(&query()).unwrap();
    let probes = group.probes();
    group.nodes[0].step(name(1), old.clone()).unwrap();
    group.nodes[0]
        .step(
            name(1),
            RaftRpc::AppendEntriesResp {
                term: first_result.term,
                success: true,
                match_index: first_result.barrier_index,
            },
        )
        .unwrap();
    assert!(
        group.nodes[0]
            .poll_linear_read(&mut second)
            .unwrap()
            .is_none(),
        "old acknowledgements must not confirm a new read"
    );
    let fresh = group.response(&probes, 1);
    assert_ne!(fresh, old);
    group.nodes[0].step(name(1), fresh).unwrap();
    let second_result = group.nodes[0]
        .poll_linear_read(&mut second)
        .unwrap()
        .unwrap();
    assert_eq!(first_result, second_result);
    assert!(group.nodes[0].poll_linear_read(&mut second).is_err());
}

#[test]
fn quorum_requires_distinct_members_with_matching_term_and_context() {
    let mut group = Group::new(5);
    let mut read = group.nodes[0].begin_linear_read(&query()).unwrap();
    let probes = group.probes();
    let response = group.response(&probes, 1);
    let RaftRpc::ReadIndexResp { term, context } = response else {
        unreachable!()
    };
    for (peer, rpc) in [
        (name(1), RaftRpc::ReadIndexResp { term, context }),
        (name(1), RaftRpc::ReadIndexResp { term, context }),
        ("outsider".into(), RaftRpc::ReadIndexResp { term, context }),
        (
            name(2),
            RaftRpc::ReadIndexResp {
                term: term - 1,
                context,
            },
        ),
        (
            name(2),
            RaftRpc::ReadIndexResp {
                term,
                context: context + 1,
            },
        ),
    ] {
        group.nodes[0].step(peer, rpc).unwrap();
        assert!(
            group.nodes[0]
                .poll_linear_read(&mut read)
                .unwrap()
                .is_none(),
            "a read quorum needs distinct current-term member confirmations"
        );
    }
    let response = group.response(&probes, 2);
    group.nodes[0].step(name(2), response).unwrap();
    assert!(group.nodes[0]
        .poll_linear_read(&mut read)
        .unwrap()
        .unwrap()
        .entry
        .is_some());
}

#[test]
fn heartbeat_retries_the_same_invocation_without_changing_disk_state() {
    let mut group = Group::new(3);
    let state = group._directories[0].path().join("raft.state");
    let before = std::fs::read(&state).unwrap();
    let mut read = group.nodes[0].begin_linear_read(&query()).unwrap();
    let first = group.probes();
    group.clock.advance(200);
    group.nodes[0].tick().unwrap();
    let retry: Vec<_> = group.nodes[0]
        .outbox()
        .unwrap()
        .into_iter()
        .filter(|(_, rpc)| matches!(rpc, RaftRpc::ReadIndex { .. }))
        .collect();
    assert_eq!(retry.len(), first.len(), "lost read probes must be retried");
    for item in &first {
        assert!(retry.contains(item));
    }
    let response = group.response(&retry, 1);
    group.nodes[0].step(name(1), response).unwrap();
    assert!(group.nodes[0]
        .poll_linear_read(&mut read)
        .unwrap()
        .is_some());
    assert_eq!(std::fs::read(state).unwrap(), before);
}

#[test]
fn dropped_or_completed_tickets_release_bounded_quorum_tracking() {
    let mut group = Group::new(3);
    let mut reads: Vec<_> = (0..64)
        .map(|_| group.nodes[0].begin_linear_read(&query()).unwrap())
        .collect();
    let probes = group.probes();
    assert!(
        matches!(
            group.nodes[0].begin_linear_read(&query()),
            Err(DurableDirectoryError::Storage(RaftStorageError::Capacity))
        ),
        "pending quorum tracking must enforce its limit"
    );
    let old = probes
        .iter()
        .find(|(target, _)| *target == name(1))
        .unwrap()
        .clone();
    drop(reads.remove(0));
    let mut fresh = group.nodes[0]
        .begin_linear_read(&query())
        .expect("dropped read must release its pending slot");
    let fresh_probes = group.probes();
    assert!(!fresh_probes.contains(&old));
    let response = group.response(&fresh_probes, 1);
    group.nodes[0].step(name(1), response).unwrap();
    assert!(group.nodes[0]
        .poll_linear_read(&mut fresh)
        .unwrap()
        .is_some());
    // Keep the completed ticket alive: it must not occupy a pending slot.
    let _next = group.nodes[0]
        .begin_linear_read(&query())
        .expect("confirmed read must release its pending slot");
    drop(reads);
    let _after_drop = group.nodes[0].begin_linear_read(&query()).unwrap();
}

#[test]
fn a_higher_term_response_revokes_even_an_already_confirmed_read() {
    let mut group = Group::new(3);
    let mut read = group.nodes[0].begin_linear_read(&query()).unwrap();
    let probes = group.probes();
    let response = group.response(&probes, 1);
    group.nodes[0].step(name(1), response).unwrap();
    let term = group.nodes[0].current_term();
    group.nodes[0]
        .step(
            name(2),
            RaftRpc::ReadIndexResp {
                term: term + 1,
                context: 100,
            },
        )
        .unwrap();
    assert_eq!(group.nodes[0].role(), Role::Follower);
    assert!(
        group.nodes[0].poll_linear_read(&mut read).is_err(),
        "term loss must reject even a confirmed read"
    );
    assert!(group.nodes[0].poll_linear_read(&mut read).is_err());
}

#[test]
fn read_probe_term_is_durable_and_invalid_identity_or_shape_cannot_change_it() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = DurableRaft::open(
        directory.path(),
        "probe",
        "b",
        vec!["a".into()],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    owner
        .step(
            "a".into(),
            RaftRpc::ReadIndex {
                term: 7,
                leader: "a".into(),
                context: 19,
            },
        )
        .unwrap();
    assert_eq!(owner.current_term(), 7);
    owner
        .step(
            "outsider".into(),
            RaftRpc::ReadIndexResp {
                term: 8,
                context: 1,
            },
        )
        .unwrap();
    assert_eq!(
        owner.current_term(),
        7,
        "non-member read responses must not change term"
    );
    assert_eq!(
        owner.outbox().unwrap(),
        [(
            "a".into(),
            RaftRpc::ReadIndexResp {
                term: 7,
                context: 19
            }
        )]
    );
    let before = std::fs::read(directory.path().join("raft.state")).unwrap();
    owner
        .step(
            "a".into(),
            RaftRpc::ReadIndex {
                term: 8,
                leader: "impostor".into(),
                context: 20,
            },
        )
        .unwrap();
    for (term, context) in [(0, 1), (8, 0)] {
        assert!(matches!(
            owner.step(
                "a".into(),
                RaftRpc::ReadIndex {
                    term,
                    leader: "a".into(),
                    context
                }
            ),
            Err(RaftStorageError::Invalid(_))
        ));
    }
    assert_eq!(owner.current_term(), 7);
    assert_eq!(
        std::fs::read(directory.path().join("raft.state")).unwrap(),
        before
    );
    assert!(owner.outbox().unwrap().is_empty());
    drop(owner);
    let owner = DurableRaft::open(
        directory.path(),
        "probe",
        "b",
        vec!["a".into()],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    assert_eq!(owner.current_term(), 7);
}

#[test]
fn concurrent_first_reads_share_only_the_current_term_initialization_entry() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut owner = DurableDirectory::open(
        directory.path(),
        "first-reads",
        "a",
        vec!["b".into()],
        clock.clone(),
    )
    .unwrap();
    clock.advance(1400);
    owner.tick().unwrap();
    let term = owner.current_term();
    owner
        .step(
            "b".into(),
            RaftRpc::RequestVoteResp {
                term,
                vote_granted: true,
            },
        )
        .unwrap();
    let mut reads: Vec<_> = (0..3)
        .map(|_| owner.begin_linear_read(&query()).unwrap())
        .collect();
    let before_commit = owner.outbox().unwrap();
    assert!(before_commit
        .iter()
        .all(|(_, rpc)| !matches!(rpc, RaftRpc::ReadIndex { .. })));
    let indexes: Vec<_> = before_commit
        .iter()
        .filter_map(|(_, rpc)| match rpc {
            RaftRpc::AppendEntries { entries, .. } => entries.last().map(|entry| entry.index),
            _ => None,
        })
        .collect();
    assert_eq!(indexes, [1]);
    owner
        .step(
            "b".into(),
            RaftRpc::AppendEntriesResp {
                term,
                success: true,
                match_index: 1,
            },
        )
        .unwrap();
    let probes: Vec<_> = owner
        .outbox()
        .unwrap()
        .into_iter()
        .filter_map(|(_, rpc)| match rpc {
            RaftRpc::ReadIndex { context, .. } => Some(context),
            _ => None,
        })
        .collect();
    assert_eq!(probes.len(), 3);
    for read in &mut reads {
        assert!(owner.poll_linear_read(read).unwrap().is_none());
    }
    for context in probes {
        owner
            .step("b".into(), RaftRpc::ReadIndexResp { term, context })
            .unwrap();
    }
    for read in &mut reads {
        let result = owner.poll_linear_read(read).unwrap().unwrap();
        assert_eq!(result.barrier_index, 1);
        assert_eq!(result.applied_index, 1);
        assert_eq!(result.entry, None);
    }
}

#[test]
fn probe_publication_failure_fences_its_response_and_preserves_published_term() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = DurableRaft::open(
        directory.path(),
        "probe-failure",
        "b",
        vec!["a".into()],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    let state = directory.path().join("raft.state");
    let backup = directory.path().join("retained-state");
    let before = std::fs::read(&state).unwrap();
    std::fs::rename(&state, &backup).unwrap();
    std::fs::create_dir(&state).unwrap();
    assert!(owner
        .step(
            "a".into(),
            RaftRpc::ReadIndex {
                term: 9,
                leader: "a".into(),
                context: 1
            }
        )
        .is_err());
    assert!(owner.is_closed());
    assert_eq!(owner.current_term(), 0);
    assert!(owner.outbox().is_err());
    assert!(owner.committed().is_err());
    assert_eq!(std::fs::read(&backup).unwrap(), before);
    drop(owner);
    std::fs::remove_dir(&state).unwrap();
    std::fs::rename(&backup, &state).unwrap();
    let owner = DurableRaft::open(
        directory.path(),
        "probe-failure",
        "b",
        vec!["a".into()],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    assert_eq!(owner.current_term(), 0);
}

#[test]
fn an_uncommitted_current_term_business_entry_needs_no_extra_initialization_entry() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut owner = DurableDirectory::open(
        directory.path(),
        "pending-business",
        "a",
        vec!["b".into()],
        clock.clone(),
    )
    .unwrap();
    clock.advance(1400);
    owner.tick().unwrap();
    let term = owner.current_term();
    owner
        .step(
            "b".into(),
            RaftRpc::RequestVoteResp {
                term,
                vote_granted: true,
            },
        )
        .unwrap();
    owner
        .propose(&DirCmd::Upsert {
            node: "worker".into(),
            endpoints: vec!["mem://worker".into()],
        })
        .unwrap();
    let mut read = owner.begin_linear_read(&query()).unwrap();
    let before_commit = owner.outbox().unwrap();
    let indexes: Vec<_> = before_commit
        .iter()
        .filter_map(|(_, rpc)| match rpc {
            RaftRpc::AppendEntries { entries, .. } => entries.last().map(|entry| entry.index),
            _ => None,
        })
        .collect();
    assert_eq!(indexes, [1]);
    assert!(before_commit
        .iter()
        .all(|(_, rpc)| !matches!(rpc, RaftRpc::ReadIndex { .. })));
    owner
        .step(
            "b".into(),
            RaftRpc::AppendEntriesResp {
                term,
                success: true,
                match_index: 1,
            },
        )
        .unwrap();
    assert!(owner.poll_linear_read(&mut read).unwrap().is_none());
    let probes: Vec<_> = owner
        .outbox()
        .unwrap()
        .into_iter()
        .filter_map(|(_, rpc)| match rpc {
            RaftRpc::ReadIndex { context, .. } => Some(context),
            _ => None,
        })
        .collect();
    assert_eq!(probes.len(), 1);
    owner
        .step(
            "b".into(),
            RaftRpc::ReadIndexResp {
                term,
                context: probes[0],
            },
        )
        .unwrap();
    let result = owner.poll_linear_read(&mut read).unwrap().unwrap();
    assert_eq!(result.barrier_index, 1);
    assert_eq!(result.entry.unwrap().version, 1);
}
