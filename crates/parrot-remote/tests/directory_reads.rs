use parrot_remote::{
    raft::{ManualClock, RaftRpc, Role},
    roles::directory::{DirCmd, DirQuery, DurableDirectory, DurableDirectoryError},
};
use std::{path::PathBuf, sync::Arc};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "parrot-read-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn put() -> DirCmd {
    DirCmd::Upsert {
        node: "worker".into(),
        endpoints: vec!["mem://worker".into()],
    }
}
fn query() -> DirQuery {
    DirQuery::Resolve {
        prefix_or_node: "worker".into(),
    }
}
#[test]
fn fresh_read_barriers_prove_absence_preserve_revision_and_cannot_be_reused_or_reopened() {
    let p = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = DurableDirectory::open(&p.0, "g", "a", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    d.tick().unwrap();
    let mut absent = d.begin_linear_read(&query()).unwrap();
    let result = d.poll_linear_read(&mut absent).unwrap().unwrap();
    assert_eq!(result.entry, None);
    assert_eq!(result.barrier_index, 1);
    assert_eq!(d.state_local().unwrap().version, 0);
    assert!(d.poll_linear_read(&mut absent).is_err());
    let original = d.submit_request("operator", "id", &put()).unwrap();
    let before = d.state_local().unwrap().clone();
    let mut read = d.begin_linear_read(&query()).unwrap();
    let value = d.poll_linear_read(&mut read).unwrap().unwrap();
    assert_eq!(value.entry, d.query_local(&query()).unwrap());
    assert_eq!(value.entry.unwrap().version, 1);
    assert_eq!(value.barrier_index, 3);
    assert_eq!(value.applied_index, 3);
    assert_eq!(d.state_local().unwrap(), &before);
    let mut foreign = d.begin_linear_read(&query()).unwrap();
    drop(d);
    let mut d = DurableDirectory::open(&p.0, "g", "a", vec![], clock.clone()).unwrap();
    assert_eq!(d.state_local().unwrap(), &before);
    assert!(d.poll_linear_read(&mut foreign).is_err());
    assert_eq!(
        d.submit_request("operator", "id", &put()).unwrap(),
        original
    );
    assert!(d.begin_linear_read(&query()).is_err());
    clock.advance(1400);
    d.tick().unwrap();
    let mut fresh = d.begin_linear_read(&query()).unwrap();
    assert_eq!(
        d.poll_linear_read(&mut fresh).unwrap().unwrap().entry,
        d.query_local(&query()).unwrap()
    );
}
#[test]
fn old_ack_or_leader_flag_cannot_complete_a_fresh_read_and_term_loss_fails_it() {
    let p = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = DurableDirectory::open(&p.0, "g", "a", vec!["b".into(), "c".into()], clock.clone())
        .unwrap();
    clock.advance(1400);
    d.tick().unwrap();
    let term = d.current_term();
    d.step(
        "b".into(),
        RaftRpc::RequestVoteResp {
            term,
            vote_granted: true,
        },
    )
    .unwrap();
    assert_eq!(d.role(), Role::Leader);
    d.propose(&put()).unwrap();
    d.step(
        "b".into(),
        RaftRpc::AppendEntriesResp {
            term,
            success: true,
            match_index: 1,
        },
    )
    .unwrap();
    assert!(d.query_local(&query()).unwrap().is_some());
    let mut read = d.begin_linear_read(&query()).unwrap();
    assert!(d.poll_linear_read(&mut read).unwrap().is_none());
    for _ in 0..3 {
        d.step(
            "b".into(),
            RaftRpc::AppendEntriesResp {
                term,
                success: true,
                match_index: 1,
            },
        )
        .unwrap();
        assert!(d.poll_linear_read(&mut read).unwrap().is_none());
    }
    d.step(
        "c".into(),
        RaftRpc::AppendEntriesResp {
            term: term + 1,
            success: false,
            match_index: 1,
        },
    )
    .unwrap();
    assert!(matches!(
        d.poll_linear_read(&mut read),
        Err(DurableDirectoryError::Storage(_))
    ));
    assert!(d.query_local(&query()).unwrap().is_some());
    assert!(d.poll_linear_read(&mut read).is_err());
}
#[test]
fn current_term_read_barrier_commits_prior_term_prefix_without_a_business_write() {
    let p = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = DurableDirectory::open(&p.0, "g", "b", vec!["a".into()], clock.clone()).unwrap();
    d.step(
        "a".into(),
        RaftRpc::AppendEntries {
            term: 2,
            leader: "a".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![parrot_remote::raft::LogEntry {
                index: 1,
                term: 2,
                cmd: put().encode().unwrap(),
            }],
            leader_commit: 0,
        },
    )
    .unwrap();
    clock.advance(1400);
    d.tick().unwrap();
    let term = d.current_term();
    d.step(
        "a".into(),
        RaftRpc::RequestVoteResp {
            term,
            vote_granted: true,
        },
    )
    .unwrap();
    let mut ticket = d.begin_linear_read(&query()).unwrap();
    assert!(d.poll_linear_read(&mut ticket).unwrap().is_none());
    d.step(
        "a".into(),
        RaftRpc::AppendEntriesResp {
            term,
            success: true,
            match_index: 2,
        },
    )
    .unwrap();
    let result = d.poll_linear_read(&mut ticket).unwrap().unwrap();
    assert_eq!(result.barrier_index, 2);
    assert_eq!(result.term, term);
    assert_eq!(result.entry.unwrap().endpoints, ["mem://worker"]);
    assert_eq!(d.state_local().unwrap().version, 1);
}
#[test]
fn malformed_barrier_and_invalid_read_do_not_mutate_or_consume_log() {
    let p = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = DurableDirectory::open(&p.0, "g", "b", vec!["a".into()], clock.clone()).unwrap();
    for bytes in [b"\xffDIR\x02\x00".to_vec(), b"\xffDIR\x03".to_vec()] {
        assert!(d
            .step(
                "a".into(),
                RaftRpc::AppendEntries {
                    term: 2,
                    leader: "a".into(),
                    prev_log_index: 0,
                    prev_log_term: 0,
                    entries: vec![parrot_remote::raft::LogEntry {
                        index: 1,
                        term: 2,
                        cmd: bytes
                    }],
                    leader_commit: 1
                }
            )
            .is_err());
        assert_eq!(d.current_term(), 0);
        assert_eq!(d.applied_index(), 0);
    }
    clock.advance(1400);
    d.tick().unwrap();
    let term = d.current_term();
    d.step(
        "a".into(),
        RaftRpc::RequestVoteResp {
            term,
            vote_granted: true,
        },
    )
    .unwrap();
    assert!(d
        .begin_linear_read(&DirQuery::Resolve {
            prefix_or_node: "x".repeat(4097)
        })
        .is_err());
    let mut ticket = d.begin_linear_read(&query()).unwrap();
    d.step(
        "a".into(),
        RaftRpc::AppendEntriesResp {
            term,
            success: true,
            match_index: 1,
        },
    )
    .unwrap();
    assert_eq!(
        d.poll_linear_read(&mut ticket)
            .unwrap()
            .unwrap()
            .barrier_index,
        1
    );
}

fn deliver(nodes: &mut [DurableDirectory], members: &[usize]) {
    let names = ["a", "b", "c"];
    for _ in 0..20 {
        let mut messages = Vec::new();
        for &i in members {
            for (target, rpc) in nodes[i].outbox().unwrap() {
                if let Some(j) = members.iter().find(|j| names[**j] == target) {
                    messages.push((i, *j, rpc));
                }
            }
        }
        if messages.is_empty() {
            return;
        }
        for (i, j, rpc) in messages {
            nodes[j].step(names[i].into(), rpc).unwrap();
        }
    }
    panic!("model network failed to settle");
}
#[test]
fn isolated_old_leader_cannot_read_stale_data_after_new_majority_commits() {
    let dirs: Vec<_> = (0..3).map(|_| Temp::new()).collect();
    let clock = Arc::new(ManualClock::new());
    let names = ["a", "b", "c"];
    let mut nodes: Vec<_> = (0..3)
        .map(|i| {
            DurableDirectory::open(
                &dirs[i].0,
                "partition",
                names[i],
                names
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, n)| n.to_string())
                    .collect(),
                clock.clone(),
            )
            .unwrap()
        })
        .collect();
    clock.advance(1400);
    nodes[0].tick().unwrap();
    deliver(&mut nodes, &[0, 1, 2]);
    assert_eq!(nodes[0].role(), Role::Leader);
    nodes[0].propose(&put()).unwrap();
    deliver(&mut nodes, &[0, 1, 2]);
    clock.advance(200);
    nodes[0].tick().unwrap();
    deliver(&mut nodes, &[0, 1, 2]);
    clock.advance(1400);
    nodes[1].tick().unwrap();
    deliver(&mut nodes, &[1, 2]);
    assert_eq!(nodes[1].role(), Role::Leader);
    let updated = DirCmd::Upsert {
        node: "worker".into(),
        endpoints: vec!["mem://new-worker".into()],
    };
    nodes[1].propose(&updated).unwrap();
    deliver(&mut nodes, &[1, 2]);
    assert_eq!(nodes[1].query_local(&query()).unwrap().unwrap().version, 2);
    assert_eq!(nodes[0].role(), Role::Leader);
    assert_eq!(nodes[0].query_local(&query()).unwrap().unwrap().version, 1);
    let mut read = nodes[0].begin_linear_read(&query()).unwrap();
    assert!(nodes[0].poll_linear_read(&mut read).unwrap().is_none());
    clock.advance(200);
    nodes[1].tick().unwrap();
    deliver(&mut nodes, &[0, 1, 2]);
    assert!(nodes[0].poll_linear_read(&mut read).is_err());
    let mut current = nodes[1].begin_linear_read(&query()).unwrap();
    deliver(&mut nodes, &[0, 1, 2]);
    let result = nodes[1].poll_linear_read(&mut current).unwrap().unwrap();
    assert_eq!(result.entry.unwrap().endpoints, ["mem://new-worker"]);
}
