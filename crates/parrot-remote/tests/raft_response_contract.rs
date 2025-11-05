//! Delayed replies must not regress confirmed replication or amplify retries.
use parrot_remote::raft::{LogEntry, ManualClock, RaftNode, RaftRpc, Role};
use std::sync::Arc;

fn elected() -> (RaftNode, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new());
    let mut node = RaftNode::new("a", vec!["b".into(), "c".into()], clock.clone());
    node.current_term = 2;
    node.log = (1..=4)
        .map(|index| LogEntry {
            index,
            term: 2,
            cmd: vec![index as u8],
        })
        .collect();
    clock.advance(1400);
    node.tick();
    let election = node.outbox();
    let mut voter = RaftNode::new("b", vec!["a".into(), "c".into()], clock.clone());
    for (peer, rpc) in election {
        if peer == "b" {
            voter.step("a".into(), rpc);
        }
    }
    for (_, rpc) in voter.outbox() {
        node.step("b".into(), rpc);
    }
    assert_eq!(node.role, Role::Leader);
    node.outbox();
    (node, clock)
}

fn reject(node: &mut RaftNode, peer: &str) {
    node.step(
        peer.into(),
        RaftRpc::AppendEntriesResp {
            term: node.current_term,
            success: false,
            match_index: 0,
        },
    );
}

#[test]
fn conflict_retry_targets_only_the_rejecting_follower() {
    let (mut node, _) = elected();
    reject(&mut node, "b");
    let retry = node.outbox();
    assert_eq!(
        retry.len(),
        1,
        "one rejection must not fan out to healthy peers"
    );
    assert_eq!(retry[0].0, "b");
    assert!(
        matches!(&retry[0].1, RaftRpc::AppendEntries { prev_log_index: 3, entries, .. } if entries.len() == 1 && entries[0].index == 4)
    );
}

#[test]
fn delayed_rejection_cannot_backtrack_confirmed_replication() {
    let (mut node, clock) = elected();
    node.propose(vec![5]).unwrap();
    let mut follower = RaftNode::new("b", vec!["a".into(), "c".into()], clock.clone());
    follower.current_term = 2;
    follower.log = node.log[..4].to_vec();
    for (to, rpc) in node.outbox() {
        if to == "b" {
            follower.step("a".into(), rpc);
        }
    }
    for (_, rpc) in follower.outbox() {
        node.step("b".into(), rpc);
    }
    assert_eq!(node.commit_index, 5);
    node.outbox();
    // A negative reply to an older probe arrives after the successful batch.
    reject(&mut node, "b");
    assert!(
        node.outbox().is_empty(),
        "confirmed prefix must not trigger a reactive retry"
    );
    clock.advance(150);
    node.tick();
    let rpc = node
        .outbox()
        .into_iter()
        .find(|(to, _)| to == "b")
        .unwrap()
        .1;
    assert!(
        matches!(rpc, RaftRpc::AppendEntries { prev_log_index: 5, entries, .. } if entries.is_empty())
    );
    assert_eq!(node.commit_index, 5);
}

#[test]
fn refusal_at_the_log_origin_waits_for_heartbeat_instead_of_rpc_looping() {
    let (mut node, clock) = elected();
    for _ in 0..4 {
        reject(&mut node, "b");
        node.outbox();
    }
    for _ in 0..100 {
        reject(&mut node, "b");
        assert!(
            node.outbox().is_empty(),
            "unchanged rejection must not generate another immediate RPC"
        );
    }
    clock.advance(150);
    node.tick();
    assert!(
        node.outbox().iter().any(|(peer, rpc)| peer == "b"
            && matches!(
                rpc,
                RaftRpc::AppendEntries {
                    prev_log_index: 0,
                    ..
                }
            )),
        "periodic recovery probes must remain enabled"
    );
}

#[test]
fn higher_term_replies_revoke_leader_hint_and_unsent_old_term_work() {
    for reply in [
        RaftRpc::RequestVoteResp {
            term: 4,
            vote_granted: false,
        },
        RaftRpc::AppendEntriesResp {
            term: 4,
            success: false,
            match_index: 0,
        },
    ] {
        let (mut node, _) = elected();
        node.propose(vec![5]).unwrap(); // queued under the obsolete term
        node.step("b".into(), reply);
        assert_eq!(node.role, Role::Follower);
        assert_eq!(node.current_term, 4);
        assert_eq!(
            node.leader_hint, None,
            "a demoted leader must not redirect clients to itself"
        );
        assert_eq!(node.propose(vec![6]).unwrap_err().0, None);
        assert!(
            node.outbox().is_empty(),
            "old leadership requests must be retired"
        );
        assert_eq!(
            node.log.len(),
            5,
            "step-down must retain the uncommitted log"
        );
    }
}

#[test]
fn denied_vote_does_not_refresh_the_election_deadline() {
    let clock = Arc::new(ManualClock::new());
    let mut node = RaftNode::new("b", vec!["a".into()], clock.clone());
    node.current_term = 2;
    node.log.push(LogEntry {
        term: 2,
        index: 1,
        cmd: vec![1],
    });
    node.leader_hint = Some("a".into());
    clock.advance(1400);
    node.step(
        "a".into(),
        RaftRpc::RequestVote {
            term: 3,
            candidate: "a".into(),
            last_log_index: 0,
            last_log_term: 0,
        },
    );
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
    node.tick();
    assert_eq!(
        node.role,
        Role::Candidate,
        "a denied stale-log candidate must not suppress the due election"
    );
    assert_eq!(node.current_term, 4);
}

#[test]
fn reordered_duplicate_replies_and_partition_healing_preserve_committed_history() {
    for seed in 1..=12u64 {
        let clock = Arc::new(ManualClock::new());
        let ids = ["a", "b", "c", "d", "e"];
        let mut nodes: Vec<_> = ids
            .iter()
            .map(|id| {
                RaftNode::new(
                    *id,
                    ids.iter()
                        .filter(|peer| *peer != id)
                        .map(|s| s.to_string())
                        .collect(),
                    clock.clone(),
                )
            })
            .collect();
        let mut pending = Vec::<(u64, usize, usize, RaftRpc)>::new();
        let mut random = seed;
        let mut draw = || {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random
        };
        let mut previous_commits = [0; 5];
        let mut proposals = 0;
        for now in (20..=12_000u64).step_by(20) {
            clock.advance(20);
            for node in &mut nodes {
                node.tick();
                if now < 10_000 && now % 200 == 0 && node.role == Role::Leader {
                    let command = format!("{}:{now}", node.id).into_bytes();
                    node.propose(command).unwrap();
                    proposals += 1;
                }
            }
            // Snapshot each round: duplicate and delay actual emitted RPCs;
            // never fabricate an ACK or manipulate a node's private progress.
            for (from, node) in nodes.iter_mut().enumerate() {
                for (peer, rpc) in node.outbox() {
                    let to = ids.iter().position(|id| *id == peer).unwrap();
                    if (2000..6000).contains(&now) && (from < 2) != (to < 2) {
                        continue;
                    }
                    if draw() % 7 == 0 {
                        pending.push((now + 100, from, to, rpc.clone()));
                    }
                    pending.push((now + (draw() % 4) * 20, from, to, rpc));
                }
            }
            // Reverse delivery order on alternating rounds, in addition to
            // jitter, so old rejections can follow newer successful replies.
            if now % 40 == 0 {
                pending.reverse();
            }
            let mut i = 0;
            while i < pending.len() {
                if pending[i].0 <= now {
                    let (_, from, to, rpc) = pending.swap_remove(i);
                    if !(2000..6000).contains(&now) || (from < 2) == (to < 2) {
                        nodes[to].step(ids[from].into(), rpc);
                    }
                } else {
                    i += 1;
                }
            }
            for (i, node) in nodes.iter().enumerate() {
                assert!(
                    node.commit_index >= previous_commits[i],
                    "commit regression at seed {seed}"
                );
                previous_commits[i] = node.commit_index;
                for other in &nodes {
                    let common = node.commit_index.min(other.commit_index) as usize;
                    assert_eq!(
                        node.log[..common],
                        other.log[..common],
                        "committed conflict at seed {seed}, time {now}"
                    );
                    if node.id != other.id
                        && node.role == Role::Leader
                        && other.role == Role::Leader
                    {
                        assert_ne!(
                            node.current_term, other.current_term,
                            "two leaders in one term"
                        );
                    }
                }
            }
        }
        assert!(proposals > 10, "seed {seed} never made sustained progress");
        let committed = nodes.iter().map(|n| n.commit_index).max().unwrap();
        assert!(committed > 10);
        for node in &nodes {
            assert_eq!(
                node.commit_index, committed,
                "seed {seed} did not converge after healing: {}",
                node.id
            );
        }
    }
}
