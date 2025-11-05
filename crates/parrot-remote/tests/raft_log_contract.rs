use parrot_remote::raft::{LogEntry, ManualClock, RaftNode, RaftRpc, Role};
use std::sync::Arc;

fn entry(index: u64, term: u64, cmd: u8) -> LogEntry {
    LogEntry {
        index,
        term,
        cmd: vec![cmd],
    }
}
fn append(node: &mut RaftNode, prev: u64, prev_term: u64, entries: Vec<LogEntry>) {
    node.step(
        "a".into(),
        RaftRpc::AppendEntries {
            term: 3,
            leader: "a".into(),
            prev_log_index: prev,
            prev_log_term: prev_term,
            entries,
            leader_commit: 0,
        },
    );
}
fn rejected(node: &mut RaftNode) {
    assert!(matches!(
        node.outbox().as_slice(),
        [(_, RaftRpc::AppendEntriesResp { success: false, .. })]
    ));
}

#[test]
fn malformed_append_cannot_ack_or_change_log() {
    for (prev, prev_term, entries) in [
        (0, 9, vec![entry(1, 1, 1)]),
        (0, 0, vec![entry(1, 0, 1)]),
        (0, 0, vec![entry(1, 2, 1), entry(2, 1, 2)]),
        (1, 2, vec![entry(2, 1, 2)]),
    ] {
        let mut node = RaftNode::new("b", vec!["a".into()], Arc::new(ManualClock::new()));
        if prev == 1 {
            node.log.push(entry(1, 2, 1));
        }
        let before = node.log.clone();
        append(&mut node, prev, prev_term, entries);
        rejected(&mut node);
        assert_eq!(node.log, before);
    }
}

#[test]
fn same_term_different_command_rejects_entire_batch() {
    let mut node = RaftNode::new("b", vec!["a".into()], Arc::new(ManualClock::new()));
    node.log.push(entry(1, 2, 1));
    let before = node.log.clone();
    append(&mut node, 0, 0, vec![entry(1, 2, 9), entry(2, 3, 2)]);
    rejected(&mut node);
    assert_eq!(node.log, before);
}

#[test]
fn longer_conflicting_follower_can_catch_up() {
    let clock = Arc::new(ManualClock::new());
    let mut leader = RaftNode::new("a", vec!["b".into(), "c".into()], clock.clone());
    leader.current_term = 2;
    leader.log = vec![entry(1, 1, 1), entry(2, 2, 2)];
    clock.advance(1400);
    leader.tick();
    leader.step(
        "c".into(),
        RaftRpc::RequestVoteResp {
            term: 3,
            vote_granted: true,
        },
    );
    assert_eq!(leader.role, Role::Leader);
    let mut follower = RaftNode::new("b", vec!["a".into(), "c".into()], clock);
    follower.log = vec![entry(1, 1, 1), entry(2, 1, 9), entry(3, 1, 9)];
    for _ in 0..6 {
        for (to, rpc) in leader.outbox() {
            if to == "b" {
                follower.step("a".into(), rpc);
            }
        }
        for (_, rpc) in follower.outbox() {
            leader.step("b".into(), rpc);
        }
    }
    assert_eq!(
        follower.log, leader.log,
        "longer rejected suffix must not block backtracking"
    );
}

#[test]
fn exhausted_term_never_wraps_or_panics() {
    let clock = Arc::new(ManualClock::new());
    let mut node = RaftNode::new("a", vec![], clock.clone());
    node.current_term = u64::MAX;
    clock.advance(1400);
    node.tick();
    assert_eq!(node.current_term, u64::MAX);
    assert_eq!(node.role, Role::Follower);
    assert!(node.outbox().is_empty());
}

#[test]
fn malformed_vote_log_metadata_is_rejected_without_spending_a_vote() {
    let clock = Arc::new(ManualClock::new());
    let mut node = RaftNode::new("b", vec!["a".into()], clock);
    for (last_log_index, last_log_term, term) in [(0, 1, 1), (1, 0, 1), (1, 2, 1)] {
        node.step(
            "a".into(),
            RaftRpc::RequestVote {
                term,
                candidate: "a".into(),
                last_log_index,
                last_log_term,
            },
        );
        assert_eq!(node.voted_for, None);
        assert!(matches!(
            node.outbox().as_slice(),
            [(
                _,
                RaftRpc::RequestVoteResp {
                    vote_granted: false,
                    ..
                }
            )]
        ));
    }
}

#[test]
fn success_ack_cannot_advance_beyond_the_latest_sent_log_range() {
    let clock = Arc::new(ManualClock::new());
    let mut node = RaftNode::new("a", vec!["b".into(), "c".into()], clock.clone());
    clock.advance(1400);
    node.tick();
    node.outbox();
    node.step(
        "b".into(),
        RaftRpc::RequestVoteResp {
            term: node.current_term,
            vote_granted: true,
        },
    );
    assert_eq!(node.role, Role::Leader);
    node.outbox();

    // A normal proposal sends index 1.  Simulate a locally visible but
    // unsent suffix and ensure a fabricated success for it cannot commit.
    node.propose(vec![1]).unwrap();
    node.outbox();
    node.log.push(entry(2, node.current_term, 2));
    node.step(
        "b".into(),
        RaftRpc::AppendEntriesResp {
            term: node.current_term,
            success: true,
            match_index: 2,
        },
    );
    assert_eq!(node.commit_index, 0);
}

#[test]
fn zero_term_requests_cannot_create_votes_or_leadership() {
    for rpc in [
        RaftRpc::RequestVote {
            term: 0,
            candidate: "a".into(),
            last_log_index: 0,
            last_log_term: 0,
        },
        RaftRpc::AppendEntries {
            term: 0,
            leader: "a".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![],
            leader_commit: 0,
        },
    ] {
        let mut node = RaftNode::new("b", vec!["a".into()], Arc::new(ManualClock::new()));
        node.step("a".into(), rpc);
        assert_eq!(node.current_term, 0);
        assert_eq!(node.voted_for, None);
        assert_eq!(node.leader_hint, None);
        assert!(matches!(
            node.outbox().as_slice(),
            [(
                _,
                RaftRpc::RequestVoteResp {
                    vote_granted: false,
                    ..
                }
            )] | [(_, RaftRpc::AppendEntriesResp { success: false, .. })]
        ));
    }
}

#[test]
fn malformed_append_preserves_election_state_and_deadline() {
    for (prev, prev_term, entries) in [
        (0, 1, vec![]),
        (1, 0, vec![]),
        (1, 51, vec![]),
        (1, 2, vec![entry(3, 2, 3)]),
        (1, 2, vec![entry(2, 51, 2)]),
        (1, 2, vec![entry(2, 0, 2)]),
        (1, 2, vec![entry(2, 3, 2), entry(3, 2, 3)]),
        (u64::MAX, 2, vec![entry(1, 2, 1)]),
    ] {
        let clock = Arc::new(ManualClock::new());
        let mut node = RaftNode::new("b", vec!["a".into()], clock.clone());
        node.current_term = 2;
        node.voted_for = Some("a".into());
        node.leader_hint = Some("a".into());
        node.log.push(entry(1, 2, 1));
        clock.advance(1400);
        node.step(
            "a".into(),
            RaftRpc::AppendEntries {
                term: 50,
                leader: "a".into(),
                prev_log_index: prev,
                prev_log_term: prev_term,
                entries,
                leader_commit: 0,
            },
        );
        rejected(&mut node);
        assert_eq!(
            node.current_term, 2,
            "malformed input changed the persisted term"
        );
        assert_eq!(node.voted_for.as_deref(), Some("a"));
        assert_eq!(node.leader_hint.as_deref(), Some("a"));
        assert_eq!(node.log, [entry(1, 2, 1)]);
        node.tick();
        assert_eq!(
            node.role,
            Role::Candidate,
            "malformed input postponed the due election"
        );
        assert_eq!(node.current_term, 3);
    }
}
