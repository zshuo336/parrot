use parrot_remote::raft::{ManualClock, RaftNode, RaftRpc, Role};
use std::sync::Arc;

#[test]
fn catch_up_batches_bound_bytes_and_entry_count_without_losing_history() {
    for payload in [vec![7; 900_000], vec![3]] {
        let clock = Arc::new(ManualClock::new());
        let mut leader = RaftNode::new("a", vec!["b".into()], clock.clone());
        let mut follower = RaftNode::new("b", vec!["a".into()], clock.clone());
        clock.advance(1500);
        leader.tick();
        leader.outbox();
        leader.step(
            "b".into(),
            RaftRpc::RequestVoteResp {
                term: leader.current_term,
                vote_granted: true,
            },
        );
        assert_eq!(leader.role, Role::Leader);
        leader.outbox();
        let count = if payload.len() == 1 { 2050 } else { 8 };
        for _ in 0..count {
            leader.propose(payload.clone()).unwrap();
            leader.outbox();
        }
        for _ in 0..12 {
            clock.advance(200);
            leader.tick();
            for (_, rpc) in leader.outbox() {
                if let RaftRpc::AppendEntries { entries, .. } = &rpc {
                    assert!(
                        entries.len() <= 1024,
                        "unbounded catch-up entry count: {}",
                        entries.len()
                    );
                    assert!(
                        entries.iter().map(|e| e.cmd.len() + 32).sum::<usize>() <= 3 * 1024 * 1024,
                        "unbounded catch-up byte count"
                    );
                }
                follower.step("a".into(), rpc);
            }
            for (_, reply) in follower.outbox() {
                leader.step("b".into(), reply);
            }
        }
        assert_eq!(follower.log, leader.log);
        assert_eq!(leader.commit_index, count);
        assert_eq!(follower.commit_index, count);
        assert_eq!(follower.committed().len() as u64, count);
        assert!(follower.committed().is_empty());
    }
}

#[test]
fn split_votes_do_not_repeat_in_lockstep_forever() {
    // These distinct IDs collide in the initial election jitter. A reliable
    // network with a fixed 40 ms delivery delay must still eventually elect.
    let clock = Arc::new(ManualClock::new());
    let mut nodes = [
        RaftNode::new("voter-6", vec!["voter-24".into()], clock.clone()),
        RaftNode::new("voter-24", vec!["voter-6".into()], clock.clone()),
    ];
    let mut pending = std::collections::VecDeque::new();
    for now in (10..=10_000).step_by(10) {
        clock.advance(10);
        for (i, node) in nodes.iter_mut().enumerate() {
            node.tick();
            for (_, rpc) in node.outbox() {
                pending.push_back((now + 40, 1 - i, node.id.clone(), rpc));
            }
        }
        while pending.front().is_some_and(|p| p.0 <= now) {
            let (_, target, from, rpc) = pending.pop_front().unwrap();
            nodes[target].step(from, rpc);
            for (_, rpc) in nodes[target].outbox() {
                pending.push_back((now + 40, 1 - target, nodes[target].id.clone(), rpc));
            }
        }
        if nodes.iter().filter(|n| n.role == Role::Leader).count() == 1 {
            return;
        }
    }
    panic!(
        "reliable network livelocked: {:?}",
        nodes
            .iter()
            .map(|n| (n.role, n.current_term))
            .collect::<Vec<_>>()
    );
}
