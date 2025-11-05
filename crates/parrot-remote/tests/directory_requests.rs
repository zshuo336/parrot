use parrot_remote::{
    raft::{LogEntry, ManualClock, RaftRpc, Role},
    roles::directory::{
        directory_command_digest, DirCmd, DirQuery, DirectoryRequestStatus as Status,
        DurableDirectory,
    },
};
use std::{path::PathBuf, sync::Arc};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "parrot-requests-{}-{}",
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
fn put(n: &str) -> DirCmd {
    DirCmd::Upsert {
        node: n.into(),
        endpoints: vec![format!("mem://{n}")],
    }
}
fn query(n: &str) -> DirQuery {
    DirQuery::Resolve {
        prefix_or_node: n.into(),
    }
}

#[test]
fn persistent_identity_survives_reply_loss_restart_conflicts_and_client_isolation() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = DurableDirectory::open(&t.0, "g", "solo", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    d.tick().unwrap();
    d.propose(&put("legacy")).unwrap();
    let original = d.submit_request("operator", "r1", &put("worker")).unwrap();
    assert!(matches!(
        original,
        Status::Applied {
            index: 2,
            revision: 2,
            ..
        }
    ));
    for _ in 0..10 {
        assert_eq!(
            d.submit_request("operator", "r1", &put("worker")).unwrap(),
            original
        );
    }
    assert_eq!(
        d.submit_request("operator", "r1", &put("different"))
            .unwrap(),
        Status::Conflict
    );
    assert_eq!(d.applied_index(), 2);
    assert_eq!(d.state_local().unwrap().version, 2);
    let before = d.state_local().unwrap().clone();
    drop(d);
    let mut d = DurableDirectory::open(&t.0, "g", "solo", vec![], clock.clone()).unwrap();
    assert_eq!(d.role(), Role::Follower);
    assert_eq!(d.state_local().unwrap(), &before);
    // The caller deliberately discarded the original transport reply. The same
    // identity reconstructs its exact result even before this replica is leader.
    assert_eq!(
        d.submit_request("operator", "r1", &put("worker")).unwrap(),
        original
    );
    assert_eq!(
        d.request_status(
            "operator",
            "r1",
            directory_command_digest(&put("different")).unwrap()
        )
        .unwrap(),
        Status::Conflict
    );
    clock.advance(1400);
    d.tick().unwrap();
    assert!(matches!(
        d.submit_request("other", "r1", &put("worker")).unwrap(),
        Status::Applied {
            index: 3,
            revision: 3,
            ..
        }
    ));
    assert_eq!(d.query_local(&query("worker")).unwrap().unwrap().version, 3);
}
// Independent encoder for the documented persisted envelope. This exercises
// replay of duplicate/conflicting entries, not only the public dedup fast path.
fn encoded(client: &str, id: &str, command: DirCmd) -> Vec<u8> {
    let mut bytes = b"\xffDIR\x01".to_vec();
    bincode::serde::encode_into_std_write(
        (client, id, command),
        &mut bytes,
        bincode::config::standard(),
    )
    .unwrap();
    bytes
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
fn replicated_duplicate_and_conflicting_envelopes_apply_business_once_and_replay_exactly() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let open = || DurableDirectory::open(&t.0, "g", "b", vec!["a".into()], clock.clone()).unwrap();
    let mut d = open();
    let entries: Vec<_> = [put("first"), put("first"), put("conflict")]
        .into_iter()
        .enumerate()
        .map(|(i, c)| LogEntry {
            index: i as u64 + 1,
            term: 2,
            cmd: encoded("operator", "id", c),
        })
        .collect();
    d.step("a".into(), append(entries, 3)).unwrap();
    assert_eq!(d.applied_index(), 3);
    assert_eq!(d.state_local().unwrap().version, 1);
    assert!(d.query_local(&query("conflict")).unwrap().is_none());
    let expected = Status::Applied {
        index: 1,
        term: 2,
        revision: 1,
    };
    assert_eq!(
        d.request_status(
            "operator",
            "id",
            directory_command_digest(&put("first")).unwrap()
        )
        .unwrap(),
        expected
    );
    drop(d);
    let d = open();
    assert_eq!(d.applied_index(), 3);
    assert_eq!(d.state_local().unwrap().version, 1);
    assert_eq!(
        d.request_status(
            "operator",
            "id",
            directory_command_digest(&put("first")).unwrap()
        )
        .unwrap(),
        expected
    );
}
#[test]
fn retry_on_new_leader_commits_old_pending_request_with_one_current_term_confirmation() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = DurableDirectory::open(&t.0, "g", "b", vec!["a".into()], clock.clone()).unwrap();
    d.step(
        "a".into(),
        append(
            vec![LogEntry {
                index: 1,
                term: 2,
                cmd: encoded("operator", "retry", put("worker")),
            }],
            0,
        ),
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
    assert_eq!(d.role(), Role::Leader);
    let pending = d
        .submit_request("operator", "retry", &put("worker"))
        .unwrap();
    assert_eq!(pending, Status::Pending { index: 2, term });
    assert_eq!(
        d.submit_request("operator", "retry", &put("worker"))
            .unwrap(),
        pending
    );
    assert_eq!(d.state_local().unwrap().version, 0);
    d.step(
        "a".into(),
        RaftRpc::AppendEntriesResp {
            term,
            success: true,
            match_index: 2,
        },
    )
    .unwrap();
    assert_eq!(d.applied_index(), 2);
    assert_eq!(d.state_local().unwrap().version, 1);
    assert_eq!(
        d.submit_request("operator", "retry", &put("worker"))
            .unwrap(),
        Status::Applied {
            index: 1,
            term: 2,
            revision: 1
        }
    );
}
#[test]
fn invalid_envelopes_and_capacity_fail_before_log_or_business_changes() {
    let t = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut d = DurableDirectory::open(&t.0, "g", "b", vec!["a".into()], clock).unwrap();
    let good = LogEntry {
        index: 1,
        term: 2,
        cmd: encoded("operator", "good", put("worker")),
    };
    for bad in [
        encoded("operator", "", put("worker")),
        {
            let mut b = encoded("operator", "bad", put("worker"));
            b.push(0);
            b
        },
        vec![0xff; 1024 * 1024 + 1],
    ] {
        assert!(d
            .step(
                "a".into(),
                append(
                    vec![
                        good.clone(),
                        LogEntry {
                            index: 2,
                            term: 2,
                            cmd: bad
                        }
                    ],
                    2
                )
            )
            .is_err());
        assert_eq!(d.applied_index(), 0);
        assert_eq!(d.state_local().unwrap().version, 0);
    }
    let path = Temp::new();
    let clock = Arc::new(ManualClock::new());
    let mut solo = DurableDirectory::open(&path.0, "g", "s", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    solo.tick().unwrap();
    assert!(solo.submit_request("operator", "", &put("worker")).is_err());
    let mut endpoints = vec!["x".repeat(4096); 255];
    endpoints.push("x".repeat(3000));
    let large = DirCmd::Upsert {
        node: "worker".into(),
        endpoints,
    };
    assert!(large.encode().unwrap().len() < 1024 * 1024);
    assert!(solo
        .submit_request(&"c".repeat(1024), &"r".repeat(128), &large)
        .is_err());
    assert_eq!(solo.applied_index(), 0);
    assert_eq!(solo.state_local().unwrap().version, 0);
}
