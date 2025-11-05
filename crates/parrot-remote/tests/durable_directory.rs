use parrot_remote::{
    raft::{DurableRaft, LogEntry, ManualClock, RaftRpc, Role},
    roles::directory::{
        DirCmd, DirQuery, DirectorySm, DurableDirectory, DurableDirectoryStatus as Status,
    },
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "parrot-directory-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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
fn put(node: &str) -> DirCmd {
    DirCmd::Upsert {
        node: node.into(),
        endpoints: vec![format!("tcp://{node}:7")],
    }
}
fn query(node: &str) -> DirQuery {
    DirQuery::Resolve {
        prefix_or_node: node.into(),
    }
}
fn open(t: &Temp, clock: Arc<ManualClock>) -> DurableDirectory {
    DurableDirectory::open(&t.0, "cluster", "b", vec!["a".into(), "c".into()], clock).unwrap()
}
fn entry(index: u64, cmd: DirCmd) -> LogEntry {
    LogEntry {
        term: 2,
        index,
        cmd: cmd.encode().unwrap(),
    }
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

#[test]
fn solo_automatically_applies_and_recovers_complete_directory_state() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = DurableDirectory::open(&t.0, "solo", "s", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    d.tick().unwrap();
    let commands = [
        put("n1"),
        put("n2"),
        DirCmd::BorderDeclare {
            prefix: "parrot://region/".into(),
            node: "n1".into(),
        },
        DirCmd::KeyAggregate {
            key: "service".into(),
            nodes: vec!["n1".into(), "n2".into()],
        },
        DirCmd::Remove { node: "n1".into() },
    ];
    let mut receipt = None;
    for c in commands {
        let r = d.propose(&c).unwrap();
        assert_eq!(d.proposal_status(&r).unwrap(), Status::Applied);
        receipt = Some(r);
    }
    let before = d.state_local().unwrap().clone();
    assert_eq!(before.version, 5);
    assert!(before.borders.is_empty());
    assert_eq!(before.key_index["service"], ["n2"]);
    drop(d);
    let d = DurableDirectory::open(&t.0, "solo", "s", vec![], clock).unwrap();
    assert_eq!(d.role(), Role::Follower);
    assert_eq!(d.applied_index(), 5);
    assert_eq!(d.state_local().unwrap(), &before);
    assert_eq!(
        d.proposal_status(&receipt.unwrap()).unwrap(),
        Status::Foreign
    );
}

#[test]
fn follower_replays_only_committed_prefix_and_duplicate_rpc_does_not_reapply() {
    let t = Temp::new();
    let mut d = open(&t, Arc::new(ManualClock::new()));
    let log = vec![entry(1, put("one")), entry(2, put("two"))];
    d.step("a".into(), append(log.clone(), 1)).unwrap();
    assert_eq!(d.applied_index(), 1);
    assert!(d.query_local(&query("two")).unwrap().is_none());
    drop(d);
    let mut d = open(&t, Arc::new(ManualClock::new()));
    assert_eq!(d.applied_index(), 1);
    assert_eq!(d.state_local().unwrap().version, 1);
    d.step("a".into(), append(log.clone(), 1)).unwrap();
    assert_eq!(d.state_local().unwrap().version, 1);
    d.step("a".into(), append(log, 2)).unwrap();
    assert_eq!(d.state_local().unwrap().version, 2);
    assert_eq!(
        d.query_local(&query("two")).unwrap().unwrap().endpoints,
        ["tcp://two:7"]
    );
}

#[test]
fn invalid_append_batch_is_rejected_before_persistence_or_success_response() {
    let t = Temp::new();
    let mut d = open(&t, Arc::new(ManualClock::new()));
    let before = std::fs::read(t.0.join("raft.state")).unwrap();
    let mut bad = entry(2, put("two"));
    bad.cmd.push(0);
    assert!(d
        .step("a".into(), append(vec![entry(1, put("one")), bad], 2))
        .is_err());
    assert_eq!(d.current_term(), 0);
    assert!(d.outbox().unwrap().is_empty());
    assert_eq!(d.state_local().unwrap(), &DirectorySm::default());
    assert_eq!(std::fs::read(t.0.join("raft.state")).unwrap(), before);
    d.step("a".into(), append(vec![entry(1, put("ok"))], 1))
        .unwrap();
    assert_eq!(d.applied_index(), 1);
}

#[test]
fn malformed_recovery_including_uncommitted_tail_preserves_files() {
    for commit in [0, 1] {
        let t = Temp::new();
        // Write a valid generic Raft snapshot with an invalid application entry.
        let mut raft = DurableRaft::open(
            &t.0,
            "directory-v1:cluster",
            "b",
            vec!["a".into(), "c".into()],
            Arc::new(ManualClock::new()),
        )
        .unwrap();
        raft.step(
            "a".into(),
            append(
                vec![LogEntry {
                    term: 2,
                    index: 1,
                    cmd: vec![255],
                }],
                commit,
            ),
        )
        .unwrap();
        drop(raft);
        let before = std::fs::read(t.0.join("raft.state")).unwrap();
        std::fs::write(t.0.join("raft.next"), b"abandoned evidence").unwrap();
        assert!(DurableDirectory::open(
            &t.0,
            "cluster",
            "b",
            vec!["a".into(), "c".into()],
            Arc::new(ManualClock::new())
        )
        .is_err());
        assert_eq!(std::fs::read(t.0.join("raft.state")).unwrap(), before);
        assert_eq!(
            std::fs::read(t.0.join("raft.next")).unwrap(),
            b"abandoned evidence"
        );
    }
}

#[test]
fn generic_raft_directory_is_not_silently_adopted_and_binding_is_enforced() {
    let t = Temp::new();
    drop(DurableRaft::open(&t.0, "cluster", "b", vec![], Arc::new(ManualClock::new())).unwrap());
    let before = std::fs::read(t.0.join("raft.state")).unwrap();
    assert!(
        DurableDirectory::open(&t.0, "cluster", "b", vec![], Arc::new(ManualClock::new())).is_err()
    );
    assert_eq!(std::fs::read(t.0.join("raft.state")).unwrap(), before);
    let t = Temp::new();
    drop(open(&t, Arc::new(ManualClock::new())));
    assert!(DurableDirectory::open(
        &t.0,
        "other",
        "b",
        vec!["a".into(), "c".into()],
        Arc::new(ManualClock::new())
    )
    .is_err());
    assert!(DurableDirectory::open(
        &t.0,
        "cluster",
        "b",
        vec!["a".into()],
        Arc::new(ManualClock::new())
    )
    .is_err());
}

#[test]
fn storage_failure_fences_queries_rpc_and_proposals_until_recovery() {
    let t = Temp::new();
    let mut d = open(&t, Arc::new(ManualClock::new()));
    d.step("a".into(), append(vec![entry(1, put("one"))], 1))
        .unwrap();
    d.outbox().unwrap();
    std::fs::write(t.0.join("raft.next"), b"cannot publish").unwrap();
    assert!(d
        .step(
            "a".into(),
            append(vec![entry(1, put("one")), entry(2, put("two"))], 2)
        )
        .is_err());
    assert!(d.is_closed());
    assert!(d.query_local(&query("one")).is_err());
    assert!(d.state_local().is_err());
    assert!(d.outbox().is_err());
    assert!(d.tick().is_err());
    assert!(d.propose(&put("three")).is_err());
    assert_eq!(d.applied_index(), 1);
    drop(d);
    let d = open(&t, Arc::new(ManualClock::new()));
    assert_eq!(d.applied_index(), 1);
    assert!(d.query_local(&query("two")).unwrap().is_none());
}

#[test]
fn acceptance_receipt_needs_quorum_and_cannot_be_satisfied_by_replaced_index() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = open(&t, clock.clone());
    clock.advance(1400);
    d.tick().unwrap();
    d.step(
        "a".into(),
        RaftRpc::RequestVoteResp {
            term: 1,
            vote_granted: true,
        },
    )
    .unwrap();
    let r = d.propose(&put("original")).unwrap();
    assert_eq!(d.proposal_status(&r).unwrap(), Status::Pending);
    assert!(d.query_local(&query("original")).unwrap().is_none());
    d.step("a".into(), append(vec![entry(1, put("replacement"))], 1))
        .unwrap();
    assert_eq!(d.proposal_status(&r).unwrap(), Status::Conflict);
    assert!(d.query_local(&query("replacement")).unwrap().is_some());
}

#[test]
fn truncated_receipt_is_conflict_even_before_its_index_is_reused() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = open(&t, clock.clone());
    clock.advance(1400);
    d.tick().unwrap();
    d.step(
        "a".into(),
        RaftRpc::RequestVoteResp {
            term: 1,
            vote_granted: true,
        },
    )
    .unwrap();
    d.propose(&put("discarded-first")).unwrap();
    let receipt = d.propose(&put("discarded-tail")).unwrap();
    assert_eq!(receipt.index(), 2);
    assert_eq!(d.proposal_status(&receipt).unwrap(), Status::Pending);
    // A higher term alone does not prove loss: keep the original pending log.
    d.step(
        "a".into(),
        RaftRpc::RequestVoteResp {
            term: 2,
            vote_granted: false,
        },
    )
    .unwrap();
    assert_eq!(d.proposal_status(&receipt).unwrap(), Status::Pending);
    // Replacing index 1 removes index 2 without immediately reusing it.
    d.step("a".into(), append(vec![entry(1, put("replacement"))], 1))
        .unwrap();
    assert_eq!(d.proposal_status(&receipt).unwrap(), Status::Conflict);
    assert!(d.query_local(&query("discarded-tail")).unwrap().is_none());
    assert!(d.query_local(&query("replacement")).unwrap().is_some());
    // Reusing the old position in a new term must not resurrect the old receipt.
    d.step(
        "a".into(),
        append(
            vec![entry(1, put("replacement")), entry(2, put("new-tail"))],
            2,
        ),
    )
    .unwrap();
    assert_eq!(d.proposal_status(&receipt).unwrap(), Status::Conflict);
    drop(d);
    let d = open(&t, clock);
    assert_eq!(d.proposal_status(&receipt).unwrap(), Status::Foreign);
    assert_eq!(d.applied_index(), 2);
    assert!(d.query_local(&query("discarded-tail")).unwrap().is_none());
    assert!(d.query_local(&query("new-tail")).unwrap().is_some());
}

#[test]
fn three_durable_directory_groups_recover_and_continue_exact_business_history() {
    use std::collections::BTreeMap;
    fn flush(nodes: &mut BTreeMap<String, DurableDirectory>) {
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
        panic!("not quiescent");
    }
    fn elect(nodes: &mut BTreeMap<String, DurableDirectory>, clock: &ManualClock) -> String {
        for _ in 0..60 {
            clock.advance(50);
            for n in nodes.values_mut() {
                n.tick().unwrap();
            }
            flush(nodes);
            let ids: Vec<_> = nodes
                .iter()
                .filter(|(_, n)| n.role() == Role::Leader)
                .map(|(id, _)| id.clone())
                .collect();
            if !ids.is_empty() {
                assert_eq!(ids.len(), 1);
                return ids[0].clone();
            }
        }
        panic!("no leader");
    }
    let dirs = [Temp::new(), Temp::new(), Temp::new()];
    let ids = ["a", "b", "c"];
    let clock = Arc::new(ManualClock::new());
    let reopen = || -> BTreeMap<String, DurableDirectory> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| {
                (
                    (*id).into(),
                    DurableDirectory::open(
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
    let r = nodes
        .get_mut(&leader)
        .unwrap()
        .propose(&put("first"))
        .unwrap();
    assert_eq!(nodes[&leader].proposal_status(&r).unwrap(), Status::Pending);
    flush(&mut nodes);
    assert_eq!(nodes[&leader].proposal_status(&r).unwrap(), Status::Applied);
    for n in nodes.values() {
        assert_eq!(n.applied_index(), 1);
        assert_eq!(n.state_local().unwrap().version, 1);
    }
    drop(nodes);
    let mut nodes = reopen();
    let leader = elect(&mut nodes, &clock);
    for c in [
        put("second"),
        DirCmd::Remove {
            node: "first".into(),
        },
    ] {
        nodes.get_mut(&leader).unwrap().propose(&c).unwrap();
        flush(&mut nodes);
    }
    let expected = nodes[&leader].state_local().unwrap().clone();
    assert_eq!(expected.version, 3);
    assert_eq!(expected.nodes.len(), 1);
    assert!(expected.nodes.contains_key("second"));
    drop(nodes);
    for n in reopen().values() {
        assert_eq!(n.state_local().unwrap(), &expected);
        assert_eq!(n.applied_index(), 3);
    }
}

#[cfg(unix)]
#[test]
fn sigkill_after_applied_receipt_recovers_exact_directory_and_releases_lock() {
    const CHILD: &str = "PARROT_DURABLE_DIRECTORY_CHILD";
    if let Ok(p) = std::env::var(CHILD) {
        let clock = Arc::new(ManualClock::new());
        let mut d = DurableDirectory::open(&p, "solo", "s", vec![], clock.clone()).unwrap();
        clock.advance(1400);
        d.tick().unwrap();
        for i in 0..8 {
            let r = d.propose(&put(&format!("n{i}"))).unwrap();
            assert_eq!(d.proposal_status(&r).unwrap(), Status::Applied);
        }
        std::fs::write(PathBuf::from(p).join("ready"), b"applied").unwrap();
        loop {
            std::thread::park();
        }
    }
    let t = Temp::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sigkill_after_applied_receipt_recovers_exact_directory_and_releases_lock",
            "--nocapture",
        ])
        .env(CHILD, &t.0)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !t.0.join("ready").exists() {
        if child.try_wait().unwrap().is_some() || std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not confirm applied receipt");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        DurableDirectory::open(&t.0, "solo", "s", vec![], Arc::new(ManualClock::new())).is_err()
    );
    child.kill().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    let d =
        DurableDirectory::open(&t.0, "solo", "s", vec![], Arc::new(ManualClock::new())).unwrap();
    assert_eq!(d.applied_index(), 8);
    assert_eq!(d.state_local().unwrap().version, 8);
    assert_eq!(d.state_local().unwrap().nodes.len(), 8);
    for i in 0..8 {
        assert_eq!(
            d.query_local(&query(&format!("n{i}")))
                .unwrap()
                .unwrap()
                .endpoints,
            [format!("tcp://n{i}:7")]
        );
    }
}
