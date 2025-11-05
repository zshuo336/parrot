use parrot_remote::raft::{
    DurableRaft, LogEntry, ManualClock, RaftRpc, RaftStorageError, Role, StateMachine,
};
use parrot_remote::roles::directory::{DirCmd, DirectorySm};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-raft-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn open(t: &Temp, clock: Arc<ManualClock>) -> DurableRaft {
    DurableRaft::open(&t.0, "cluster", "b", vec!["a".into(), "c".into()], clock).unwrap()
}
fn vote(candidate: &str, term: u64) -> RaftRpc {
    RaftRpc::RequestVote {
        term,
        candidate: candidate.into(),
        last_log_index: 0,
        last_log_term: 0,
    }
}
fn grant(node: &mut DurableRaft, candidate: &str, term: u64, granted: bool) {
    node.step(candidate.into(), vote(candidate, term)).unwrap();
    assert!(
        matches!(node.outbox().unwrap().as_slice(), [(_, RaftRpc::RequestVoteResp { vote_granted, .. })] if *vote_granted == granted)
    );
}
fn command(node: &str) -> Vec<u8> {
    bincode::serde::encode_to_vec(
        DirCmd::Upsert {
            node: node.into(),
            endpoints: vec![format!("tcp://{node}:7000")],
        },
        bincode::config::standard(),
    )
    .unwrap()
}
fn append(entries: Vec<LogEntry>, commit: u64) -> RaftRpc {
    RaftRpc::AppendEntries {
        term: 2,
        leader: "a".into(),
        prev_log_index: 0,
        prev_log_term: 0,
        entries,
        leader_commit: commit,
    }
}
fn history() -> Vec<LogEntry> {
    (1..=3)
        .map(|index| LogEntry {
            term: 2,
            index,
            cmd: command(&format!("n{index}")),
        })
        .collect()
}

#[test]
fn delayed_refusal_and_step_down_preserve_durable_committed_history() {
    let clock = Arc::new(ManualClock::new());
    let a_dir = Temp::new();
    let b_dir = Temp::new();
    let reopen = || {
        DurableRaft::open(
            &a_dir.0,
            "cluster",
            "a",
            vec!["b".into(), "c".into()],
            clock.clone(),
        )
        .unwrap()
    };
    let mut a = reopen();
    let mut b = open(&b_dir, clock.clone());
    // A persisted uncommitted history predates this election. B is empty and
    // will reject the new leader's initial last-index probe.
    a.step(
        "c".into(),
        RaftRpc::AppendEntries {
            term: 2,
            leader: "c".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: history(),
            leader_commit: 0,
        },
    )
    .unwrap();
    a.outbox().unwrap();
    clock.advance(1400);
    a.tick().unwrap();
    for (to, rpc) in a.outbox().unwrap() {
        if to == "b" {
            b.step("a".into(), rpc).unwrap();
        }
    }
    for (_, reply) in b.outbox().unwrap() {
        a.step("b".into(), reply).unwrap();
    }
    assert_eq!(a.role(), Role::Leader);
    for (to, rpc) in a.outbox().unwrap() {
        if to == "b" {
            b.step("a".into(), rpc).unwrap();
        }
    }
    let responses = b.outbox().unwrap();
    let stale = responses[0].1.clone();
    assert!(matches!(
        stale,
        RaftRpc::AppendEntriesResp { success: false, .. }
    ));
    for (_, reply) in responses {
        a.step("b".into(), reply).unwrap();
    }
    fn pump(a: &mut DurableRaft, b: &mut DurableRaft) {
        for _ in 0..16 {
            let outgoing = a.outbox().unwrap();
            if outgoing.is_empty() {
                return;
            }
            for (to, rpc) in outgoing {
                if to == "b" {
                    b.step("a".into(), rpc).unwrap();
                }
            }
            for (_, reply) in b.outbox().unwrap() {
                a.step("b".into(), reply).unwrap();
            }
        }
        panic!("replication did not settle");
    }
    pump(&mut a, &mut b);
    a.propose(command("current-term")).unwrap();
    pump(&mut a, &mut b);
    assert_eq!(a.commit_index(), 4);
    assert_eq!(b.commit_index(), 4);
    assert_eq!(a.log(), b.log());
    a.step("b".into(), stale).unwrap();
    assert!(a.outbox().unwrap().is_empty());
    let before = a.log().to_vec();
    let next_term = a.current_term() + 1;
    a.step(
        "b".into(),
        RaftRpc::AppendEntriesResp {
            term: next_term,
            success: false,
            match_index: 0,
        },
    )
    .unwrap();
    assert_eq!(a.leader_hint(), None);
    assert_eq!(a.role(), Role::Follower);
    drop(a);
    let recovered = reopen();
    assert_eq!(recovered.current_term(), next_term);
    assert_eq!(recovered.voted_for(), None);
    assert_eq!(recovered.commit_index(), 4);
    assert_eq!(recovered.log(), before);
}

#[test]
fn restart_preserves_vote_and_allows_only_one_candidate_per_term() {
    let t = Temp::new();
    let mut n = open(&t, Arc::new(ManualClock::new()));
    grant(&mut n, "a", 7, true);
    drop(n);
    let mut n = open(&t, Arc::new(ManualClock::new()));
    assert_eq!(n.current_term(), 7);
    assert_eq!(n.voted_for(), Some("a"));
    assert_eq!(n.role(), Role::Follower);
    grant(&mut n, "c", 7, false);
    grant(&mut n, "a", 7, true);
    grant(&mut n, "c", 8, true);
    drop(n);
    let mut n = open(&t, Arc::new(ManualClock::new()));
    grant(&mut n, "a", 8, false);
}

#[test]
fn zero_term_vote_is_rejected_without_fencing_or_touching_the_snapshot() {
    let dir = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut node = open(&dir, clock.clone());
    let original = std::fs::read(dir.0.join("raft.state")).unwrap();
    let result = node.step("a".into(), vote("a", 0));
    assert!(
        matches!(result, Err(RaftStorageError::Invalid(_))),
        "{result:?}"
    );
    assert!(
        !node.is_closed(),
        "malformed input must not kill the durable owner"
    );
    assert_eq!(std::fs::read(dir.0.join("raft.state")).unwrap(), original);
    assert!(!dir.0.join("raft.next").exists());
    grant(&mut node, "a", 1, true);
    drop(node);
    let recovered = open(&dir, clock);
    assert_eq!(recovered.current_term(), 1);
    assert_eq!(recovered.voted_for(), Some("a"));
}

#[test]
fn restart_replays_only_committed_directory_commands() {
    let t = Temp::new();
    let mut n = open(&t, Arc::new(ManualClock::new()));
    n.step("a".into(), append(history(), 2)).unwrap();
    assert_eq!(n.committed().unwrap().len(), 2);
    drop(n);
    let mut n = open(&t, Arc::new(ManualClock::new()));
    assert_eq!(n.log(), history());
    assert_eq!(n.commit_index(), 2);
    let mut sm = DirectorySm::default();
    for e in n.committed().unwrap() {
        sm.apply(&e.cmd).unwrap();
    }
    assert_eq!(sm.version, 2);
    assert_eq!(sm.nodes.len(), 2);
    assert!(!sm.nodes.contains_key("n3"));
    assert!(n.committed().unwrap().is_empty());
    n.step("a".into(), append(history(), 3)).unwrap();
    for e in n.committed().unwrap() {
        sm.apply(&e.cmd).unwrap();
    }
    assert_eq!(sm.version, 3);
    assert_eq!(sm.nodes["n3"].endpoints, ["tcp://n3:7000"]);
}

#[test]
fn local_proposal_is_durable_but_not_committed_without_quorum() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut n = open(&t, clock.clone());
    clock.advance(1400);
    n.tick().unwrap();
    n.outbox().unwrap();
    n.step(
        "a".into(),
        RaftRpc::RequestVoteResp {
            term: 1,
            vote_granted: true,
        },
    )
    .unwrap();
    assert_eq!(n.role(), Role::Leader);
    assert_eq!(n.propose(command("local")).unwrap(), 1);
    assert!(n.committed().unwrap().is_empty());
    drop(n);
    let mut n = open(&t, Arc::new(ManualClock::new()));
    assert_eq!(n.log()[0].cmd, command("local"));
    assert_eq!(n.commit_index(), 0);
    assert_eq!(n.voted_for(), Some("b"));
    grant(&mut n, "a", 1, false);
}

#[test]
fn solo_commit_and_recovery_rebuild_exact_directory_state() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut n = DurableRaft::open(&t.0, "solo", "s", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    n.tick().unwrap();
    for i in 0..12 {
        n.propose(command(&format!("n{i}"))).unwrap();
    }
    let mut before = DirectorySm::default();
    for e in n.committed().unwrap() {
        before.apply(&e.cmd).unwrap();
    }
    drop(n);
    let mut n = DurableRaft::open(&t.0, "solo", "s", vec![], clock).unwrap();
    assert_eq!(n.role(), Role::Follower);
    let mut after = DirectorySm::default();
    for e in n.committed().unwrap() {
        after.apply(&e.cmd).unwrap();
    }
    assert_eq!(after, before);
    assert_eq!(after.version, 12);
}

#[test]
fn binding_and_corruption_fail_without_rewriting_evidence() {
    let t = Temp::new();
    drop(open(&t, Arc::new(ManualClock::new())));
    let main = t.0.join("raft.state");
    let original = std::fs::read(&main).unwrap();
    for (cluster, id, peers) in [
        ("other", "b", vec!["a", "c"]),
        ("cluster", "other", vec!["a", "c"]),
        ("cluster", "b", vec!["a"]),
    ] {
        assert!(DurableRaft::open(
            &t.0,
            cluster,
            id,
            peers.into_iter().map(String::from).collect(),
            Arc::new(ManualClock::new())
        )
        .is_err());
        assert_eq!(std::fs::read(&main).unwrap(), original);
    }
    // Canonical member ordering and duplicate/self entries preserve the same binding.
    drop(
        DurableRaft::open(
            &t.0,
            "cluster",
            "b",
            vec!["c".into(), "b".into(), "a".into(), "a".into()],
            Arc::new(ManualClock::new()),
        )
        .unwrap(),
    );
    for bytes in [original[..original.len() - 1].to_vec(), {
        let mut b = original.clone();
        *b.last_mut().unwrap() ^= 1;
        b
    }] {
        std::fs::write(&main, &bytes).unwrap();
        assert!(DurableRaft::open(
            &t.0,
            "cluster",
            "b",
            vec!["a".into(), "c".into()],
            Arc::new(ManualClock::new())
        )
        .is_err());
        assert_eq!(std::fs::read(&main).unwrap(), bytes);
    }
}

#[test]
fn write_failure_fences_queued_rpc_and_commit_until_reopen() {
    let t = Temp::new();
    let mut n = open(&t, Arc::new(ManualClock::new()));
    n.step("a".into(), vote("a", 1)).unwrap(); // Leave a previously queued RPC.
    std::fs::write(t.0.join("raft.next"), b"interrupted").unwrap();
    assert!(n.step("a".into(), append(history(), 3)).is_err());
    assert!(n.is_closed());
    assert_eq!(n.current_term(), 1);
    assert_eq!(n.commit_index(), 0);
    assert!(n.log().is_empty());
    assert_eq!(n.role(), Role::Follower);
    assert_eq!(n.leader_hint(), None);
    assert!(matches!(n.outbox(), Err(RaftStorageError::Closed)));
    assert!(matches!(n.committed(), Err(RaftStorageError::Closed)));
    assert!(matches!(n.tick(), Err(RaftStorageError::Closed)));
    assert!(matches!(
        n.propose(command("x")),
        Err(RaftStorageError::Closed)
    ));
    drop(n);
    let n = open(&t, Arc::new(ManualClock::new()));
    assert_eq!(n.current_term(), 1);
    assert_eq!(n.voted_for(), Some("a"));
    assert!(n.log().is_empty());
    assert!(!t.0.join("raft.next").exists());
}

#[test]
fn interrupted_initial_state_is_never_reset() {
    let t = Temp::new();
    std::fs::write(t.0.join("raft.next"), b"torn initial write").unwrap();
    assert!(DurableRaft::open(&t.0, "cluster", "b", vec![], Arc::new(ManualClock::new())).is_err());
    assert!(!t.0.join("raft.state").exists());
    assert_eq!(
        std::fs::read(t.0.join("raft.next")).unwrap(),
        b"torn initial write"
    );
}

#[test]
fn command_capacity_rejection_has_no_log_effect() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut n = DurableRaft::open(&t.0, "solo", "s", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    n.tick().unwrap();
    assert!(matches!(
        n.propose(vec![0; 1024 * 1024 + 1]),
        Err(RaftStorageError::Capacity)
    ));
    assert!(n.log().is_empty());
    assert_eq!(n.propose(command("ok")).unwrap(), 1);
}

#[cfg(unix)]
#[test]
fn owner_lock_is_cross_process_and_sigkill_retains_acknowledged_history() {
    const CHILD: &str = "PARROT_RAFT_STORAGE_CHILD";
    if let Ok(path) = std::env::var(CHILD) {
        let dir = PathBuf::from(path);
        let result = DurableRaft::open(
            &dir,
            "cluster",
            "b",
            vec!["a".into(), "c".into()],
            Arc::new(ManualClock::new()),
        );
        if std::env::var_os("PARROT_RAFT_LOCK_ONLY").is_some() {
            assert!(result.is_err());
            return;
        }
        let mut n = result.unwrap();
        grant(&mut n, "a", 2, true);
        n.step("a".into(), append(history(), 2)).unwrap();
        let replies = n.outbox().unwrap();
        assert!(matches!(
            replies.as_slice(),
            [(
                _,
                RaftRpc::AppendEntriesResp {
                    success: true,
                    match_index: 3,
                    ..
                }
            )]
        ));
        std::fs::write(dir.join("ready"), b"synced").unwrap();
        loop {
            std::thread::park();
        }
    }
    let t = Temp::new();
    let n = open(&t, Arc::new(ManualClock::new()));
    let child = || {
        let mut c = std::process::Command::new(std::env::current_exe().unwrap());
        c.args([
            "--exact",
            "owner_lock_is_cross_process_and_sigkill_retains_acknowledged_history",
            "--nocapture",
        ])
        .env(CHILD, &t.0);
        c
    };
    assert!(child()
        .env("PARROT_RAFT_LOCK_ONLY", "1")
        .status()
        .unwrap()
        .success());
    drop(n);
    let mut child = child().spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !t.0.join("ready").exists() {
        if child.try_wait().unwrap().is_some() || std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not confirm synced history");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child.kill().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    let mut n = open(&t, Arc::new(ManualClock::new()));
    assert_eq!(n.current_term(), 2);
    assert_eq!(n.voted_for(), Some("a"));
    assert_eq!(n.log(), history());
    assert_eq!(n.committed().unwrap(), history()[..2]);
    grant(&mut n, "c", 2, false);
}

#[test]
fn recovered_uncommitted_suffix_can_be_replaced_but_committed_prefix_cannot() {
    let t = Temp::new();
    let mut n = open(&t, Arc::new(ManualClock::new()));
    n.step("a".into(), append(history(), 1)).unwrap();
    drop(n);
    let mut n = open(&t, Arc::new(ManualClock::new()));
    let prefix = n.committed().unwrap();
    n.step(
        "a".into(),
        RaftRpc::AppendEntries {
            term: 3,
            leader: "a".into(),
            prev_log_index: 1,
            prev_log_term: 2,
            entries: vec![LogEntry {
                term: 3,
                index: 2,
                cmd: command("replacement"),
            }],
            leader_commit: 2,
        },
    )
    .unwrap();
    assert_eq!(n.committed().unwrap()[0].cmd, command("replacement"));
    assert_eq!(n.log().len(), 2);
    n.outbox().unwrap();
    let before = n.log().to_vec();
    n.step(
        "a".into(),
        RaftRpc::AppendEntries {
            term: 4,
            leader: "a".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![LogEntry {
                term: 4,
                index: 1,
                cmd: command("overwrite"),
            }],
            leader_commit: 1,
        },
    )
    .unwrap();
    assert!(matches!(
        n.outbox().unwrap().as_slice(),
        [(_, RaftRpc::AppendEntriesResp { success: false, .. })]
    ));
    assert_eq!(n.log(), before);
    drop(n);
    let mut n = open(&t, Arc::new(ManualClock::new()));
    assert_eq!(n.log(), before);
    assert_eq!(n.committed().unwrap()[0], prefix[0]);
}

#[test]
fn three_disk_owners_recover_and_commit_again_after_all_restart() {
    use std::collections::BTreeMap;
    fn flush(nodes: &mut BTreeMap<String, DurableRaft>) {
        for _ in 0..128 {
            let mut messages = Vec::new();
            for (from, n) in nodes.iter_mut() {
                for (to, rpc) in n.outbox().unwrap() {
                    messages.push((from.clone(), to, rpc));
                }
            }
            if messages.is_empty() {
                return;
            }
            for (from, to, rpc) in messages {
                nodes.get_mut(&to).unwrap().step(from, rpc).unwrap();
            }
        }
        panic!("replication failed to quiesce");
    }
    fn elect(nodes: &mut BTreeMap<String, DurableRaft>, clock: &ManualClock) -> String {
        for _ in 0..60 {
            clock.advance(50);
            for n in nodes.values_mut() {
                n.tick().unwrap();
            }
            flush(nodes);
            let leaders: Vec<_> = nodes
                .iter()
                .filter(|(_, n)| n.role() == Role::Leader)
                .map(|(id, _)| id.clone())
                .collect();
            if !leaders.is_empty() {
                assert_eq!(leaders.len(), 1);
                return leaders[0].clone();
            }
        }
        panic!("election did not complete");
    }
    let dirs = [Temp::new(), Temp::new(), Temp::new()];
    let clock = Arc::new(ManualClock::new());
    let ids = ["a", "b", "c"];
    let reopen = || -> BTreeMap<String, DurableRaft> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| {
                (
                    (*id).into(),
                    DurableRaft::open(
                        &dirs[i].0,
                        "three",
                        *id,
                        ids.iter()
                            .filter(|p| *p != id)
                            .map(|s| (*s).into())
                            .collect(),
                        clock.clone(),
                    )
                    .unwrap(),
                )
            })
            .collect()
    };
    let mut nodes = reopen();
    let leader = elect(&mut nodes, &clock);
    let first_term = nodes[&leader].current_term();
    for node in ["first", "second"] {
        nodes
            .get_mut(&leader)
            .unwrap()
            .propose(command(node))
            .unwrap();
        flush(&mut nodes);
    }
    for n in nodes.values_mut() {
        assert_eq!(n.commit_index(), 2);
        assert_eq!(n.committed().unwrap().len(), 2);
    }
    drop(nodes);
    let mut nodes = reopen();
    for n in nodes.values_mut() {
        assert_eq!(n.current_term(), first_term);
        assert_eq!(n.committed().unwrap().len(), 2);
    }
    let leader = elect(&mut nodes, &clock);
    assert!(nodes[&leader].current_term() > first_term);
    nodes
        .get_mut(&leader)
        .unwrap()
        .propose(command("third"))
        .unwrap();
    flush(&mut nodes);
    let expected = nodes[&leader].log().to_vec();
    for n in nodes.values() {
        assert_eq!(n.commit_index(), 3);
        assert_eq!(n.log(), expected);
    }
    drop(nodes);
    for mut n in reopen().into_values() {
        let mut sm = DirectorySm::default();
        for e in n.committed().unwrap() {
            sm.apply(&e.cmd).unwrap();
        }
        assert_eq!(sm.version, 3);
        assert_eq!(sm.nodes.len(), 3);
        for key in ["first", "second", "third"] {
            assert!(sm.nodes.contains_key(key));
        }
    }
}

#[cfg(unix)]
#[test]
fn snapshot_and_lock_symlinks_are_rejected_without_touching_target() {
    for name in ["raft.state", "raft.next", ".parrot-raft.lock"] {
        let t = Temp::new();
        let target = t.0.join("target");
        std::fs::write(&target, b"protected").unwrap();
        std::os::unix::fs::symlink(&target, t.0.join(name)).unwrap();
        assert!(
            DurableRaft::open(&t.0, "solo", "s", vec![], Arc::new(ManualClock::new())).is_err()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"protected");
    }
}

#[test]
fn checksummed_but_invalid_snapshots_are_rejected_without_repair() {
    use sha2::{Digest, Sha256};
    let t = Temp::new();
    drop(open(&t, Arc::new(ManualClock::new())));
    for (term, vote, log, commit) in [
        (2, Some("outsider"), history(), 1),
        (2, None, history(), 4),
        (1, None, history(), 1),
        (
            2,
            None,
            vec![LogEntry {
                term: 2,
                index: 2,
                cmd: vec![],
            }],
            0,
        ),
        (
            2,
            None,
            vec![LogEntry {
                term: 0,
                index: 1,
                cmd: vec![],
            }],
            0,
        ),
        (
            2,
            None,
            vec![
                LogEntry {
                    term: 2,
                    index: 1,
                    cmd: vec![],
                },
                LogEntry {
                    term: 1,
                    index: 2,
                    cmd: vec![],
                },
            ],
            0,
        ),
    ] {
        let body = bincode::serde::encode_to_vec(
            (
                1u8,
                "cluster",
                "b",
                vec!["a", "c"],
                term as u64,
                vote,
                log,
                commit as u64,
            ),
            bincode::config::standard(),
        )
        .unwrap();
        let mut bytes = b"PRAFT01\0".to_vec();
        bytes.extend_from_slice(&Sha256::digest(&body));
        bytes.extend_from_slice(&body);
        std::fs::write(t.0.join("raft.state"), &bytes).unwrap();
        assert!(DurableRaft::open(
            &t.0,
            "cluster",
            "b",
            vec!["a".into(), "c".into()],
            Arc::new(ManualClock::new())
        )
        .is_err());
        assert_eq!(std::fs::read(t.0.join("raft.state")).unwrap(), bytes);
    }
}

#[test]
fn full_snapshot_capacity_rejects_before_mutation_and_keeps_owner_usable() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut n = DurableRaft::open(&t.0, "solo", "s", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    n.tick().unwrap();
    for seq in 1..=15 {
        assert_eq!(n.propose(vec![seq as u8; 1024 * 1024]).unwrap(), seq);
        if seq < 15 {
            n.committed().unwrap();
        }
    }
    let before = std::fs::read(t.0.join("raft.state")).unwrap();
    assert!(matches!(
        n.propose(vec![16; 1024 * 1024]),
        Err(RaftStorageError::Capacity)
    ));
    assert!(
        !n.is_closed(),
        "capacity is admission failure, not disk failure"
    );
    assert_eq!(std::fs::read(t.0.join("raft.state")).unwrap(), before);
    assert!(!t.0.join("raft.next").exists());
    assert_eq!(n.role(), Role::Leader);
    assert_eq!(n.leader_hint(), Some("s"));
    assert_eq!(n.commit_index(), 15);
    assert_eq!(n.log().len(), 15);
    let pending = n.committed().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].index, 15,
        "rejection must preserve pending apply"
    );
    assert_eq!(n.propose(vec![16]).unwrap(), 16);
    drop(n);
    let mut n = DurableRaft::open(&t.0, "solo", "s", vec![], clock).unwrap();
    let replay = n.committed().unwrap();
    assert_eq!(replay.len(), 16);
    for (i, e) in replay[..15].iter().enumerate() {
        assert_eq!(e.cmd, vec![(i + 1) as u8; 1024 * 1024]);
    }
    assert_eq!(replay[15].cmd, vec![16]);
}

#[test]
fn capacity_admission_reserves_space_for_later_term_vote_and_commit_metadata() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let long_peer = "v".repeat(1024);
    let peers = vec!["a".into(), long_peer.clone()];
    let mut n = DurableRaft::open(&t.0, "margin", "s", peers.clone(), clock.clone()).unwrap();
    clock.advance(1400);
    n.tick().unwrap();
    n.step(
        "a".into(),
        RaftRpc::RequestVoteResp {
            term: 1,
            vote_granted: true,
        },
    )
    .unwrap();
    n.outbox().unwrap();
    for _ in 0..15 {
        n.propose(vec![7; 1024 * 1024]).unwrap();
        n.outbox().unwrap();
    }
    let before = std::fs::read(t.0.join("raft.state")).unwrap();
    let candidate = LogEntry {
        index: 16,
        term: 1,
        cmd: vec![8; 16 * 1024 * 1024 - before.len() - 64],
    };
    let projected: Vec<_> = n.log().iter().chain(std::iter::once(&candidate)).collect();
    // Independently encode the actual v1 snapshot: this fits today's short
    // vote and term, but leaves too little space for a later valid long voter.
    let actual = bincode::serde::encode_to_vec(
        (1u8, "margin", "s", &peers, 1u64, Some("s"), projected, 0u64),
        bincode::config::standard(),
    )
    .unwrap();
    assert!(actual.len() + 40 <= 16 * 1024 * 1024);
    assert!(matches!(
        n.propose(candidate.cmd.clone()),
        Err(RaftStorageError::Capacity)
    ));
    assert!(!n.is_closed());
    assert_eq!(std::fs::read(t.0.join("raft.state")).unwrap(), before);
    let mut smaller = candidate.cmd;
    smaller.truncate(smaller.len() - 2048);
    assert_eq!(n.propose(smaller).unwrap(), 16);
    n.outbox().unwrap();
    n.step(
        long_peer.clone(),
        RaftRpc::RequestVote {
            term: u64::MAX,
            candidate: long_peer.clone(),
            last_log_index: 16,
            last_log_term: 1,
        },
    )
    .unwrap();
    assert_eq!(n.voted_for(), Some(long_peer.as_str()));
    n.step(
        long_peer.clone(),
        RaftRpc::AppendEntries {
            term: u64::MAX,
            leader: long_peer.clone(),
            prev_log_index: 16,
            prev_log_term: 1,
            entries: vec![],
            leader_commit: 16,
        },
    )
    .unwrap();
    assert!(!n.is_closed());
    assert_eq!(n.committed().unwrap().len(), 16);
    drop(n);
    let recovered = DurableRaft::open(&t.0, "margin", "s", peers, clock).unwrap();
    assert_eq!(recovered.current_term(), u64::MAX);
    assert_eq!(recovered.voted_for(), Some(long_peer.as_str()));
    assert_eq!(recovered.commit_index(), 16);
}

#[test]
fn cumulative_append_capacity_preserves_state_and_allows_shorter_replacement() {
    for (count, payload) in [(15u64, vec![7; 1024 * 1024]), (65_536, vec![])] {
        let t = Temp::new();
        let mut n = open(&t, Arc::new(ManualClock::new()));
        let entries: Vec<_> = (1..=count)
            .map(|index| LogEntry {
                index,
                term: 2,
                cmd: payload.clone(),
            })
            .collect();
        n.step("a".into(), append(entries, 0)).unwrap();
        let queued = n.outbox().unwrap();
        assert!(matches!(
            queued.as_slice(),
            [(_, RaftRpc::AppendEntriesResp { success: true, .. })]
        ));
        let before = std::fs::read(t.0.join("raft.state")).unwrap();
        // Each RPC is individually within limits; only the resulting log is too large.
        let error = n
            .step(
                "a".into(),
                RaftRpc::AppendEntries {
                    term: 3,
                    leader: "a".into(),
                    prev_log_index: count,
                    prev_log_term: 2,
                    entries: vec![LogEntry {
                        index: count + 1,
                        term: 3,
                        cmd: payload,
                    }],
                    leader_commit: count + 1,
                },
            )
            .unwrap_err();
        assert!(matches!(error, RaftStorageError::Capacity));
        assert!(
            !n.is_closed(),
            "cumulative capacity must reject before mutation"
        );
        assert_eq!(n.current_term(), 2);
        assert_eq!(n.log().len() as u64, count);
        assert_eq!(n.commit_index(), 0);
        assert!(n.committed().unwrap().is_empty());
        assert!(n.outbox().unwrap().is_empty());
        assert_eq!(std::fs::read(t.0.join("raft.state")).unwrap(), before);
        assert!(!t.0.join("raft.next").exists());
        // Capacity must be calculated from the replacement, not old + incoming.
        n.step(
            "a".into(),
            RaftRpc::AppendEntries {
                term: 3,
                leader: "a".into(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![LogEntry {
                    index: 1,
                    term: 3,
                    cmd: vec![9],
                }],
                leader_commit: 1,
            },
        )
        .unwrap();
        assert_eq!(n.log().len(), 1);
        assert_eq!(n.committed().unwrap()[0].cmd, vec![9]);
        drop(n);
        let recovered = open(&t, Arc::new(ManualClock::new()));
        assert_eq!(recovered.current_term(), 3);
        assert_eq!(recovered.commit_index(), 1);
        assert_eq!(recovered.log().len(), 1);
    }
}
